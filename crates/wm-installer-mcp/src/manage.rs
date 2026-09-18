//! Tools that manage this server's own memory rather than an installation.
//!
//! Three things the servers had nowhere to keep: which installations exist and
//! what is in them, the settings that were otherwise re-stated on every call,
//! and credentials.
//!
//! # Why these do not default to a dry run
//!
//! Every tool that changes an *installation* defaults to a dry run, because its
//! defaults — ports, instance names, install locations — are reasonable and
//! often wrong for a given site, and the user is the only one who knows which.
//! Nothing here has that shape: every value is supplied by the caller, the
//! effect is one small file under the config directory, and it is undone by
//! calling again. A confirmation step for "write this name down" is friction
//! that buys nothing. The two that destroy something — forgetting an
//! installation, removing a credential — take `confirm` instead.

use std::path::PathBuf;

use mcp_rt::args::{flag, opt_str, req_str, str_list};
use mcp_rt::{Tool, ToolError, ToolResult};
use serde_json::{json, Value};
use wm_core::config::{self, Defaults};
use wm_core::inventory::Inventory;
use wm_core::registry::{self, Install};
use wm_core::secrets::{self, Store};

/// Register an installation, or update the record of one.
pub fn install_register() -> Tool {
    Tool::new(
        "install_register",
        "Record an installation under a short name, so later calls can say install: \"b2b\" \
         instead of repeating its path, and so the release, platform and download centre it \
         was built from — none of which an installation records about itself — survive the \
         session. Reads the installation immediately and stores the component list as a dated \
         snapshot. Registering a name that already exists updates it, keeping the fields not \
         given. Takes effect at once; there is no dry run.",
        json!({
            "type": "object",
            "required": ["name", "wm_home"],
            "properties": {
                "name": { "type": "string", "description": "Short handle: letters, digits, '-', '_' and '.'." },
                "wm_home": { "type": "string", "description": "Installation root." },
                "release": { "type": "string", "description": "Release it was built from, e.g. 12.1." },
                "platform": { "type": "string", "description": "Platform code, e.g. LNXAMD64." },
                "host": { "type": "string", "description": "Download centre it was fetched from." },
                "sum_home": { "type": "string", "description": "Update Manager home that patches it." },
                "installer_bin": { "type": "string", "description": "Shipped installer binary used against it." },
                "installer_jar": { "type": "string", "description": "sagInstaller.jar, for install/jars/DistMan.jar." },
                "requested": { "type": "array", "items": { "type": "string" }, "description": "What was asked for, before the dependency closure." },
                "notes": { "type": "string" }
            }
        }),
        Box::new(|args| {
            let name = req_str(args, "name")?;
            let wm_home = PathBuf::from(req_str(args, "wm_home")?);
            let mut record = match registry::get(&name) {
                Ok(mut existing) => {
                    existing.wm_home = wm_home;
                    existing
                }
                Err(_) => Install::new(&name, &wm_home).map_err(ToolError::invalid)?,
            };
            set_if_given(args, "release", &mut record.release);
            set_if_given(args, "platform", &mut record.platform);
            set_if_given(args, "host", &mut record.host);
            set_path_if_given(args, "sum_home", &mut record.sum_home);
            set_path_if_given(args, "installer_bin", &mut record.installer_bin);
            set_path_if_given(args, "installer_jar", &mut record.installer_jar);
            set_if_given(args, "notes", &mut record.notes);
            let requested = str_list(args, "requested");
            if !requested.is_empty() {
                record.requested = requested;
            }

            // Read it now rather than at first use: an unreadable path is worth
            // knowing about while the caller is still looking at it.
            let inventory = record.refresh().map_err(|e| {
                ToolError::failed(format!(
                    "cannot read the installation at {}: {e}",
                    record.wm_home.display()
                ))
            })?;
            record.save().map_err(ToolError::failed)?;

            Ok(ToolResult::structured(
                format!(
                    "registered {name} -> {} ({} products, {} runtime(s), {} fix readme(s))",
                    record.wm_home.display(),
                    inventory.products.len(),
                    inventory.runtimes.len(),
                    inventory.fixes.len(),
                ),
                json!({ "install": record, "record_path": record.path() }),
            ))
        }),
    )
}

