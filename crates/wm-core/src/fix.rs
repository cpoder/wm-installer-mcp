//! Applying a fix without Update Manager.
//!
//! A fix is the same shape as a product module: a signed JAR whose entries are
//! rooted at the installation directory. What makes it a fix is two pieces of
//! metadata.
//!
//! `META-INF/MANIFEST.MF` names it and says which p2 repositories inside the
//! installation it refreshes:
//!
//! ```text
//! Display-Fix-Name: Platform Manager 12.1.0 FIX 1
//! Fix-Name: wMFix.SPM
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
//! The file actions are reproduced here. The `osgi*IU` family drives an Eclipse
//! p2 director and is reported rather than performed — see [`Action`].

use std::collections::BTreeMap;
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
        /// Profile names.
        profiles: Vec<String>,
    },
    /// Discard a runtime's OSGi caches so it re-reads its bundles.
    OsgiCleanCache {
        /// Profile names.
        profiles: Vec<String>,
    },
    /// An action this engine does not perform.
    ///
    /// The `osgi*IU`, `osgiPublish` and `p2` verbs drive a p2 director; running
    /// them means resolving installable units, which is not reimplemented. They
    /// are surfaced so a caller sees exactly what is left undone rather than
    /// believing a fix fully applied.
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
    /// `Require-SUM-Build`, the Update Manager build the vendor tool would demand.
    pub requires_sum_build: Option<String>,
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
            let entry = zip
                .by_index(index)
                .map_err(|e| Error::Exec(format!("cannot read entry {index}: {e}")))?;
            let name = entry.name().to_string();
            if !name.starts_with("META-INF") && !name.ends_with('/') {
                entries.push(name);
            }
        }

        Ok(Self {
            path: path.to_path_buf(),
            name: manifest.get("Fix-Name").cloned(),
            display_name: manifest.get("Display-Fix-Name").cloned(),
            group: manifest.get("Display-Group-Name").cloned(),
            requires_sum_build: manifest.get("Require-SUM-Build").cloned(),
            p2_repositories: manifest
                .get("P2-Repositories")
                .map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            phases: parse_instructions(&instructions),
            entries,
        })
    }

    /// Every action, in phase order.
    pub fn actions(&self) -> impl Iterator<Item = &Action> {
        self.phases.iter().flat_map(|p| p.actions.iter())
    }

    /// Actions this engine cannot carry out.
    pub fn unsupported(&self) -> Vec<&Action> {
        self.actions().filter(|a| !a.is_supported()).collect()
    }

    /// Profiles the fix expects stopped before it is applied.
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
}

/// What applying a fix did, or would do.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    /// Whether anything was written.
    pub dry_run: bool,
    /// Files extracted, relative to the installation.
    pub extracted: Vec<String>,
    /// Paths removed.
    pub deleted: Vec<String>,
    /// OSGi caches cleared.
    pub caches_cleared: Vec<String>,
    /// Bundles replaced inside a runtime profile.
    pub profile_updates: Vec<ProfileUpdate>,
    /// Actions reported but not performed.
    pub not_performed: Vec<Action>,
    /// Anything the caller must deal with.
    pub warnings: Vec<String>,
}

/// One bundle replaced in a profile.
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
}

