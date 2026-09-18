//! Where the servers keep what they must remember between calls.
//!
//! Both servers were stateless: every call carried its own `wm_home`, release,
//! platform and credentials, and anything learned during a call was forgotten
//! when it returned. That is fine for one call and wrong for a session — the
//! agent re-types the same installation path twenty times, re-discovers the
//! same product list, and the operator keeps an entitlement key in the client's
//! environment because there was nowhere else to put it.
//!
//! This module is the single place both servers ask where anything lives, and
//! it draws a line between two kinds of directory:
//!
//! * the **state directory** — `~/.wm-mcp`, moved by `WM_STATE_DIR` — is a
//!   cache. Fetched product trees, downloaded artifacts, job logs. Deleting it
//!   costs a download, nothing more, and it grows to gigabytes, so it is the
//!   directory people move onto a bigger disk or empty.
//! * the **config directory** — `~/.wm-mcp/config`, moved by `WM_CONFIG_DIR` —
//!   is not derivable from anywhere. Which installations exist, what each was
//!   built from, and the encrypted credential store. Losing it loses
//!   information no re-run recovers.
//!
//! The config directory deliberately does **not** follow `WM_STATE_DIR`. That
//! variable is routinely pointed at a scratch or repository-local path — this
//! project's own `.work/state` is one — and an entitlement key is a bearer
//! token: it must not follow a cache into a working tree or a tmpfs. Pinning
//! config under `$HOME` unless `WM_CONFIG_DIR` says otherwise keeps the two
//! decisions independent.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Config file holding the defaults, relative to [`config_dir`].
const DEFAULTS_FILE: &str = "config.json";

/// The user's home directory, or `/tmp` when the environment does not say.
fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Where durable configuration lives: `WM_CONFIG_DIR`, else `~/.wm-mcp/config`.
///
/// Note what this does not read: `WM_STATE_DIR`. See the module documentation.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("WM_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    home().join(".wm-mcp").join("config")
}

/// Where registered installations are recorded, one file each.
pub fn installs_dir() -> PathBuf {
    config_dir().join("installs")
}

/// Where the caches live: product trees, artifacts, job logs.
pub fn state_dir() -> PathBuf {
    std::env::var("WM_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".wm-mcp"))
}

/// Where jobs keep their logs.
///
/// The single definition for both servers. A job written somewhere the status
/// call does not look is a job that cannot be followed, and the two servers
/// disagreed about this whenever `WM_STATE_DIR` was set.
pub fn jobs_dir() -> PathBuf {
    std::env::var("WM_JOBS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| state_dir().join("jobs"))
}

/// Where fetched product trees are cached.
pub fn catalog_dir() -> PathBuf {
    state_dir().join("catalog")
}

/// Where downloaded artifacts are cached, per sandbox.
pub fn artifacts_dir() -> PathBuf {
    state_dir().join("artifacts")
}

/// Create `dir`, and on Unix make it readable only by its owner.
///
/// The config directory holds the credential store and the key that opens it;
/// a default umask would leave both group-readable.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    set_private(dir, 0o700)
}

/// Write `bytes` to `path` atomically, owner-only.
///
/// Two servers and any number of detached jobs share this directory, so a
/// half-written record is a real possibility: write beside the target and
/// rename, which is atomic within a filesystem.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temp, bytes).map_err(|e| Error::io(&temp, e))?;
    set_private(&temp, 0o600)?;
    std::fs::rename(&temp, path).map_err(|e| Error::io(path, e))?;
    Ok(())
}

/// Restrict `path` to its owner. A no-op where the platform has no Unix modes.
#[cfg(unix)]
fn set_private(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| Error::io(path, e))
}

