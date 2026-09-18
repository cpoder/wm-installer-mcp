//! Native tools: talk to IBM directly, no shipped installer involved.

use std::path::{Path, PathBuf};

use mcp_rt::args::{flag, opt_str, opt_usize, req_str, str_list};
use mcp_rt::{Tool, ToolError, ToolResult};
use serde_json::{json, Value};
use wm_core::inventory::Inventory;
use wm_core::registry;
use wm_core::sdc::{self, Session};
use wm_core::tree::ProductTree;
use wm_core::{deps, install, profile, runner};

// Every path this server uses comes from `wm_core::config`, which is also what
// the Update Manager server reads. Two definitions of where jobs live means a
// job written where the status call does not look.
pub use wm_core::config::jobs_dir;

/// One credential, from the environment first and then the encrypted store.
///
/// The environment keeps winning because that is how a CI run injects a key for
/// one invocation without touching the machine's store, and because an operator
/// who exports a variable to override the store expects the override to work.
fn credential(var: &str, stored: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| wm_core::secrets::lookup(stored))
}

fn credentials() -> Result<(String, String), ToolError> {
    let missing = |var: &str, stored: &str| {
        ToolError::invalid(format!(
            "no IBM entitlement credentials: ${var} is not set and {stored:?} is not in the \
             credential store. Set it with credential_set, or export ${var}."
        ))
    };
    let user = credential("WM_EMPOWER_USER", wm_core::secrets::EMPOWER_USER)
        .ok_or_else(|| missing("WM_EMPOWER_USER", wm_core::secrets::EMPOWER_USER))?;
    let key = credential("WM_EMPOWER_KEY", wm_core::secrets::EMPOWER_KEY)
        .ok_or_else(|| missing("WM_EMPOWER_KEY", wm_core::secrets::EMPOWER_KEY))?;
    Ok((user, key))
}

fn host(args: &Value) -> String {
    opt_str(args, "host")
        .or_else(|| std::env::var("WM_SDC_HOST").ok())
        .or_else(|| defaults().host)
        .unwrap_or_else(|| sdc::DEFAULT_HOST.to_string())
}

/// The stored defaults, or an empty set when they cannot be read.
///
/// A malformed config file must not take every tool down with it; the tools
/// that manage the file report the parse error properly.
fn defaults() -> wm_core::config::Defaults {
    wm_core::config::Defaults::load().unwrap_or_default()
}

/// The release identifier, from the call or from the stored default.
fn release_arg(args: &Value) -> Result<String, ToolError> {
    opt_str(args, "release")
        .or_else(|| defaults().release)
        .ok_or_else(|| {
            ToolError::invalid(
                "no release given and none is configured; call sdc_releases to see what this \
                 account is entitled to, or set one with config_set",
            )
        })
}

/// The platform code, from the call, the stored default, or Linux x86-64.
fn platform_arg(args: &Value) -> String {
    // Upper-cased because the catalogue's codes are — LNXAMD64, W64, AIX — and
    // the cache is keyed by the string given. `lnxamd64` was quietly producing
    // a second copy of the same 2 MB tree under a second name.
    opt_str(args, "platform")
        .or_else(|| defaults().platform)
        .unwrap_or_else(|| "LNXAMD64".into())
        .to_uppercase()
}

/// Pick the release the caller meant out of what the account is entitled to.
///
/// The same release has four names depending on where it was last seen: `12.1`
/// from `sdc_releases`, `webM121` from the name of the cached tree file,
/// `2026_May` as the release code, and a display name. They are one release,
/// and rejecting three of them costs a round trip that discovers nothing. When
/// nothing matches, the error lists what the account actually has — the old
/// "no entitlement for release webM121" left the caller guessing twice.
fn pick_release<'a>(
    releases: &'a [sdc::Release],
    wanted: &str,
) -> Result<&'a sdc::Release, ToolError> {
    let wanted = wanted.trim();
    let same = |a: &str| a.eq_ignore_ascii_case(wanted);
    let found = releases
        .iter()
        .find(|r| same(&r.release))
        .or_else(|| releases.iter().find(|r| same(&r.code)))
        .or_else(|| {
            releases
                .iter()
                .find(|r| r.sandbox().is_some_and(|s| same(&s)))
        })
        .or_else(|| releases.iter().find(|r| same(&r.display_name)))
        .or_else(|| {
            releases
                .iter()
                .find(|r| r.repository().is_some_and(|s| same(&s)))
        });
    found.ok_or_else(|| {
        let known: Vec<String> = releases
            .iter()
            .map(|r| match r.sandbox() {
                Some(sandbox) => format!("{} ({sandbox})", r.release),
                None => r.release.clone(),
            })
            .collect();
        ToolError::failed(format!(
            "no entitlement for release {wanted:?}. This account is entitled to: {}",
            if known.is_empty() {
                "nothing".to_string()
            } else {
                known.join(", ")
            }
        ))
    })
}

/// The installation a call is aimed at, named either way.
///
/// `install` is always a registered name. `install_dir` and `wm_home` may be
/// either a path or a registered name, so once an installation is registered
/// its name works everywhere a path used to go.
fn target_path(args: &Value) -> Result<Option<PathBuf>, ToolError> {
    if let Some(name) = opt_str(args, "install") {
        return registry::get(&name)
            .map(|i| Some(i.wm_home))
            .map_err(ToolError::invalid);
    }
    let Some(given) = opt_str(args, "install_dir").or_else(|| opt_str(args, "wm_home")) else {
        return Ok(None);
    };
    registry::resolve(&given)
        .map(Some)
        .map_err(ToolError::invalid)
}

