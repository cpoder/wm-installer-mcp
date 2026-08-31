//! What is actually installed in a webMethods home.
//!
//! Everything here is read from the filesystem, so it works on a stopped
//! installation, needs no credentials, and cannot perturb anything. That makes
//! it the right first call before planning any change.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::catalog::Catalog;
use crate::{Error, Result};

/// A product found in the installation.
#[derive(Debug, Clone, Serialize)]
pub struct InstalledProduct {
    /// Component name, e.g. `TNServer`.
    pub component: String,
    /// Installer group, e.g. `TradingNetworks`.
    pub group: String,
    /// Product code, e.g. `TN`.
    pub code: String,
    /// Version, e.g. `12.1.0.0.139`.
    pub version: String,
    /// Full versioned path, usable directly in `InstallProducts`.
    pub path: String,
}

/// A fix whose readme the Update Manager left behind.
#[derive(Debug, Clone, Serialize)]
pub struct AppliedFix {
    /// Readme basename, which encodes product, release and fix number.
    pub readme: String,
    /// Where the readme lives — the component it was applied to.
    pub scope: String,
}

/// A runnable server instance or profile.
#[derive(Debug, Clone, Serialize)]
pub struct Runtime {
    /// What kind of runtime, e.g. `IntegrationServer` or `profile`.
    pub kind: String,
    /// Instance or profile name.
    pub name: String,
    /// Directory on disk.
    pub path: PathBuf,
}

/// A snapshot of an installation.
#[derive(Debug, Clone, Serialize)]
pub struct Inventory {
    /// The installation root.
    pub wm_home: PathBuf,
    /// Products, ordered by group then component.
    pub products: Vec<InstalledProduct>,
    /// Integration Server instances and platform profiles.
    pub runtimes: Vec<Runtime>,
    /// Fix readmes found on disk.
    pub fixes: Vec<AppliedFix>,
}

impl Inventory {
    /// Read an installation.
    pub fn read(wm_home: &Path) -> Result<Self> {
        if !wm_home.is_dir() {
            return Err(Error::NotFound {
                what: "installation",
                path: wm_home.to_path_buf(),
            });
        }
        let catalog = Catalog::load(wm_home)?;
        let mut products: Vec<InstalledProduct> = catalog
            .iter()
            .map(|p| InstalledProduct {
                component: p.path.component.clone(),
                group: p.path.group.clone(),
                code: p.path.code().to_string(),
                version: p.path.version().to_string(),
                path: p.path.raw.clone(),
            })
            .collect();
        products.sort_by(|a, b| (&a.group, &a.component).cmp(&(&b.group, &b.component)));

        Ok(Self {
            wm_home: wm_home.to_path_buf(),
            products,
            runtimes: read_runtimes(wm_home),
            fixes: read_fixes(wm_home),
        })
    }

    /// Products whose component or code contains `needle`, case-insensitively.
    pub fn find(&self, needle: &str) -> Vec<&InstalledProduct> {
        let needle = needle.to_lowercase();
        self.products
            .iter()
            .filter(|p| {
                p.component.to_lowercase().contains(&needle)
                    || p.code.to_lowercase().contains(&needle)
                    || p.group.to_lowercase().contains(&needle)
            })
            .collect()
    }
}

/// Integration Server instances live under `IntegrationServer/instances/<name>`,
/// platform runtimes under `profiles/<name>`. Both are directories next to a
/// handful of files, so filter to directories that look like runtimes.
fn read_runtimes(wm_home: &Path) -> Vec<Runtime> {
    let mut runtimes = Vec::new();
    let instances = wm_home.join("IntegrationServer").join("instances");
    for path in subdirectories(&instances) {
        // `logs` is bookkeeping, not an instance.
        let name = file_name(&path);
        if name == "logs" {
            continue;
        }
        runtimes.push(Runtime {
            kind: "IntegrationServer".into(),
            name,
            path,
        });
    }
    for path in subdirectories(&wm_home.join("profiles")) {
        let name = file_name(&path);
        runtimes.push(Runtime {
            kind: "profile".into(),
            name,
            path,
        });
    }
    runtimes.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
    runtimes
}

/// Update Manager drops a readme per fix. They are the only on-disk trace that
/// survives without asking Update Manager itself, so they are a useful
/// credential-free approximation of "what is patched here".
fn read_fixes(wm_home: &Path) -> Vec<AppliedFix> {
    let mut fixes = Vec::new();
    let mut scan = |dir: PathBuf, scope: &str| {
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "txt") {
                fixes.push(AppliedFix {
                    readme: file_name(&path),
                    scope: scope.to_string(),
                });
            }
        }
    };
    scan(wm_home.join("updateReadmes"), "suite");
    scan(
        wm_home.join("IntegrationServer").join("updateReadmes"),
        "IntegrationServer",
    );
    fixes.sort_by(|a, b| (&a.scope, &a.readme).cmp(&(&b.scope, &b.readme)));
    fixes
}

fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_home_is_an_error() {
        let err = Inventory::read(Path::new("/nonexistent/wm")).unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }));
    }

    #[test]
    fn find_matches_component_code_and_group() {
        let inventory = Inventory {
            wm_home: PathBuf::from("/opt/wm"),
            products: vec![InstalledProduct {
                component: "TNServer".into(),
                group: "TradingNetworks".into(),
                code: "TN".into(),
                version: "12.1".into(),
                path: "e2ei/11/TN_12.1/TradingNetworks/TNServer".into(),
            }],
            runtimes: Vec::new(),
            fixes: Vec::new(),
        };
        assert_eq!(inventory.find("tnserver").len(), 1);
        assert_eq!(inventory.find("trading").len(), 1);
        assert_eq!(inventory.find("mws").len(), 0);
    }
}
