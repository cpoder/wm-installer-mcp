//! Native tools: talk to IBM directly, no shipped installer involved.

use std::path::{Path, PathBuf};

use mcp_rt::args::{flag, opt_str, opt_usize, req_str, str_list};
use mcp_rt::{Tool, ToolError, ToolResult};
use serde_json::{json, Value};
use wm_core::sdc::{self, Session};
use wm_core::tree::ProductTree;
use wm_core::{deps, install, profile, runner};

/// Where fetched product trees and artifacts are kept between calls.
fn state_dir() -> PathBuf {
    std::env::var("WM_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            Path::new(&home).join(".wm-mcp")
        })
}

fn jobs_dir() -> PathBuf {
    std::env::var("WM_JOBS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| state_dir().join("jobs"))
}

fn credentials() -> Result<(String, String), ToolError> {
    let user = std::env::var("WM_EMPOWER_USER")
        .map_err(|_| ToolError::invalid("WM_EMPOWER_USER is not set"))?;
    let key = std::env::var("WM_EMPOWER_KEY")
        .map_err(|_| ToolError::invalid("WM_EMPOWER_KEY is not set"))?;
    Ok((user, key))
}

fn host(args: &Value) -> String {
    opt_str(args, "host")
        .or_else(|| std::env::var("WM_SDC_HOST").ok())
        .unwrap_or_else(|| sdc::DEFAULT_HOST.to_string())
}

fn login(args: &Value) -> Result<Session, ToolError> {
    let (user, key) = credentials()?;
    Session::login(&host(args), &user, &key).map_err(ToolError::failed)
}

/// Cache path for one release/platform tree.
fn tree_path(sandbox: &str, platform: &str) -> PathBuf {
    state_dir()
        .join("catalog")
        .join(format!("{sandbox}-{platform}.tree"))
}

/// Load a cached tree, or fetch and cache it.
fn tree_for(
    args: &Value,
    release: &str,
    platform: &str,
) -> Result<(ProductTree, String), ToolError> {
    let session = login(args)?;
    let releases = session.releases().map_err(ToolError::failed)?;
    let entry = releases
        .iter()
        .find(|r| r.release == release)
        .ok_or_else(|| ToolError::failed(format!("no entitlement for release {release}")))?;
    let sandbox = entry
        .sandbox()
        .ok_or_else(|| ToolError::failed(format!("release {release} names no sandbox")))?;

    let cached = tree_path(&sandbox, platform);
    let text = if cached.is_file() && !flag(args, "refresh", false) {
        std::fs::read_to_string(&cached)
            .map_err(|e| ToolError::failed(format!("cannot read the cached tree: {e}")))?
    } else {
        let fetched = session
            .product_tree(&sandbox, platform)
            .map_err(ToolError::failed)?;
        if let Some(parent) = cached.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&cached, &fetched);
        fetched
    };
    let tree = ProductTree::parse(&text).map_err(ToolError::failed)?;
    Ok((tree, sandbox))
}

