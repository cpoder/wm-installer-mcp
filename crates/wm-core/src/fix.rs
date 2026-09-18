//! Applying a fix without Update Manager.
//!
//! A fix is the same shape as a product module: a signed JAR whose entries are
//! rooted at the installation directory. What makes it a fix is two pieces of
//! metadata.
//!
//! `META-INF/MANIFEST.MF` names it, versions it, says what it needs and which
//! p2 repositories inside the installation it refreshes:
//!
//! ```text
//! Display-Fix-Name: Platform Manager 12.1.0 FIX 1
//! Fix-Name: wMFix.SPM
//! Fix-Version: 12.1.0.0001-0556
//! Require-Fix: wMFix.CCShared;version=12.1.0.0001
//! P2-Repositories: common/runtime/bundles/spm/eclipse
//! Require-SUM-Build: 11.0.0.0003-0257
//! ```
//!
//! `META-INF/instructions.txt` is a numbered recipe, `;`-separated actions per
//! phase, continued across lines with a trailing backslash:
//!
//! ```text
//! install.phase3=osgiShutdown(profile:SPM);
//! install.phase4=delete(file:PlatformManager/migrate/lib);
//! install.phase5=extract(include:PlatformManager/**/*);\
//! osgiCleanCache(profiles:SPM);
//! ```
//!
//! This engine performs `extract`, `delete` and `osgiCleanCache`, reports
//! `osgiShutdown` rather than stopping anything, and reports every other verb
//! of Update Manager's vocabulary as not performed — see [`Action`].
//!
//! # What a fix does to a profile
//!
//! A fix ships the **whole** repository it refreshes, not only the bundles it
//! changes: `wMFix.TPS.SharedBundles` carries 121 plugins and 30 features, most
//! of them byte-identical to what is already installed. Update Manager then
//! re-provisions every profile whose `install/profiles/<p>.data` names that
//! repository. Without a p2 director, the equivalent is decided line by line
//! in `bundles.info`, and three rules keep it from doing damage:
//!
//! * a line is a candidate only when its exact `(name, version)` jar was in
//!   the refreshed repository **before** the fix — a profile legitimately
//!   carries `gson` at 2.10.1 from one repository and at 2.9.0 from another,
//!   and a fix to the second must not touch the first;
//! * a candidate moves only to a strictly newer build, never to an older one,
//!   and never onto a `(name, version)` the profile already lists, which would
//!   leave the same bundle twice;
//! * the fix as a whole is refused when the repository already carries its
//!   level or a newer one, because the repository's feature versions are the
//!   fix level (`…feature_12.1.0.0003-0779.jar`), and laying an older index
//!   over a newer one is a downgrade nothing would report.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Error, Result};

/// One action in a fix's recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Unpack the archive, restricted to entries matching a glob.
    Extract {
        /// Glob against archive entry paths; `**/*` covers a whole subtree.
        include: String,
    },
    /// Remove a file or directory, relative to the installation.
    Delete {
        /// Path relative to the installation root.
        path: String,
    },
    /// Stop a runtime before touching its files.
    OsgiShutdown {
        /// Profile names. Empty means the target product's own runtime.
        profiles: Vec<String>,
    },
    /// Discard a runtime's OSGi caches so it re-reads its bundles.
    OsgiCleanCache {
        /// Profile names.
        profiles: Vec<String>,
    },
    /// An action this engine does not perform.
    ///
    /// Update Manager's action bundles register about a hundred verbs —
    /// `replace`, `setProperty`, `startScript`, `installISPackage`, the
    /// `osgi*` family that drives a p2 director, and so on. They are surfaced
    /// so a caller sees exactly what is left undone rather than believing a
    /// fix fully applied.
    Unsupported {
        /// The verb.
        verb: String,
        /// The action as written.
        raw: String,
    },
}

impl Action {
    /// Whether this engine can carry the action out.
    pub fn is_supported(&self) -> bool {
        !matches!(self, Action::Unsupported { .. })
    }
}

/// One numbered phase.
#[derive(Debug, Clone, Serialize)]
pub struct Phase {
    /// Phase number; phases run in ascending order.
    pub number: u32,
    /// Actions, in the order written.
    pub actions: Vec<Action>,
}

/// A fix or product the manifest names, with a minimum version when it gives one.
///
/// Written `wMFix.CCShared;version=12.1.0.0001` in `Require-Fix` and
/// `Install-After`, `wMProduct.SPM;version=12.1.0` in `Require-Product`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Requirement {
    /// `wMFix.CCShared`, `wMProduct.SPM`.
    pub name: String,
    /// The minimum version, when stated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl Requirement {
    fn parse_one(item: &str) -> Self {
        let mut parts = item.split(';');
        let name = parts.next().unwrap_or("").trim().to_string();
        let version = parts.find_map(|p| {
            p.trim()
                .strip_prefix("version=")
                .map(|v| v.trim().to_string())
        });
        Self { name, version }
    }

    /// Parse a comma-separated list of requirements.
    fn parse_list(text: &str) -> Vec<Self> {
        text.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Self::parse_one)
            .collect()
    }
}

/// A fix archive, read but not applied.
#[derive(Debug, Clone, Serialize)]
pub struct Fix {
    /// Where the archive is.
    pub path: PathBuf,
    /// `Fix-Name`, e.g. `wMFix.SPM`.
    pub name: Option<String>,
    /// `Display-Fix-Name`.
    pub display_name: Option<String>,
    /// `Display-Group-Name`.
    pub group: Option<String>,
    /// `Fix-Version`, e.g. `12.1.0.0003-0779`: the fix level this archive brings.
    pub version: Option<String>,
    /// `Empower-Fix-Id`, the identifier the download centre and the readmes use.
    pub empower_id: Option<String>,
    /// `Require-SUM-Build`, the Update Manager build the vendor tool would demand.
    pub requires_sum_build: Option<String>,
    /// `Require-Product`: the product this fix patches.
    pub require_product: Option<Requirement>,
    /// `Require-Fix`: fixes that must already be installed.
    pub requires_fixes: Vec<Requirement>,
    /// `Install-After`: fixes that go first when installed in the same batch.
    pub install_after: Vec<Requirement>,
    /// `P2-Repositories`: repositories inside the installation this fix refreshes.
    pub p2_repositories: Vec<String>,
    /// Install phases, ordered.
    pub phases: Vec<Phase>,
    /// Entry paths carried, excluding `META-INF`.
    pub entries: Vec<String>,
}