/// A required installation, named by path or by registration.
///
/// The tools that act on an existing installation all take `wm_home`; routing
/// them through one function is what lets a registered name work in every one of
/// them rather than in the handful that happened to be updated.
fn required_home(args: &Value) -> Result<PathBuf, ToolError> {
    if let Some(name) = opt_str(args, "install") {
        return registry::get(&name)
            .map(|i| i.wm_home)
            .map_err(ToolError::invalid);
    }
    registry::resolve(&req_str(args, "wm_home")?).map_err(ToolError::invalid)
}

/// What the target installation already carries, or `None` when there is no
/// installation there yet — which is the fresh-install case, not an error.
fn existing_inventory(path: &Path) -> Option<Inventory> {
    Inventory::read(path).ok()
}

fn login(args: &Value) -> Result<Session, ToolError> {
    let (user, key) = credentials()?;
    Session::login(&host(args), &user, &key).map_err(ToolError::failed)
}

/// Cache path for one release/platform tree.
fn tree_path(sandbox: &str, platform: &str) -> PathBuf {
    wm_core::config::catalog_dir().join(format!("{sandbox}-{platform}.tree"))
}

/// A cached tree, when the release was named the way the cache is keyed.
///
/// The cache file is `<sandbox>-<platform>.tree`, so a caller who says
/// `webM121` — which is what the file is called, and the first thing anyone
/// reads off the disk — is naming the cache entry directly. A release number
/// like `12.1` still needs the entitlement list to map it to a sandbox, because
/// nothing local relates the two.
fn cached_tree(release: &str, platform: &str) -> Option<(ProductTree, String)> {
    let sandbox = release.trim();
    let path = tree_path(sandbox, platform);
    if !path.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let tree = ProductTree::parse(&text).ok()?;
    Some((tree, sandbox.to_string()))
}

/// Load a cached tree, or fetch and cache it.
fn tree_for(
    args: &Value,
    release: &str,
    platform: &str,
) -> Result<(ProductTree, String), ToolError> {
    // A cached tree names its own sandbox, so planning against one needs no
    // credentials and no network at all. Only the lookup from a release number
    // to a sandbox did, and it was enough to make every planning call fail on a
    // machine whose key had gone — with a 2 MB answer sitting in the cache.
    if !flag(args, "refresh", false) {
        if let Some((tree, sandbox)) = cached_tree(release, platform) {
            return Ok((tree, sandbox));
        }
    }
    let session = login(args)?;
    let releases = session.releases().map_err(ToolError::failed)?;
    let entry = pick_release(&releases, release)?;
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

/// The shared part of `native_plan` and `native_install`: turn what the caller
/// asked for into the closure, then split that against what is already there.
struct Selection {
    /// Seeds that matched a product in the catalogue.
    seeds: Vec<String>,
    /// Seeds that matched nothing.
    unknown: Vec<String>,
    /// Prerequisites nothing in the catalogue satisfies.
    unsatisfied: Vec<deps::Unsatisfied>,
    /// The full dependency closure, before anything is subtracted.
    closure: Vec<String>,
    /// The installation this was priced against, if there is one.
    target: Option<PathBuf>,
    /// How the closure divides against that installation. `None` when the
    /// target does not exist yet, which is a fresh install.
    delta: Option<install::Delta>,
    /// Fix readmes the target carries, which is what a reinstall would undo.
    installed_fixes: usize,
}

impl Selection {
    /// What would actually be written: the closure for a fresh installation,
    /// the difference for an existing one, plus the version changes when the
    /// caller has forced them.
    fn products(&self, force: bool) -> Vec<String> {
        match &self.delta {
            None => self.closure.clone(),
            Some(delta) if force => delta.forced(),
            Some(delta) => delta.to_install.clone(),
        }
    }

    /// The version changes left alone, which the caller must be told about.
    fn not_performed(&self) -> &[install::VersionChange] {
        self.delta.as_ref().map_or(&[], |d| &d.version_changes)
    }

    /// Structured form of the subtraction, for a tool result.
    fn as_json(&self) -> Value {
        match &self.delta {
            None => json!({
                "target": self.target.as_ref().map(|p| p.display().to_string()),
                "fresh_install": true,
            }),
            Some(delta) => json!({
                "target": self.target.as_ref().map(|p| p.display().to_string()),
                "fresh_install": false,
                "installed_fixes": self.installed_fixes,
                "already_installed": delta.already_installed,
                "already_installed_count": delta.already_installed.len(),
                "version_changes": delta.version_changes,
                "to_install": delta.to_install,
            }),
        }
    }
}

/// Resolve a selection and subtract the target installation from it.
fn select(args: &Value, tree: &ProductTree) -> Result<Selection, ToolError> {
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
        return Err(ToolError::invalid(format!(
            "none of the requested products exist in the catalogue: {}. catalog_search finds \
             the exact versioned paths",
            unknown.join(", ")
        )));
    }
    let resolution = deps::resolve(&catalog, &seeds, flag(args, "include_mandatory", true))
        .map_err(ToolError::failed)?;
    let closure = resolution.paths();

    let target = target_path(args)?;
    let existing = target.as_deref().and_then(existing_inventory);
    Ok(Selection {
        seeds,
        unknown,
        unsatisfied: resolution.unsatisfied,
        delta: existing.as_ref().map(|inv| install::delta(&closure, inv)),
        installed_fixes: existing.as_ref().map_or(0, |inv| inv.fixes.len()),
        closure,
        target,
    })
}

