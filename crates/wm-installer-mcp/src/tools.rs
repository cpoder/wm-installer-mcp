//! Tool registry for the installer server.

use std::path::{Path, PathBuf};

use mcp_rt::{Server, Tool, ToolError, ToolResult};
use serde_json::{json, Value};
use wm_core::catalog::{Catalog, ProductPath};
use wm_core::deps;
use wm_core::diag;
use wm_core::inventory::Inventory;
use wm_core::runner::{self, Environment};
use wm_core::script::{InstallScript, Severity, Source};

use mcp_rt::args::{flag, opt_i32, opt_str, opt_usize, req_str, str_list};

/// Default installer server for 12.1, as shipped in `sagInstaller.jar`.
const DEFAULT_SERVER_URL: &str = "https://sdc.webmethods.io/cgi-bin/dataservewebM121.cgi";

/// Build the configured server.
pub fn server() -> Server {
    Server::new("wm-installer", env!("CARGO_PKG_VERSION"))
        .instructions(
            "Installs, provisions and patches IBM webMethods without the setup wizard, and \
             drives the product's own tooling — the p2 director, dbConfigurator.sh, \
             is_instance.sh — for everything the installer lays down.\n\n\
             HOW TO USE THIS SERVER. Every tool that changes anything defaults to a dry run. \
             The dry run returns a `settings` list naming each value and whether it came from \
             the caller or from a default. **Show that list to the user in full, ask whether \
             the defaults suit them or they want any changed, and only then call again with \
             `apply: true`.** Never apply on the first call. Ports, instance names, install \
             locations and target platforms all have defaults that are reasonable and often \
             wrong for a given site; the user is the only one who knows which.\n\n\
             Long operations return a job id. Poll it with `job_status`, which reports the \
             phase, bytes fetched against the total, elapsed time and an estimate of what is \
             left — relay that to the user rather than leaving them without feedback for four \
             minutes. A person at a terminal can run `wm-installer-mcp --watch <job-id>` for a \
             live screen. `diagnose_log` explains a failed run.\n\n\
             For a selection, `inventory_read` on a reference installation gives the exact \
             versioned product paths, `native_plan` closes it over its prerequisites and prices \
             the download, and `native_install` performs it.",
        )
        .tool(crate::native::sdc_releases())
        .tool(crate::native::sdc_catalog())
        .tool(crate::native::native_plan())
        .tool(crate::native::profile_provision())
        .tool(crate::native::database_plan())
        .tool(crate::native::database_configure())
        .tool(crate::native::native_install())
        .tool(crate::native::instance_create())
        .tool(crate::native::profile_capture())
        .tool(crate::native::profile_replay())
        .tool(inventory_read())
        .tool(catalog_search())
        .tool(plan_resolve())
        .tool(script_generate())
        .tool(script_validate())
        .tool(image_build())
        .tool(install_run())
        .tool(job_status())
        .tool(diagnose_log())
}

fn wm_home(args: &Value) -> Result<PathBuf, ToolError> {
    opt_str(args, "wm_home")
        .or_else(|| std::env::var("WM_HOME").ok())
        .map(PathBuf::from)
        .ok_or_else(|| ToolError::invalid("no wm_home given and WM_HOME is not set"))
}

fn installer_bin(args: &Value) -> Result<PathBuf, ToolError> {
    let path = opt_str(args, "installer_bin")
        .or_else(|| std::env::var("WM_INSTALLER_BIN").ok())
        .map(PathBuf::from)
        .ok_or_else(|| {
            ToolError::invalid("no installer_bin given and WM_INSTALLER_BIN is not set")
        })?;
    if !path.is_file() {
        return Err(ToolError::invalid(format!(
            "installer not found at {}",
            path.display()
        )));
    }
    Ok(path)
}

use crate::native::jobs_dir;

fn load_catalog(args: &Value) -> Result<(PathBuf, Catalog), ToolError> {
    let home = wm_home(args)?;
    let catalog = Catalog::load(&home).map_err(ToolError::failed)?;
    Ok((home, catalog))
}

