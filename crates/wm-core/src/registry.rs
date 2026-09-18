//! The installations this machine knows about.
//!
//! Every tool took a `wm_home` and forgot it. In a session of any length that
//! means the same absolute path typed thirty times, the release and platform it
//! was built from re-stated at each planning call, and no way at all to ask
//! "what is installed where" without being told where to look first.
//!
//! A record here is deliberately thin, because most of what you want to know
//! about an installation is already on its own disk. `install/products/*.prop`
//! is authoritative for what is installed, and it stays correct when a fix is
//! applied or someone runs the shipped wizard behind our back. Copying it into
//! a registry would produce a second answer that drifts.
//!
//! So the registry holds what the disk does *not* say:
//!
//! * a short name, so a call can say `install: "b2b"` instead of a path;
//! * the release, platform and download centre it was built from — needed to
//!   plan an addition, and unrecoverable from the installation afterwards;
//! * the selection that was *asked for*, as distinct from the closure that was
//!   installed, which is what you need to replay or extend it;
//! * where the things that are not inside it live: the Update Manager home, the
//!   installer binary, its jar.
//!
//! and one snapshot of the component list, taken when the record was written,
//! marked with its date. A snapshot answers "what is in that host" when the
//! path is not mounted; when the path is readable the live read wins and the
//! drift between the two is reported rather than hidden.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{installs_dir, write_private};
use crate::inventory::Inventory;
use crate::{Error, Result};

/// What was on disk when the record was last refreshed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// When it was taken, RFC 3339.
    pub taken_at: String,
    /// Versioned product paths, as `install/products/*.prop` gave them.
    pub products: Vec<String>,
    /// Integration Server instances and platform profiles, as `kind:name`.
    pub runtimes: Vec<String>,
    /// Fix readmes found on disk.
    pub fixes: Vec<String>,
}

impl Snapshot {
    /// Take a snapshot of `inventory`.
    pub fn of(inventory: &Inventory) -> Self {
        Self {
            taken_at: now(),
            products: inventory.products.iter().map(|p| p.path.clone()).collect(),
            runtimes: inventory
                .runtimes
                .iter()
                .map(|r| format!("{}:{}", r.kind, r.name))
                .collect(),
            fixes: inventory.fixes.iter().map(|f| f.readme.clone()).collect(),
        }
    }
}

/// One installation this machine knows about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Install {
    /// The handle: what a tool call says instead of a path.
    pub name: String,
    /// The installation root.
    pub wm_home: PathBuf,
    /// Release it was built from, e.g. `12.1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// Platform code, e.g. `LNXAMD64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Download centre it was fetched from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Update Manager home that patches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sum_home: Option<PathBuf>,
    /// The shipped installer binary used against it, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installer_bin: Option<PathBuf>,
    /// `sagInstaller.jar`, for laying down `install/jars/DistMan.jar`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installer_jar: Option<PathBuf>,
    /// What the operator asked for, before the dependency closure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested: Vec<String>,
    /// Anything the operator wants to remember about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// When the record was first written, RFC 3339.
    pub registered_at: String,
    /// The last job that changed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_job: Option<String>,
    /// The component list as of the last refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
}

impl Install {
    /// A new record for `name` at `wm_home`.
    pub fn new(name: &str, wm_home: &Path) -> Result<Self> {
        Ok(Self {
            name: valid_name(name)?,
            wm_home: wm_home.to_path_buf(),
            release: None,
            platform: None,
            host: None,
            sum_home: None,
            installer_bin: None,
            installer_jar: None,
            requested: Vec::new(),
            notes: None,
            registered_at: now(),
            last_job: None,
            snapshot: None,
        })
    }

    /// Read the installation and store the result as the snapshot.
    ///
    /// Returns the inventory so the caller can report it without reading twice.
    pub fn refresh(&mut self) -> Result<Inventory> {
        let inventory = Inventory::read(&self.wm_home)?;
        self.snapshot = Some(Snapshot::of(&inventory));
        Ok(inventory)
    }

    /// Where this record is stored.
    pub fn path(&self) -> PathBuf {
        installs_dir().join(format!("{}.json", self.name))
    }

