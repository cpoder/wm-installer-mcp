//! Capturing an Eclipse p2 profile and replaying it onto another installation.
//!
//! Building a profile from scratch means running a p2 director — see
//! `docs/p2-profiles.md` for why that is not reimplemented. Replaying one is a
//! different problem, and a tractable one: for a given release and product
//! selection the resolved profile is deterministic, so it can be captured once
//! and laid down again.
//!
//! What makes the capture small is that the bundles are already on the target.
//! Every jar in a profile's `plugins/` comes from the installation's own p2
//! repositories under `common/runtime/bundles/*/eclipse/plugins/` — all 494 of
//! them, for Platform Manager 12.1 — and those arrive with the products. So a
//! capture carries the *bundle list* and the configuration, a couple of
//! megabytes, and the replay copies the jars locally.
//!
//! Absolute paths are the other half of the problem: `config.ini`, the launcher
//! scripts and the p2 registry all name the installation they were built in.
//! Anything that is valid UTF-8 has those paths replaced by
//! `{{WM_HOME}}` and `{{PROFILE_DIR}}` on capture, and substituted back on
//! replay; anything else is carried byte for byte.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Placeholder for the installation root.
const WM_HOME_TOKEN: &str = "{{WM_HOME}}";

/// Placeholder for the profile directory.
const PROFILE_DIR_TOKEN: &str = "{{PROFILE_DIR}}";

/// Placeholder for the profile's own name.
///
/// `config.ini` names the profile by *relative* path as well as absolute —
/// `osgi.framework.extensions` reaches its hook bundles through
/// `../../../../../../profiles/SPM/plugins/…`. Tokenising only absolute paths
/// leaves a replayed profile loading another profile's framework extensions.
const PROFILE_NAME_TOKEN: &str = "{{PROFILE_NAME}}";

/// Directories carried verbatim, relative to the profile.
const CARRIED: &[&str] = &["configuration", "bin", "dropins", "templates", "p2"];

/// Files that belong to a *run* of the source profile, not to the profile.
///
/// A lock or a wrapper anchor makes a replayed profile look like it is already
/// running, and the launcher then refuses to start; the pid and status files
/// describe a process on another machine.
const RUNTIME_STATE: &[&str] = &["bin/.lock", "bin/wrapper.anchor", "bin/shutdown.anchor"];

/// Suffixes of per-run files, matched anywhere.
const RUNTIME_SUFFIXES: &[&str] = &[".pid", ".status", ".java.status", ".lck"];

/// Subtrees the runtime regenerates, which must not be carried.
///
/// The OSGi caches in particular encode bundle ids and absolute locations from
/// the machine they were written on; replaying them produces a runtime that
/// starts against stale state.
const REGENERATED: &[&str] = &[
    "configuration/org.eclipse.osgi",
    "configuration/org.eclipse.core.runtime",
    "configuration/org.eclipse.equinox.app",
    // Only Tomcat's scratch area is regenerated. `conf/` and `resources/`
    // beside it are product content, and dropping them costs the profile its
    // `server.xml` — which Tomcat then replaces with a stock one carrying an
    // AJP connector the product never configures.
    "configuration/tomcat/work",
];

/// One bundle a profile runs, as listed in `bundles.info`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bundle {
    /// Symbolic name.
    pub name: String,
    /// Version.
    pub version: String,
    /// Jar file name, e.g. `com.example_1.0.0.jar`.
    pub jar: String,
    /// Location exactly as `bundles.info` records it.
    ///
    /// Most bundles are copied into the profile and read as `plugins/<jar>`,
    /// but a handful — the framework itself, the simple configurator, a few
    /// shared libraries — are referenced in place under
    /// `../../common/runtime/bundles/…`. Copying those into the profile changes
    /// its layout for no benefit, so the location is carried verbatim and
    /// decides whether the jar is copied at all.
    pub location: String,
    /// OSGi start level.
    pub start_level: String,
    /// Whether the framework starts it.
    pub started: String,
}