#[cfg(not(unix))]
fn set_private(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Settings that would otherwise be repeated on every call.
///
/// Every field is optional and every one is overridden by an explicit argument.
/// The point is not to hide a value but to stop asking for it: a site installs
/// one release onto one platform from one download centre, and re-stating that
/// on each of forty calls is how the wrong platform ends up in a plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Defaults {
    /// Release to plan and install against, e.g. `12.1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// Platform code, e.g. `LNXAMD64`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Download centre host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The shipped installer binary, for the tools that drive it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installer_bin: Option<PathBuf>,
    /// `sagInstaller.jar`, laid down as `install/jars/DistMan.jar`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installer_jar: Option<PathBuf>,
    /// Registered installation to use when a call names none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<String>,
}

/// The names [`Defaults::set`] understands, in the order they are reported.
pub const DEFAULT_KEYS: &[&str] = &[
    "release",
    "platform",
    "host",
    "installer_bin",
    "installer_jar",
    "install",
];

impl Defaults {
    /// Read the stored defaults, or an empty set when none were ever written.
    pub fn load() -> Result<Self> {
        let path = config_dir().join(DEFAULTS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                Error::Malformed(format!("{} is not valid JSON: {e}", path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::io(&path, e)),
        }
    }

    /// Persist the defaults.
    pub fn save(&self) -> Result<()> {
        let path = config_dir().join(DEFAULTS_FILE);
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Malformed(format!("cannot serialise the defaults: {e}")))?;
        write_private(&path, text.as_bytes())
    }

    /// Read one default by name, as it appears in [`DEFAULT_KEYS`].
    pub fn get(&self, key: &str) -> Option<String> {
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
        match key {
            "release" => self.release.clone(),
            "platform" => self.platform.clone(),
            "host" => self.host.clone(),
            "installer_bin" => path(&self.installer_bin),
            "installer_jar" => path(&self.installer_jar),
            "install" => self.install.clone(),
            _ => None,
        }
    }

    /// Set one default by name; an empty value clears it.
    ///
    /// Returns an error naming the accepted keys rather than silently storing a
    /// setting no tool will ever read.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let value = value.trim();
        let text = (!value.is_empty()).then(|| value.to_string());
        let path = text.clone().map(PathBuf::from);
        match key {
            "release" => self.release = text,
            "platform" => self.platform = text,
            "host" => self.host = text,
            "installer_bin" => self.installer_bin = path,
            "installer_jar" => self.installer_jar = path,
            "install" => self.install = text,
            other => {
                return Err(Error::Malformed(format!(
                    "unknown setting {other:?}; known settings are {}",
                    DEFAULT_KEYS.join(", ")
                )))
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_round_trip() {
        let mut defaults = Defaults::default();
        defaults.set("platform", "LNXAMD64").unwrap();
        assert_eq!(defaults.get("platform").as_deref(), Some("LNXAMD64"));
    }

    #[test]
    fn an_empty_value_clears_a_setting() {
        let mut defaults = Defaults::default();
        defaults.set("release", "12.1").unwrap();
        defaults.set("release", "").unwrap();
        assert_eq!(defaults.get("release"), None);
    }

    #[test]
    fn an_unknown_setting_names_the_known_ones() {
        let err = Defaults::default().set("plaform", "x").unwrap_err();
        assert!(err.to_string().contains("platform"), "{err}");
    }

    #[test]
    fn every_documented_key_is_settable() {
        for key in DEFAULT_KEYS {
            Defaults::default()
                .set(key, "value")
                .unwrap_or_else(|e| panic!("{key} is documented but not settable: {e}"));
        }
    }

    #[test]
    fn the_config_directory_does_not_follow_the_state_directory() {
        // An entitlement key is a bearer token. `WM_STATE_DIR` gets pointed at
        // scratch paths and working trees; the credential store must not go
        // along for the ride. This test is the guard on that decision.
        let config = config_dir();
        assert!(
            !config.starts_with(state_dir()) || std::env::var_os("WM_STATE_DIR").is_none(),
            "config {} moved with WM_STATE_DIR",
            config.display()
        );
        assert!(!jobs_dir().starts_with(&config));
        assert!(!artifacts_dir().starts_with(&config));
    }
}