/// Apply a fix to `wm_home`.
///
/// With `dry_run` the plan is computed and nothing is written, which is the
/// right first call: a fix that expects a stopped runtime will otherwise
/// replace files under a running server.
pub fn apply(fix: &Fix, wm_home: &Path, dry_run: bool) -> Result<Applied> {
    let mut applied = Applied {
        dry_run,
        extracted: Vec::new(),
        deleted: Vec::new(),
        caches_cleared: Vec::new(),
        profile_updates: Vec::new(),
        not_performed: Vec::new(),
        warnings: Vec::new(),
    };

    for profile in fix.profiles() {
        let dir = wm_home.join("profiles").join(&profile);
        if dir.is_dir() && is_running(&dir) {
            applied.warnings.push(format!(
                "profile {profile} looks like it is running (a wrapper anchor or lock is present); \
                 stop it before applying"
            ));
        }
    }

    // The `extract` actions of a fix cover its product files. The bundles it
    // delivers travel separately, under the paths named by `P2-Repositories`,
    // and the vendor tool refreshes those repositories and then re-provisions
    // each profile from them. Extract them the same way.
    for repository in &fix.p2_repositories {
        let pattern = format!("{}/**/*", repository.trim_end_matches('/'));
        applied
            .extracted
            .extend(extract(fix, wm_home, &pattern, dry_run)?);
    }

    for phase in &fix.phases {
        for action in &phase.actions {
            match action {
                Action::Extract { include } => {
                    let written = extract(fix, wm_home, include, dry_run)?;
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
                        if !dry_run {
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
                        let cleared = clean_cache(wm_home, profile, dry_run)?;
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

    // Replacing bundles in a profile is not a resolve: the profile already
    // holds a consistent set, and a fix ships newer builds of bundles already
    // in it. Anything the fix adds that the profile does not carry is left
    // alone — adding an installable unit is what needs a director.
    for profile in profiles_of(wm_home) {
        applied
            .profile_updates
            .extend(update_profile(fix, wm_home, &profile, dry_run)?);
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

/// Replace, in one profile, every bundle the fix ships a newer build of.
fn update_profile(
    fix: &Fix,
    wm_home: &Path,
    profile: &str,
    dry_run: bool,
) -> Result<Vec<ProfileUpdate>> {
    let profile_dir = wm_home.join("profiles").join(profile);
    let info_path = profile_dir
        .join("configuration")
        .join("org.eclipse.equinox.simpleconfigurator")
        .join("bundles.info");
    let Ok(info) = fs::read_to_string(&info_path) else {
        return Ok(Vec::new());
    };

    // What the fix delivers, by bundle symbolic name.
    let mut delivered: BTreeMap<String, (String, String)> = BTreeMap::new();
    for entry in &fix.entries {
        let Some(file) = entry.rsplit('/').next() else {
            continue;
        };
        let Some(stem) = file.strip_suffix(".jar") else {
            continue;
        };
        let Some((name, version)) = stem.rsplit_once('_') else {
            continue;
        };
        delivered.insert(name.to_string(), (version.to_string(), entry.clone()));
    }

    let mut updates = Vec::new();
    let mut rewritten = String::with_capacity(info.len());
    for line in info.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            rewritten.push_str(line);
            rewritten.push('\n');
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        // name,version,location,startlevel,started
        let [name, version, _location, start_level, started] = fields[..] else {
            rewritten.push_str(line);
            rewritten.push('\n');
            continue;
        };
        match delivered.get(name) {
            Some((new_version, source)) if new_version != version => {
                let jar = format!("{name}_{new_version}.jar");
                if !dry_run {
                    let from = wm_home.join(source);
                    let to = profile_dir.join("plugins").join(&jar);
                    if let Some(parent) = to.parent() {
                        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
                    }
                    fs::copy(&from, &to).map_err(|e| Error::io(&to, e))?;
                    // The superseded jar is left in place: the profile registry
                    // still references it, and removing it turns a rollback
                    // into a re-download.
                }
                rewritten.push_str(&format!(
                    "{name},{new_version},plugins/{jar},{start_level},{started}\n"
                ));
                updates.push(ProfileUpdate {
                    profile: profile.to_string(),
                    bundle: name.to_string(),
                    from: version.to_string(),
                    to: new_version.clone(),
                });
            }
            _ => {
                rewritten.push_str(line);
                rewritten.push('\n');
            }
        }
    }

    if !updates.is_empty() && !dry_run {
        // Keep the previous list: a bundles.info that is wrong stops the
        // runtime, and having the old one beside it makes that recoverable.
        let backup = info_path.with_extension("info.before-fix");
        let _ = fs::copy(&info_path, &backup);
        fs::write(&info_path, rewritten).map_err(|e| Error::io(&info_path, e))?;
    }
    Ok(updates)
}

/// Unpack the entries a glob selects.
fn extract(fix: &Fix, wm_home: &Path, include: &str, dry_run: bool) -> Result<Vec<String>> {
    let file = fs::File::open(&fix.path).map_err(|e| Error::io(&fix.path, e))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| Error::Exec(format!("{} unreadable: {e}", fix.path.display())))?;
    let mut written = Vec::new();

    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|e| Error::Exec(format!("cannot read entry {index}: {e}")))?;
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
            entry
                .read_to_end(&mut bytes)
                .map_err(|e| Error::Exec(format!("cannot read {name}: {e}")))?;
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
    let joined = text.replace("\\\n", "").replace("\\\r\n", "");
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
    let list = || -> Vec<String> {
        value
            .split(',')
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn replaces_only_bundles_the_profile_already_carries() {
        let home = std::env::temp_dir().join(format!("wm-fix-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let info_dir =
            home.join("profiles/SPM/configuration/org.eclipse.equinox.simpleconfigurator");
        fs::create_dir_all(&info_dir).expect("dirs");
        fs::write(
            info_dir.join("bundles.info"),
            "#encoding=UTF-8\n\
             com.example.a,1.0.0,plugins/com.example.a_1.0.0.jar,4,true\n\
             com.example.b,2.0.0,plugins/com.example.b_2.0.0.jar,4,false\n",
        )
        .expect("info");

        let fix = Fix {
            path: PathBuf::from("unused"),
            name: Some("wMFix.Test".into()),
            display_name: None,
            group: None,
            requires_sum_build: None,
            p2_repositories: Vec::new(),
            phases: Vec::new(),
            entries: vec![
                // A newer build of a bundle the profile has.
                "common/runtime/bundles/x/eclipse/plugins/com.example.a_1.1.0.jar".into(),
                // A bundle the profile does not carry: adding it would be a
                // resolve, so it must be left alone.
                "common/runtime/bundles/x/eclipse/plugins/com.example.new_9.0.0.jar".into(),
                // The same version the profile already has.
                "common/runtime/bundles/x/eclipse/plugins/com.example.b_2.0.0.jar".into(),
            ],
        };

        let updates = update_profile(&fix, &home, "SPM", true).expect("update");
        assert_eq!(updates.len(), 1, "only the superseded bundle is replaced");
        assert_eq!(updates[0].bundle, "com.example.a");
        assert_eq!(updates[0].from, "1.0.0");
        assert_eq!(updates[0].to, "1.1.0");
        // A dry run leaves the file untouched.
        let text = fs::read_to_string(info_dir.join("bundles.info")).expect("read");
        assert!(text.contains("com.example.a,1.0.0"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_profile_without_a_bundle_list_is_skipped() {
        let home = std::env::temp_dir().join(format!("wm-fix-empty-{}", std::process::id()));
        let fix = Fix {
            path: PathBuf::from("unused"),
            name: None,
            display_name: None,
            group: None,
            requires_sum_build: None,
            p2_repositories: Vec::new(),
            phases: Vec::new(),
            entries: vec!["x/plugins/com.example.a_1.1.0.jar".into()],
        };
        assert!(update_profile(&fix, &home, "Nope", true)
            .expect("update")
            .is_empty());
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
}