/// A captured profile: everything needed to lay it down elsewhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Profile name, e.g. `SPM`.
    pub name: String,
    /// Installation the capture was taken from.
    pub source_home: String,
    /// Bundles, in `bundles.info` order.
    pub bundles: Vec<Bundle>,
    /// Every jar the profile's `plugins/` holds.
    ///
    /// This is not the same set as [`Manifest::bundles`]: the framework itself,
    /// its extensions, the launcher and the bootstrap hooks sit in `plugins/`
    /// but are named by `config.ini` rather than by `bundles.info`. Replaying
    /// only what `bundles.info` lists leaves a profile that cannot start.
    pub plugins: Vec<String>,
    /// Files carried, relative to the profile directory.
    pub files: Vec<String>,
    /// Files whose text had installation paths replaced by placeholders.
    pub tokenised: Vec<String>,
    /// Files that were executable in the source profile.
    ///
    /// Recorded rather than guessed: making every carried file executable is
    /// untidy, and missing the wrapper launcher makes the profile unstartable.
    #[serde(default)]
    pub executable: Vec<String>,
}

/// Capture the profile `name` from `wm_home` into the archive at `output`.
pub fn capture(wm_home: &Path, name: &str, output: &Path) -> Result<Manifest> {
    let profile_dir = wm_home.join("profiles").join(name);
    if !profile_dir.join("configuration").is_dir() {
        return Err(Error::NotFound {
            what: "p2 profile",
            path: profile_dir,
        });
    }
    let bundles = read_bundles(&profile_dir)?;
    let plugins = list_plugins(&profile_dir);

    let home_text = wm_home.display().to_string();
    let profile_text = profile_dir.display().to_string();
    let relative_profile = format!("profiles/{name}/");

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let file = fs::File::create(output).map_err(|e| Error::io(output, e))?;
    let mut zip = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut files = Vec::new();
    let mut tokenised = Vec::new();
    let mut executable = Vec::new();

    let mut roots: Vec<PathBuf> = CARRIED.iter().map(|d| profile_dir.join(d)).collect();
    // Root-level files such as eclipse.ini and artifacts.xml.
    for entry in fs::read_dir(&profile_dir).map_err(|e| Error::io(&profile_dir, e))? {
        let path = entry.map_err(|e| Error::io(&profile_dir, e))?.path();
        if path.is_file() {
            roots.push(path);
        }
    }

    for root in roots {
        if !root.exists() {
            continue;
        }
        for path in walk(&root)? {
            let Ok(relative) = path.strip_prefix(&profile_dir) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            let regenerated = REGENERATED
                .iter()
                .any(|r| relative == *r || relative.starts_with(&format!("{r}/")));
            let per_run = RUNTIME_STATE.contains(&relative.as_str())
                || RUNTIME_SUFFIXES
                    .iter()
                    .any(|suffix| relative.ends_with(suffix));
            if regenerated || per_run {
                continue;
            }
            let bytes = fs::read(&path).map_err(|e| Error::io(&path, e))?;
            let (payload, was_text) = match String::from_utf8(bytes.clone()) {
                Ok(text) if text.contains(&home_text) || text.contains(&relative_profile) => {
                    // Longest first: the profile directory extends the
                    // installation root, and the relative form is a suffix of
                    // the absolute one.
                    let replaced = text
                        .replace(&profile_text, PROFILE_DIR_TOKEN)
                        .replace(&home_text, WM_HOME_TOKEN)
                        .replace(
                            &relative_profile,
                            &format!("profiles/{PROFILE_NAME_TOKEN}/"),
                        );
                    (replaced.into_bytes(), true)
                }
                _ => (bytes, false),
            };
            zip.start_file(format!("files/{relative}"), options)
                .map_err(|e| Error::Exec(format!("cannot add {relative}: {e}")))?;
            zip.write_all(&payload)
                .map_err(|e| Error::Exec(format!("cannot write {relative}: {e}")))?;
            if is_executable(&path) {
                executable.push(relative.clone());
            }
            files.push(relative.clone());
            if was_text {
                tokenised.push(relative);
            }
        }
    }

    let manifest = Manifest {
        name: name.to_string(),
        source_home: home_text,
        bundles,
        plugins,
        files,
        tokenised,
        executable,
    };
    let json = serde_json::to_string_pretty(&manifest).map_err(|e| {
        Error::Exec(format!(
            "cannot serialise the manifest for {}: {e}",
            output.display()
        ))
    })?;
    zip.start_file("manifest.json", options).map_err(|e| {
        Error::Exec(format!(
            "cannot add the manifest to {}: {e}",
            output.display()
        ))
    })?;
    zip.write_all(json.as_bytes()).map_err(|e| {
        Error::Exec(format!(
            "cannot write the manifest to {}: {e}",
            output.display()
        ))
    })?;
    zip.finish()
        .map_err(|e| Error::Exec(format!("cannot finish {}: {e}", output.display())))?;
    Ok(manifest)
}