impl Fix {
    /// Read a fix archive.
    pub fn read(path: &Path) -> Result<Self> {
        let file = fs::File::open(path).map_err(|e| Error::io(path, e))?;
        let mut zip = zip::ZipArchive::new(file)
            .map_err(|e| Error::Exec(format!("{} is not an archive: {e}", path.display())))?;

        let manifest = read_entry(&mut zip, "META-INF/MANIFEST.MF").unwrap_or_default();
        let manifest = parse_manifest(&manifest);
        let instructions = read_entry(&mut zip, "META-INF/instructions.txt").unwrap_or_default();

        let mut entries = Vec::new();
        for index in 0..zip.len() {
            let entry = zip.by_index(index).map_err(|e| {
                Error::Exec(format!(
                    "cannot read entry {index} of {}: {e}",
                    path.display()
                ))
            })?;
            let name = entry.name().to_string();
            if !name.starts_with("META-INF") && !name.ends_with('/') {
                entries.push(name);
            }
        }

        let list = |key: &str| {
            manifest
                .get(key)
                .map(|v| Requirement::parse_list(v))
                .unwrap_or_default()
        };
        Ok(Self {
            path: path.to_path_buf(),
            name: manifest.get("Fix-Name").cloned(),
            display_name: manifest.get("Display-Fix-Name").cloned(),
            group: manifest.get("Display-Group-Name").cloned(),
            version: manifest.get("Fix-Version").cloned(),
            empower_id: manifest.get("Empower-Fix-Id").cloned(),
            requires_sum_build: manifest.get("Require-SUM-Build").cloned(),
            require_product: manifest
                .get("Require-Product")
                .map(|v| Requirement::parse_one(v)),
            requires_fixes: list("Require-Fix"),
            install_after: list("Install-After"),
            p2_repositories: manifest
                .get("P2-Repositories")
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().trim_end_matches('/').to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            phases: parse_instructions(&instructions),
            entries,
        })
    }

    /// `<Fix-Name>_<Fix-Version>`, the identifier Update Manager uses for its
    /// backups and its cache; `None` when the manifest lacks either half.
    pub fn id(&self) -> Option<String> {
        Some(format!(
            "{}_{}",
            self.name.as_ref()?,
            self.version.as_ref()?
        ))
    }

    /// Every action, in phase order.
    pub fn actions(&self) -> impl Iterator<Item = &Action> {
        self.phases.iter().flat_map(|p| p.actions.iter())
    }

    /// Actions this engine cannot carry out.
    pub fn unsupported(&self) -> Vec<&Action> {
        self.actions().filter(|a| !a.is_supported()).collect()
    }

    /// Profiles the recipe names as needing to be stopped or cleaned.
    ///
    /// Not the whole story: a fix that refreshes a shared repository touches
    /// every profile provisioned from it, whatever the recipe says. See
    /// [`Applied::warnings`].
    pub fn profiles(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .actions()
            .filter_map(|a| match a {
                Action::OsgiShutdown { profiles } | Action::OsgiCleanCache { profiles } => {
                    Some(profiles.clone())
                }
                _ => None,
            })
            .flatten()
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// What the archive delivers into each repository it refreshes.
    fn delivered(&self) -> BTreeMap<String, Repository> {
        let mut repositories: BTreeMap<String, Repository> = self
            .p2_repositories
            .iter()
            .map(|r| (r.clone(), Repository::default()))
            .collect();
        for entry in &self.entries {
            for (repo, contents) in repositories.iter_mut() {
                if let Some(inside) = entry.strip_prefix(repo.as_str()) {
                    if inside.starts_with('/') {
                        contents.add(inside, entry);
                    }
                }
            }
        }
        repositories
    }
}

/// How to apply a fix.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Compute and report, write nothing.
    pub dry_run: bool,
    /// Go ahead although the fix looks already applied or older than what is
    /// installed, or although a profile it touches looks like it is running.
    /// Never makes a profile line move to an older build.
    pub force: bool,
}

/// What applying a fix did, or would do.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Whether anything was written. False in a dry run, and false when
    /// [`Applied::blocked`] is not empty and `force` was not given.
    pub performed: bool,
    /// Why the fix was not (or would not be) applied. Empty when it applies.
    pub blocked: Vec<String>,
    /// Each refreshed repository, with its level before and the level delivered.
    pub repositories: Vec<RepositoryLevel>,
    /// Files extracted, relative to the installation.
    pub extracted: Vec<String>,
    /// Paths removed.
    pub deleted: Vec<String>,
    /// OSGi caches cleared.
    pub caches_cleared: Vec<String>,
    /// Bundles replaced or refreshed inside a runtime profile.
    pub profile_updates: Vec<ProfileUpdate>,
    /// Actions reported but not performed.
    pub not_performed: Vec<Action>,
    /// Anything the caller must deal with.
    pub warnings: Vec<String>,
    /// What a check of every rewritten `bundles.info` found. Empty is good.
    pub verification: Vec<String>,
}

/// The fix level a repository is at, read from its feature versions.
#[derive(Debug, Clone, Serialize)]
pub struct RepositoryLevel {
    /// Repository path relative to the installation.
    pub path: String,
    /// Highest feature version on disk before the fix, if any features are there.
    pub installed: Option<String>,
    /// Highest feature version the fix delivers, else its `Fix-Version`.
    pub delivered: Option<String>,
}

/// One bundle changed in a profile.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileUpdate {
    /// Profile name.
    pub profile: String,
    /// Bundle symbolic name.
    pub bundle: String,
    /// Version the profile carried.
    pub from: String,
    /// Version the fix delivers.
    pub to: String,
    /// Whether the version moved or only the jar's bytes did.
    pub kind: UpdateKind,
}

/// What kind of change a [`ProfileUpdate`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateKind {
    /// The line moved to a newer build.
    Replaced,
    /// Same version, but the fix ships different bytes for the jar the
    /// profile keeps its own copy of.
    Refreshed,
}