/// What happened to each seed the caller supplied.
struct Seeds {
    /// Versioned paths to resolve.
    paths: Vec<String>,
    /// Paths accepted verbatim although the catalogue does not contain them.
    ///
    /// This is not hypothetical: a 12.1 installation carries no `.prop` for
    /// webMethods Flat File even though the package is installed, so its path
    /// has to be supplied literally. Such a product cannot be closed over — it
    /// declares no prerequisites we can read — but it must still reach
    /// `InstallProducts`, so it is kept and flagged rather than dropped.
    external: Vec<String>,
    /// Names that match no product and are not a versioned path either.
    unresolved: Vec<String>,
}

/// Turn seeds that may be component names into versioned product paths.
fn to_paths(catalog: &Catalog, seeds: &[String]) -> Seeds {
    let mut out = Seeds {
        paths: Vec::new(),
        external: Vec::new(),
        unresolved: Vec::new(),
    };
    for seed in seeds {
        if catalog.get(seed).is_some() {
            out.paths.push(seed.clone());
        } else if let Some(path) = catalog.path_of(seed) {
            out.paths.push(path.raw.clone());
        } else if ProductPath::parse(seed).is_ok() {
            out.paths.push(seed.clone());
            out.external.push(seed.clone());
        } else {
            out.unresolved.push(seed.clone());
        }
    }
    out
}

fn environment(args: &Value) -> Environment {
    Environment {
        tmpdir: opt_str(args, "tmpdir").map(PathBuf::from),
        java_options: opt_str(args, "java_options"),
        disable_cpu_detection_test: flag(args, "disable_cpu_detection_test", false),
        extra: args
            .get("env")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
        stdin_feed: None,
        passthrough: str_list(args, "passthrough_env"),
    }
}

fn inventory_read() -> Tool {
    Tool::new(
        "inventory_read",
        "Read an installed webMethods home: products with their exact versioned installer \
         paths, Integration Server instances, platform profiles, and the fix readmes on disk. \
         Needs no credentials and does not touch the installation.",
        json!({
            "type": "object",
            "properties": {
                "wm_home": { "type": "string", "description": "Installation root; defaults to $WM_HOME." },
                "filter": { "type": "string", "description": "Only products whose component, code or group contains this." }
            }
        }),
        Box::new(|args| {
            let home = wm_home(args)?;
            let inventory = Inventory::read(&home).map_err(ToolError::failed)?;
            let products: Vec<_> = match opt_str(args, "filter") {
                Some(needle) => inventory.find(&needle).into_iter().cloned().collect(),
                None => inventory.products.clone(),
            };
            let summary = format!(
                "{}: {} products ({} shown), {} runtimes, {} fix readmes",
                home.display(),
                inventory.products.len(),
                products.len(),
                inventory.runtimes.len(),
                inventory.fixes.len(),
            );
            Ok(ToolResult::structured(
                summary,
                json!({
                    "wm_home": inventory.wm_home,
                    "products": products,
                    "runtimes": inventory.runtimes,
                    "fixes": inventory.fixes,
                }),
            ))
        }),
    )
}

fn catalog_search() -> Tool {
    Tool::new(
        "catalog_search",
        "Search the product catalogue of a reference installation and return the versioned \
         paths to feed to plan_resolve or script_generate, together with each product's \
         declared prerequisites.",
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "wm_home": { "type": "string" },
                "query": { "type": "string", "description": "Substring matched against component, group and product code." },
                "limit": { "type": "integer", "description": "Maximum results (default 50)." }
            }
        }),
        Box::new(|args| {
            let (_, catalog) = load_catalog(args)?;
            let query = req_str(args, "query")?.to_lowercase();
            let limit = opt_usize(args, "limit").unwrap_or(50);
            let hits: Vec<Value> = catalog
                .iter()
                .filter(|p| {
                    p.path.component.to_lowercase().contains(&query)
                        || p.path.group.to_lowercase().contains(&query)
                        || p.path.code().to_lowercase().contains(&query)
                })
                .take(limit)
                .map(|p| {
                    json!({
                        "path": p.path.raw,
                        "component": p.path.component,
                        "group": p.path.group,
                        "code": p.path.code(),
                        "version": p.path.version(),
                        "requires": p.requires,
                    })
                })
                .collect();
            Ok(ToolResult::structured(
                format!(
                    "{} of {} products match {query:?}",
                    hits.len(),
                    catalog.len()
                ),
                json!({ "matches": hits }),
            ))
        }),
    )
}