/// Every registered installation, with what each carries.
pub fn install_list() -> Tool {
    Tool::new(
        "install_list",
        "List the installations this machine knows about and what is in each: product count, \
         Integration Server instances and platform profiles, fix readmes, and the release and \
         platform it was built from. Components are read from each installation's own \
         install/products/*.prop when its path is readable, so the answer is current rather \
         than remembered; when it is not readable the stored snapshot is returned with its \
         date and said to be a snapshot.",
        json!({ "type": "object", "properties": {
            "products": { "type": "boolean", "description": "Include every product path, not just the count." }
        } }),
        Box::new(|args| {
            let installs = registry::list().map_err(ToolError::failed)?;
            if installs.is_empty() {
                return Ok(ToolResult::structured(
                    format!(
                        "no installations are registered. install_register records one; the \
                         records live in {}",
                        config::installs_dir().display()
                    ),
                    json!({ "installs": [] }),
                ));
            }
            let detailed = flag(args, "products", false);
            let rows: Vec<Value> = installs.iter().map(|i| summarise(i, detailed)).collect();
            let lines: Vec<String> = installs
                .iter()
                .zip(&rows)
                .map(|(install, row)| {
                    format!(
                        "  {:16}  {:40}  {} products, {} runtime(s), {} fix(es){}",
                        install.name,
                        install.wm_home.display(),
                        row["products"].as_u64().unwrap_or(0),
                        row["runtimes"].as_array().map_or(0, Vec::len),
                        row["fixes"].as_u64().unwrap_or(0),
                        match row["source"].as_str() {
                            Some("snapshot") => format!(
                                " — from a snapshot of {}, the path is not readable now",
                                row["taken_at"].as_str().unwrap_or("an unknown date")
                            ),
                            _ => String::new(),
                        }
                    )
                })
                .collect();
            Ok(ToolResult::structured(
                format!(
                    "{} registered installation(s):\n{}",
                    installs.len(),
                    lines.join("\n")
                ),
                json!({ "installs": rows }),
            ))
        }),
    )
}

/// One installation in full.
pub fn install_show() -> Tool {
    Tool::new(
        "install_show",
        "Everything recorded about one installation, plus a fresh read of what is on its \
         disk: products, Integration Server instances, platform profiles and fix readmes. \
         Reports what has changed since the stored snapshot was taken — a product installed \
         or removed behind this server's back shows up here rather than in a plan that \
         assumed otherwise.",
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" },
                "refresh": { "type": "boolean", "description": "Store the fresh read as the new snapshot (default true)." }
            }
        }),
        Box::new(|args| {
            let name = req_str(args, "name")?;
            let mut record = registry::get(&name).map_err(ToolError::invalid)?;
            let inventory = Inventory::read(&record.wm_home).map_err(|e| {
                ToolError::failed(format!(
                    "{name} is registered at {} but cannot be read: {e}",
                    record.wm_home.display()
                ))
            })?;
            let drift = drift_against_snapshot(&record, &inventory);

            let mut summary = format!(
                "{name} at {}: {} products, {} runtime(s), {} fix readme(s)",
                record.wm_home.display(),
                inventory.products.len(),
                inventory.runtimes.len(),
                inventory.fixes.len(),
            );
            if let (Some(release), Some(platform)) = (&record.release, &record.platform) {
                summary.push_str(&format!("\nbuilt from {release} on {platform}"));
            }
            if let Some(drift) = &drift {
                summary.push_str(&format!(
                    "\n\nchanged since the snapshot of {}: {} product(s) added, {} removed",
                    drift["since"].as_str().unwrap_or("?"),
                    drift["added"].as_array().map_or(0, Vec::len),
                    drift["removed"].as_array().map_or(0, Vec::len),
                ));
            }
            if flag(args, "refresh", true) {
                record.snapshot = Some(wm_core::registry::Snapshot::of(&inventory));
                record.save().map_err(ToolError::failed)?;
            }
            Ok(ToolResult::structured(
                summary,
                json!({ "install": record, "inventory": inventory, "drift": drift }),
            ))
        }),
    )
}