/// Releases this account may install.
pub fn sdc_releases() -> Tool {
    Tool::new(
        "sdc_releases",
        "List the webMethods releases this IBM account is entitled to install, straight from \
         the download centre. Needs WM_EMPOWER_USER and WM_EMPOWER_KEY; no installer binary \
         and no existing installation.",
        json!({ "type": "object", "properties": { "host": { "type": "string" } } }),
        Box::new(|args| {
            let session = login(args)?;
            let releases = session.releases().map_err(ToolError::failed)?;
            let rows: Vec<Value> = releases
                .iter()
                .map(|r| {
                    json!({
                        "release": r.release,
                        "display_name": r.display_name,
                        "code": r.code,
                        "sandbox": r.sandbox(),
                        "repository": r.repository(),
                    })
                })
                .collect();
            Ok(ToolResult::structured(
                format!(
                    "{} entitled release(s): {}",
                    releases.len(),
                    releases
                        .iter()
                        .map(|r| r.release.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                json!({ "releases": rows }),
            ))
        }),
    )
}

/// Fetch the product catalogue for a release.
pub fn sdc_catalog() -> Tool {
    Tool::new(
        "sdc_catalog",
        "Fetch the product tree for one release and platform from IBM and cache it. This is \
         the authoritative catalogue: it carries the exact versioned product paths, the \
         prerequisites, and every artifact with its size and sha256 — so no reference \
         installation is needed, and products absent from a local tree (webMethods Flat File, \
         for one) are present here.",
        json!({
            "type": "object",
            "required": ["release"],
            "properties": {
                "release": { "type": "string", "description": "e.g. 12.1" },
                "platform": { "type": "string", "description": "LNXAMD64 (default), W64, AIX, SOLAMD64, LNXS390X." },
                "refresh": { "type": "boolean", "description": "Re-fetch even if cached." },
                "query": { "type": "string", "description": "Only return products matching this substring." },
                "limit": { "type": "integer" },
                "host": { "type": "string" }
            }
        }),
        Box::new(|args| {
            let release = req_str(args, "release")?;
            let platform = opt_str(args, "platform").unwrap_or_else(|| "LNXAMD64".into());
            let (tree, sandbox) = tree_for(args, &release, &platform)?;
            let catalog = tree.catalog();

            let matches: Vec<Value> = match opt_str(args, "query") {
                Some(query) => {
                    let needle = query.to_lowercase();
                    catalog
                        .iter()
                        .filter(|p| {
                            p.path.component.to_lowercase().contains(&needle)
                                || p.path.group.to_lowercase().contains(&needle)
                                || p.path.code().to_lowercase().contains(&needle)
                        })
                        .take(opt_usize(args, "limit").unwrap_or(50))
                        .map(|p| {
                            json!({
                                "path": p.path.raw,
                                "component": p.path.component,
                                "group": p.path.group,
                                "version": p.path.version(),
                                "requires": p.requires,
                            })
                        })
                        .collect()
                }
                None => Vec::new(),
            };
            let total: u64 = tree
                .artifacts()
                .iter()
                .filter_map(|a| a.compressed_size)
                .sum();
            Ok(ToolResult::structured(
                format!(
                    "{release} ({sandbox}/{platform}): {} products, {} artifacts, {:.1} GB in full",
                    tree.product_count(),
                    tree.artifacts().len(),
                    total as f64 / 1e9
                ),
                json!({
                    "release": release,
                    "sandbox": sandbox,
                    "platform": platform,
                    "products": tree.product_count(),
                    "artifacts": tree.artifacts().len(),
                    "matches": matches,
                    "cache": tree_path(&sandbox, &platform),
                }),
            ))
        }),
    )
}

/// Resolve a selection and price the download.
pub fn native_plan() -> Tool {
    Tool::new(
        "native_plan",
        "Resolve a product selection against IBM's catalogue and report exactly what would be \
         downloaded and installed: the prerequisite closure, the artifact list with its total \
         size, and — importantly — which products declare Java install panels that a native \
         install cannot run.",
        json!({
            "type": "object",
            "required": ["release", "products"],
            "properties": {
                "release": { "type": "string" },
                "platform": { "type": "string" },
                "products": { "type": "array", "items": { "type": "string" }, "description": "Component names or full versioned paths." },
                "include_mandatory": { "type": "boolean", "description": "Inject the undeclared base products (default true)." },
                "host": { "type": "string" }
            }
        }),
        Box::new(|args| {
            let release = req_str(args, "release")?;
            let platform = opt_str(args, "platform").unwrap_or_else(|| "LNXAMD64".into());
            let (tree, _) = tree_for(args, &release, &platform)?;
            let catalog = tree.catalog();

            let mut seeds = Vec::new();
            let mut unknown = Vec::new();
            for wanted in str_list(args, "products") {
                match catalog
                    .get(&wanted)
                    .map(|_| wanted.clone())
                    .or_else(|| catalog.path_of(&wanted).map(|p| p.raw.clone()))
                {
                    Some(path) => seeds.push(path),
                    None => unknown.push(wanted),
                }
            }
            if seeds.is_empty() {
                return Err(ToolError::invalid(
                    "none of the requested products exist in the catalogue",
                ));
            }
            let resolution = deps::resolve(&catalog, &seeds, flag(args, "include_mandatory", true))
                .map_err(ToolError::failed)?;
            let paths = resolution.paths();
            let plan = install::plan(&tree, &paths);

            let mut summary = format!(
                "{} products ({} after closure), {} artifacts, {:.2} GB to download",
                seeds.len(),
                plan.products.len(),
                plan.artifacts.len(),
                plan.download_bytes as f64 / 1e9
            );
            if !plan.products_with_panels.is_empty() {
                summary.push_str(&format!(
                    "; {} product(s) declare install panels a native install does not run",
                    plan.products_with_panels.len()
                ));
            }
            Ok(ToolResult::structured(
                summary,
                json!({
                    "complete": resolution.unsatisfied.is_empty() && unknown.is_empty(),
                    "unknown_products": unknown,
                    "unsatisfied": resolution.unsatisfied,
                    "products": paths,
                    "artifact_count": plan.artifacts.len(),
                    "download_bytes": plan.download_bytes,
                    "expanded_bytes": plan.expanded_bytes,
                    "products_with_panels": plan.products_with_panels,
                }),
            ))
        }),
    )
}

/// Run a native install as a detached job.
pub fn native_install() -> Tool {
    Tool::new(
        "native_install",
        "Download and install a product selection straight from IBM: no installer binary, no \
         JVM, no image. Every artifact is verified against the sha256 the catalogue declares \
         before it is unpacked, and the installation is left self-describing \
         (install/bms/*.contents, install/products/*.prop). Returns a job id — poll it with \
         job_status. Java install panels are not run; see native_plan.",
        json!({
            "type": "object",
            "required": ["release", "products", "install_dir"],
            "properties": {
                "release": { "type": "string" },
                "platform": { "type": "string" },
                "products": { "type": "array", "items": { "type": "string" } },
                "installer_jar": { "type": "string", "description": "Path to the installer's own jar (sagInstaller.jar, inside the downloaded installer). It is laid down as install/jars/DistMan.jar, which is where the shipped tooling looks for it — is_instance.xml puts it on the instance manager's classpath. Defaults to $WM_INSTALLER_JAR." },
                "install_dir": { "type": "string" },
                "include_mandatory": { "type": "boolean" },
                "host": { "type": "string" }
            }
        }),
        Box::new(|args| {
            // Validate the plan before spawning: a job that fails on its first
            // call has cost a process and told the caller nothing new.
            let release = req_str(args, "release")?;
            let install_dir = req_str(args, "install_dir")?;
            let platform = opt_str(args, "platform").unwrap_or_else(|| "LNXAMD64".into());
            credentials()?;
            let (tree, _) = tree_for(args, &release, &platform)?;
            let catalog = tree.catalog();
            let mut seeds = Vec::new();
            for wanted in str_list(args, "products") {
                if let Some(path) = catalog
                    .get(&wanted)
                    .map(|_| wanted.clone())
                    .or_else(|| catalog.path_of(&wanted).map(|p| p.raw.clone()))
                {
                    seeds.push(path);
                }
            }
            if seeds.is_empty() {
                return Err(ToolError::invalid(
                    "none of the requested products exist in the catalogue",
                ));
            }
            let resolution = deps::resolve(&catalog, &seeds, flag(args, "include_mandatory", true))
                .map_err(ToolError::failed)?;

            let spec = json!({
                "release": release,
                "platform": platform,
                "install_dir": install_dir,
                "products": resolution.paths(),
                "host": host(args),
                "installer_jar": opt_str(args, "installer_jar")
                    .or_else(|| std::env::var("WM_INSTALLER_JAR").ok()),
            });
            let jobs = jobs_dir();
            std::fs::create_dir_all(&jobs)
                .map_err(|e| ToolError::failed(format!("cannot create {}: {e}", jobs.display())))?;
            let spec_path = jobs.join(format!("install-spec-{}.json", std::process::id()));
            std::fs::write(&spec_path, spec.to_string())
                .map_err(|e| ToolError::failed(format!("cannot write the job spec: {e}")))?;

            let me = std::env::current_exe()
                .map_err(|e| ToolError::failed(format!("cannot locate this executable: {e}")))?;
            let env = runner::Environment {
                // Credentials stay in this process's environment and are
                // inherited by the job; nothing is written to the wrapper.
                passthrough: vec!["WM_EMPOWER_USER".into(), "WM_EMPOWER_KEY".into()],
                ..runner::Environment::default()
            };
            let job = runner::spawn(
                &jobs,
                "native",
                &me,
                &["--install-job".to_string(), spec_path.display().to_string()],
                &env,
            )
            .map_err(ToolError::failed)?;
            Ok(ToolResult::structured(
                format!("native install started as {} into {install_dir}", job.id),
                json!({ "job_id": job.id, "log": job.log, "products": resolution.len() }),
            ))
        }),
    )
}

/// Create an Integration Server instance.
pub fn instance_create() -> Tool {
    Tool::new(
        "instance_create",
        "Create an Integration Server instance by running the product's own \
         IntegrationServer/instances/is_instance.sh, which drives is_instance.xml through the \
         Ant that ships in common/lib/ant. The instance is created by the product's tooling, so \
         the product recognises it afterwards. What this adds is a dry run that prints the exact \
         command, with passwords masked. Defaults to a dry run.",
        json!({
            "type": "object",
            "required": ["wm_home"],
            "properties": {
                "wm_home": { "type": "string", "description": "Installation root." },
                "name": { "type": "string", "description": "Instance name (default \"default\")." },
                "primary_port": { "type": "integer", "description": "HTTP port; the script defaults to 5555." },
                "secure_port": { "type": "integer", "description": "HTTPS port; the script defaults to 5543." },
                "diagnostic_port": { "type": "integer", "description": "The script defaults to 9999." },
                "jmx_port": { "type": "integer", "description": "The script defaults to 8075." },
                "bind_address": { "type": "string", "description": "Default bind address for the ports." },
                "admin_password": { "type": "string", "description": "Administrator password; the script reuses the install-time one when omitted." },
                "license_file": { "type": "string", "description": "Path to an Integration Server licence key file." },
                "packages": { "type": "array", "items": { "type": "string" }, "description": "Non-core packages to include." },
                "db_type": { "type": "string", "description": "ORACLE, DB2, SQLSERVER, MYSQLCE, MYSQLEE or POSTGRESQL. Omitted, the instance uses the embedded database." },
                "db_alias": { "type": "string" },
                "db_url": { "type": "string" },
                "db_username": { "type": "string" },
                "db_password": { "type": "string" },
                "native": { "type": "boolean", "description": "Build the instance directly instead of running is_instance.sh. Use only when the installation has no install/jars/DistMan.jar — that jar ships with the installer binary, not with the products, and the shipped script's instance manager needs it. An instance built this way is not one IBM's tooling created." },
                "apply": { "type": "boolean", "description": "Set true to run; otherwise a dry run." }
            }
        }),
        Box::new(|args| {
            let wm_home = PathBuf::from(req_str(args, "wm_home")?);
            let name = opt_str(args, "name").unwrap_or_else(|| "default".into());
            let options = wm_core::instance::ant::Options {
                primary_port: opt_port(args, "primary_port")?,
                secure_port: opt_port(args, "secure_port")?,
                diagnostic_port: opt_port(args, "diagnostic_port")?,
                jmx_port: opt_port(args, "jmx_port")?,
                admin_password: opt_str(args, "admin_password"),
                bind_address: opt_str(args, "bind_address"),
                license_file: opt_str(args, "license_file"),
                packages: str_list(args, "packages"),
                db_type: opt_str(args, "db_type"),
                db_alias: opt_str(args, "db_alias"),
                db_url: opt_str(args, "db_url"),
                db_username: opt_str(args, "db_username"),
                db_password: opt_str(args, "db_password"),
            };
            if flag(args, "native", false) {
                if !flag(args, "apply", false) {
                    return Ok(ToolResult::structured(
                        format!(
                            "dry run: would build instance {name} directly, without is_instance.sh"
                        ),
                        json!({ "native": true }),
                    ));
                }
                let spec = wm_core::instance::InstanceSpec {
                    name: name.clone(),
                    primary_port: options.primary_port.unwrap_or(5555),
                    secure_port: options.secure_port.unwrap_or(5543),
                    diagnostic_port: options.diagnostic_port.unwrap_or(9999),
                    jmx_port: options.jmx_port.unwrap_or(8075),
                    bind_address: options.bind_address.clone().unwrap_or_default(),
                    lock_mode: "full".into(),
                    admin_password: options.admin_password.clone(),
                    change_password_on_login: false,
                    extra_packages: options.packages.clone(),
                };
                let created =
                    wm_core::instance::create(&wm_home, &spec).map_err(ToolError::failed)?;
                return Ok(ToolResult::structured(
                    format!(
                        "instance {name} built directly at {}: {} template file(s), {} package(s). \
                         Not created by the product's own tooling.",
                        created.path.display(),
                        created.template_files,
                        created.packages.len()
                    ),
                    json!({ "path": created.path, "native": true, "skipped": created.skipped }),
                ));
            }

            let invocation = wm_core::instance::ant::create(&wm_home, &name, &options)
                .map_err(ToolError::failed)?;
            if !flag(args, "apply", false) {
                return Ok(ToolResult::structured(
                    format!("dry run: would create instance {name}"),
                    json!({ "command": invocation.display() }),
                ));
            }
            let started = std::time::Instant::now();
            let (ok, transcript) =
                wm_core::instance::ant::run(&invocation).map_err(ToolError::failed)?;
            if !ok {
                return Err(ToolError::failed(format!(
                    "is_instance.sh failed:\n{}",
                    tail(&transcript, 25)
                )));
            }
            let path = wm_home
                .join("IntegrationServer")
                .join("instances")
                .join(&name);
            Ok(ToolResult::structured(
                format!(
                    "instance {name} created at {} in {:.1}s",
                    path.display(),
                    started.elapsed().as_secs_f64()
                ),
                json!({ "name": name, "path": path }),
            ))
        }),
    )
}

/// An optional port argument.
fn opt_port(args: &Value, key: &str) -> Result<Option<u16>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|p| *p > 0 && *p <= u64::from(u16::MAX))
            .map(|p| Some(p as u16))
            .ok_or_else(|| ToolError::invalid(format!("{key} must be a port number"))),
    }
}