fn plan_resolve() -> Tool {
    Tool::new(
        "plan_resolve",
        "Close a product selection over its prerequisites. The installer does not do this: \
         -writeImage embeds exactly what you list, and the install then refuses because \
         'products they require do not exist in the image'. Seeds may be component names \
         (TNServer) or full versioned paths. Also injects License Agreement, Java Package and \
         CustomInstall, which every installation needs but nothing declares.",
        json!({
            "type": "object",
            "required": ["seeds"],
            "properties": {
                "wm_home": { "type": "string" },
                "seeds": { "type": "array", "items": { "type": "string" }, "description": "Component names (TNServer) or full versioned paths. A path absent from the catalogue is kept verbatim and reported, since some installed products have no .prop file." },
                "include_mandatory": { "type": "boolean", "description": "Inject the mandatory base products (default true)." }
            }
        }),
        Box::new(|args| {
            let (_, catalog) = load_catalog(args)?;
            let seeds = str_list(args, "seeds");
            if seeds.is_empty() {
                return Err(ToolError::invalid("seeds is empty"));
            }
            let seeds = to_paths(&catalog, &seeds);
            let resolution = deps::resolve(
                &catalog,
                &seeds.paths,
                flag(args, "include_mandatory", true),
            )
            .map_err(ToolError::failed)?;

            let added = resolution.products.len().saturating_sub(seeds.paths.len());
            let mut summary = format!(
                "{} seeds -> {} products (+{} prerequisites)",
                seeds.paths.len(),
                resolution.len(),
                added
            );
            if !seeds.external.is_empty() {
                summary.push_str(&format!(
                    "; {} path(s) kept but absent from the catalogue, so not closed over",
                    seeds.external.len()
                ));
            }
            if !seeds.unresolved.is_empty() {
                summary.push_str(&format!(
                    "; {} seed(s) match no product",
                    seeds.unresolved.len()
                ));
            }
            if !resolution.unsatisfied.is_empty() {
                summary.push_str(&format!(
                    "; {} prerequisite pattern(s) nothing satisfies",
                    resolution.unsatisfied.len()
                ));
            }
            if !resolution.caveats.is_empty() {
                summary.push_str(&format!("; {} caveat(s)", resolution.caveats.len()));
            }
            Ok(ToolResult::structured(
                summary,
                json!({
                    // A path kept verbatim is an advisory, not an incomplete
                    // closure: it is in the selection, we simply cannot read its
                    // own prerequisites. Only a name that matched nothing, or a
                    // prerequisite nothing satisfies, makes the plan unusable.
                    "complete": resolution.unsatisfied.is_empty() && seeds.unresolved.is_empty(),
                    "products": resolution.products,
                    "install_products": resolution.paths(),
                    "external_paths": seeds.external,
                    "unresolved_seeds": seeds.unresolved,
                    "unsatisfied": resolution.unsatisfied,
                    "caveats": resolution.caveats,
                }),
            ))
        }),
    )
}