/// Forget an installation, leaving the installation itself alone.
pub fn install_forget() -> Tool {
    Tool::new(
        "install_forget",
        "Remove a registered installation's record. The installation on disk is not touched \
         and nothing in it changes: this deletes a note about it. Needs confirm=true.",
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" },
                "confirm": { "type": "boolean", "description": "Set true to remove the record." }
            }
        }),
        Box::new(|args| {
            let name = req_str(args, "name")?;
            let record = registry::get(&name).map_err(ToolError::invalid)?;
            if !flag(args, "confirm", false) {
                return Ok(ToolResult::structured(
                    format!(
                        "would forget {name} ({}). The installation itself is untouched. Call \
                         again with confirm=true.",
                        record.wm_home.display()
                    ),
                    json!({ "install": record }),
                ));
            }
            let removed = registry::forget(&name).map_err(ToolError::failed)?;
            Ok(ToolResult::text(if removed {
                format!("forgot {name}. {} is untouched.", record.wm_home.display())
            } else {
                format!("{name} was not registered")
            }))
        }),
    )
}

/// Where everything lives, and what is configured.
pub fn config_show() -> Tool {
    Tool::new(
        "config_show",
        "Report this server's configuration: the stored defaults and where each directory is \
         — config, state, jobs, catalogue cache, artifact cache — plus whether the credential \
         store holds anything and how it is sealed. The one call that answers \"where did that \
         job's log go\" and \"which release will a call without one use\".",
        json!({ "type": "object", "properties": {} }),
        Box::new(|_args| {
            let defaults = Defaults::load().map_err(ToolError::failed)?;
            let store = Store::open().ok();
            let settings: Vec<Value> = config::DEFAULT_KEYS
                .iter()
                .map(|key| {
                    json!({
                        "setting": key,
                        "value": defaults.get(key),
                    })
                })
                .collect();
            let lines: Vec<String> = config::DEFAULT_KEYS
                .iter()
                .map(|key| {
                    format!(
                        "  {:14}  {}",
                        key,
                        defaults.get(key).unwrap_or_else(|| "(not set)".into())
                    )
                })
                .collect();
            let credentials = match &store {
                Some(store) => format!(
                    "{} secret(s), {}",
                    store.names().len(),
                    store.protection().explain()
                ),
                None => "cannot be opened — credential_list says why".to_string(),
            };
            Ok(ToolResult::structured(
                format!(
                    "defaults:\n{}\n\ndirectories:\n  config       {}\n  installs     {}\n  \
                     state        {}\n  jobs         {}\n  catalogue    {}\n  artifacts    {}\n\n\
                     credentials: {credentials}\n\nThe config directory is pinned under $HOME \
                     and does not follow WM_STATE_DIR; move it with WM_CONFIG_DIR.",
                    lines.join("\n"),
                    config::config_dir().display(),
                    config::installs_dir().display(),
                    config::state_dir().display(),
                    config::jobs_dir().display(),
                    config::catalog_dir().display(),
                    config::artifacts_dir().display(),
                ),
                json!({
                    "defaults": settings,
                    "directories": {
                        "config": config::config_dir(),
                        "installs": config::installs_dir(),
                        "state": config::state_dir(),
                        "jobs": config::jobs_dir(),
                        "catalog": config::catalog_dir(),
                        "artifacts": config::artifacts_dir(),
                    },
                    "credentials": {
                        "count": store.as_ref().map(|s| s.names().len()),
                        "protection": store.as_ref().map(|s| s.protection()),
                    },
                }),
            ))
        }),
    )
}