/// What replaying a capture produced.
#[derive(Debug, Clone, Serialize)]
pub struct Replayed {
    /// Where the profile was written.
    pub path: PathBuf,
    /// Configuration files laid down.
    pub files: usize,
    /// Bundles copied from the installation's own repositories.
    pub bundles: usize,
    /// Bundles left referenced in place rather than copied.
    pub referenced_bundles: usize,
    /// Bundles the capture names that the installation does not carry.
    pub missing_bundles: Vec<String>,
    /// Anything the caller should know.
    pub warnings: Vec<String>,
}

/// Replay a capture onto `wm_home`.
///
/// Bundles are resolved from the target's own `common/runtime/bundles`, so the
/// products must already be installed. A capture that names a bundle the target
/// does not have is reported rather than guessed at: the resulting profile would
/// not start, and saying so is more useful than a partial one.
pub fn replay(
    capture: &Path,
    wm_home: &Path,
    name: Option<&str>,
    dry_run: bool,
) -> Result<Replayed> {
    let file = fs::File::open(capture).map_err(|e| Error::io(capture, e))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| Error::Exec(format!("{} is not a capture: {e}", capture.display())))?;

    let manifest: Manifest = {
        let mut entry = zip
            .by_name("manifest.json")
            .map_err(|e| Error::Exec(format!("no manifest in {}: {e}", capture.display())))?;
        let mut text = String::new();
        entry.read_to_string(&mut text).map_err(|e| {
            Error::Exec(format!("manifest of {} unreadable: {e}", capture.display()))
        })?;
        serde_json::from_str(&text)
            .map_err(|e| Error::Malformed(format!("manifest of {}: {e}", capture.display())))?
    };

    let name = name.unwrap_or(&manifest.name).to_string();
    let profile_dir = wm_home.join("profiles").join(&name);
    let mut warnings = Vec::new();
    if profile_dir.exists() {
        warnings.push(format!(
            "{} already exists; files are overwritten",
            profile_dir.display()
        ));
    }

    // Where the target keeps its bundles.
    let available = index_bundles(wm_home);
    let mut missing = Vec::new();
    let mut copied = 0usize;
    let referenced = manifest
        .bundles
        .iter()
        .filter(|b| !b.location.starts_with("plugins/"))
        .count();

    // Copy what the profile's plugins directory held, not merely what
    // bundles.info lists: the framework, its extensions and the launcher live
    // there and are named by config.ini instead.
    let mut wanted: Vec<&String> = manifest.plugins.iter().collect();
    if wanted.is_empty() {
        // A capture from an older build recorded only the bundle list.
        wanted = manifest
            .bundles
            .iter()
            .filter(|b| b.location.starts_with("plugins/"))
            .map(|b| &b.jar)
            .collect();
    }
    for jar in wanted {
        match available.get(jar) {
            Some(source) => {
                if !dry_run {
                    let target = profile_dir.join("plugins").join(jar);
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
                    }
                    fs::copy(source, &target).map_err(|e| Error::io(&target, e))?;
                }
                copied += 1;
            }
            None => missing.push(jar.clone()),
        }
    }

    let home_text = wm_home.display().to_string();
    let profile_text = profile_dir.display().to_string();
    let mut written = 0usize;
    if !dry_run {
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index).map_err(|e| {
                Error::Exec(format!(
                    "cannot read entry {index} of {}: {e}",
                    capture.display()
                ))
            })?;
            let entry_name = entry.name().to_string();
            let Some(relative) = entry_name.strip_prefix("files/") else {
                continue;
            };
            let Some(safe) = safe_relative(relative) else {
                return Err(Error::Exec(format!(
                    "capture entry escapes the profile: {relative:?}"
                )));
            };
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(|e| {
                Error::Exec(format!(
                    "cannot read {relative} from {}: {e}",
                    capture.display()
                ))
            })?;
            if manifest.tokenised.iter().any(|t| t == relative) {
                if let Ok(text) = String::from_utf8(bytes.clone()) {
                    bytes = text
                        .replace(PROFILE_DIR_TOKEN, &profile_text)
                        .replace(WM_HOME_TOKEN, &home_text)
                        .replace(PROFILE_NAME_TOKEN, &name)
                        .into_bytes();
                }
            }
            let target = profile_dir.join(&safe);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
            }
            fs::write(&target, &bytes).map_err(|e| Error::io(&target, e))?;
            if manifest.executable.iter().any(|e| e == relative) {
                set_executable(&target);
            }
            written += 1;
        }

        // bundles.info is regenerated rather than carried: it names jar
        // locations, and the capture is the authority on the bundle set.
        write_bundles_info(&profile_dir, &manifest.bundles)?;
    } else {
        written = manifest.files.len();
    }

    if !missing.is_empty() {
        warnings.push(format!(
            "{} bundle(s) named by the capture are not in this installation; install the \
             products they belong to before starting the profile",
            missing.len()
        ));
    }

    Ok(Replayed {
        path: profile_dir,
        files: written,
        bundles: copied,
        referenced_bundles: referenced,
        missing_bundles: missing,
        warnings,
    })
}