/// Capture a p2 profile for replay elsewhere.
pub fn profile_capture() -> Tool {
    Tool::new(
        "profile_capture",
        "Capture an Eclipse p2 profile (Platform Manager, My webMethods Server) into a small          portable archive: the bundle list and the configuration, with installation paths          replaced by placeholders. The bundle jars are not carried — every one of them comes          from the installation's own repositories, so a replay copies them locally. Building a          profile from scratch needs a p2 director; replaying a known-good one does not.",
        json!({
            "type": "object",
            "required": ["wm_home", "profile", "output"],
            "properties": {
                "wm_home": { "type": "string", "description": "Installation to capture from." },
                "profile": { "type": "string", "description": "Profile name, e.g. SPM or MWS_default." },
                "output": { "type": "string", "description": "Archive to write." }
            }
        }),
        Box::new(|args| {
            let wm_home = PathBuf::from(req_str(args, "wm_home")?);
            let name = req_str(args, "profile")?;
            let output = PathBuf::from(req_str(args, "output")?);
            let manifest =
                profile::capture(&wm_home, &name, &output).map_err(ToolError::failed)?;
            let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
            Ok(ToolResult::structured(
                format!(
                    "captured {name}: {} bundles, {} config file(s), {:.1} MB at {}",
                    manifest.bundles.len(),
                    manifest.files.len(),
                    size as f64 / 1e6,
                    output.display()
                ),
                json!({
                    "output": output,
                    "profile": manifest.name,
                    "bundles": manifest.bundles.len(),
                    "files": manifest.files.len(),
                    "tokenised": manifest.tokenised.len(),
                    "bytes": size,
                }),
            ))
        }),
    )
}