/// Set a default.
pub fn config_set() -> Tool {
    Tool::new(
        "config_set",
        "Set a default that calls may then omit: release, platform, host, installer_bin, \
         installer_jar, install. An explicit argument always wins over a default; an empty \
         value clears one. Takes effect at once; there is no dry run.",
        json!({
            "type": "object",
            "required": ["setting", "value"],
            "properties": {
                "setting": { "type": "string", "description": "release, platform, host, installer_bin, installer_jar or install." },
                "value": { "type": "string", "description": "The value, or \"\" to clear it." }
            }
        }),
        Box::new(|args| {
            let key = req_str(args, "setting")?;
            let value = opt_str(args, "value").unwrap_or_default();
            let mut defaults = Defaults::load().map_err(ToolError::failed)?;
            let previous = defaults.get(&key);
            defaults.set(&key, &value).map_err(ToolError::invalid)?;
            // A default naming an installation that is not registered would
            // fail later, in a call that did not mention it.
            if key == "install" && !value.trim().is_empty() {
                registry::get(&value).map_err(ToolError::invalid)?;
            }
            defaults.save().map_err(ToolError::failed)?;
            Ok(ToolResult::structured(
                match (&previous, defaults.get(&key)) {
                    (_, Some(now)) => format!("{key} = {now}"),
                    (Some(was), None) => format!("{key} cleared (was {was})"),
                    (None, None) => format!("{key} was not set"),
                },
                json!({ "setting": key, "value": defaults.get(&key), "previous": previous }),
            ))
        }),
    )
}

/// Store a secret.
pub fn credential_set() -> Tool {
    Tool::new(
        "credential_set",
        "Store a secret in the encrypted credential store, so it does not have to live in the \
         MCP client's configuration file or the environment. The IBM entitlement key goes \
         under \"empower.key\" and the account under \"empower.user\"; a secret belonging to \
         one registered installation is named install.<name>.<what>, for example \
         install.b2b.db.password. The value is never returned by any tool, never written into \
         a generated script, and never put in a job wrapper. Takes effect at once.",
        json!({
            "type": "object",
            "required": ["name", "value"],
            "properties": {
                "name": { "type": "string", "description": "empower.user, empower.key, or install.<name>.<what>." },
                "value": { "type": "string" }
            }
        }),
        Box::new(|args| {
            let name = req_str(args, "name")?;
            let value = req_str(args, "value")?;
            let mut store = Store::open().map_err(ToolError::failed)?;
            let replaced = store.set(&name, &value);
            store.save().map_err(ToolError::failed)?;
            let shadowed = shadowing_variable(&name).filter(|var| std::env::var_os(var).is_some());
            let mut summary = format!(
                "{} {name} in {} ({})",
                if replaced { "replaced" } else { "stored" },
                store.path().display(),
                store.protection().explain(),
            );
            if let Some(var) = shadowed {
                summary.push_str(&format!(
                    "\n\nnote: ${var} is set in this server's environment and takes precedence \
                     over the stored value. Unset it for the store to be used."
                ));
            }
            Ok(ToolResult::structured(
                summary,
                json!({ "name": name, "replaced": replaced, "shadowed_by": shadowed }),
            ))
        }),
    )
}

