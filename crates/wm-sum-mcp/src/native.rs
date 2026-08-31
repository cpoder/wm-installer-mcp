//! Native fix tools: ask IBM directly, no Update Manager involved.

use std::path::{Path, PathBuf};

use mcp_rt::args::{flag, opt_str, req_str};
use mcp_rt::{Tool, ToolError, ToolResult};
use serde_json::{json, Value};
use wm_core::fixes::{self, Inventory};
use wm_core::sdc::{self, Session};

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

fn install_dir(args: &Value) -> Result<PathBuf, ToolError> {
    opt_str(args, "install_dir")
        .or_else(|| std::env::var("WM_HOME").ok())
        .map(PathBuf::from)
        .ok_or_else(|| ToolError::invalid("no install_dir given and WM_HOME is not set"))
}

/// Resolve the release of an installation from its own product versions.
fn release_of(install_dir: &Path) -> Option<String> {
    let catalog = wm_core::catalog::Catalog::load(install_dir).ok()?;
    // Take the most common major.minor across products: a tree carries a few
    // components at older versions (EDI at 9.12, EDIINT at 8.2), so the mode is
    // a better answer than the first entry.
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for product in catalog.iter() {
        let version = product.path.version();
        let mut parts = version.split('.');
        if let (Some(major), Some(minor)) = (parts.next(), parts.next()) {
            *counts.entry(format!("{major}.{minor}")).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(release, _)| release)
}

/// List the fixes IBM offers for an installation.
pub fn fixes_available() -> Tool {
    Tool::new(
        "fixes_available",
        "Ask IBM which fixes apply to an installation, natively — no Update Manager, no \
         terminal, no console wizard. Reads the installation's own product records to build \
         the inventory the service expects, and returns each fix with its version, target \
         product, group and download size.",
        json!({
            "type": "object",
            "properties": {
                "install_dir": { "type": "string", "description": "Installation to check; defaults to $WM_HOME." },
                "release": { "type": "string", "description": "Release, e.g. 12.1. Inferred from the installation when omitted." },
                "platform": { "type": "string", "description": "LNXAMD64 by default." },
                "show_all": { "type": "boolean", "description": "Return everything published rather than only what is missing." },
                "host": { "type": "string" }
            }
        }),
        Box::new(|args| {
            let (user, key) = credentials()?;
            let target = install_dir(args)?;
            let platform = opt_str(args, "platform").unwrap_or_else(|| "LNXAMD64".into());
            let release_wanted = opt_str(args, "release")
                .or_else(|| release_of(&target))
                .ok_or_else(|| {
                    ToolError::failed(format!(
                        "cannot tell which release {} is; pass release explicitly",
                        target.display()
                    ))
                })?;

            let session = Session::login(&host(args), &user, &key).map_err(ToolError::failed)?;
            let releases = session.releases().map_err(ToolError::failed)?;
            let release = releases
                .iter()
                .find(|r| r.release == release_wanted)
                .ok_or_else(|| {
                    ToolError::failed(format!("no entitlement for release {release_wanted}"))
                })?;
            let sandbox = release
                .sandbox()
                .ok_or_else(|| ToolError::failed("release names no sandbox"))?;
            let fix_repository = session
                .fix_repository(&sandbox)
                .map_err(ToolError::failed)?
                .ok_or_else(|| ToolError::failed("sandbox publishes no fix repository"))?;

            let inventory = Inventory::read(&target, &platform).map_err(ToolError::failed)?;
            let show_all = flag(args, "show_all", false);
            let found = fixes::available(&session, &fix_repository, &inventory, show_all)
                .map_err(ToolError::failed)?;

            let total: u64 = found.iter().filter_map(|f| f.size).sum();
            Ok(ToolResult::structured(
                format!(
                    "{} fix(es) for {} ({release_wanted}, {} products), {:.2} GB",
                    found.len(),
                    target.display(),
                    inventory.products.len(),
                    total as f64 / 1e9
                ),
                json!({
                    "release": release_wanted,
                    "fix_repository": fix_repository,
                    "inventory_products": inventory.products.len(),
                    "show_all": show_all,
                    "fixes": found,
                    "total_bytes": total,
                }),
            ))
        }),
    )
}

/// Show the inventory that would be sent, without sending it.
pub fn fixes_inventory() -> Tool {
    Tool::new(
        "fixes_inventory",
        "Build the inventory document that describes an installation to IBM's fix service, and \
         return it without contacting anyone. Useful to see exactly what would be disclosed, \
         and to check an installation is legible before asking about fixes.",
        json!({
            "type": "object",
            "properties": {
                "install_dir": { "type": "string" },
                "platform": { "type": "string" }
            }
        }),
        Box::new(|args| {
            let target = install_dir(args)?;
            let platform = opt_str(args, "platform").unwrap_or_else(|| "LNXAMD64".into());
            let inventory = Inventory::read(&target, &platform).map_err(ToolError::failed)?;
            Ok(ToolResult::structured(
                format!(
                    "{} products from {}; inferred release {}",
                    inventory.products.len(),
                    target.display(),
                    release_of(&target).unwrap_or_else(|| "unknown".into())
                ),
                json!({
                    "inventory": inventory,
                    "request_body": inventory.to_request(),
                }),
            ))
        }),
    )
}

/// Download fixes into a local directory.
pub fn fixes_download() -> Tool {
    Tool::new(
        "fixes_download",
        "Download fixes from IBM into a directory, verified against the sha256 the repository          declares — no Update Manager, no terminal. The result is an offline set usable on a          disconnected host. Applying a fix still needs Update Manager: the p2 provisioning          actions are not reimplemented.",
        json!({
            "type": "object",
            "required": ["output_dir"],
            "properties": {
                "output_dir": { "type": "string", "description": "Where to write the fixes." },
                "install_dir": { "type": "string" },
                "release": { "type": "string" },
                "platform": { "type": "string" },
                "fixes": { "type": "array", "items": { "type": "string" }, "description": "Fix ids to take; all applicable ones when omitted." },
                "with_readmes": { "type": "boolean", "description": "Also fetch each fix's readme (default true)." },
                "host": { "type": "string" }
            }
        }),
        Box::new(|args| {
            let (user, key) = credentials()?;
            let target = install_dir(args)?;
            let output = PathBuf::from(req_str(args, "output_dir")?);
            let platform = opt_str(args, "platform").unwrap_or_else(|| "LNXAMD64".into());
            let release_wanted = opt_str(args, "release")
                .or_else(|| release_of(&target))
                .ok_or_else(|| ToolError::failed("cannot tell which release this is"))?;
            let wanted: Vec<String> = args
                .get("fixes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();

            let mut session =
                Session::login(&host(args), &user, &key).map_err(ToolError::failed)?;
            let releases = session.releases().map_err(ToolError::failed)?;
            let release = releases
                .iter()
                .find(|r| r.release == release_wanted)
                .ok_or_else(|| ToolError::failed(format!("no entitlement for {release_wanted}")))?;
            let sandbox = release.sandbox().ok_or_else(|| ToolError::failed("no sandbox"))?;
            let cgi = release.cgi().ok_or_else(|| ToolError::failed("no CGI"))?.to_string();
            let fix_repository = session
                .fix_repository(&sandbox)
                .map_err(ToolError::failed)?
                .ok_or_else(|| ToolError::failed("sandbox publishes no fix repository"))?;

            let inventory = Inventory::read(&target, &platform).map_err(ToolError::failed)?;
            let offered = fixes::available(&session, &fix_repository, &inventory, false)
                .map_err(ToolError::failed)?;
            let selected: Vec<&wm_core::fixes::Fix> = if wanted.is_empty() {
                offered.iter().collect()
            } else {
                offered
                    .iter()
                    .filter(|f| wanted.iter().any(|w| *w == f.id || *w == f.label()))
                    .collect()
            };
            if selected.is_empty() {
                return Err(ToolError::failed(
                    "no fix matched; call fixes_available to see what is offered".to_string(),
                ));
            }

            let index_bytes = session
                .fix_artifact_index(&cgi, &fix_repository)
                .map_err(ToolError::failed)?;
            let index = fixes::parse_artifact_index(&index_bytes).map_err(ToolError::failed)?;
            std::fs::create_dir_all(&output)
                .map_err(|e| ToolError::failed(format!("cannot create {}: {e}", output.display())))?;

            let keep_readmes = flag(args, "with_readmes", true);
            let mut written = Vec::new();
            let mut bytes_total = 0u64;
            for fix in &selected {
                for artifact in fixes::artifacts_of(&index, fix) {
                    if artifact.classifier == "readme" && !keep_readmes {
                        continue;
                    }
                    let path = artifact.path();
                    let bytes = session
                        .fix_artifact(&cgi, &fix_repository, &path)
                        .map_err(ToolError::failed)?;
                    if let Some(expected) = &artifact.sha256 {
                        let got = wm_core::sdc::sha256_hex(&bytes);
                        if got != *expected {
                            return Err(ToolError::failed(format!(
                                "{path} failed verification: expected {expected}, got {got}"
                            )));
                        }
                    }
                    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
                    let file = output.join(&name);
                    std::fs::write(&file, &bytes)
                        .map_err(|e| ToolError::failed(format!("cannot write {name}: {e}")))?;
                    bytes_total += bytes.len() as u64;
                    written.push(json!({ "file": name, "bytes": bytes.len(), "verified": artifact.sha256.is_some() }));
                }
            }
            Ok(ToolResult::structured(
                format!(
                    "{} fix(es), {} file(s), {:.2} GB into {}",
                    selected.len(),
                    written.len(),
                    bytes_total as f64 / 1e9,
                    output.display()
                ),
                json!({
                    "output_dir": output,
                    "fix_repository": fix_repository,
                    "fixes": selected.iter().map(|f| f.label()).collect::<Vec<_>>(),
                    "files": written,
                    "total_bytes": bytes_total,
                }),
            ))
        }),
    )
}

/// Read a downloaded fix and report what applying it would do.
pub fn fix_inspect() -> Tool {
    Tool::new(
        "fix_inspect",
        "Read a downloaded fix archive and report its recipe: the manifest, the p2 repositories          it refreshes, the numbered install phases with their actions, and which actions this          engine cannot perform. A fix is a signed JAR rooted at the installation directory plus          META-INF/instructions.txt; nothing is written.",
        json!({
            "type": "object",
            "required": ["path"],
            "properties": { "path": { "type": "string", "description": "Path to the fix archive." } }
        }),
        Box::new(|args| {
            let path = req_str(args, "path")?;
            let fix = wm_core::fix::Fix::read(Path::new(&path)).map_err(ToolError::failed)?;
            let unsupported = fix.unsupported().len();
            let mut summary = format!(
                "{}: {} entries, {} phase(s)",
                fix.display_name.clone().or_else(|| fix.name.clone()).unwrap_or_else(|| path.clone()),
                fix.entries.len(),
                fix.phases.len()
            );
            if unsupported > 0 {
                summary.push_str(&format!("; {unsupported} action(s) need a p2 director"));
            }
            Ok(ToolResult::structured(summary, json!({ "fix": fix })))
        }),
    )
}

/// Apply a fix to an installation.
pub fn fix_apply() -> Tool {
    Tool::new(
        "fix_apply",
        "Apply a downloaded fix to an installation natively — extract, delete and OSGi cache          actions, no Update Manager. Defaults to a dry run, which is the right first call: a          fix expects its runtimes stopped, and this reports whether they look like they are          running. Actions that need a p2 director are listed as not performed rather than          silently skipped.",
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string", "description": "Path to the fix archive." },
                "install_dir": { "type": "string", "description": "Installation to patch; defaults to $WM_HOME." },
                "apply": { "type": "boolean", "description": "Set true to write; otherwise a dry run." }
            }
        }),
        Box::new(|args| {
            let path = req_str(args, "path")?;
            let target = install_dir(args)?;
            if !target.is_dir() {
                return Err(ToolError::invalid(format!("no installation at {}", target.display())));
            }
            let fix = wm_core::fix::Fix::read(Path::new(&path)).map_err(ToolError::failed)?;
            let dry_run = !flag(args, "apply", false);
            let applied =
                wm_core::fix::apply(&fix, &target, dry_run).map_err(ToolError::failed)?;

            let mut summary = format!(
                "{}: {} file(s) {}, {} deleted, {} bundle(s) replaced in profiles, {} cache(s) cleared",
                if dry_run { "dry run" } else { "applied" },
                applied.extracted.len(),
                if dry_run { "would be written" } else { "written" },
                applied.deleted.len(),
                applied.profile_updates.len(),
                applied.caches_cleared.len()
            );
            if !applied.not_performed.is_empty() {
                summary.push_str(&format!("; {} action(s) not performed", applied.not_performed.len()));
            }
            if !applied.warnings.is_empty() {
                summary.push_str(&format!("; {} warning(s)", applied.warnings.len()));
            }
            Ok(ToolResult::structured(
                summary,
                json!({
                    "fix": fix.name,
                    "result": applied,
                    "profiles_to_stop": fix.profiles(),
                }),
            ))
        }),
    )
}

/// Parse a `content.jar` that was captured elsewhere.
pub fn fixes_parse_metadata() -> Tool {
    Tool::new(
        "fixes_parse_metadata",
        "Parse a p2 fix metadata archive (content.jar) into a fix list. Update Manager caches \
         these; reading one tells you what a past session was offered without repeating the \
         call.",
        json!({
            "type": "object",
            "required": ["path"],
            "properties": { "path": { "type": "string", "description": "Path to content.jar." } }
        }),
        Box::new(|args| {
            let path = req_str(args, "path")?;
            let bytes = std::fs::read(&path)
                .map_err(|e| ToolError::failed(format!("cannot read {path}: {e}")))?;
            let found = fixes::parse_content_jar(&bytes).map_err(ToolError::failed)?;
            Ok(ToolResult::structured(
                format!("{} fix(es) in {path}", found.len()),
                json!({ "fixes": found }),
            ))
        }),
    )
}
