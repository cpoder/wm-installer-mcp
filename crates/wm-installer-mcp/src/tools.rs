//! Tool registry for the installer server.

use std::path::{Path, PathBuf};

use mcp_rt::{Server, Tool, ToolError, ToolResult};
use serde_json::{json, Value};
use wm_core::catalog::{Catalog, ProductPath};
use wm_core::client;
use wm_core::deps;
use wm_core::diag;
use wm_core::inventory::Inventory;
use wm_core::runner::{self, Environment};
use wm_core::script::{InstallScript, Severity, Source};

use mcp_rt::args::{flag, opt_i32, opt_str, opt_usize, req_str, str_list};

/// Which product's failure signatures this server's jobs are matched against.
const TOOL: diag::Tool = diag::Tool::Installer;

/// Default installer server for 12.1, as shipped in `sagInstaller.jar`.
const DEFAULT_SERVER_URL: &str = "https://sdc.webmethods.io/cgi-bin/dataservewebM121.cgi";

/// Build the configured server.
pub fn server() -> Server {
    Server::new("wm-installer", env!("CARGO_PKG_VERSION"))
        .instructions(
            "Installs, provisions and patches IBM webMethods without the setup wizard, and \
             drives the product's own tooling — the p2 director, dbConfigurator.sh, \
             is_instance.sh — for everything the installer lays down.\n\n\
             HOW TO USE THIS SERVER. Every tool that changes an installation defaults to a dry \
             run. The dry run returns a `settings` list naming each value and whether it came \
             from the caller or from a default. **Show that list to the user in full, ask \
             whether the defaults suit them or they want any changed, and only then call again \
             with `apply: true`.** Never apply on the first call. Ports, instance names, \
             install locations and target platforms all have defaults that are reasonable and \
             often wrong for a given site; the user is the only one who knows which.\n\n\
             NAMING AN INSTALLATION. `install_register` records one under a short name; every \
             tool that takes `wm_home` or `install_dir` then accepts that name in place of the \
             path, and `install` always means a registered name. `install_list` shows what is \
             registered and what is in each — read live from the installation's own \
             install/products/*.prop, so it is current rather than remembered. \
             `config_set` makes a release, platform or installation the default for calls that \
             omit it. These tools, and the credential ones, change this server's own \
             configuration rather than an installation, so they take effect immediately; the \
             two that destroy something take `confirm` instead.\n\n\
             INSTALLING INTO SOMETHING THAT EXISTS. `native_install` and `native_plan` \
             subtract what the target already carries. Products present at the same version \
             are not re-fetched; products present at a *different* version are reported as NOT \
             performed rather than overwritten, because a .prop records the version a product \
             was installed at and not its fix level — unpacking a catalogue version over a \
             patched product replaces corrected files with base-version copies, whichever \
             version is numerically higher. Relay that list to the user; the route that keeps \
             the fix level is the Update Manager server, and `force: true` is a last resort.\n\n\
             CREDENTIALS. The IBM entitlement key is a bearer token. `credential_set` puts it \
             in an encrypted store under the config directory instead of the client's \
             configuration file; `credential_list` names what is held and how it is sealed, \
             never the values. An environment variable of the same meaning still wins, which \
             is how a single run overrides the machine's store. Values are never returned, \
             never written into a generated script, and never put in a job wrapper.\n\n\
             JOBS. Long operations return a job id. Poll it with `job_status`, which reports \
             the phase, bytes fetched against the total, elapsed time and an estimate of what \
             is left — relay that rather than leaving the user without feedback for four \
             minutes. A failed job comes back with the matching failure signature, its cause \
             and remedy, the job directory and the tail of the log, all in the text. A person \
             at a terminal can run `wm-installer-mcp --watch <job-id>` for a live screen. \
             `config_show` says where jobs, caches and configuration live.\n\n\
             For a selection, `catalog_search` finds exact versioned paths and says which are \
             already installed, `native_plan` closes the selection over its prerequisites and \
             prices the difference, and `native_install` performs it.",
        )
        .tool(crate::native::sdc_releases())
        .tool(crate::native::sdc_catalog())
        .tool(crate::native::native_plan())
        .tool(crate::native::profile_provision())
        .tool(crate::native::database_plan())
        .tool(crate::native::database_configure())
        .tool(crate::native::native_install())
        .tool(crate::native::instance_create())
        .tool(crate::native::instance_update())
        .tool(crate::native::profile_capture())
        .tool(crate::native::profile_replay())
        .tool(crate::native::install_verify())
        .tool(inventory_read())
        .tool(catalog_search())
        .tool(plan_resolve())
        .tool(script_generate())
        .tool(script_validate())
        .tool(image_build())
        .tool(install_run())
        .tool(job_status())
        .tool(diagnose_log())
        .tool(crate::manage::install_register())
        .tool(crate::manage::install_list())
        .tool(crate::manage::install_show())
        .tool(crate::manage::install_forget())
        .tool(crate::manage::config_show())
        .tool(crate::manage::config_set())
        .tool(crate::manage::credential_set())
        .tool(crate::manage::credential_list())
        .tool(crate::manage::credential_remove())
        .tool(crate::manage::installer_check())
}