/// What the store holds — names only.
pub fn credential_list() -> Tool {
    Tool::new(
        "credential_list",
        "Name what the credential store holds, how it is sealed and where it is. Values are \
         never returned. Also reports which stored secrets are currently shadowed by an \
         environment variable of the same meaning, which is the usual reason a key that was \
         stored does not appear to be in use.",
        json!({ "type": "object", "properties": {} }),
        Box::new(|_args| {
            let store = Store::open().map_err(ToolError::failed)?;
            let names = store.names();
            let shadowed: Vec<&str> = names
                .iter()
                .filter(|name| {
                    shadowing_variable(name).is_some_and(|v| std::env::var_os(v).is_some())
                })
                .copied()
                .collect();
            let mut summary = if names.is_empty() {
                format!(
                    "the credential store at {} is empty. credential_set fills it; \
                     {} is what seals it.",
                    store.path().display(),
                    store.protection().explain()
                )
            } else {
                format!(
                    "{} secret(s) in {}:\n{}\n\n{}",
                    names.len(),
                    store.path().display(),
                    names
                        .iter()
                        .map(|n| format!("  {n}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    store.protection().explain(),
                )
            };
            if !shadowed.is_empty() {
                summary.push_str(&format!(
                    "\n\nshadowed by the environment, which wins: {}",
                    shadowed.join(", ")
                ));
            }
            Ok(ToolResult::structured(
                summary,
                json!({
                    "path": store.path(),
                    "protection": store.protection(),
                    "names": names,
                    "shadowed_by_environment": shadowed,
                }),
            ))
        }),
    )
}

/// Remove a secret.
pub fn credential_remove() -> Tool {
    Tool::new(
        "credential_remove",
        "Remove a secret from the credential store. Needs confirm=true — nothing else holds a \
         copy, and an entitlement key has to be reissued rather than recovered.",
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" },
                "confirm": { "type": "boolean" }
            }
        }),
        Box::new(|args| {
            let name = req_str(args, "name")?;
            let mut store = Store::open().map_err(ToolError::failed)?;
            if store.get(&name).is_none() {
                return Err(ToolError::invalid(format!(
                    "the store holds no {name:?}; credential_list names what it holds"
                )));
            }
            if !flag(args, "confirm", false) {
                return Ok(ToolResult::text(format!(
                    "would remove {name} from {}. Nothing else holds a copy. Call again with \
                     confirm=true.",
                    store.path().display()
                )));
            }
            store.remove(&name);
            store.save().map_err(ToolError::failed)?;
            Ok(ToolResult::text(format!("removed {name}")))
        }),
    )
}

/// Check a shipped installer binary against what the catalogue expects.
pub fn installer_check() -> Tool {
    Tool::new(
        "installer_check",
        "Report the version of a shipped installer binary and whether the download centre has \
         outgrown it. The server rejects an old client only after the run has authenticated \
         and fetched the whole product list — a minute in, with a message that reads like a \
         network fault — so this answers the question first, from the binary's own header and \
         a cached catalogue. No credentials and no network.\n\n\
         There is no tool that fetches a newer installer: the .bin is not in the product tree \
         (the catalogue's installer product carries panel jars, not the client) and it is \
         distributed through Passport Advantage and Fix Central, which authenticate an IBMid \
         rather than an entitlement key. Neither is a protocol this server speaks. \
         native_install needs no installer binary at all.",
        json!({
            "type": "object",
            "properties": {
                "installer_bin": { "type": "string", "description": "The binary to check. Defaults to the configured installer_bin, then $WM_INSTALLER_BIN." },
                "platform": { "type": "string", "description": "Platform whose cached catalogue to compare against (default LNXAMD64)." }
            }
        }),
        Box::new(|args| {
            let bin = opt_str(args, "installer_bin")
                .or_else(|| {
                    Defaults::load()
                        .ok()
                        .and_then(|d| d.installer_bin)
                        .map(|p| p.display().to_string())
                })
                .or_else(|| std::env::var("WM_INSTALLER_BIN").ok())
                .map(PathBuf::from)
                .ok_or_else(|| {
                    ToolError::invalid(
                        "no installer_bin given, none configured, and $WM_INSTALLER_BIN is not set",
                    )
                })?;
            if !bin.is_file() {
                return Err(ToolError::invalid(format!(
                    "no installer at {}",
                    bin.display()
                )));
            }
            let catalog = crate::tools::cached_catalog(args);
            let check =
                wm_core::client::check(Some(&bin), catalog.as_ref()).map_err(ToolError::failed)?;
            let summary = match (&check.local, &check.catalog) {
                (Some(local), Some(_)) => match check.warning() {
                    Some(warning) => format!("{}: {warning}", bin.display()),
                    None => format!(
                        "{} is installer client {local}, which the catalogue accepts",
                        bin.display()
                    ),
                },
                (Some(local), None) => format!(
                    "{} is installer client {local}. No cached catalogue for this platform, so \
                     there is nothing to compare it against — run sdc_catalog first.",
                    bin.display()
                ),
                (None, _) => format!(
                    "{} declares no VERSION in its header: it is not a self-extracting \
                     installer of the shape this reads, or it is from another generation.",
                    bin.display()
                ),
            };
            Ok(ToolResult::structured(
                summary,
                json!({ "installer_bin": bin, "check": check }),
            ))
        }),
    )
}