fn script_generate() -> Tool {
    Tool::new(
        "script_generate",
        "Generate an unattended install script. Two modes: 'server' downloads from IBM and \
         needs ServerURL plus credentials; 'image' installs from a prebuilt image and needs \
         none. Credentials should be left as $NAME$ placeholders, which the installer \
         substitutes from the environment at read time. Validates before returning.",
        json!({
            "type": "object",
            "required": ["install_dir", "products"],
            "properties": {
                "install_dir": { "type": "string", "description": "Target directory, e.g. /opt/webmethods." },
                "products": { "type": "array", "items": { "type": "string" }, "description": "Versioned product paths, normally plan_resolve's install_products." },
                "mode": { "type": "string", "enum": ["server", "image"], "description": "Default server." },
                "image_file": { "type": "string", "description": "Image path; required for mode=image." },
                "server_url": { "type": "string", "description": "Defaults to the 12.1 installer server." },
                "username": { "type": "string", "description": "Default $WM_EMPOWER_USER$." },
                "password": { "type": "string", "description": "Default $WM_EMPOWER_KEY$." },
                "admin_password": { "type": "string", "description": "Default $WM_ADMIN_PASSWORD$." },
                "write_to": { "type": "string", "description": "Also write the script to this path." }
            }
        }),
        Box::new(|args| {
            let install_dir = req_str(args, "install_dir")?;
            let products = str_list(args, "products");
            if products.is_empty() {
                return Err(ToolError::invalid("products is empty"));
            }
            let mode = opt_str(args, "mode").unwrap_or_else(|| "server".into());
            let source = match mode.as_str() {
                "image" => Source::Image {
                    file: opt_str(args, "image_file")
                        .ok_or_else(|| ToolError::invalid("mode=image requires image_file"))?,
                },
                "server" => Source::Server {
                    url: opt_str(args, "server_url").unwrap_or_else(|| DEFAULT_SERVER_URL.into()),
                    username: opt_str(args, "username")
                        .unwrap_or_else(|| "$WM_EMPOWER_USER$".into()),
                    password: opt_str(args, "password")
                        .unwrap_or_else(|| "$WM_EMPOWER_KEY$".into()),
                },
                other => return Err(ToolError::invalid(format!("unknown mode {other:?}"))),
            };
            let script = InstallScript {
                install_dir,
                source,
                admin_password: Some(
                    opt_str(args, "admin_password").unwrap_or_else(|| "$WM_ADMIN_PASSWORD$".into()),
                ),
                products,
                extra: Default::default(),
                preamble: vec![
                    "generated by wm-installer-mcp".into(),
                    "$NAME$ placeholders are substituted from the environment at read time".into(),
                ],
            };
            let findings = script.validate();
            let rendered = script.render();
            if let Some(path) = opt_str(args, "write_to") {
                script.write(Path::new(&path)).map_err(ToolError::failed)?;
            }
            let errors = findings
                .iter()
                .filter(|f| f.severity == Severity::Error)
                .count();
            Ok(ToolResult::structured(
                format!(
                    "{} products, {} error(s), {} warning(s); placeholders: {}",
                    script.products.len(),
                    errors,
                    findings.len() - errors,
                    if script.placeholders().is_empty() {
                        "none".to_string()
                    } else {
                        script.placeholders().join(", ")
                    }
                ),
                json!({
                    "script": rendered,
                    "findings": findings,
                    "placeholders": script.placeholders(),
                    "written_to": opt_str(args, "write_to"),
                }),
            ))
        }),
    )
}

fn script_validate() -> Tool {
    Tool::new(
        "script_validate",
        "Check a script against the installer's own rules (DistManUtils.isScriptValid) plus \
         the two that abort the run without being part of it: a missing adminPassword and a \
         weak one. Cheap to run, and saves an hour when it catches something.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Script file to read." },
                "text": { "type": "string", "description": "Script content, if not on disk." }
            }
        }),
        Box::new(|args| {
            let script = match (opt_str(args, "path"), opt_str(args, "text")) {
                (Some(path), _) => InstallScript::read(Path::new(&path)),
                (None, Some(text)) => InstallScript::parse(&text),
                (None, None) => return Err(ToolError::invalid("give either path or text")),
            }
            .map_err(ToolError::failed)?;

            let findings = script.validate();
            let errors = findings
                .iter()
                .filter(|f| f.severity == Severity::Error)
                .count();
            let summary = if errors == 0 {
                format!(
                    "the installer will accept this script ({} products, {} warning(s))",
                    script.products.len(),
                    findings.len()
                )
            } else {
                format!("the installer will reject this script: {errors} error(s)")
            };
            Ok(ToolResult::structured(
                summary,
                json!({
                    "valid": errors == 0,
                    "findings": findings,
                    "install_dir": script.install_dir,
                    "product_count": script.products.len(),
                    "placeholders": script.placeholders(),
                }),
            ))
        }),
    )
}