/// Read `bundles.info`.
fn read_bundles(profile_dir: &Path) -> Result<Vec<Bundle>> {
    let path = profile_dir
        .join("configuration")
        .join("org.eclipse.equinox.simpleconfigurator")
        .join("bundles.info");
    let text = fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
    Ok(parse_bundles(&text))
}

/// Parse `name,version,location,startLevel,started` lines.
pub fn parse_bundles(text: &str) -> Vec<Bundle> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').collect();
            let [name, version, location, start_level, started] = fields[..] else {
                return None;
            };
            Some(Bundle {
                name: name.to_string(),
                version: version.to_string(),
                jar: location.rsplit('/').next().unwrap_or(location).to_string(),
                location: location.to_string(),
                start_level: start_level.to_string(),
                started: started.to_string(),
            })
        })
        .collect()
}

/// Render `bundles.info` for a bundle set.
pub fn render_bundles_info(bundles: &[Bundle]) -> String {
    let mut text = String::from("#encoding=UTF-8\n#version=1\n");
    for b in bundles {
        text.push_str(&format!(
            "{},{},{},{},{}\n",
            b.name, b.version, b.location, b.start_level, b.started
        ));
    }
    text
}

fn write_bundles_info(profile_dir: &Path, bundles: &[Bundle]) -> Result<()> {
    let dir = profile_dir
        .join("configuration")
        .join("org.eclipse.equinox.simpleconfigurator");
    fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
    let path = dir.join("bundles.info");
    fs::write(&path, render_bundles_info(bundles)).map_err(|e| Error::io(&path, e))
}

/// Jar file names in a profile's `plugins/`.
fn list_plugins(profile_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(profile_dir.join("plugins")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every bundle jar the installation carries, by file name.
fn index_bundles(wm_home: &Path) -> BTreeMap<String, PathBuf> {
    let mut index = BTreeMap::new();
    let bundles = wm_home.join("common").join("runtime").join("bundles");
    let Ok(groups) = fs::read_dir(&bundles) else {
        return index;
    };
    for group in groups.flatten() {
        let plugins = group.path().join("eclipse").join("plugins");
        let Ok(entries) = fs::read_dir(&plugins) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                // Several groups ship the same jar; any copy will do.
                index
                    .entry(entry.file_name().to_string_lossy().into_owned())
                    .or_insert(path);
            }
        }
    }
    index
}

/// Every file under `root`, or `root` itself when it is a file.
fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    if root.is_file() {
        return Ok(vec![root.to_path_buf()]);
    }
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(path),
                Ok(kind) if kind.is_file() => out.push(path),
                _ => {}
            }
        }
    }
    out.sort();
    Ok(out)
}