/// Overwrite `field` when the call names it; an empty value clears it.
fn set_if_given(args: &Value, key: &str, field: &mut Option<String>) {
    if let Some(value) = args.get(key).and_then(Value::as_str) {
        *field = (!value.trim().is_empty()).then(|| value.trim().to_string());
    }
}

/// The same for a path-valued field.
fn set_path_if_given(args: &Value, key: &str, field: &mut Option<PathBuf>) {
    let mut text = field.as_ref().map(|p| p.display().to_string());
    set_if_given(args, key, &mut text);
    *field = text.map(PathBuf::from);
}

/// The environment variable that would override a stored secret, if any.
fn shadowing_variable(name: &str) -> Option<&'static str> {
    match name {
        secrets::EMPOWER_USER => Some("WM_EMPOWER_USER"),
        secrets::EMPOWER_KEY => Some("WM_EMPOWER_KEY"),
        _ => None,
    }
}

/// One row for `install_list`, read live when the path allows and from the
/// stored snapshot when it does not.
fn summarise(install: &Install, detailed: bool) -> Value {
    let live = Inventory::read(&install.wm_home).ok();
    let (products, runtimes, fixes, source, taken_at) = match (&live, &install.snapshot) {
        (Some(inventory), _) => (
            inventory.products.iter().map(|p| p.path.clone()).collect(),
            inventory
                .runtimes
                .iter()
                .map(|r| format!("{}:{}", r.kind, r.name))
                .collect::<Vec<_>>(),
            inventory.fixes.len(),
            "disk",
            None,
        ),
        (None, Some(snapshot)) => (
            snapshot.products.clone(),
            snapshot.runtimes.clone(),
            snapshot.fixes.len(),
            "snapshot",
            Some(snapshot.taken_at.clone()),
        ),
        (None, None) => (Vec::new(), Vec::new(), 0, "unreadable", None),
    };
    json!({
        "name": install.name,
        "wm_home": install.wm_home,
        "release": install.release,
        "platform": install.platform,
        "sum_home": install.sum_home,
        "readable": live.is_some(),
        "source": source,
        "taken_at": taken_at,
        "products": products.len(),
        "product_paths": detailed.then_some(products),
        "runtimes": runtimes,
        "fixes": fixes,
        "notes": install.notes,
    })
}

/// What changed on disk since the record's snapshot was taken.
fn drift_against_snapshot(install: &Install, inventory: &Inventory) -> Option<Value> {
    let snapshot = install.snapshot.as_ref()?;
    let before: std::collections::BTreeSet<&str> =
        snapshot.products.iter().map(String::as_str).collect();
    let now: std::collections::BTreeSet<&str> =
        inventory.products.iter().map(|p| p.path.as_str()).collect();
    let added: Vec<&str> = now.difference(&before).copied().collect();
    let removed: Vec<&str> = before.difference(&now).copied().collect();
    let fixes_before = snapshot.fixes.len();
    let fixes_now = inventory.fixes.len();
    if added.is_empty() && removed.is_empty() && fixes_before == fixes_now {
        return None;
    }
    Some(json!({
        "since": snapshot.taken_at,
        "added": added,
        "removed": removed,
        "fixes_before": fixes_before,
        "fixes_now": fixes_now,
    }))
}

/// Resolve `wm_home`-shaped arguments through the registry, for the tools that
/// take one. Exposed so both servers spell it the same way.
pub fn resolve_home(given: &str) -> Result<PathBuf, ToolError> {
    registry::resolve(given).map_err(ToolError::invalid)
}