/// Replay a captured profile onto an installation.
pub fn profile_replay() -> Tool {
    Tool::new(
        "profile_replay",
        "Lay a captured p2 profile down on an installation, resolving its bundles from that          installation's own repositories and substituting its paths. Dry run by default. A          bundle the capture names but the target does not carry is reported, not guessed: the          profile would not start, and saying so is more useful than a partial one.",
        json!({
            "type": "object",
            "required": ["capture", "wm_home"],
            "properties": {
                "capture": { "type": "string", "description": "Archive from profile_capture." },
                "wm_home": { "type": "string", "description": "Installation to write into." },
                "profile": { "type": "string", "description": "Name to give it; the captured name by default." },
                "apply": { "type": "boolean", "description": "Set true to write; otherwise a dry run." }
            }
        }),
        Box::new(|args| {
            let capture = PathBuf::from(req_str(args, "capture")?);
            let wm_home = PathBuf::from(req_str(args, "wm_home")?);
            let name = opt_str(args, "profile");
            let dry_run = !flag(args, "apply", false);
            let done = profile::replay(&capture, &wm_home, name.as_deref(), dry_run)
                .map_err(ToolError::failed)?;
            let mut summary = format!(
                "{}: {} bundle(s) resolved, {} file(s) at {}",
                if dry_run { "dry run" } else { "replayed" },
                done.bundles,
                done.files,
                done.path.display()
            );
            if !done.missing_bundles.is_empty() {
                summary
                    .push_str(&format!("; {} bundle(s) missing", done.missing_bundles.len()));
            }
            Ok(ToolResult::structured(summary, json!({ "result": done })))
        }),
    )
}