fn image_build() -> Tool {
    Tool::new(
        "image_build",
        "Start building an installation image from a script (-writeImage). Runs for tens of \
         minutes and needs roughly twice the image size free in tmpdir, so it returns a job \
         id to poll with job_status. Credentials come from the environment via the script's \
         $NAME$ placeholders — pass them in `env`.",
        json!({
            "type": "object",
            "required": ["script", "output"],
            "properties": {
                "script": { "type": "string", "description": "Path to the install script." },
                "output": { "type": "string", "description": "Image file to write." },
                "platform": { "type": "string", "description": "LNXAMD64 (default), W64, AIX, SOLAMD64, LNXS390X." },
                "installer_bin": { "type": "string" },
                "tmpdir": { "type": "string" },
                "java_options": { "type": "string" },
                "disable_cpu_detection_test": { "type": "boolean" },
                "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Variables for the script's $NAME$ placeholders." },
                "passthrough_env": { "type": "array", "items": { "type": "string" }, "description": "Variable names this server already has, referenced by the job rather than written into it — use for credentials." }
            }
        }),
        Box::new(|args| {
            let installer = installer_bin(args)?;
            let script = req_str(args, "script")?;
            let output = req_str(args, "output")?;
            let platform = opt_str(args, "platform").unwrap_or_else(|| "LNXAMD64".into());

            let parsed = InstallScript::read(Path::new(&script)).map_err(ToolError::failed)?;
            let blocking: Vec<_> = parsed
                .validate()
                .into_iter()
                .filter(|f| f.severity == Severity::Error)
                .collect();
            if !blocking.is_empty() {
                return Err(ToolError::failed(format!(
                    "refusing to start: the installer would reject this script ({})",
                    blocking
                        .iter()
                        .map(|f| f.message.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                )));
            }

            let cmd_args = vec![
                "-console".into(),
                "-readScript".into(),
                script.clone(),
                "-writeImage".into(),
                output.clone(),
                "-imagePlatform".into(),
                platform.clone(),
                "-debugLvl".into(),
                "verbose".into(),
            ];
            let job = runner::spawn(
                &jobs_dir(),
                "image",
                &installer,
                &cmd_args,
                &environment(args),
            )
            .map_err(ToolError::failed)?;
            Ok(ToolResult::structured(
                format!("image build started as {} -> {output}", job.id),
                json!({ "job_id": job.id, "job_dir": job.dir, "log": job.log, "command": job.command }),
            ))
        }),
    )
}