/// Apply a fix to `wm_home`.
///
/// A dry run computes everything and writes nothing, which is the right first
/// call: it says which repositories change level, which profile lines move,
/// and whether a profile the fix touches looks like it is running.
pub fn apply(fix: &Fix, wm_home: &Path, options: Options) -> Result<Applied> {
    let mut applied = Applied {
        dry_run: options.dry_run,
        performed: false,
        blocked: Vec::new(),
        repositories: Vec::new(),
        extracted: Vec::new(),
        deleted: Vec::new(),
        caches_cleared: Vec::new(),
        profile_updates: Vec::new(),
        not_performed: Vec::new(),
        warnings: Vec::new(),
        verification: Vec::new(),
    };

    // The repositories as they are now: what came from where is decided on
    // this snapshot, before anything is overlaid.
    let delivered = fix.delivered();
    let mut on_disk: BTreeMap<String, Repository> = BTreeMap::new();
    for repo in &fix.p2_repositories {
        let disk = Repository::from_disk(wm_home, repo);
        let level = RepositoryLevel {
            path: repo.clone(),
            installed: disk.level(),
            delivered: delivered
                .get(repo)
                .and_then(Repository::level)
                .or_else(|| fix.version.clone()),
        };
        if let (Some(installed), Some(brought)) = (&level.installed, &level.delivered) {
            match osgi_version_cmp(brought, installed) {
                Ordering::Less => applied.blocked.push(format!(
                    "{repo} is at {installed}, newer than the {brought} this fix delivers: \
                     applying it would downgrade the repository. Uninstalling or reverting \
                     is Update Manager's job"
                )),
                Ordering::Equal => applied.blocked.push(format!(
                    "{repo} is already at {brought}: this fix looks applied"
                )),
                Ordering::Greater => {}
            }
        }
        applied.repositories.push(level);
        on_disk.insert(repo.clone(), disk);
    }

    let file = fs::File::open(&fix.path).map_err(|e| Error::io(&fix.path, e))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| Error::Exec(format!("{} unreadable: {e}", fix.path.display())))?;

    let mut plans = Vec::new();
    for profile in profiles_of(wm_home) {
        if let Some(plan) = plan_profile(&mut zip, wm_home, &profile, &delivered, &on_disk)? {
            applied.warnings.extend(plan.warnings.iter().cloned());
            plans.push(plan);
        }
    }

    // A running runtime is checked on every profile the fix concerns: the
    // ones its recipe names, and the ones whose bundle list it would rewrite.
    let mut concerned: BTreeSet<String> = fix.profiles().into_iter().collect();
    concerned.extend(
        plans
            .iter()
            .filter(|p| !p.updates.is_empty())
            .map(|p| p.profile.clone()),
    );
    let running: Vec<String> = concerned
        .iter()
        .filter(|p| {
            let dir = wm_home.join("profiles").join(p);
            dir.is_dir() && is_running(&dir)
        })
        .cloned()
        .collect();
    for profile in &running {
        applied.warnings.push(format!(
            "profile {profile} looks like it is running (a wrapper anchor or lock is \
             present); stop it before applying"
        ));
    }
    if !running.is_empty() && !options.dry_run {
        applied.blocked.push(format!(
            "{} profile(s) this fix touches look like they are running: {}",
            running.len(),
            running.join(", ")
        ));
    }

    let perform = !options.dry_run && (applied.blocked.is_empty() || options.force);
    if perform && !applied.blocked.is_empty() {
        applied
            .warnings
            .push("force=true: applied despite the reasons listed under blocked".into());
    }
    applied.performed = perform;
    let dry = !perform;

    // The bundles a fix delivers travel under the paths named by
    // `P2-Repositories`; the vendor tool refreshes those repositories and then
    // re-provisions each profile from them. Extract them the same way.
    for repository in &fix.p2_repositories {
        let pattern = format!("{repository}/**/*");
        applied
            .extracted
            .extend(extract(fix, wm_home, &pattern, dry)?);
    }

    for phase in &fix.phases {
        for action in &phase.actions {
            match action {
                Action::Extract { include } => {
                    let written = extract(fix, wm_home, include, dry)?;
                    applied.extracted.extend(written);
                }
                Action::Delete { path } => {
                    let Some(relative) = safe_relative(path) else {
                        return Err(Error::Exec(format!(
                            "fix asks to delete a path outside the installation: {path:?}"
                        )));
                    };
                    let target = wm_home.join(&relative);
                    if target.exists() {
                        if !dry {
                            let removed = if target.is_dir() {
                                fs::remove_dir_all(&target)
                            } else {
                                fs::remove_file(&target)
                            };
                            removed.map_err(|e| Error::io(&target, e))?;
                        }
                        applied
                            .deleted
                            .push(relative.to_string_lossy().into_owned());
                    }
                }
                Action::OsgiCleanCache { profiles } => {
                    for profile in profiles {
                        let cleared = clean_cache(wm_home, profile, dry)?;
                        applied.caches_cleared.extend(cleared);
                    }
                }
                // Stopping a runtime is the operator's call, not this engine's:
                // it may be under a service manager, and killing it midway is
                // worse than refusing.
                Action::OsgiShutdown { .. } => {}
                other @ Action::Unsupported { .. } => {
                    applied.not_performed.push(other.clone());
                }
            }
        }
    }

    for plan in plans {
        let profile_dir = wm_home.join("profiles").join(&plan.profile);
        if !dry && !plan.updates.is_empty() {
            for (target, source) in &plan.copies {
                let from = wm_home.join(source);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
                }
                fs::copy(&from, target).map_err(|e| Error::io(target, e))?;
            }
            // Keep the previous list: a bundles.info that is wrong stops the
            // runtime, and having the old one beside it makes that recoverable.
            let backup = plan.info_path.with_extension("info.before-fix");
            fs::copy(&plan.info_path, &backup).map_err(|e| Error::io(&backup, e))?;
            fs::write(&plan.info_path, plan.lines.join(""))
                .map_err(|e| Error::io(&plan.info_path, e))?;
        }
        applied.verification.extend(verify_lines(
            &plan.profile,
            &plan.lines,
            (!dry).then_some(profile_dir.as_path()),
        ));
        applied.profile_updates.extend(plan.updates);
    }
    Ok(applied)
}

/// Profiles present in an installation.
fn profiles_of(wm_home: &Path) -> Vec<String> {
    let root = wm_home.join("profiles");
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().join("configuration").is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The bundles and features a p2 repository holds, by symbolic name.
#[derive(Debug, Default, Clone)]
struct Repository {
    /// Feature symbolic name → versions present.
    features: BTreeMap<String, BTreeSet<String>>,
    /// Plugin symbolic name → versions present.
    plugins: BTreeMap<String, BTreeSet<String>>,
    /// `(plugin name, version)` → path relative to the installation.
    sources: BTreeMap<(String, String), String>,
}

impl Repository {
    /// Read a repository from disk. `repo` may or may not end in `eclipse`;
    /// both layouts occur in `P2-Repositories`.
    fn from_disk(wm_home: &Path, repo: &str) -> Self {
        let mut contents = Self::default();
        for base in [repo.to_string(), format!("{repo}/eclipse")] {
            for kind in ["plugins", "features"] {
                let dir = wm_home.join(&base).join(kind);
                let Ok(entries) = fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let file = entry.file_name().to_string_lossy().into_owned();
                    let relative = format!("{base}/{kind}/{file}");
                    let inside = &relative[repo.len()..];
                    contents.add(inside, &relative);
                }
            }
        }
        contents
    }

    /// Classify one path. `inside` is the part after the repository path,
    /// `relative` the whole path relative to the installation.
    fn add(&mut self, inside: &str, relative: &str) {
        let file = inside.rsplit('/').next().unwrap_or(inside);
        let Some((name, version)) = split_jar(file) else {
            return;
        };
        let bucket = if inside.contains("/features/") {
            &mut self.features
        } else if inside.contains("/plugins/") {
            &mut self.plugins
        } else {
            return;
        };
        bucket
            .entry(name.to_string())
            .or_default()
            .insert(version.to_string());
        if inside.contains("/plugins/") {
            self.sources.insert(
                (name.to_string(), version.to_string()),
                relative.to_string(),
            );
        }
    }

    /// The fix level: the highest feature version held.
    fn level(&self) -> Option<String> {
        self.features
            .values()
            .flatten()
            .max_by(|a, b| osgi_version_cmp(a, b))
            .cloned()
    }
}

/// `name_version.jar` → `(name, version)`. Versions never contain `_`; names
/// occasionally do, so the split is on the last one.
fn split_jar(file: &str) -> Option<(&str, &str)> {
    file.strip_suffix(".jar")?.rsplit_once('_')
}