/// The installation a call is about.
///
/// Three ways to name one, and the point of the registry is that they are
/// interchangeable: `install` is always a registered name, `wm_home` is a path
/// or a registered name, and a session that registered one installation and made
/// it the default need give neither.
fn wm_home(args: &Value) -> Result<PathBuf, ToolError> {
    if let Some(name) = opt_str(args, "install") {
        return crate::manage::resolve_home(&name);
    }
    let given = opt_str(args, "wm_home")
        .or_else(|| std::env::var("WM_HOME").ok())
        .or_else(|| {
            wm_core::config::Defaults::load()
                .ok()
                .and_then(|d| d.install)
        })
        .ok_or_else(|| {
            ToolError::invalid(
                "no installation named: pass wm_home (a path or a registered name) or install \
                 (a registered name), set $WM_HOME, or make one the default with config_set \
                 setting=install. install_list shows what is registered.",
            )
        })?;
    crate::manage::resolve_home(&given)
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

/// A cached product tree to check the installer binary against.
///
/// Deliberately offline: a pre-flight check that needs credentials and a
/// network round trip is a check that gets skipped exactly when it would have
/// helped. Trees are cached as `<sandbox>-<platform>.tree`, so when one tree is
/// cached for the platform in play there is no ambiguity about which release
/// this machine works with. When there are several, the check reports the
/// binary's own version and leaves the comparison alone rather than guessing.
pub(crate) fn cached_catalog(args: &Value) -> Option<Catalog> {
    cached_catalog_with_path(args).map(|(catalog, _)| catalog)
}

/// Refuse to start a run the download centre will reject for the client's age.
///
/// The rejection happens a minute in, after the product list has been fetched,
/// and reads like a network failure. Checking first costs a 64 KiB read.
fn preflight_installer(args: &Value, installer: &Path) -> Result<client::Check, ToolError> {
    let check =
        client::check(Some(installer), cached_catalog(args).as_ref()).map_err(ToolError::failed)?;
    if check.outdated && !flag(args, "skip_version_check", false) {
        return Err(ToolError::failed(format!(
            "refusing to start: {}\n\nPass skip_version_check=true to run it anyway — the \
             comparison is against the installer infrastructure version the catalogue \
             declares, which is evidence and not a statement from the server.",
            check.warning().unwrap_or_default()
        )));
    }
    Ok(check)
}

/// The catalogue a planning call searches, and what went into it.
///
/// An installation's own `.prop` files describe what is installed and nothing
/// else, so a seed naming a product that merely *exists* — the usual case when
/// planning an addition — resolved to nothing, and the answer read as though the
/// product were not real: "2 seed(s) match no product" for two perfectly valid
/// 12.1 products, with the release catalogue sitting in the cache unconsulted.
/// The installation's entries still win, because its versions are the ones on
/// disk; the cached release tree supplies everything else.
///
/// The sources are returned so a miss can say where it looked. A search that
/// does not name the haystack makes the reader guess whether the product is
/// absent or the catalogue is.
fn load_catalog(args: &Value) -> Result<(PathBuf, Catalog, Vec<String>), ToolError> {
    let home = wm_home(args)?;
    let installed = Catalog::load(&home).map_err(ToolError::failed)?;
    let mut sources = vec![format!(
        "{} ({} products installed)",
        home.join("install").join("products").display(),
        installed.len()
    )];
    if !flag(args, "release_catalog", true) {
        return Ok((home, installed, sources));
    }
    let Some((release, path)) = cached_catalog_with_path(args) else {
        sources.push(
            "no cached release catalogue for this platform — sdc_catalog fetches one, which \
             is what lets a product that is not installed be planned for"
                .to_string(),
        );
        return Ok((home, installed, sources));
    };
    sources.push(format!(
        "{} ({} products available)",
        path.display(),
        release.len()
    ));
    Ok((home, installed.extended_with(&release), sources))
}

/// A cached release catalogue, with the file it came from.
fn cached_catalog_with_path(args: &Value) -> Option<(Catalog, PathBuf)> {
    let platform = opt_str(args, "platform")
        .or_else(|| {
            wm_core::config::Defaults::load()
                .ok()
                .and_then(|d| d.platform)
        })
        .unwrap_or_else(|| "LNXAMD64".into())
        .to_uppercase();
    let suffix = format!("-{platform}.tree");
    let mut matching: Vec<PathBuf> = std::fs::read_dir(wm_core::config::catalog_dir())
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(&suffix))
        })
        .collect();
    matching.sort();
    let path = matching.pop()?;
    let text = std::fs::read_to_string(&path).ok()?;
    Some((
        wm_core::tree::ProductTree::parse(&text).ok()?.catalog(),
        path,
    ))
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