/// Perform the install described by `spec_path`. Entry point for the job process.
pub fn run_install_job(spec_path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(spec_path).map_err(|e| e.to_string())?;
    let spec: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let get = |k: &str| {
        spec.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    let (user, key) = (
        std::env::var("WM_EMPOWER_USER").map_err(|_| "WM_EMPOWER_USER is not set".to_string())?,
        std::env::var("WM_EMPOWER_KEY").map_err(|_| "WM_EMPOWER_KEY is not set".to_string())?,
    );
    let host = get("host");
    let release_wanted = get("release");
    let platform = get("platform");
    let install_dir = PathBuf::from(get("install_dir"));
    let products: Vec<String> = spec
        .get("products")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    println!("authenticating against {host}");
    let mut session = Session::login(&host, &user, &key).map_err(|e| e.to_string())?;
    let releases = session.releases().map_err(|e| e.to_string())?;
    let release = releases
        .iter()
        .find(|r| r.release == release_wanted)
        .ok_or_else(|| format!("no entitlement for release {release_wanted}"))?;
    let sandbox = release.sandbox().ok_or("release names no sandbox")?;
    let repository = release.repository().ok_or("release names no repository")?;
    let cgi = release.cgi().ok_or("release names no CGI")?.to_string();

    let cached = tree_path(&sandbox, &platform);
    let text = match std::fs::read_to_string(&cached) {
        Ok(text) => text,
        Err(_) => session
            .product_tree(&sandbox, &platform)
            .map_err(|e| e.to_string())?,
    };
    let tree = ProductTree::parse(&text).map_err(|e| e.to_string())?;

    let artifacts = tree.artifacts_for_selection(products.iter().map(String::as_str));
    let total: u64 = artifacts.iter().filter_map(|a| a.compressed_size).sum();
    println!(
        "{} products, {} artifacts, {:.2} GB to fetch into {}",
        products.len(),
        artifacts.len(),
        total as f64 / 1e9,
        install_dir.display()
    );
    std::fs::create_dir_all(&install_dir).map_err(|e| e.to_string())?;
    let cache = state_dir().join("artifacts").join(&sandbox);

    let mut done = 0usize;
    let mut bytes = 0u64;
    for artifact in &artifacts {
        let fetched = install::fetch(&mut session, &cgi, &repository, artifact, &cache)
            .map_err(|e| e.to_string())?;
        let modes = install::Modes::read(&fetched.path).map_err(|e| e.to_string())?;
        let unpacked =
            install::unpack(&fetched.path, &install_dir, &modes).map_err(|e| e.to_string())?;
        install::write_contents(&install_dir, artifact, &unpacked).map_err(|e| e.to_string())?;
        done += 1;
        bytes += fetched.size;
        println!(
            "[{done}/{}] {} {} -> {} file(s)",
            artifacts.len(),
            if fetched.from_cache {
                "cached"
            } else {
                "fetched"
            },
            artifact.name,
            unpacked.files.len()
        );
    }
    // Resource jars used to be skipped as "the shipped installer's wizard
    // resources". They are not: `IntegrationServer/instances/is_instance.xml`
    // puts DistMan, CustomInstall, wMInstTools and the rest of `install/jars`
    // on the classpath of the instance manager it forks. Driving the product's
    // own tooling means installing the tooling.
    let jars = tree.select(
        products.iter().map(String::as_str),
        wm_core::tree::ArtifactKind::ResourceJar,
    );
    let jar_dir = install_dir.join("install").join("jars");
    std::fs::create_dir_all(&jar_dir).map_err(|e| e.to_string())?;
    let mut jars_done = 0usize;
    for jar in &jars {
        let fetched = install::fetch(&mut session, &cgi, &repository, jar, &cache)
            .map_err(|e| e.to_string())?;
        let name = if jar.name.ends_with(".jar") {
            jar.name.clone()
        } else {
            format!("{}.jar", jar.name)
        };
        std::fs::copy(&fetched.path, jar_dir.join(&name)).map_err(|e| e.to_string())?;
        jars_done += 1;
        bytes += fetched.size;
    }
    // `install/jars/DistMan.jar` is the installer's own jar, which the
    // installer lays down under that name. It is not in the product catalogue,
    // and the shipped tooling needs it: `is_instance.xml` puts it on the
    // classpath of the instance manager it forks. Replacing the installer means
    // doing what it does, and this is part of it.
    let installer_jar = spec
        .get("installer_jar")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    if let Some(source) = installer_jar {
        let target = jar_dir.join("DistMan.jar");
        std::fs::copy(&source, &target)
            .map_err(|e| format!("cannot install {} as DistMan.jar: {e}", source.display()))?;
        jars_done += 1;
        println!("installed the installer's own jar as install/jars/DistMan.jar");
    } else if !jar_dir.join("DistMan.jar").is_file() {
        println!(
            "note: install/jars/DistMan.jar is absent. It is the installer's own jar \
             (sagInstaller.jar) rather than a catalogue product, and the shipped \
             is_instance.sh needs it. Pass installer_jar to lay it down."
        );
    }
    if jars_done > 0 {
        println!("installed {jars_done} tooling jar(s) into install/jars");
    }

    for product in &products {
        install::write_prop(&install_dir, product, &tree).map_err(|e| e.to_string())?;
    }
    println!(
        "installed {done} artifact(s) and {jars_done} jar(s), {:.2} GB",
        bytes as f64 / 1e9
    );

    let panels: Vec<&String> = products
        .iter()
        .filter(|p| !tree.panels_for(p).is_empty())
        .collect();
    if !panels.is_empty() {
        println!(
            "note: {} product(s) declare Java install panels that were not run; \
             instance creation and administrator-password seeding are not done",
            panels.len()
        );
    }
    Ok(())
}

/// Report the database components an installation ships and what installing
/// them would do.
pub fn database_plan() -> Tool {
    Tool::new(
        "database_plan",
        "Report the database components an installation ships, and for each one the create \
         script set and the chain of migrations that would bring it to its newest version. \
         This is what `common/db/bin/dbConfigurator.sh` decides, without the JVM — and unlike \
         that tool it reports the plan before touching anything. Components that ship no \
         scripts for the chosen database are listed with the databases they do support.",
        json!({
            "type": "object",
            "required": ["wm_home"],
            "properties": {
                "wm_home": { "type": "string", "description": "Installation to inspect." },
                "database": { "type": "string", "description": "postgresql, oracle, sqlserver, db2, mysql or sybase (default postgresql)." },
                "components": { "type": "array", "items": { "type": "string" }, "description": "Component names; default every component found." }
            }
        }),
        Box::new(|args| {
            let home = PathBuf::from(req_str(args, "wm_home")?);
            let database = opt_str(args, "database").unwrap_or_else(|| "postgresql".into());
            let wanted = str_list(args, "components");
            let components = wm_core::database::discover(&home).map_err(ToolError::failed)?;

            let mut plans = Vec::new();
            let mut unsupported = Vec::new();
            for component in &components {
                if !wanted.is_empty() && !wanted.contains(&component.name) {
                    continue;
                }
                match wm_core::database::plan(component, &database) {
                    Ok(plan) => plans.push(plan),
                    Err(_) => unsupported.push(json!({
                        "component": component.name,
                        "code": component.code,
                        "ships": wm_core::database::databases(component)
                            .into_iter().collect::<Vec<_>>(),
                    })),
                }
            }
            let scripts: usize = plans.iter().map(|p| p.scripts.len()).sum();
            let summary = format!(
                "{} component(s) installable for {database}, {scripts} script(s) in total; \
                 {} ship no {database} scripts",
                plans.len(),
                unsupported.len()
            );
            Ok(ToolResult::structured(
                summary,
                json!({ "plans": plans, "unsupported": unsupported }),
            ))
        }),
    )
}

/// Create database schemas by driving the shipped configurator.
pub fn database_configure() -> Tool {
    Tool::new(
        "database_configure",
        "Create the database schemas a product needs, by running the product's own \
         common/db/bin/dbConfigurator.sh with the product's own JVM. The schema is never \
         reimplemented here: IBM ships the configurator, its Java classes and its JDBC drivers \
         with the installation, and a schema it created is one IBM supports. What this adds is \
         the orchestration the shipped tool leaves to the caller — pulling in each component's \
         declared prerequisites, ordering them, and reporting the exact commands before running \
         any of them. Every database webMethods supports works, because the tool doing the work \
         is the vendor's. Defaults to a dry run.",
        json!({
            "type": "object",
            "required": ["wm_home", "components", "database", "url", "user", "password"],
            "properties": {
                "wm_home": { "type": "string", "description": "Installation whose configurator to run." },
                "components": { "type": "array", "items": { "type": "string" }, "description": "Component names, e.g. TradingNetworks. Prerequisites are added automatically." },
                "database": { "type": "string", "description": "postgresql, oracle, sqlserver, db2, mysql or sybase." },
                "url": { "type": "string", "description": "JDBC URL, e.g. jdbc:wm:postgresql://host:5432;DatabaseName=wmdb" },
                "user": { "type": "string" },
                "password": { "type": "string" },
                "admin_user": { "type": "string", "description": "Database administrator account, when the action needs one." },
                "admin_password": { "type": "string" },
                "tablespace_dir": { "type": "string" },
                "tablespace_data": { "type": "string" },
                "tablespace_index": { "type": "string" },
                "tablespace_blob": { "type": "string" },
                "bufferpool": { "type": "string" },
                "apply": { "type": "boolean", "description": "Set true to run; otherwise a dry run listing the commands." }
            }
        }),
        Box::new(|args| {
            let home = PathBuf::from(req_str(args, "wm_home")?);
            let database = req_str(args, "database")?;
            let wanted = str_list(args, "components");
            if wanted.is_empty() {
                return Err(ToolError::invalid("name at least one component"));
            }
            let connection = wm_core::database::Connection {
                url: req_str(args, "url")?,
                user: req_str(args, "user")?,
                password: req_str(args, "password")?,
                admin_user: opt_str(args, "admin_user"),
                admin_password: opt_str(args, "admin_password"),
                tablespace_dir: opt_str(args, "tablespace_dir"),
                tablespace_data: opt_str(args, "tablespace_data"),
                tablespace_index: opt_str(args, "tablespace_index"),
                tablespace_blob: opt_str(args, "tablespace_blob"),
                bufferpool: opt_str(args, "bufferpool"),
            };

            let components = wm_core::database::discover(&home).map_err(ToolError::failed)?;
            // Components are not independent: asking for one must install what
            // it declares as a prerequisite, and in the right order.
            let order =
                wm_core::database::order(&components, &wanted).map_err(ToolError::failed)?;
            let dry_run = !flag(args, "apply", false);

            let mut done = Vec::new();
            for component in order {
                let plan =
                    wm_core::database::plan(component, &database).map_err(ToolError::failed)?;
                let invocation =
                    wm_core::database::invocation(&home, component, &database, &connection)
                        .map_err(ToolError::failed)?;
                if dry_run {
                    done.push(json!({
                        "component": component.name,
                        "code": component.code,
                        "target": plan.target,
                        "scripts": plan.scripts.len(),
                        "command": invocation.display(),
                    }));
                    continue;
                }
                let (ok, transcript) =
                    wm_core::database::run(&invocation).map_err(ToolError::failed)?;
                if !ok {
                    return Err(ToolError::failed(format!(
                        "{} failed:\n{}",
                        component.name,
                        tail(&transcript, 25)
                    )));
                }
                done.push(json!({
                    "component": component.name,
                    "code": component.code,
                    "target": plan.target,
                    "scripts": plan.scripts.len(),
                    "status": "complete",
                }));
            }

            let summary = format!(
                "{}: {} component(s) on {database}",
                if dry_run { "dry run" } else { "configured" },
                done.len()
            );
            Ok(ToolResult::structured(
                summary,
                json!({ "components": done }),
            ))
        }),
    )
}

/// The last `lines` lines of a transcript, which is where a failure says why.
fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Provision a profile with the shipped p2 director.
pub fn profile_provision() -> Tool {
    Tool::new(
        "profile_provision",
        "Create an Eclipse p2 profile the supported way: by running the product's own p2 \
         director, from the product's own JVM and launcher, both of which ship with the \
         installation and need no pre-existing profile. Takes about thirty seconds, and that is \
         the price of a profile whose p2 registry IBM's own tooling still recognises. Use \
         profile_capture and profile_replay to copy the result to other machines in a fraction \
         of a second. Defaults to a dry run that prints the command.",
        json!({
            "type": "object",
            "required": ["wm_home", "profile", "roots"],
            "properties": {
                "wm_home": { "type": "string", "description": "Installation whose director and repositories to use." },
                "profile": { "type": "string", "description": "Profile name, e.g. SPM." },
                "destination": { "type": "string", "description": "Where to create it; defaults to <wm_home>/profiles/<profile>." },
                "roots": { "type": "array", "items": { "type": "string" }, "description": "Root features. A `.feature.group` suffix is added if missing." },
                "os": { "type": "string", "description": "Default linux." },
                "ws": { "type": "string", "description": "Default gtk." },
                "arch": { "type": "string", "description": "Default x86_64." },
                "apply": { "type": "boolean", "description": "Set true to run; otherwise a dry run." }
            }
        }),
        Box::new(|args| {
            let home = PathBuf::from(req_str(args, "wm_home")?);
            let profile = req_str(args, "profile")?;
            let destination = opt_str(args, "destination")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("profiles").join(&profile));
            let roots = str_list(args, "roots");
            if roots.is_empty() {
                return Err(ToolError::invalid("name at least one root feature"));
            }
            let env = wm_core::resolve::Environment {
                os: opt_str(args, "os").unwrap_or_else(|| "linux".into()),
                ws: opt_str(args, "ws").unwrap_or_else(|| "gtk".into()),
                arch: opt_str(args, "arch").unwrap_or_else(|| "x86_64".into()),
            };

            let invocation =
                wm_core::profile::director::invocation(&home, &destination, &profile, &roots, &env)
                    .map_err(ToolError::failed)?;
            if !flag(args, "apply", false) {
                return Ok(ToolResult::structured(
                    format!(
                        "dry run: would provision {profile} into {}",
                        destination.display()
                    ),
                    json!({ "command": invocation.display() }),
                ));
            }

            let started = std::time::Instant::now();
            let (ok, transcript) =
                wm_core::profile::director::run(&invocation).map_err(ToolError::failed)?;
            if !ok {
                return Err(ToolError::failed(format!(
                    "the director failed:\n{}",
                    tail(&transcript, 25)
                )));
            }
            let bundles = destination
                .join("configuration/org.eclipse.equinox.simpleconfigurator/bundles.info");
            let count = std::fs::read_to_string(&bundles)
                .map(|t| {
                    t.lines()
                        .filter(|l| !l.starts_with('#') && l.contains(','))
                        .count()
                })
                .unwrap_or(0);
            Ok(ToolResult::structured(
                format!(
                    "provisioned {profile}: {count} bundle(s) in {:.1}s",
                    started.elapsed().as_secs_f64()
                ),
                json!({ "profile": profile, "destination": destination, "bundles": count }),
            ))
        }),
    )
}