fn install_run() -> Tool {
    Tool::new(
        "install_run",
        "Start an installation from a script, optionally from an image. Returns a job id; \
         poll it with job_status. Validates the script first, because the installer only \
         reports an invalid one after it has started.",
        json!({
            "type": "object",
            "required": ["script"],
            "properties": {
                "script": { "type": "string" },
                "image": { "type": "string", "description": "Image to install from (-readImage). The script must also carry ImageFile." },
                "installer_bin": { "type": "string" },
                "debug_file": { "type": "string", "description": "Diagnostics file; -debug alone writes to stderr and is easily lost." },
                "tmpdir": { "type": "string" },
                "java_options": { "type": "string" },
                "disable_cpu_detection_test": { "type": "boolean" },
                "env": { "type": "object", "additionalProperties": { "type": "string" } },
                "passthrough_env": { "type": "array", "items": { "type": "string" }, "description": "Variable names referenced by the job rather than written into it." }
            }
        }),
        Box::new(|args| {
            let installer = installer_bin(args)?;
            let script = req_str(args, "script")?;
            let parsed = InstallScript::read(Path::new(&script)).map_err(ToolError::failed)?;
            let blocking: Vec<_> = parsed
                .validate()
                .into_iter()
                .filter(|f| f.severity == Severity::Error)
                .collect();
            if !blocking.is_empty() {
                return Err(ToolError::failed(format!(
                    "refusing to start: the installer would reject this script ({})",
                    blocking
                        .iter()
                        .map(|f| f.message.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                )));
            }

            let mut cmd_args = vec!["-console".into(), "-readScript".into(), script.clone()];
            if let Some(image) = opt_str(args, "image") {
                cmd_args.push("-readImage".into());
                cmd_args.push(image);
            }
            cmd_args.push("-debugLvl".into());
            cmd_args.push("verbose".into());
            if let Some(debug_file) = opt_str(args, "debug_file") {
                cmd_args.push("-debugFile".into());
                cmd_args.push(debug_file);
                cmd_args.push("-maxLogSize".into());
                cmd_args.push("20M".into());
            }
            let job = runner::spawn(
                &jobs_dir(),
                "install",
                &installer,
                &cmd_args,
                &environment(args),
            )
            .map_err(ToolError::failed)?;
            Ok(ToolResult::structured(
                format!("installation started as {}", job.id),
                json!({ "job_id": job.id, "job_dir": job.dir, "log": job.log, "command": job.command }),
            ))
        }),
    )
}

fn job_status() -> Tool {
    Tool::new(
        "job_status",
        "Poll a job started by image_build or install_run: whether it is still running, its \
         exit code, the tail of its log, and — when it failed — the matching diagnosis.",
        json!({
            "type": "object",
            "required": ["job_id"],
            "properties": {
                "job_id": { "type": "string" },
                "lines": { "type": "integer", "description": "Log lines to return (default 40)." }
            }
        }),
        Box::new(|args| {
            let id = req_str(args, "job_id")?;
            let dir = jobs_dir().join(&id);
            if !dir.is_dir() {
                return Err(ToolError::invalid(format!("no such job: {id}")));
            }
            let log = dir.join("output.log");
            let state = runner::job_state(&dir);
            let tail = runner::tail(&log, opt_usize(args, "lines").unwrap_or(40))
                .map_err(ToolError::failed)?;
            let exit_code = match state {
                runner::JobState::Finished { exit_code } => Some(exit_code),
                runner::JobState::Running => None,
            };
            let diagnoses = match exit_code {
                Some(code) if code != 0 => {
                    diag::diagnose(&tail, Some(code), Some(diag::Tool::Installer))
                }
                _ => Vec::new(),
            };
            let progress = wm_core::progress::Progress::read(&dir);
            let summary = match exit_code {
                None => match &progress {
                    Some(p) => format!(
                        "{id}: {} — {:.0}% ({} of {}), {} elapsed{}",
                        p.phase,
                        p.fraction() * 100.0,
                        wm_core::progress::human_bytes(p.bytes_done),
                        wm_core::progress::human_bytes(p.bytes_total),
                        wm_core::progress::human_time(p.elapsed()),
                        match p.remaining() {
                            Some(left) =>
                                format!(", about {} left", wm_core::progress::human_time(left)),
                            None => String::new(),
                        }
                    ),
                    None => format!("{id}: running"),
                },
                Some(0) => format!("{id}: finished successfully"),
                Some(code) => format!(
                    "{id}: failed with exit code {code}, {} known cause(s)",
                    diagnoses.len()
                ),
            };
            Ok(ToolResult::structured(
                summary,
                json!({
                    "job_id": id,
                    "state": state,
                    "log": log,
                    "tail": tail,
                    "diagnoses": diagnoses,
                    "progress": progress,
                }),
            ))
        }),
    )
}

fn diagnose_log() -> Tool {
    Tool::new(
        "diagnose_log",
        "Match installer output against known failure signatures and return the cause and \
         the fix. Covers the invalid-script rejections, incomplete images, the missing \
         adminPassword (exit 30), the OpenJ9 JIT abort on hosts with an inconsistent CPUID, \
         and the empty-log trap where -debug writes to stderr.",
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Log content." },
                "path": { "type": "string", "description": "Log file to read instead." },
                "exit_code": { "type": "integer" }
            }
        }),
        Box::new(|args| {
            let text = match (opt_str(args, "text"), opt_str(args, "path")) {
                (Some(text), _) => text,
                (None, Some(path)) => std::fs::read_to_string(&path)
                    .map_err(|e| ToolError::failed(format!("cannot read {path}: {e}")))?,
                (None, None) => return Err(ToolError::invalid("give either text or path")),
            };
            let found = diag::diagnose(
                &text,
                opt_i32(args, "exit_code"),
                Some(diag::Tool::Installer),
            );
            let summary = if found.is_empty() {
                "no known signature matched".to_string()
            } else {
                format!(
                    "{} known cause(s): {}",
                    found.len(),
                    found[0].signature.cause
                )
            };
            Ok(ToolResult::structured(
                summary,
                json!({ "diagnoses": found }),
            ))
        }),
    )
}