/// Whether a variable name looks like it holds a secret.
///
/// `env` values are written to the job's wrapper, which stays on disk for the
/// life of the job. A name that says "key" or "password" is refused there and
/// pointed at the two places a secret can go without being written.
fn looks_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "PASS", "PWD", "SECRET", "TOKEN", "CREDENTIAL"]
        .iter()
        .any(|needle| upper.contains(needle))
}

fn environment(args: &Value) -> Result<Environment, ToolError> {
    let extra: Vec<(String, String)> = args
        .get("env")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    if let Some((name, _)) = extra.iter().find(|(name, _)| looks_secret(name)) {
        return Err(ToolError::invalid(format!(
            "env would write the value of {name} into the job's wrapper script on disk. Name \
             it in passthrough_env when this server's environment has it, or store it with \
             credential_set; env is for placeholders that are not secrets."
        )));
    }
    Ok(Environment {
        tmpdir: opt_str(args, "tmpdir").map(PathBuf::from),
        java_options: opt_str(args, "java_options"),
        disable_cpu_detection_test: flag(args, "disable_cpu_detection_test", false),
        extra,
        stdin_feed: None,
        passthrough: str_list(args, "passthrough_env"),
        // A script's $WM_EMPOWER_KEY$ placeholder resolves from the job's
        // environment. When the key lives in the credential store rather than
        // this process's environment, this is what puts it there — without it
        // ever reaching the wrapper the job runs from.
        secret_env: wm_core::secrets::job_environment(),
        // The installer reads its $NAME$ placeholders from the environment, so
        // nothing is scrubbed here; that is for programs that take a secret as
        // an argument.
        scrub: Vec::new(),
    })
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
                "install": { "type": "string", "description": "A registered installation, as an alternative to wm_home; install_list shows the names." },
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
                "install": { "type": "string", "description": "A registered installation, as an alternative to wm_home; install_list shows the names." },
                "wm_home": { "type": "string" },
                "platform": { "type": "string", "description": "Platform whose cached release catalogue to search alongside the installation's own (default LNXAMD64)." },
                "release_catalog": { "type": "boolean", "description": "Search the cached release catalogue as well as the installation, so products that exist but are not installed can be found (default true)." },
                "query": { "type": "string", "description": "Substring matched against component, group and product code." },
                "limit": { "type": "integer", "description": "Maximum results (default 50)." }
            }
        }),
        Box::new(|args| {
            let (home, catalog, sources) = load_catalog(args)?;
            let installed = Catalog::load(&home).map_err(ToolError::failed)?;
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
                        "installed": installed.contains(&p.path.raw),
                    })
                })
                .collect();
            let already = hits
                .iter()
                .filter(|h| h["installed"].as_bool().unwrap_or(false))
                .count();
            Ok(ToolResult::structured(
                format!(
                    "{} of {} products match {query:?}; {already} already installed, {} \
                     available to add.\nsearched:\n{}",
                    hits.len(),
                    catalog.len(),
                    hits.len() - already,
                    sources
                        .iter()
                        .map(|s| format!("  {s}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                json!({ "matches": hits, "searched": sources }),
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
                "install": { "type": "string", "description": "A registered installation, as an alternative to wm_home; install_list shows the names." },
                "wm_home": { "type": "string" },
                "platform": { "type": "string", "description": "Platform whose cached release catalogue to search alongside the installation's own (default LNXAMD64)." },
                "release_catalog": { "type": "boolean", "description": "Search the cached release catalogue as well as the installation, so products that exist but are not installed can be found (default true)." },
                "seeds": { "type": "array", "items": { "type": "string" }, "description": "Component names (TNServer) or full versioned paths. A path absent from the catalogue is kept verbatim and reported, since some installed products have no .prop file." },
                "include_mandatory": { "type": "boolean", "description": "Inject the mandatory base products (default true)." }
            }
        }),
        Box::new(|args| {
            let (_, catalog, sources) = load_catalog(args)?;
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
                    "; {} seed(s) match no product ({}) in:\n{}",
                    seeds.unresolved.len(),
                    seeds.unresolved.join(", "),
                    sources
                        .iter()
                        .map(|s| format!("  {s}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
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
                    "searched": sources,
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
         id to poll with job_status. The script's $NAME$ placeholders resolve from the job's \
         environment: name credentials in passthrough_env when this server has them, or store \
         them with credential_set. `env` is for placeholders that are not secrets — its \
         values are written to the job's wrapper on disk, and a name that looks like a secret \
         is refused there.",
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
                "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Variables for the script's $NAME$ placeholders that are not secrets; written to the job's wrapper on disk." },
                "passthrough_env": { "type": "array", "items": { "type": "string" }, "description": "Variable names this server already has, referenced by the job rather than written into it — use for credentials." },
                "skip_version_check": { "type": "boolean", "description": "Start even when the installer binary looks older than the catalogue expects." }
            }
        }),
        Box::new(|args| {
            let installer = installer_bin(args)?;
            let check = preflight_installer(args, &installer)?;
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
                &environment(args)?,
            )
            .map_err(ToolError::failed)?;
            let mut summary = format!("image build started as {} -> {output}", job.id);
            if let Some(version) = &check.local {
                summary.push_str(&format!(" with installer client {version}"));
            }
            if let Some(warning) = check.warning() {
                summary.push_str(&format!("\n\nnote: {warning}"));
            }
            Ok(ToolResult::structured(
                summary,
                json!({
                    "job_id": job.id,
                    "job_dir": job.dir,
                    "log": job.log,
                    "command": job.command,
                    "installer": check,
                }),
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
                "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Variables for the script's $NAME$ placeholders that are not secrets; written to the job's wrapper on disk." },
                "passthrough_env": { "type": "array", "items": { "type": "string" }, "description": "Variable names referenced by the job rather than written into it — use for credentials." },
                "platform": { "type": "string", "description": "Platform whose cached catalogue the installer's version is checked against." },
                "skip_version_check": { "type": "boolean", "description": "Start even when the installer binary looks older than the catalogue expects." }
            }
        }),
        Box::new(|args| {
            let installer = installer_bin(args)?;
            let check = preflight_installer(args, &installer)?;
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
                &environment(args)?,
            )
            .map_err(ToolError::failed)?;
            let mut summary = format!("installation started as {}", job.id);
            if let Some(version) = &check.local {
                summary.push_str(&format!(" with installer client {version}"));
            }
            if let Some(warning) = check.warning() {
                summary.push_str(&format!("\n\nnote: {warning}"));
            }
            Ok(ToolResult::structured(
                summary,
                json!({
                    "job_id": job.id,
                    "job_dir": job.dir,
                    "log": job.log,
                    "command": job.command,
                    "installer": check,
                }),
            ))
        }),
    )
}

fn job_status() -> Tool {
    Tool::new(
        "job_status",
        "Poll a job: whether it is still running, its progress, its exit code, the matching \
         failure signature when it failed, and the tail of its log. A failed job returns the \
         cause, the remedy and the evidence in the text itself, along with the job directory \
         — there is nothing further to go and find.",
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
            let report = runner::Report::read(
                &jobs_dir(),
                &id,
                opt_usize(args, "lines").unwrap_or(40),
                TOOL,
            )
            .map_err(|e| ToolError::invalid(e.to_string()))?;
            let structured = serde_json::to_value(&report)
                .map_err(|e| ToolError::failed(format!("cannot serialise the job report: {e}")))?;
            Ok(ToolResult::structured(report.summary(), structured))
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