/// The changes one fix makes to one profile's `bundles.info`.
struct ProfilePlan {
    profile: String,
    info_path: PathBuf,
    /// The file as it will be written, one line per element, newlines kept.
    lines: Vec<String>,
    updates: Vec<ProfileUpdate>,
    /// Jar copies to make into the profile: (target, source relative to the installation).
    copies: Vec<(PathBuf, String)>,
    warnings: Vec<String>,
}

/// Decide, line by line, what a fix does to one profile.
///
/// See the module documentation for the three rules. Only the lines whose
/// exact `(name, version)` jar sat in a refreshed repository before the fix
/// are candidates; everything else is copied through untouched.
fn plan_profile(
    zip: &mut zip::ZipArchive<fs::File>,
    wm_home: &Path,
    profile: &str,
    delivered: &BTreeMap<String, Repository>,
    on_disk: &BTreeMap<String, Repository>,
) -> Result<Option<ProfilePlan>> {
    let profile_dir = wm_home.join("profiles").join(profile);
    let info_path = profile_dir
        .join("configuration")
        .join("org.eclipse.equinox.simpleconfigurator")
        .join("bundles.info");
    let Ok(info) = fs::read_to_string(&info_path) else {
        return Ok(None);
    };

    let mut plan = ProfilePlan {
        profile: profile.to_string(),
        info_path,
        lines: Vec::new(),
        updates: Vec::new(),
        copies: Vec::new(),
        warnings: Vec::new(),
    };

    let parsed: Vec<Option<[&str; 5]>> = info
        .lines()
        .map(|line| {
            if line.starts_with('#') || line.trim().is_empty() {
                return None;
            }
            let fields: Vec<&str> = line.split(',').collect();
            <[&str; 5]>::try_from(fields).ok()
        })
        .collect();
    // Every (name, version) the profile lists, kept current as lines move, so
    // that no move lands on a line that already exists.
    let mut listed: BTreeSet<(String, String)> = parsed
        .iter()
        .flatten()
        .map(|f| (f[0].to_string(), f[1].to_string()))
        .collect();

    for (line, fields) in info.lines().zip(parsed.iter()) {
        let Some([name, version, location, start_level, started]) = fields else {
            plan.lines.push(format!("{line}\n"));
            continue;
        };
        // Which refreshed repository this line came from, if any: the one that
        // held exactly this build before the fix.
        let origin = on_disk.iter().find(|(_, repo)| {
            repo.plugins
                .get(*name)
                .is_some_and(|versions| versions.contains(*version))
        });
        let Some((repo, _)) = origin else {
            plan.lines.push(format!("{line}\n"));
            continue;
        };
        let shipped = delivered.get(repo).and_then(|d| d.plugins.get(*name));
        match shipped {
            Some(versions) if versions.contains(*version) => {
                // Same build: the profile's own copy is refreshed when the
                // bytes differ. A bundle referenced in place needs nothing,
                // the repository extraction refreshes it.
                if location.starts_with("plugins/") {
                    let source = &delivered[repo].sources[&(name.to_string(), version.to_string())];
                    let target = profile_dir.join(location);
                    let new = entry_bytes(zip, source)?;
                    if fs::read(&target).ok().as_deref() != Some(new.as_slice()) {
                        plan.copies.push((target, source.clone()));
                        plan.updates.push(ProfileUpdate {
                            profile: profile.to_string(),
                            bundle: name.to_string(),
                            from: version.to_string(),
                            to: version.to_string(),
                            kind: UpdateKind::Refreshed,
                        });
                    }
                }
                plan.lines.push(format!("{line}\n"));
            }
            Some(versions) => {
                let mut newer: Vec<&String> = versions
                    .iter()
                    .filter(|v| osgi_version_cmp(v, version) == Ordering::Greater)
                    .collect();
                newer.sort_by(|a, b| osgi_version_cmp(a, b));
                let Some(pick) = newer.first().map(|v| (*v).clone()) else {
                    plan.warnings.push(format!(
                        "{profile}: {name} {version} came from {repo}, which the fix leaves at {}; \
                         not downgraded",
                        versions.iter().cloned().collect::<Vec<_>>().join(", ")
                    ));
                    plan.lines.push(format!("{line}\n"));
                    continue;
                };
                if listed.contains(&(name.to_string(), pick.clone())) {
                    plan.warnings.push(format!(
                        "{profile}: {name} {version} would move to {pick}, which the profile \
                         already lists; left as is — the p2 director would collapse the two"
                    ));
                    plan.lines.push(format!("{line}\n"));
                    continue;
                }
                let jar = format!("{name}_{pick}.jar");
                let new_location = if location.starts_with("plugins/") {
                    let target = profile_dir.join("plugins").join(&jar);
                    let source = delivered[repo].sources[&(name.to_string(), pick.clone())].clone();
                    plan.copies.push((target, source));
                    format!("plugins/{jar}")
                } else {
                    match location.rsplit_once('/') {
                        Some((dir, _)) => format!("{dir}/{jar}"),
                        None => jar.clone(),
                    }
                };
                plan.lines.push(format!(
                    "{name},{pick},{new_location},{start_level},{started}\n"
                ));
                listed.remove(&(name.to_string(), version.to_string()));
                listed.insert((name.to_string(), pick.clone()));
                plan.updates.push(ProfileUpdate {
                    profile: profile.to_string(),
                    bundle: name.to_string(),
                    from: version.to_string(),
                    to: pick,
                    kind: UpdateKind::Replaced,
                });
            }
            None => {
                plan.warnings.push(format!(
                    "{profile}: the fix drops {name} from {repo}; the profile keeps {version}, \
                     which a p2 director might have removed"
                ));
                plan.lines.push(format!("{line}\n"));
            }
        }
    }
    Ok(Some(plan))
}

/// Check a rewritten bundle list: no `(name, version)` twice, and when the
/// profile directory is given, every jar the profile keeps its own copy of is
/// there.
fn verify_lines(profile: &str, lines: &[String], profile_dir: Option<&Path>) -> Vec<String> {
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut findings = Vec::new();
    for line in lines {
        let line = line.trim_end();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        let [name, version, location, ..] = fields[..] else {
            continue;
        };
        if !seen.insert((name.to_string(), version.to_string())) {
            findings.push(format!(
                "{profile}: bundles.info lists {name} {version} twice"
            ));
        }
        if let Some(dir) = profile_dir {
            if location.starts_with("plugins/") && !dir.join(location).exists() {
                findings.push(format!(
                    "{profile}: bundles.info names {location}, which is not there"
                ));
            }
        }
    }
    findings
}

/// Compare two versions the way OSGi does: up to three numeric segments, then
/// the qualifier as a string, an absent qualifier sorting first.
///
/// `2.10.1 > 2.9.0`, `12.1.0.0003-0779 > 12.1.0.0001-0731`, `32.1.1.jre >
/// 32.1.1`.
pub fn osgi_version_cmp(a: &str, b: &str) -> Ordering {
    fn parse(version: &str) -> ([u64; 3], String) {
        let mut numbers = [0u64; 3];
        let mut rest = version;
        for slot in numbers.iter_mut() {
            let (head, tail) = match rest.split_once('.') {
                Some((h, t)) => (h, t),
                None => (rest, ""),
            };
            let Ok(n) = head.parse::<u64>() else {
                return (numbers, rest.to_string());
            };
            *slot = n;
            rest = tail;
            if tail.is_empty() {
                break;
            }
        }
        (numbers, rest.to_string())
    }
    let (na, qa) = parse(a);
    let (nb, qb) = parse(b);
    na.cmp(&nb).then_with(|| qa.cmp(&qb))
}