/// One line per version change, for a human to read.
fn describe_changes(changes: &[install::VersionChange]) -> String {
    let width = changes
        .iter()
        .map(|c| c.component.len())
        .max()
        .unwrap_or(0)
        .min(40);
    changes
        .iter()
        .map(|c| {
            format!(
                "  {:width$}  installed {}  catalogue {}",
                c.component, c.installed_version, c.catalog_version
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Why a version change is not performed, and what to do instead.
///
/// A product's `.prop` file records the version it was *installed* at, never its
/// fix level: a product Update Manager has patched still reports its base
/// version, and the only on-disk trace of the patching is the readmes counted
/// here. So "the catalogue has a newer version" is not a reason to unpack it —
/// doing that replaces patched files with base-version copies whichever version
/// is numerically higher. The path that raises a patched product is Update
/// Manager, not this one.
fn why_not_performed(installed_fixes: usize) -> String {
    let patched = if installed_fixes > 0 {
        format!(
            " This installation carries {installed_fixes} fix readme(s), and a .prop file \
             records the version a product was installed at rather than its fix level: \
             unpacking the catalogue version over a patched product replaces corrected files \
             with base-version copies — including when the catalogue version is the newer of \
             the two — and nothing afterwards records that the fix level dropped."
        )
    } else {
        String::new()
    };
    format!(
        "Products already present under a different version are not reinstalled.{patched} \
         Move them with the Update Manager server instead — fixes_available, fixes_download, \
         fix_apply — which is the path that keeps the fix level. force=true overwrites them \
         from the catalogue, and is a last resort."
    )
}

pub fn native_plan() -> Tool {
    Tool::new(
        "native_plan",
        "Resolve a product selection against IBM's catalogue and report exactly what would be \
         downloaded and installed: the prerequisite closure, the artifact list with its total \
         size, and — importantly — which products declare Java install panels that a native \
         install cannot run. Name an existing installation with `install`, `install_dir` or \
         `wm_home` and the plan is priced against what it already has: products present at the \
         same version drop out, products present at a different version are listed apart as \
         not-performed, and the size quoted is the size of the difference rather than of the \
         whole closure. Without one, it prices a fresh installation.",
        json!({
            "type": "object",
            "required": ["products"],
            "properties": {
                "release": { "type": "string", "description": "Release number, code, sandbox or display name — 12.1, 2026_May and webM121 all work. Defaults to the configured release." },
                "platform": { "type": "string" },
                "products": { "type": "array", "items": { "type": "string" }, "description": "Component names or full versioned paths." },
                "install": { "type": "string", "description": "Registered installation to price this against; install_list shows the names." },
                "install_dir": { "type": "string", "description": "Installation to price this against, as a path or a registered name. Omitted, the plan is for a fresh installation." },
                "include_mandatory": { "type": "boolean", "description": "Inject the undeclared base products (default true)." },
                "host": { "type": "string" }
            }
        }),
        Box::new(|args| {
            let release = release_arg(args)?;
            let platform = platform_arg(args);
            let (tree, _) = tree_for(args, &release, &platform)?;
            let selection = select(args, &tree)?;

            let products = selection.products(false);
            let plan = install::plan(&tree, &products);
            // What the closure would have cost without the subtraction, so the
            // saving is stated rather than left to be inferred.
            let whole = install::plan(&tree, &selection.closure);

            let mut summary = match &selection.delta {
                None => format!(
                    "{} products ({} after closure), {} artifacts, {:.2} GB to download",
                    selection.seeds.len(),
                    plan.products.len(),
                    plan.artifacts.len(),
                    plan.download_bytes as f64 / 1e9
                ),
                Some(delta) => format!(
                    "{} of {} products to install ({} already present at these versions), \
                     {} artifacts, {} to download — against the whole closure's {}",
                    products.len(),
                    selection.closure.len(),
                    delta.already_installed.len(),
                    plan.artifacts.len(),
                    wm_core::progress::human_bytes(plan.download_bytes),
                    wm_core::progress::human_bytes(whole.download_bytes),
                ),
            };
            if !selection.not_performed().is_empty() {
                summary.push_str(&format!(
                    "\n\n{} product(s) present under a different version, NOT performed:\n{}\n\n{}",
                    selection.not_performed().len(),
                    describe_changes(selection.not_performed()),
                    why_not_performed(selection.installed_fixes),
                ));
            }
            if !plan.products_with_panels.is_empty() {
                summary.push_str(&format!(
                    "\n\n{} product(s) declare install panels a native install does not run.",
                    plan.products_with_panels.len()
                ));
            }
            Ok(ToolResult::structured(
                summary,
                json!({
                    "complete": selection.unsatisfied.is_empty() && selection.unknown.is_empty(),
                    "unknown_products": selection.unknown,
                    "unsatisfied": selection.unsatisfied,
                    "release": release,
                    "platform": platform,
                    "closure": selection.closure,
                    "products": products,
                    "artifact_count": plan.artifacts.len(),
                    "download_bytes": plan.download_bytes,
                    "expanded_bytes": plan.expanded_bytes,
                    "whole_closure_download_bytes": whole.download_bytes,
                    "products_with_panels": plan.products_with_panels,
                    "installation": selection.as_json(),
                }),
            ))
        }),
    )
}

/// Run a native install as a detached job, incrementally.
pub fn native_install() -> Tool {
    Tool::new(
        "native_install",
        "Download and install a product selection straight from IBM: no installer binary, no \
         JVM, no image. Every artifact is verified against the sha256 the catalogue declares \
         before it is unpacked, and the installation is left self-describing \
         (install/bms/*.contents, install/products/*.prop). Installing into an existing \
         installation is incremental: products already present at the same version are not \
         fetched, and products present at a *different* version are reported as not performed \
         rather than overwritten — laying a catalogue version over a patched product silently \
         undoes applied fixes. `force: true` overwrites them anyway. Returns a job id — poll it \
         with job_status. Java install panels are not run; see native_plan.",
        json!({
            "type": "object",
            "required": ["products"],
            "properties": {
                "release": { "type": "string", "description": "Release number, code, sandbox or display name. Defaults to the configured release." },
                "platform": { "type": "string" },
                "products": { "type": "array", "items": { "type": "string" } },
                "installer_jar": { "type": "string", "description": "Path to the installer's own jar (sagInstaller.jar, inside the downloaded installer). It is laid down as install/jars/DistMan.jar, which is where the shipped tooling looks for it — is_instance.xml puts it on the instance manager's classpath. Defaults to the configured installer_jar, then $WM_INSTALLER_JAR." },
                "install": { "type": "string", "description": "Registered installation to install into; install_list shows the names." },
                "install_dir": { "type": "string", "description": "Where to install, as a path or a registered name. A directory that does not exist yet is a fresh installation." },
                "include_mandatory": { "type": "boolean" },
                "force": { "type": "boolean", "description": "Also reinstall products the target carries under a different version, overwriting them. This is how an applied fix is lost; default false." },
                "host": { "type": "string" }
            }
        }),
        Box::new(|args| {
            // Validate the plan before spawning: a job that fails on its first
            // call has cost a process and told the caller nothing new.
            let release = release_arg(args)?;
            let platform = platform_arg(args);
            let install_dir = target_path(args)?.ok_or_else(|| {
                ToolError::invalid(
                    "no install_dir given: name a directory to install into, or a registered \
                     installation with install",
                )
            })?;
            credentials()?;
            let (tree, _) = tree_for(args, &release, &platform)?;
            let selection = select(args, &tree)?;

            let force = flag(args, "force", false);
            let products = selection.products(force);
            let not_performed = selection.not_performed().to_vec();

            // Nothing to fetch. Say which of the two reasons it is, because
            // "already installed" and "refused to overwrite" call for very
            // different next steps.
            if products.is_empty() {
                let summary = if not_performed.is_empty() {
                    format!(
                        "nothing to install: all {} products of the closure are already present \
                         in {} at these versions",
                        selection.closure.len(),
                        install_dir.display()
                    )
                } else {
                    format!(
                        "nothing installed. {} product(s) are present under a different \
                         version, NOT performed:\n{}\n\n{}",
                        not_performed.len(),
                        describe_changes(&not_performed),
                        why_not_performed(selection.installed_fixes),
                    )
                };
                return Ok(ToolResult::structured(
                    summary,
                    json!({
                        "job_id": Value::Null,
                        "products": 0,
                        "installation": selection.as_json(),
                    }),
                ));
            }

            let register = registry::find_by_home(&install_dir).map(|i| i.name);
            let spec = json!({
                "release": release,
                "platform": platform,
                "install_dir": install_dir.display().to_string(),
                "products": products,
                "host": host(args),
                "installer_jar": opt_str(args, "installer_jar")
                    .or_else(|| defaults().installer_jar.map(|p| p.display().to_string()))
                    .or_else(|| std::env::var("WM_INSTALLER_JAR").ok()),
                // Carried so the job's own log records what it deliberately
                // left alone, not only what it wrote.
                "skipped_already_installed": selection
                    .delta
                    .as_ref()
                    .map_or(0, |d| d.already_installed.len()),
                "skipped_version_changes": not_performed
                    .iter()
                    .map(|c| c.installed.clone())
                    .collect::<Vec<_>>(),
                "register": register,
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
                // inherited by the job; nothing is written to the wrapper. The
                // passphrase goes too, so a job can reopen the credential store.
                passthrough: vec![
                    "WM_EMPOWER_USER".into(),
                    "WM_EMPOWER_KEY".into(),
                    wm_core::secrets::PASSPHRASE_VAR.into(),
                    "WM_CONFIG_DIR".into(),
                ],
                // The job authenticates on its own behalf, so a key held only
                // in the store has to reach its environment.
                secret_env: wm_core::secrets::job_environment(),
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

            let mut summary = match &selection.delta {
                None => format!(
                    "native install started as {} into {}: {} product(s)",
                    job.id,
                    install_dir.display(),
                    products.len()
                ),
                Some(delta) => format!(
                    "incremental install started as {} into {}: {} product(s) of a {}-product \
                     closure, {} already present",
                    job.id,
                    install_dir.display(),
                    products.len(),
                    selection.closure.len(),
                    delta.already_installed.len()
                ),
            };
            if !not_performed.is_empty() {
                summary.push_str(&format!(
                    "\n\n{} product(s) present under a different version, {}:\n{}\n\n{}",
                    not_performed.len(),
                    if force {
                        "OVERWRITTEN because force=true"
                    } else {
                        "NOT performed"
                    },
                    describe_changes(&not_performed),
                    if force {
                        format!(
                            "force=true, so these are being overwritten from the catalogue.{} \
                             Re-apply anything they were patched with through the Update \
                             Manager server once this job finishes.",
                            if selection.installed_fixes > 0 {
                                format!(
                                    " This installation carries {} fix readme(s); patched \
                                     files inside these products are being replaced by \
                                     base-version copies.",
                                    selection.installed_fixes
                                )
                            } else {
                                String::new()
                            }
                        )
                    } else {
                        why_not_performed(selection.installed_fixes)
                    },
                ));
            }
            Ok(ToolResult::structured(
                summary,
                json!({
                    "job_id": job.id,
                    "log": job.log,
                    "job_dir": jobs.join(&job.id).display().to_string(),
                    "products": products.len(),
                    "forced": force,
                    "installation": selection.as_json(),
                }),
            ))
        }),
    )
}

/// Check an installation against the manifests it carries.
pub fn install_verify() -> Tool {
    Tool::new(
        "install_verify",
        "Check what an installation says it holds against what is on its disk, by reading the \
         manifests it carries in install/bms/*.contents. This is the difference between a \
         product being *claimed* and being *complete*: install/products/*.prop records that a \
         product was placed, which is what an incremental plan trusts, and a run that failed \
         part-way leaves that record standing.\n\n\
         A declared file being absent is usually correct, and the report classifies rather \
         than counts. Applying a fix deletes files and puts newer ones in their place without \
         rewriting the manifest, so an absence with a differently-versioned neighbour beside \
         it is Update Manager having done its job. One finding is conclusive — an artifact of \
         which *nothing* was written, which is a product recorded as installed that is not \
         there. The rest are absences nothing accounts for: a prompt to look, not a verdict, \
         because a mature installation has other innocent reasons for them and this does not \
         adjudicate between them.\n\n\
         Needs no credentials and touches nothing, so it works on a stopped installation. It \
         checks presence, not content: the manifests carry no sizes or digests, so a truncated \
         file passes.",
        json!({
            "type": "object",
            "properties": {
                "install": { "type": "string", "description": "A registered installation, as an alternative to wm_home; install_list shows the names." },
                "wm_home": { "type": "string", "description": "Installation root, or a registered name." },
                "product": { "type": "string", "description": "Check only artifacts whose name or product path contains this, e.g. \"Deployer\" or \"DEP_12.1\". Omitted, everything is checked." },
                "limit": { "type": "integer", "description": "Paths to list per artifact (default 10). The counts are always complete." },
                "superseded": { "type": "boolean", "description": "List the absences a fix accounts for as well. Off by default: on a patched installation they are the overwhelming majority and none of them are faults." }
            }
        }),
        Box::new(|args| {
            let wm_home = required_home(args)?;
            let report = wm_core::install::verify(&wm_home, opt_str(args, "product").as_deref())
                .map_err(ToolError::failed)?;
            let limit = opt_usize(args, "limit").unwrap_or(10);

            if report.artifacts == 0 {
                return Err(ToolError::failed(format!(
                    "{} carries no manifests to check{}",
                    wm_home.display(),
                    match opt_str(args, "product") {
                        Some(product) => format!(" that match {product:?}."),
                        None =>
                            ". install/bms/*.contents is written by a native install and by the \
                             shipped installer, so an installation with none was made another \
                             way."
                                .to_string(),
                    }
                )));
            }

            let mut summary = format!(
                "{}: {} artifact(s), {} declared path(s). {} absent — {} superseded by a newer \
                 file, {} unaccounted for.",
                wm_home.display(),
                report.artifacts,
                report.declared,
                report.absent,
                report.superseded,
                report.unexplained,
            );
            if report.is_sound() {
                summary
                    .push_str("\n\nNothing here is wrong: every absence is a file a fix replaced.");
            }

            let mut show = |heading: &str, checks: &[wm_core::install::ArtifactCheck]| {
                if checks.is_empty() {
                    return;
                }
                summary.push_str(&format!("\n\n{heading}"));
                for check in checks.iter().take(20) {
                    summary.push_str(&format!(
                        "\n\n{} — {} of {} absent, {} unaccounted for{}{}",
                        check.artifact,
                        check.absent.len(),
                        check.declared,
                        check.unexplained,
                        match &check.product {
                            Some(product) => format!("\n  {product}"),
                            None => String::new(),
                        },
                        if check.base.is_empty() {
                            String::new()
                        } else {
                            format!("\n  paths are relative to {}/", check.base)
                        },
                    ));
                    let listed = check.absent.iter().filter(|absent| {
                        absent.superseded_by.is_none() || flag(args, "superseded", false)
                    });
                    let mut shown = 0usize;
                    for absent in listed.clone().take(limit) {
                        summary.push_str(&format!(
                            "\n  {}{}",
                            absent.path,
                            match &absent.superseded_by {
                                Some(newer) => format!("   (superseded by {newer})"),
                                None => String::new(),
                            }
                        ));
                        shown += 1;
                    }
                    let total = listed.count();
                    if total > shown {
                        summary.push_str(&format!("\n  … and {} more", total - shown));
                    }
                }
                if checks.len() > 20 {
                    summary.push_str(&format!(
                        "\n\n… and {} further artifact(s).",
                        checks.len() - 20
                    ));
                }
            };
            show(
                "NOTHING WAS WRITTEN for these artifacts. The installation records the product \
                 and the files were never placed — an incremental plan will leave it alone, so \
                 reinstall it with native_install and force=true.",
                &report.never_written,
            );
            show("Absences nothing accounts for:", &report.incomplete);
            if !report.unreadable.is_empty() {
                summary.push_str(&format!(
                    "\n\n{} manifest(s) could not be read: {}",
                    report.unreadable.len(),
                    report.unreadable.join(", ")
                ));
            }

            let structured = serde_json::to_value(&report)
                .map_err(|e| ToolError::failed(format!("cannot serialise the report: {e}")))?;
            let result = ToolResult::structured(summary, structured);
            Ok(if report.is_sound() {
                result
            } else {
                result.into_error()
            })
        }),
    )
}

/// A package published inside another package, rather than held in the
/// repository.
///
/// `WmDeployer/pub/WmDeployerResource.zip` is the case that costs an afternoon:
/// Deployer needs `WmDeployerResource` on every runtime it queries, including
/// the one it runs on, and that package is not a repository package — it is an
/// archive Deployer publishes for distribution. `is_instance.sh` cannot copy
/// what is not in the repository, and says nothing about why.
fn published_archive(repository: &Path, name: &str) -> Option<String> {
    let exact = format!("{name}.zip");
    let mut found: Vec<(bool, String)> = Vec::new();
    for package in std::fs::read_dir(repository)
        .ok()?
        .flatten()
        .map(|e| e.path())
    {
        let Ok(files) = std::fs::read_dir(package.join("pub")) else {
            continue;
        };
        for file in files.flatten().map(|e| e.path()) {
            let Some(filename) = file.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            // `WmDeployerResource.zip` and `WmDeployerResource-11.zip` both
            // match; the unversioned one is the current package, and the other
            // is kept for older runtimes.
            if filename.ends_with(".zip") && filename.starts_with(name) {
                found.push((filename == exact, file.display().to_string()));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().next().map(|(_, path)| path)
}

/// Copy freshly installed packages into a running instance.
pub fn instance_update() -> Tool {
    Tool::new(
        "instance_update",
        "Bring an existing Integration Server instance up to date with the packages the \
         installation carries, by running the product's own \
         IntegrationServer/instances/is_instance.sh update. This step is required and \
         invisible: installing a product that ships Integration Server packages fills the \
         repository at IntegrationServer/packages and leaves every instance alone, so the \
         server answers 'Unknown package' for a package the installation plainly contains and \
         a successful install stays inoperative. Called with no package list, it reports which \
         packages the installation has that this instance does not, and changes nothing. \
         Defaults to a dry run.",
        json!({
            "type": "object",
            "properties": {
                "install": { "type": "string", "description": "A registered installation, as an alternative to wm_home; install_list shows the names." },
                "wm_home": { "type": "string", "description": "Installation root, or a registered name." },
                "name": { "type": "string", "description": "Instance name (default \"default\")." },
                "packages": { "type": "array", "items": { "type": "string" }, "description": "Packages to copy in. \"all\" means every non-core package, which is the script's own keyword — what counts as core is is_core_packages.properties' business, not this server's. Omitted, nothing is copied and the call only reports the difference." },
                "db_type": { "type": "string", "description": "ORACLE, DB2, SQLSERVER, MYSQLCE, MYSQLEE or POSTGRESQL." },
                "db_alias": { "type": "string" },
                "db_url": { "type": "string" },
                "db_username": { "type": "string" },
                "db_password": { "type": "string" },
                "apply": { "type": "boolean", "description": "Set true to run; otherwise a dry run." }
            }
        }),
        Box::new(|args| {
            let wm_home = required_home(args)?;
            let name = opt_str(args, "name").unwrap_or_else(|| "default".into());
            let instance_dir = wm_home
                .join("IntegrationServer")
                .join("instances")
                .join(&name);
            if !instance_dir.is_dir() {
                return Err(ToolError::invalid(format!(
                    "no instance {name:?} at {}; instance_create makes one",
                    instance_dir.display()
                )));
            }
            let missing = wm_core::instance::packages_not_in_instance(&wm_home, &name);
            let packages = str_list(args, "packages");

            // Nothing to copy and nothing asked for: this is the diagnosis, and
            // running the script would change nothing.
            if packages.is_empty() {
                let summary = if missing.is_empty() {
                    format!(
                        "instance {name} already carries every package the installation has. \
                         Nothing to do."
                    )
                } else {
                    format!(
                        "instance {name} is missing {} package(s) the installation carries:\n{}\n\n\
                         Call again with packages=[…] to copy specific ones, or \
                         packages=[\"all\"] for every non-core package. Nothing was changed.",
                        missing.len(),
                        missing
                            .iter()
                            .map(|p| format!("  {p}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                };
                return Ok(ToolResult::structured(
                    summary,
                    json!({
                        "instance": name,
                        "missing_from_instance": missing,
                        "changed": false,
                    }),
                ));
            }

            let options = wm_core::instance::ant::UpdateOptions {
                packages: packages.clone(),
                db_type: opt_str(args, "db_type"),
                db_alias: opt_str(args, "db_alias"),
                db_url: opt_str(args, "db_url"),
                db_username: opt_str(args, "db_username"),
                db_password: opt_str(args, "db_password"),
            };
            let invocation = wm_core::instance::ant::update(&wm_home, &name, &options)
                .map_err(ToolError::failed)?;

            // A package asked for that the repository does not hold is the
            // mistake worth catching here: the script copies what it finds and
            // says nothing about what it did not.
            let repository = wm_home.join("IntegrationServer").join("packages");
            let unknown: Vec<&String> = packages
                .iter()
                .filter(|p| p.as_str() != wm_core::instance::ant::ALL_PACKAGES)
                .filter(|p| !repository.join(p).join("manifest.v3").is_file())
                .collect();
            if !unknown.is_empty() {
                let published: Vec<String> = unknown
                    .iter()
                    .filter_map(|name| published_archive(&repository, name))
                    .collect();
                let mut message = format!(
                    "{} is not in {}: {}. A package is only there once it carries a \
                     manifest.v3, which is what Integration Server reads.",
                    if unknown.len() == 1 {
                        "a package"
                    } else {
                        "some packages"
                    },
                    repository.display(),
                    unknown
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                if !published.is_empty() {
                    message.push_str(&format!(
                        "\n\nBut it does ship, published by another package for distribution \
                         rather than held in the repository:\n{}\nA package that arrives this \
                         way is not installed by is_instance.sh at all. It is copied into \
                         <instance>/replicate/inbound and installed through the server's own \
                         wm.server.packages:packageInstall — which is also what the \
                         administration console's install button does when it works.",
                        published
                            .iter()
                            .map(|p| format!("  {p}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                return Err(ToolError::invalid(message));
            }

            if !flag(args, "apply", false) {
                let mut settings = Vec::new();
                setting(&mut settings, "instance", &name, args.get("name").is_some());
                setting(&mut settings, "packages", packages.join(", "), true);
                for (key, value) in [
                    ("db_type", &options.db_type),
                    ("db_alias", &options.db_alias),
                    ("db_url", &options.db_url),
                    ("db_username", &options.db_username),
                ] {
                    if let Some(value) = value {
                        setting(&mut settings, key, value, true);
                    }
                }
                return Ok(ToolResult::structured(
                    format!(
                        "dry run: would copy {} package(s) into instance {name}. Put the \
                         settings below to the user, confirm or amend them, then call again \
                         with apply=true.",
                        packages.len()
                    ),
                    json!({
                        "command": invocation.display(),
                        "settings": settings,
                        "missing_from_instance": missing,
                    }),
                ));
            }

            let (ok, output) =
                wm_core::instance::ant::run(&invocation).map_err(ToolError::failed)?;
            let remaining = wm_core::instance::packages_not_in_instance(&wm_home, &name);
            let copied: Vec<&String> = missing.iter().filter(|p| !remaining.contains(p)).collect();
            let summary = format!(
                "{} instance {name}: {} package(s) now present that were not. Restart the \
                 instance for it to load them.",
                if ok { "updated" } else { "FAILED to update" },
                copied.len(),
            );
            let result = ToolResult::structured(
                summary,
                json!({
                    "instance": name,
                    "ok": ok,
                    "copied": copied,
                    "still_missing": remaining,
                    "output": output,
                    "command": invocation.display(),
                }),
            );
            Ok(if ok { result } else { result.into_error() })
        }),
    )
}

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
                "install": { "type": "string", "description": "A registered installation, as an alternative to wm_home; install_list shows the names." },
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
            let wm_home = required_home(args)?;
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
                // The Ant script supplies its own defaults for anything not
                // passed, so say what those are rather than leaving a blank.
                let mut settings = Vec::new();
                setting(&mut settings, "name", &name, args.get("name").is_some());
                for (key, default) in [
                    ("primary_port", "5555"),
                    ("secure_port", "5543"),
                    ("diagnostic_port", "9999"),
                    ("jmx_port", "8075"),
                ] {
                    match args.get(key).and_then(|v| v.as_u64()) {
                        Some(v) => setting(&mut settings, key, v, true),
                        None => setting(&mut settings, key, default, false),
                    }
                }
                setting(
                    &mut settings,
                    "admin_password",
                    if options.admin_password.is_some() {
                        "as given"
                    } else {
                        "the password set when the product was installed"
                    },
                    options.admin_password.is_some(),
                );
                setting(
                    &mut settings,
                    "database",
                    options.db_type.as_deref().unwrap_or("embedded"),
                    options.db_type.is_some(),
                );
                setting(
                    &mut settings,
                    "bind_address",
                    options.bind_address.as_deref().unwrap_or("every interface"),
                    options.bind_address.is_some(),
                );
                return Ok(ToolResult::structured(
                    format!(
                        "dry run: would create instance {name}. Put the settings below to the \
                         user, confirm or amend them, then call again with apply=true."
                    ),
                    json!({ "command": invocation.display(), "settings": settings }),
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
                "install": { "type": "string", "description": "A registered installation, as an alternative to wm_home; install_list shows the names." },
                "wm_home": { "type": "string", "description": "Installation to capture from." },
                "profile": { "type": "string", "description": "Profile name, e.g. SPM or MWS_default." },
                "output": { "type": "string", "description": "Archive to write." }
            }
        }),
        Box::new(|args| {
            let wm_home = required_home(args)?;
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
                "install": { "type": "string", "description": "A registered installation, as an alternative to wm_home; install_list shows the names." },
                "wm_home": { "type": "string", "description": "Installation to write into." },
                "profile": { "type": "string", "description": "Name to give it; the captured name by default." },
                "apply": { "type": "boolean", "description": "Set true to write; otherwise a dry run." }
            }
        }),
        Box::new(|args| {
            let capture = PathBuf::from(req_str(args, "capture")?);
            let wm_home = required_home(args)?;
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
    let release = pick_release(&releases, &release_wanted).map_err(|e| e.to_string())?;
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

    let job_dir = std::env::var("WM_JOB_DIR").map(PathBuf::from).ok();
    let artifacts = tree.artifacts_for_selection(products.iter().map(String::as_str));
    let total: u64 = artifacts.iter().filter_map(|a| a.compressed_size).sum();
    let mut progress = wm_core::progress::Progress::new("downloading", artifacts.len(), total);
    if let Some(dir) = &job_dir {
        progress.write(dir);
    }
    println!(
        "{} products, {} artifacts, {:.2} GB to fetch into {}",
        products.len(),
        artifacts.len(),
        total as f64 / 1e9,
        install_dir.display()
    );
    // What the planner deliberately left out. A log that records only what was
    // written cannot afterwards answer "why is that product still at .938".
    let skipped = spec
        .get("skipped_already_installed")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if skipped > 0 {
        println!("{skipped} product(s) already present at these versions were not re-fetched");
    }
    let not_performed: Vec<&str> = spec
        .get("skipped_version_changes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !not_performed.is_empty() {
        println!(
            "{} product(s) present under a different version were NOT performed:",
            not_performed.len()
        );
        for product in &not_performed {
            println!("  {product}");
        }
    }
    std::fs::create_dir_all(&install_dir).map_err(|e| e.to_string())?;
    let cache = wm_core::config::artifacts_dir().join(&sandbox);

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
        if let Some(dir) = &job_dir {
            progress.step(&artifact.name, fetched.size, dir);
        }
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
    if let Some(dir) = &job_dir {
        let jar_bytes: u64 = jars.iter().filter_map(|j| j.compressed_size).sum();
        progress.phase("tooling jars", jars.len(), jar_bytes, dir);
    }
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
        if let Some(dir) = &job_dir {
            progress.step(&jar.name, fetched.size, dir);
        }
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
    let summary = format!(
        "installed {done} artifact(s) and {jars_done} jar(s), {}",
        wm_core::progress::human_bytes(bytes)
    );
    if let Some(dir) = &job_dir {
        progress.finish(true, &summary, dir);
    }
    println!("{summary}");

    // Keep the registry's component list honest: the installation just changed,
    // and a snapshot that still describes the state before it is worse than
    // none. Failing to update it must not fail the install, which succeeded.
    if let Some(name) = spec.get("register").and_then(Value::as_str) {
        match wm_core::registry::get(name) {
            Ok(mut record) => {
                record.last_job = std::env::var("WM_JOB_DIR").ok().and_then(|d| {
                    PathBuf::from(d)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                });
                record.release.get_or_insert(release_wanted.clone());
                record.platform.get_or_insert(platform.clone());
                match record.refresh().and_then(|_| record.save()) {
                    Ok(()) => println!("refreshed the registry record for {name}"),
                    Err(e) => println!("note: cannot refresh the registry record for {name}: {e}"),
                }
            }
            Err(e) => println!("note: cannot read the registry record for {name}: {e}"),
        }
    }

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
                "install": { "type": "string", "description": "A registered installation, as an alternative to wm_home; install_list shows the names." },
                "wm_home": { "type": "string", "description": "Installation to inspect." },
                "database": { "type": "string", "description": "postgresql, oracle, sqlserver, db2, mysql or sybase (default postgresql)." },
                "components": { "type": "array", "items": { "type": "string" }, "description": "Component names; default every component found." }
            }
        }),
        Box::new(|args| {
            let home = required_home(args)?;
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
                "install": { "type": "string", "description": "A registered installation, as an alternative to wm_home; install_list shows the names." },
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
            let home = required_home(args)?;
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

/// Record a setting and where its value came from.
///
/// A dry run that lists only what will happen still hides half the decision:
/// the caller cannot tell which values it chose and which the tool chose for
/// it. Every default is reported explicitly so the agent can put them to the
/// user before anything runs.
fn setting(into: &mut Vec<Value>, name: &str, value: impl std::fmt::Display, from_caller: bool) {
    into.push(json!({
        "setting": name,
        "value": value.to_string(),
        "source": if from_caller { "you asked for it" } else { "default" },
    }));
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
                "install": { "type": "string", "description": "A registered installation, as an alternative to wm_home; install_list shows the names." },
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
            let home = required_home(args)?;
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