fn safe_relative(entry: &str) -> Option<PathBuf> {
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

/// Whether a file carries the owner-execute bit.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o100 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "bat" || e == "exe")
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "#encoding=UTF-8\n\
                        #version=1\n\
                        angus-activation,2.0.3,plugins/angus-activation_2.0.3.jar,4,true\n\
                        \n\
                        avro,1.11.5,plugins/avro_1.11.5.jar,4,false\n\
                        org.eclipse.osgi,3.23.100,../../common/runtime/bundles/platform/eclipse/plugins/org.eclipse.osgi_3.23.100.jar,-1,true\n";

    #[test]
    fn parses_and_renders_the_bundle_list() {
        let bundles = parse_bundles(INFO);
        assert_eq!(bundles.len(), 3);
        assert_eq!(bundles[0].name, "angus-activation");
        assert_eq!(bundles[0].jar, "angus-activation_2.0.3.jar");
        assert_eq!(bundles[1].started, "false");

        let rendered = render_bundles_info(&bundles);
        // A round trip must be stable, since replay rewrites this file.
        assert_eq!(parse_bundles(&rendered), bundles);
        assert!(rendered.starts_with("#encoding=UTF-8\n#version=1\n"));
    }

    #[test]
    fn a_referenced_bundle_keeps_its_location() {
        let bundles = parse_bundles(INFO);
        let framework = bundles
            .iter()
            .find(|b| b.name == "org.eclipse.osgi")
            .expect("framework");
        assert!(framework
            .location
            .starts_with("../../common/runtime/bundles/"));
        // Rendering must not rewrite it into the profile.
        assert!(render_bundles_info(&bundles).contains(&framework.location));
    }

    #[test]
    fn a_malformed_line_is_skipped_not_guessed() {
        assert!(parse_bundles("only,three,fields\n").is_empty());
    }

    #[test]
    fn the_relative_profile_reference_is_tokenised_too() {
        let home = "/opt/wm";
        let profile = "/opt/wm/profiles/SPM";
        let relative = "profiles/SPM/";
        // As it appears in config.ini's osgi.framework.extensions.
        let text = "ext=reference\\:file\\:../../../../../../profiles/SPM/plugins/hook.jar\n\
                    root=/opt/wm/profiles/SPM/configuration\n";
        let captured = text
            .replace(profile, PROFILE_DIR_TOKEN)
            .replace(home, WM_HOME_TOKEN)
            .replace(relative, &format!("profiles/{PROFILE_NAME_TOKEN}/"));
        assert!(
            !captured.contains("profiles/SPM/"),
            "no source name may survive: {captured}"
        );

        let replayed = captured
            .replace(PROFILE_DIR_TOKEN, "/srv/wm/profiles/SPM2")
            .replace(WM_HOME_TOKEN, "/srv/wm")
            .replace(PROFILE_NAME_TOKEN, "SPM2");
        assert!(replayed.contains("../../../../../../profiles/SPM2/plugins/hook.jar"));
        assert!(replayed.contains("root=/srv/wm/profiles/SPM2/configuration"));
    }

    #[test]
    fn capture_entries_cannot_escape_the_profile() {
        assert!(safe_relative("../../etc/passwd").is_none());
        assert!(safe_relative("/etc/passwd").is_none());
        assert_eq!(
            safe_relative("configuration/config.ini"),
            Some(PathBuf::from("configuration/config.ini"))
        );
    }

    #[test]
    fn per_run_state_is_not_carried() {
        let excluded =
            |p: &str| RUNTIME_STATE.contains(&p) || RUNTIME_SUFFIXES.iter().any(|s| p.ends_with(s));
        // A lock or anchor would make the replayed profile look already running.
        assert!(excluded("bin/.lock"));
        assert!(excluded("bin/wrapper.anchor"));
        assert!(excluded("bin/sagmws121_default_1.pid"));
        assert!(excluded("bin/sagmws121_default_1.java.status"));
        // The launcher itself is not state.
        assert!(!excluded("bin/sagmws121_default_1"));
        assert!(!excluded("configuration/config.ini"));
    }

    #[test]
    fn regenerated_subtrees_are_recognised() {
        let skipped = |p: &str| {
            REGENERATED
                .iter()
                .any(|r| p == *r || p.starts_with(&format!("{r}/")))
        };
        assert!(skipped("configuration/org.eclipse.osgi"));
        assert!(skipped("configuration/org.eclipse.osgi/414/data/x"));
        assert!(!skipped(
            "configuration/org.eclipse.equinox.simpleconfigurator/bundles.info"
        ));
        assert!(!skipped("configuration/config.ini"));
    }

    #[test]
    fn the_longer_path_is_tokenised_first() {
        // The profile directory extends the installation root, so replacing the
        // root first would leave "{{WM_HOME}}/profiles/SPM" and the profile
        // token would never match.
        let home = "/opt/wm";
        let profile = "/opt/wm/profiles/SPM";
        let text = "a=/opt/wm/profiles/SPM/configuration\nb=/opt/wm/common\n";
        let out = text
            .replace(profile, PROFILE_DIR_TOKEN)
            .replace(home, WM_HOME_TOKEN);
        assert!(out.contains("a={{PROFILE_DIR}}/configuration"));
        assert!(out.contains("b={{WM_HOME}}/common"));
    }
}

/// Provision a profile with the product's own p2 director.
///
/// The director, its launcher and a JVM all ship with the installation, and
/// nothing about running them needs a profile to exist first — the launcher
/// lives in `common/runtime/bundles/platform/eclipse/plugins`. So this is the
/// supported way to create a profile, and it is the default here.
///
/// It costs about thirty seconds. That is the price of a profile whose p2
/// registry IBM's own tooling will still recognise, and it is worth paying.
pub mod director {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use serde::Serialize;