/// Unpack the entries a glob selects.
fn extract(fix: &Fix, wm_home: &Path, include: &str, dry_run: bool) -> Result<Vec<String>> {
    let file = fs::File::open(&fix.path).map_err(|e| Error::io(&fix.path, e))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| Error::Exec(format!("{} unreadable: {e}", fix.path.display())))?;
    let mut written = Vec::new();

    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|e| {
            Error::Exec(format!(
                "cannot read entry {index} of {}: {e}",
                fix.path.display()
            ))
        })?;
        let name = entry.name().to_string();
        if name.starts_with("META-INF") || name.ends_with('/') || !glob_matches(include, &name) {
            continue;
        }
        let Some(relative) = safe_relative(&name) else {
            return Err(Error::Exec(format!(
                "fix entry escapes the installation: {name:?}"
            )));
        };
        if !dry_run {
            let target = wm_home.join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
            }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(|e| {
                Error::Exec(format!(
                    "cannot read {name} from {}: {e}",
                    fix.path.display()
                ))
            })?;
            fs::write(&target, &bytes).map_err(|e| Error::io(&target, e))?;
        }
        written.push(relative.to_string_lossy().into_owned());
    }
    Ok(written)
}

/// Remove the OSGi framework caches of a profile so it re-reads its bundles.
fn clean_cache(wm_home: &Path, profile: &str, dry_run: bool) -> Result<Vec<String>> {
    let configuration = wm_home.join("profiles").join(profile).join("configuration");
    let mut cleared = Vec::new();
    for name in [
        "org.eclipse.osgi",
        "org.eclipse.core.runtime",
        "org.eclipse.equinox.app",
    ] {
        let dir = configuration.join(name);
        if dir.is_dir() {
            if !dry_run {
                fs::remove_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
            }
            cleared.push(format!("profiles/{profile}/configuration/{name}"));
        }
    }
    Ok(cleared)
}

/// Whether a profile looks like it is running.
fn is_running(profile_dir: &Path) -> bool {
    ["bin/wrapper.anchor", "bin/.lock"]
        .iter()
        .any(|p| profile_dir.join(p).exists())
}

/// Parse the manifest's main section, honouring 72-column continuation lines.
fn parse_manifest(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut key: Option<String> = None;
    let mut value = String::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            // A blank line ends the main section; per-entry sections follow and
            // are signature digests, not metadata.
            break;
        }
        if let Some(rest) = line.strip_prefix(' ') {
            value.push_str(rest);
            continue;
        }
        if let Some(k) = key.take() {
            map.insert(k, value.trim().to_string());
            value.clear();
        }
        if let Some((k, v)) = line.split_once(':') {
            key = Some(k.trim().to_string());
            value = v.trim().to_string();
        }
    }
    if let Some(k) = key {
        map.insert(k, value.trim().to_string());
    }
    map
}

/// Parse `install.phaseN=action(...);action(...)`, joining backslash continuations.
fn parse_instructions(text: &str) -> Vec<Phase> {
    let joined = text.replace("\\\r\n", "").replace("\\\n", "");
    let mut phases: BTreeMap<u32, Vec<Action>> = BTreeMap::new();
    for line in joined.lines() {
        let line = line.trim();
        let Some((key, body)) = line.split_once('=') else {
            continue;
        };
        let Some(number) = key
            .trim()
            .strip_prefix("install.phase")
            .and_then(|n| n.parse().ok())
        else {
            continue;
        };
        let actions = phases.entry(number).or_default();
        for raw in body.split(';') {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            actions.push(parse_action(raw));
        }
    }
    phases
        .into_iter()
        .map(|(number, actions)| Phase { number, actions })
        .collect()
}