    /// Write the record.
    pub fn save(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(|e| {
            Error::Malformed(format!("cannot serialise install {}: {e}", self.name))
        })?;
        write_private(&self.path(), text.as_bytes())
    }
}

/// Read one record by name.
pub fn get(name: &str) -> Result<Install> {
    let name = valid_name(name)?;
    let path = installs_dir().join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            let known = list().unwrap_or_default();
            Error::Malformed(if known.is_empty() {
                format!("no installation named {name:?} is registered, and none are")
            } else {
                format!(
                    "no installation named {name:?} is registered; known: {}",
                    known
                        .iter()
                        .map(|i| i.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        } else {
            Error::io(&path, e)
        }
    })?;
    serde_json::from_str(&text)
        .map_err(|e| Error::Malformed(format!("{} is not an install record: {e}", path.display())))
}

/// Every record, by name.
///
/// A record that will not parse is skipped rather than failing the listing: one
/// corrupt file should not hide the rest.
pub fn list() -> Result<Vec<Install>> {
    let dir = installs_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(&dir, e)),
    };
    let mut installs: Vec<Install> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect();
    installs.sort_by(|a: &Install, b: &Install| a.name.cmp(&b.name));
    Ok(installs)
}

/// Forget a record. Returns whether there was one. The installation on disk is
/// not touched: this removes a note about it, nothing more.
pub fn forget(name: &str) -> Result<bool> {
    let name = valid_name(name)?;
    let path = installs_dir().join(format!("{name}.json"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io(&path, e)),
    }
}

/// Find the record covering `wm_home`, if one is registered.
///
/// Used to attach a job to the installation it changed without making the
/// caller name it twice.
pub fn find_by_home(wm_home: &Path) -> Option<Install> {
    let wanted = std::fs::canonicalize(wm_home).unwrap_or_else(|_| wm_home.to_path_buf());
    list()
        .ok()?
        .into_iter()
        .find(|i| std::fs::canonicalize(&i.wm_home).unwrap_or_else(|_| i.wm_home.clone()) == wanted)
}

/// Resolve `name_or_path` to an installation root.
///
/// The one place the two ways of naming an installation meet: a registered name
/// wins, anything containing a separator is a path, and a bare word that is
/// neither is an error that lists the names that would have worked. Guessing —
/// treating an unknown name as a relative path — produces "not found at ./b2b",
/// which sends the reader looking for a directory instead of a typo.
pub fn resolve(name_or_path: &str) -> Result<PathBuf> {
    let looks_like_a_path =
        name_or_path.contains(std::path::MAIN_SEPARATOR) || name_or_path.starts_with('~');
    if looks_like_a_path {
        return Ok(PathBuf::from(name_or_path));
    }
    get(name_or_path).map(|i| i.wm_home)
}

/// Reject names that would escape the directory or collide with its layout.
fn valid_name(name: &str) -> Result<String> {
    let name = name.trim();
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.starts_with('.');
    if ok {
        Ok(name.to_string())
    } else {
        Err(Error::Malformed(format!(
            "{name:?} is not a usable install name: letters, digits, '-', '_' and '.', not \
             starting with '.', at most 64 characters"
        )))
    }
}

/// The current time, RFC 3339, seconds resolution.
fn now() -> String {
    time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_may_not_escape_the_directory() {
        for bad in ["../etc/passwd", "a/b", ".hidden", "", "with space"] {
            assert!(valid_name(bad).is_err(), "{bad:?} was accepted");
        }
        for good in ["b2b", "wm12.1", "demo_2", "a-b"] {
            assert!(valid_name(good).is_ok(), "{good:?} was rejected");
        }
    }

    #[test]
    fn a_path_resolves_to_itself() {
        assert_eq!(
            resolve("/opt/webmethods").unwrap(),
            PathBuf::from("/opt/webmethods")
        );
    }

    #[test]
    fn an_unknown_name_is_not_treated_as_a_relative_path() {
        let err = resolve("definitely-not-registered").unwrap_err();
        assert!(err.to_string().contains("registered"), "{err}");
    }

    #[test]
    fn now_is_rfc3339() {
        let stamp = now();
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert_eq!(stamp.len(), 20, "{stamp}");
    }
}