    use crate::error::{Error, Result};

    /// A director invocation, before it is run.
    #[derive(Debug, Clone, Serialize)]
    pub struct Invocation {
        pub java: PathBuf,
        pub launcher: PathBuf,
        pub args: Vec<String>,
    }

    impl Invocation {
        pub fn display(&self) -> String {
            format!(
                "{} -jar {} {}",
                self.java.display(),
                self.launcher.display(),
                self.args.join(" ")
            )
        }
    }

    /// Find the launcher of a *bootable* Eclipse, which is not the same thing
    /// as finding a launcher jar.
    ///
    /// `common/runtime/bundles/platform/eclipse/plugins` holds the launcher and
    /// the director, but that directory is a p2 repository, not an
    /// installation: it has no `configuration/config.ini`, and running from it
    /// fails with "Unable to acquire application service". The installer runs
    /// p2 from its own bootstrap profile at `install/profile`, and so does
    /// this.
    pub fn launcher(wm_home: &Path) -> Result<PathBuf> {
        let plugins = wm_home.join("install").join("profile").join("plugins");
        if !plugins
            .join("..")
            .join("configuration")
            .join("config.ini")
            .is_file()
        {
            return Err(Error::Malformed(format!(
                "no bootable p2 runtime at {}; the installer's own profile is what runs the \
                 director, and this installation does not have one",
                wm_home.join("install").join("profile").display()
            )));
        }
        let mut found: Vec<PathBuf> = std::fs::read_dir(&plugins)
            .map_err(|e| Error::io(plugins.clone(), e))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with("org.eclipse.equinox.launcher_") && n.ends_with(".jar")
                })
            })
            .collect();
        found.sort();
        found.pop().ok_or_else(|| {
            Error::Malformed(format!("no Equinox launcher under {}", plugins.display()))
        })
    }

    /// Every p2 repository the installation ships, as `file:` URLs.
    pub fn repositories(wm_home: &Path) -> Result<Vec<String>> {
        let root = wm_home.join("common").join("runtime").join("bundles");
        let mut out = Vec::new();
        for group in std::fs::read_dir(&root)
            .map_err(|e| Error::io(root.clone(), e))?
            .flatten()
        {
            let eclipse = group.path().join("eclipse");
            if eclipse.join("content.xml").is_file() {
                out.push(format!("file:{}", eclipse.display()));
            }
        }
        out.sort();
        Ok(out)
    }

    /// Build the director invocation that provisions `roots` into `destination`.
    pub fn invocation(
        wm_home: &Path,
        destination: &Path,
        profile: &str,
        roots: &[String],
        env: &crate::resolve::Environment,
    ) -> Result<Invocation> {
        let java = wm_home.join("jvm").join("jvm").join("bin").join("java");
        if !java.is_file() {
            return Err(Error::Malformed(format!(
                "no JVM at {}; the director cannot run",
                java.display()
            )));
        }
        let units: Vec<String> = roots
            .iter()
            .map(|r| {
                if r.ends_with(".feature.group") {
                    r.clone()
                } else {
                    format!("{r}.feature.group")
                }
            })
            .collect();
        Ok(Invocation {
            java,
            launcher: launcher(wm_home)?,
            args: vec![
                "-application".into(),
                "org.eclipse.equinox.p2.director".into(),
                "-repository".into(),
                repositories(wm_home)?.join(","),
                "-installIU".into(),
                units.join(","),
                "-destination".into(),
                destination.display().to_string(),
                "-profile".into(),
                profile.to_string(),
                "-profileProperties".into(),
                "org.eclipse.update.install.features=true".into(),
                "-p2.os".into(),
                env.os.clone(),
                "-p2.ws".into(),
                env.ws.clone(),
                "-p2.arch".into(),
                env.arch.clone(),
                "-roaming".into(),
            ],
        })
    }

    /// Run it, returning whether it succeeded and what it said.
    pub fn run(invocation: &Invocation) -> Result<(bool, String)> {
        let output = Command::new(&invocation.java)
            .arg("-jar")
            .arg(&invocation.launcher)
            .args(&invocation.args)
            .output()
            .map_err(|e| {
                Error::Exec(format!(
                    "cannot run the p2 director ({}): {e}",
                    invocation.display()
                ))
            })?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok((output.status.success(), text))
    }
}