/// Parse one `verb(key:value)` action.
fn parse_action(raw: &str) -> Action {
    let Some((verb, rest)) = raw.split_once('(') else {
        return Action::Unsupported {
            verb: raw.to_string(),
            raw: raw.to_string(),
        };
    };
    let verb = verb.trim();
    let args = rest.trim_end_matches(')').trim();
    let value = args.split_once(':').map(|(_, v)| v.trim()).unwrap_or(args);
    // Profile lists are written `SPM,MWS_default` in one fix and `SPM|CCE` in
    // another; Update Manager accepts both.
    let list = || -> Vec<String> {
        value
            .split([',', '|'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    };
    match verb {
        "extract" => Action::Extract {
            include: value.to_string(),
        },
        "delete" => Action::Delete {
            path: value.to_string(),
        },
        "osgiShutdown" => Action::OsgiShutdown { profiles: list() },
        "osgiCleanCache" => Action::OsgiCleanCache { profiles: list() },
        other => Action::Unsupported {
            verb: other.to_string(),
            raw: raw.to_string(),
        },
    }
}

/// Match an archive entry against the simple globs fixes use.
///
/// Only `**` (any depth) and `*` (within one segment) appear in practice, so
/// the matcher covers those rather than pulling in a glob crate.
fn glob_matches(pattern: &str, name: &str) -> bool {
    fn walk(p: &[u8], n: &[u8]) -> bool {
        if p.is_empty() {
            return n.is_empty();
        }
        if p.starts_with(b"**") {
            let rest = &p[2..];
            let rest = rest.strip_prefix(b"/").unwrap_or(rest);
            // `**` matches any number of segments, including none.
            for skip in 0..=n.len() {
                if walk(rest, &n[skip..]) {
                    return true;
                }
            }
            return false;
        }
        if p[0] == b'*' {
            // A single star stops at a separator.
            let limit = n.iter().position(|&c| c == b'/').unwrap_or(n.len());
            for skip in 0..=limit {
                if walk(&p[1..], &n[skip..]) {
                    return true;
                }
            }
            return false;
        }
        !n.is_empty() && p[0] == n[0] && walk(&p[1..], &n[1..])
    }
    walk(pattern.as_bytes(), name.as_bytes())
}

/// Refuse a path that would write outside the installation.
fn safe_relative(entry: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut clean = PathBuf::new();
    for component in Path::new(entry).components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!clean.as_os_str().is_empty()).then_some(clean)
}

fn read_entry(zip: &mut zip::ZipArchive<fs::File>, name: &str) -> Option<String> {
    let mut entry = zip.by_name(name).ok()?;
    let mut text = String::new();
    entry.read_to_string(&mut text).ok()?;
    Some(text)
}

fn entry_bytes(zip: &mut zip::ZipArchive<fs::File>, name: &str) -> Result<Vec<u8>> {
    let mut entry = zip
        .by_name(name)
        .map_err(|e| Error::Exec(format!("the fix names {name} but does not carry it: {e}")))?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|e| Error::Exec(format!("cannot read {name} from the fix: {e}")))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    const INSTRUCTIONS: &str = "install.phase3=osgiShutdown(profile:SPM);\n\
                                install.phase4=delete(file:PlatformManager/migrate/lib);\n\
                                install.phase5=extract(include:PlatformManager/**/*);\\\n\
                                osgiCleanCache(profiles:SPM,MWS_default);\n";

    #[test]
    fn parses_phases_in_order() {
        let phases = parse_instructions(INSTRUCTIONS);
        assert_eq!(
            phases.iter().map(|p| p.number).collect::<Vec<_>>(),
            [3, 4, 5]
        );
        assert_eq!(
            phases[0].actions,
            [Action::OsgiShutdown {
                profiles: vec!["SPM".into()]
            }]
        );
        assert_eq!(
            phases[1].actions,
            [Action::Delete {
                path: "PlatformManager/migrate/lib".into()
            }]
        );
    }

    #[test]
    fn a_backslash_continues_a_phase() {
        let phases = parse_instructions(INSTRUCTIONS);
        // The continued line belongs to phase 5, giving it two actions.
        assert_eq!(phases[2].actions.len(), 2);
        assert_eq!(
            phases[2].actions[1],
            Action::OsgiCleanCache {
                profiles: vec!["SPM".into(), "MWS_default".into()]
            }
        );
    }

    #[test]
    fn profiles_are_split_on_pipes_as_well_as_commas() {
        // wMFix.CCShared writes `osgiShutdown(profile:SPM|CCE)`.
        assert_eq!(
            parse_action("osgiShutdown(profile:SPM|CCE)"),
            Action::OsgiShutdown {
                profiles: vec!["SPM".into(), "CCE".into()]
            }
        );
        // wMFix.TPS.SharedBundles writes `osgiShutdown()`: the target
        // product's own runtime, which the recipe does not name.
        assert_eq!(
            parse_action("osgiShutdown()"),
            Action::OsgiShutdown { profiles: vec![] }
        );
    }

    #[test]
    fn an_unknown_verb_is_reported_not_ignored() {
        let phases = parse_instructions("install.phase1=osgiInstallIU(iu:com.example);\n");
        match &phases[0].actions[0] {
            Action::Unsupported { verb, .. } => assert_eq!(verb, "osgiInstallIU"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
        assert!(!phases[0].actions[0].is_supported());
    }

    #[test]
    fn reads_a_wrapped_manifest() {
        let manifest = "Manifest-Version: 1.0\n\
                        Fix-Name: wMFix.SPM\n\
                        P2-Repositories: common/runtime/bundles/spm/eclipse\n\
                        Display-Fix-Name: Platform Manager 12.1.0\n \
                        FIX 1\n\
                        \n\
                        Name: some/entry\n\
                        SHA-256-Digest: ignored\n";
        let map = parse_manifest(manifest);
        assert_eq!(map.get("Fix-Name").map(String::as_str), Some("wMFix.SPM"));
        // A continuation line belongs to the previous header.
        assert_eq!(
            map.get("Display-Fix-Name").map(String::as_str),
            Some("Platform Manager 12.1.0FIX 1")
        );
        // The per-entry sections after the blank line are not metadata.
        assert!(!map.contains_key("SHA-256-Digest"));
    }

    #[test]
    fn requirements_carry_a_name_and_a_minimum_version() {
        let list = Requirement::parse_list(
            "wMFix.OSGI.Migration;version=12.1.0.0002,wMFix.OSGI.Platform;version=12.1.0.0003, wMFix.Bare",
        );
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].name, "wMFix.OSGI.Migration");
        assert_eq!(list[0].version.as_deref(), Some("12.1.0.0002"));
        assert_eq!(list[2].name, "wMFix.Bare");
        assert_eq!(list[2].version, None);
    }

    #[test]
    fn osgi_versions_compare_numerically_then_by_qualifier() {
        use Ordering::*;
        assert_eq!(osgi_version_cmp("2.10.1", "2.9.0"), Greater);
        assert_eq!(
            osgi_version_cmp("12.1.0.0003-0779", "12.1.0.0001-0731"),
            Greater
        );
        assert_eq!(
            osgi_version_cmp("12.1.0.0001-0731", "12.1.0.0001-0731"),
            Equal
        );
        assert_eq!(osgi_version_cmp("32.1.1.jre", "32.1.1"), Greater);
        assert_eq!(osgi_version_cmp("25.1.0.jre", "32.1.1.jre"), Less);
        assert_eq!(osgi_version_cmp("1.0", "1.0.0"), Equal);
        assert_eq!(
            osgi_version_cmp("12.1.0.0000-0280", "12.1.0.0001-0731"),
            Less
        );
    }

    #[test]
    fn globs_cover_what_fixes_use() {
        assert!(glob_matches(
            "PlatformManager/**/*",
            "PlatformManager/lib/a.jar"
        ));
        assert!(glob_matches(
            "PlatformManager/**/*",
            "PlatformManager/a.jar"
        ));
        assert!(!glob_matches("PlatformManager/**/*", "common/lib/a.jar"));
        assert!(glob_matches("**/*.jar", "a/b/c.jar"));
        assert!(!glob_matches("**/*.jar", "a/b/c.txt"));
        // A single star does not cross a separator.
        assert!(glob_matches("common/*/x", "common/lib/x"));
        assert!(!glob_matches("common/*/x", "common/lib/deep/x"));
    }

    #[test]
    fn refuses_paths_outside_the_installation() {
        assert!(safe_relative("../etc/passwd").is_none());
        assert!(safe_relative("/etc/passwd").is_none());
        assert_eq!(
            safe_relative("common/lib"),
            Some(PathBuf::from("common/lib"))
        );
    }

    // ---- a forged installation and a forged fix -------------------------

    const REPO: &str = "common/runtime/bundles/ext";

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A fresh, empty installation directory. Removed by `Home::drop`.
    struct Home(PathBuf);

    impl Home {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, AtomicOrdering::SeqCst);
            let root =
                std::env::temp_dir().join(format!("wm-fix-{tag}-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("home");
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn put(&self, relative: &str, bytes: &[u8]) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).expect("dirs");
            fs::write(path, bytes).expect("write");
        }

        /// A repository at `level`, holding `plugins` as `(name, version, bytes)`.
        fn repository(&self, level: &str, plugins: &[(&str, &str, &[u8])]) {
            self.put(
                &format!("{REPO}/eclipse/features/com.example.feature_{level}.jar"),
                b"feature",
            );
            for (name, version, bytes) in plugins {
                self.put(
                    &format!("{REPO}/eclipse/plugins/{name}_{version}.jar"),
                    bytes,
                );
            }
        }

        /// A profile whose `bundles.info` lists `bundles` as `(name, version,
        /// bytes)`, each with its own copy under `plugins/`.
        fn profile(&self, name: &str, bundles: &[(&str, &str, &[u8])]) {
            let mut info = String::from("#encoding=UTF-8\n#version=1\n");
            for (bundle, version, bytes) in bundles {
                self.put(
                    &format!("profiles/{name}/plugins/{bundle}_{version}.jar"),
                    bytes,
                );
                info.push_str(&format!(
                    "{bundle},{version},plugins/{bundle}_{version}.jar,4,true\n"
                ));
            }
            self.put(
                &format!(
                    "profiles/{name}/configuration/org.eclipse.equinox.simpleconfigurator/bundles.info"
                ),
                info.as_bytes(),
            );
        }

        fn bundles_info(&self, profile: &str) -> String {
            fs::read_to_string(self.0.join(format!(
                "profiles/{profile}/configuration/org.eclipse.equinox.simpleconfigurator/bundles.info"
            )))
            .expect("bundles.info")
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Write a fix archive delivering the repository at `level` with `plugins`.
    fn forged_fix(
        home: &Home,
        version: &str,
        level: &str,
        plugins: &[(&str, &str, &[u8])],
        instructions: &str,
    ) -> Fix {
        let path = home.path().join(format!("wMFix.Test_{version}"));
        let file = fs::File::create(&path).expect("archive");
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        let manifest = format!(
            "Manifest-Version: 1.0\n\
             Display-Fix-Name: Test 12.1 Fix\n\
             P2-Repositories: {REPO}\n\
             Fix-Name: wMFix.Test\n\
             Fix-Version: {version}\n\
             Require-Fix: wMFix.Other;version=12.1.0.0001\n\
             Install-After: wMFix.First;version=12.1.0.0002,wMFix.Second\n\
             \n"
        );
        zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.start_file("META-INF/instructions.txt", options)
            .unwrap();
        zip.write_all(instructions.as_bytes()).unwrap();
        zip.start_file(
            format!("{REPO}/eclipse/features/com.example.feature_{level}.jar"),
            options,
        )
        .unwrap();
        zip.write_all(b"feature").unwrap();
        for (name, plugin_version, bytes) in plugins {
            zip.start_file(
                format!("{REPO}/eclipse/plugins/{name}_{plugin_version}.jar"),
                options,
            )
            .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        Fix::read(&path).expect("read the forged fix")
    }

    const DRY: Options = Options {
        dry_run: true,
        force: false,
    };
    const APPLY: Options = Options {
        dry_run: false,
        force: false,
    };
    const FORCE: Options = Options {
        dry_run: false,
        force: true,
    };

    #[test]
    fn reads_version_and_requirements_from_the_manifest() {
        let home = Home::new("manifest");
        let fix = forged_fix(&home, "12.1.0.0003-0779", "12.1.0.0003-0779", &[], "");
        assert_eq!(fix.version.as_deref(), Some("12.1.0.0003-0779"));
        assert_eq!(fix.id().as_deref(), Some("wMFix.Test_12.1.0.0003-0779"));
        assert_eq!(fix.requires_fixes[0].name, "wMFix.Other");
        assert_eq!(fix.install_after.len(), 2);
        assert_eq!(fix.install_after[1].version, None);
        assert_eq!(fix.p2_repositories, vec![REPO.to_string()]);
    }

    #[test]
    fn a_bundle_from_another_repository_is_left_alone() {
        // The real case: SPM lists gson at 2.10.1 (from another repository)
        // and at 2.9.0 (from `ext`). The `ext` fix delivers gson 2.9.0 again.
        // Indexing by name alone rewrote the 2.10.1 line to 2.9.0 and left
        // the profile with the same bundle twice.
        let home = Home::new("other-repo");
        home.repository(
            "12.1.0.0001-0731",
            &[("com.google.gson", "2.9.0", b"gson29")],
        );
        home.profile(
            "SPM",
            &[
                ("com.google.gson", "2.10.1", b"gson210"),
                ("com.google.gson", "2.9.0", b"gson29"),
            ],
        );
        let fix = forged_fix(
            &home,
            "12.1.0.0003-0779",
            "12.1.0.0003-0779",
            &[("com.google.gson", "2.9.0", b"gson29")],
            "install.phase3=osgiShutdown();\n",
        );

        let applied = apply(&fix, home.path(), APPLY).expect("apply");
        assert!(applied.blocked.is_empty(), "{:?}", applied.blocked);
        assert!(applied.performed);
        assert!(
            applied.profile_updates.is_empty(),
            "{:?}",
            applied.profile_updates
        );
        assert!(
            applied.verification.is_empty(),
            "{:?}",
            applied.verification
        );
        let info = home.bundles_info("SPM");
        assert!(info.contains("com.google.gson,2.10.1,"), "{info}");
        assert_eq!(info.matches("com.google.gson,2.9.0,").count(), 1, "{info}");
    }

    #[test]
    fn a_superseded_bundle_moves_to_the_delivered_build() {
        let home = Home::new("supersede");
        home.repository("12.1.0.0001-0731", &[("com.example.a", "1.0.0", b"a1")]);
        home.profile("SPM", &[("com.example.a", "1.0.0", b"a1")]);
        let fix = forged_fix(
            &home,
            "12.1.0.0003-0779",
            "12.1.0.0003-0779",
            &[("com.example.a", "1.1.0", b"a11")],
            "",
        );

        let dry = apply(&fix, home.path(), DRY).expect("dry run");
        assert_eq!(dry.profile_updates.len(), 1);
        assert!(!dry.performed);
        assert!(home.bundles_info("SPM").contains("com.example.a,1.0.0,"));

        let applied = apply(&fix, home.path(), APPLY).expect("apply");
        assert!(applied.performed);
        let [update] = &applied.profile_updates[..] else {
            panic!("expected one update, got {:?}", applied.profile_updates);
        };
        assert_eq!(
            (update.from.as_str(), update.to.as_str()),
            ("1.0.0", "1.1.0")
        );
        assert_eq!(update.kind, UpdateKind::Replaced);
        let info = home.bundles_info("SPM");
        assert!(
            info.contains("com.example.a,1.1.0,plugins/com.example.a_1.1.0.jar,4,true"),
            "{info}"
        );
        assert!(!info.contains("com.example.a,1.0.0,"), "{info}");
        assert_eq!(
            fs::read(
                home.path()
                    .join("profiles/SPM/plugins/com.example.a_1.1.0.jar")
            )
            .unwrap(),
            b"a11"
        );
        assert!(
            applied.verification.is_empty(),
            "{:?}",
            applied.verification
        );
        assert!(home
            .path()
            .join("profiles/SPM/configuration/org.eclipse.equinox.simpleconfigurator/bundles.info.before-fix")
            .is_file());
        assert_eq!(
            applied.repositories[0].installed.as_deref(),
            Some("12.1.0.0001-0731")
        );
        assert_eq!(
            applied.repositories[0].delivered.as_deref(),
            Some("12.1.0.0003-0779")
        );
    }

    #[test]
    fn a_bundle_the_profile_does_not_carry_is_not_added() {
        let home = Home::new("not-added");
        home.repository("12.1.0.0001-0731", &[("com.example.a", "1.0.0", b"a1")]);
        home.profile("SPM", &[("com.example.a", "1.0.0", b"a1")]);
        let fix = forged_fix(
            &home,
            "12.1.0.0003-0779",
            "12.1.0.0003-0779",
            &[
                ("com.example.a", "1.0.0", b"a1"),
                ("com.example.new", "9.0.0", b"new"),
            ],
            "",
        );
        let applied = apply(&fix, home.path(), APPLY).expect("apply");
        assert!(
            applied.profile_updates.is_empty(),
            "{:?}",
            applied.profile_updates
        );
        assert!(!home.bundles_info("SPM").contains("com.example.new"));
    }

    #[test]
    fn same_version_with_new_bytes_is_refreshed() {
        let home = Home::new("refresh");
        home.repository("12.1.0.0001-0731", &[("com.example.a", "1.0.0", b"old")]);
        home.profile("SPM", &[("com.example.a", "1.0.0", b"old")]);
        let fix = forged_fix(
            &home,
            "12.1.0.0003-0779",
            "12.1.0.0003-0779",
            &[("com.example.a", "1.0.0", b"new")],
            "",
        );
        let applied = apply(&fix, home.path(), APPLY).expect("apply");
        assert_eq!(applied.profile_updates.len(), 1);
        assert_eq!(applied.profile_updates[0].kind, UpdateKind::Refreshed);
        assert_eq!(
            fs::read(
                home.path()
                    .join("profiles/SPM/plugins/com.example.a_1.0.0.jar")
            )
            .unwrap(),
            b"new"
        );
        // The line itself is unchanged.
        assert!(home
            .bundles_info("SPM")
            .contains("com.example.a,1.0.0,plugins/com.example.a_1.0.0.jar,4,true"));
    }

    #[test]
    fn an_older_fix_is_refused_and_force_overrides() {
        let home = Home::new("older");
        home.repository("12.1.0.0003-0779", &[("com.example.a", "1.1.0", b"a11")]);
        home.profile("SPM", &[("com.example.a", "1.1.0", b"a11")]);
        let older = forged_fix(
            &home,
            "12.1.0.0001-0731",
            "12.1.0.0001-0731",
            &[("com.example.a", "1.0.0", b"a1")],
            "",
        );

        let dry = apply(&older, home.path(), DRY).expect("dry run");
        assert!(
            dry.blocked.iter().any(|b| b.contains("downgrade")),
            "{:?}",
            dry.blocked
        );

        let refused = apply(&older, home.path(), APPLY).expect("apply");
        assert!(!refused.performed);
        assert!(!home
            .path()
            .join(format!("{REPO}/eclipse/plugins/com.example.a_1.0.0.jar"))
            .exists());

        let forced = apply(&older, home.path(), FORCE).expect("force");
        assert!(forced.performed);
        assert!(forced.warnings.iter().any(|w| w.contains("force=true")));
        // Even forced, a profile line never moves to an older build.
        assert!(
            forced.profile_updates.is_empty(),
            "{:?}",
            forced.profile_updates
        );
        assert!(home.bundles_info("SPM").contains("com.example.a,1.1.0,"));
    }

    #[test]
    fn the_same_level_is_reported_as_already_applied() {
        let home = Home::new("same");
        home.repository("12.1.0.0003-0779", &[("com.example.a", "1.1.0", b"a11")]);
        home.profile("SPM", &[("com.example.a", "1.1.0", b"a11")]);
        let fix = forged_fix(
            &home,
            "12.1.0.0003-0779",
            "12.1.0.0003-0779",
            &[("com.example.a", "1.1.0", b"a11")],
            "",
        );
        let applied = apply(&fix, home.path(), APPLY).expect("apply");
        assert!(!applied.performed);
        assert!(
            applied.blocked.iter().any(|b| b.contains("already")),
            "{:?}",
            applied.blocked
        );
    }

    #[test]
    fn a_running_profile_blocks_apply_but_only_warns_in_a_dry_run() {
        let home = Home::new("running");
        home.repository("12.1.0.0001-0731", &[("com.example.a", "1.0.0", b"a1")]);
        // The recipe names nothing (`osgiShutdown()`), yet the profile is
        // touched because its bundle comes from the refreshed repository.
        home.profile("MWS_default", &[("com.example.a", "1.0.0", b"a1")]);
        home.put("profiles/MWS_default/bin/.lock", b"");
        let fix = forged_fix(
            &home,
            "12.1.0.0003-0779",
            "12.1.0.0003-0779",
            &[("com.example.a", "1.1.0", b"a11")],
            "install.phase3=osgiShutdown();\n",
        );

        let dry = apply(&fix, home.path(), DRY).expect("dry run");
        assert!(dry.blocked.is_empty(), "{:?}", dry.blocked);
        assert!(
            dry.warnings
                .iter()
                .any(|w| w.contains("MWS_default looks like it is running")),
            "{:?}",
            dry.warnings
        );

        let refused = apply(&fix, home.path(), APPLY).expect("apply");
        assert!(!refused.performed);
        assert!(
            refused.blocked.iter().any(|b| b.contains("running")),
            "{:?}",
            refused.blocked
        );
        assert!(home
            .bundles_info("MWS_default")
            .contains("com.example.a,1.0.0,"));

        let forced = apply(&fix, home.path(), FORCE).expect("force");
        assert!(forced.performed);
        assert!(home
            .bundles_info("MWS_default")
            .contains("com.example.a,1.1.0,"));
    }

    #[test]
    fn a_move_onto_a_listed_version_is_declined_rather_than_duplicated() {
        // The profile already lists 1.1.0 (say, from another repository) and
        // 1.0.0 from `ext`; the fix moves `ext` to 1.1.0. Rewriting the 1.0.0
        // line would list 1.1.0 twice, so it stays and the caller is told.
        let home = Home::new("collapse");
        home.repository("12.1.0.0001-0731", &[("com.example.a", "1.0.0", b"a1")]);
        home.profile(
            "SPM",
            &[
                ("com.example.a", "1.1.0", b"a11"),
                ("com.example.a", "1.0.0", b"a1"),
            ],
        );
        let fix = forged_fix(
            &home,
            "12.1.0.0003-0779",
            "12.1.0.0003-0779",
            &[("com.example.a", "1.1.0", b"a11")],
            "",
        );
        let applied = apply(&fix, home.path(), APPLY).expect("apply");
        assert!(
            applied.profile_updates.is_empty(),
            "{:?}",
            applied.profile_updates
        );
        assert!(
            applied.warnings.iter().any(|w| w.contains("already lists")),
            "{:?}",
            applied.warnings
        );
        assert!(
            applied.verification.is_empty(),
            "{:?}",
            applied.verification
        );
    }

    #[test]
    fn verification_catches_a_duplicated_line() {
        let lines = vec![
            "#encoding=UTF-8\n".to_string(),
            "a,1.0.0,plugins/a_1.0.0.jar,4,true\n".to_string(),
            "a,1.0.0,plugins/a_1.0.0.jar,4,true\n".to_string(),
            "a,1.1.0,plugins/a_1.1.0.jar,4,true\n".to_string(),
        ];
        let findings = verify_lines("SPM", &lines, None);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].contains("a 1.0.0 twice"));
    }
}
