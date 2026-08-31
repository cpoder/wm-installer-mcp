//! The product catalogue, read from an installed webMethods tree.
//!
//! `<WM_HOME>/install/products/<Component>.prop` is written by the installer for
//! every product it deployed. Each file is a flat `key=value` list keyed by the
//! product's own versioned path, of which four entries matter here:
//!
//! ```text
//! product=e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer
//! <path>/props/requiresRegexp=e2ei/11/.*/.*/SCGCommon,e2ei/11/.*/.*/PIECore
//! <path>/props/includeRegexp=e2ei/11/.*/.*/integrationServer:e2ei/11/.*/.*/TNSspm
//! <path>/props/sagProductCode=TNS
//! ```
//!
//! A reference installation is therefore a usable catalogue: it names the exact
//! versioned paths the installer expects in `InstallProducts`, which is the one
//! thing that cannot be guessed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Error, Result};

/// A versioned product path: `e2ei/11/<code>_<version>/<group>/<component>`.
///
/// This is the identifier the installer consumes in `InstallProducts`, and the
/// one `requiresRegexp` patterns are matched against.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ProductPath {
    /// The full path as it appears in the `.prop` file.
    pub raw: String,
    /// Product code and version, e.g. `TN_12.1.0.0.139`.
    pub release: String,
    /// Installer group, e.g. `TradingNetworks`.
    pub group: String,
    /// Component name, e.g. `TNServer`.
    pub component: String,
}

impl ProductPath {
    /// Parse a path, rejecting anything that is not five `/`-separated segments.
    pub fn parse(raw: &str) -> Result<Self> {
        let parts: Vec<&str> = raw.split('/').collect();
        let [_e2ei, _major, release, group, component] = parts[..] else {
            return Err(Error::Malformed(format!(
                "product path {raw:?} is not e2ei/<n>/<code>_<version>/<group>/<component>"
            )));
        };
        Ok(Self {
            raw: raw.to_string(),
            release: release.to_string(),
            group: group.to_string(),
            component: component.to_string(),
        })
    }

    /// The product code without its version, e.g. `TN` for `TN_12.1.0.0.139`.
    pub fn code(&self) -> &str {
        self.release
            .split_once('_')
            .map_or(self.release.as_str(), |(code, _)| code)
    }

    /// The version without its product code, e.g. `12.1.0.0.139`.
    pub fn version(&self) -> &str {
        self.release
            .split_once('_')
            .map_or("", |(_, version)| version)
    }
}

impl std::fmt::Display for ProductPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

/// One product as declared by its `.prop` file.
#[derive(Debug, Clone, Serialize)]
pub struct Product {
    /// Versioned installer path.
    pub path: ProductPath,
    /// Basename of the `.prop` file, which is the component name.
    pub prop_name: String,
    /// `requiresRegexp`: patterns this product's prerequisites must match.
    pub requires: Vec<String>,
    /// `requiresVersionRegexp`: version constraint parallel to `requires`,
    /// positionally aligned with it. Entries may be empty.
    pub requires_versions: Vec<String>,
    /// `productRequires`, which the installer treats as *overriding*
    /// `requiresRegexp` when present. Nothing in a 12.1 installation uses it;
    /// it is carried so that a catalogue that does can be flagged.
    pub product_requires: Option<String>,
    /// `includeRegexp`: products the installer offers to pull in alongside.
    pub includes: Vec<String>,
    /// `sagProductCode`, when declared.
    pub product_code: Option<String>,
    /// `containerProductId`, when declared — the product this one plugs into.
    pub container: Option<String>,
}

/// All products found in a reference installation.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    by_path: BTreeMap<String, Product>,
    source: PathBuf,
}

impl Catalog {
    /// Load `<wm_home>/install/products/*.prop`.
    ///
    /// Unparseable files are skipped rather than fatal: a partially migrated
    /// installation still yields a usable catalogue, and the caller can compare
    /// [`Catalog::len`] against the file count if it cares.
    pub fn load(wm_home: &Path) -> Result<Self> {
        let dir = wm_home.join("install").join("products");
        if !dir.is_dir() {
            return Err(Error::NotFound {
                what: "product catalog",
                path: dir,
            });
        }
        let mut by_path = BTreeMap::new();
        let entries = fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(&dir, e))?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "prop") {
                continue;
            }
            let text = fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
            let prop_name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some(product) = parse_prop(&text, prop_name) {
                by_path.insert(product.path.raw.clone(), product);
            }
        }
        Ok(Self {
            by_path,
            source: dir,
        })
    }

    /// Build a catalogue from already-parsed products. Useful in tests.
    pub fn from_products(products: impl IntoIterator<Item = Product>) -> Self {
        let by_path = products
            .into_iter()
            .map(|p| (p.path.raw.clone(), p))
            .collect();
        Self {
            by_path,
            source: PathBuf::new(),
        }
    }

    /// The directory this catalogue was read from.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Number of products.
    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    /// Whether the catalogue is empty.
    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    /// Look a product up by its full versioned path.
    pub fn get(&self, path: &str) -> Option<&Product> {
        self.by_path.get(path)
    }

    /// Iterate over every product, ordered by path.
    pub fn iter(&self) -> impl Iterator<Item = &Product> {
        self.by_path.values()
    }

    /// Every versioned path in the catalogue.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.by_path.keys().map(String::as_str)
    }

    /// Resolve a component name (the `.prop` basename, e.g. `TNServer`) to its
    /// versioned path. This is what lets a caller say "Trading Networks" and get
    /// `e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer` without knowing the build.
    pub fn path_of(&self, component: &str) -> Option<&ProductPath> {
        self.by_path
            .values()
            .find(|p| p.prop_name == component || p.path.component == component)
            .map(|p| &p.path)
    }
}

/// Extract the fields we care about from one `.prop` file.
///
/// Returns `None` when the file declares no `product=` line, which happens for
/// stubs the installer leaves behind.
fn parse_prop(text: &str, prop_name: String) -> Option<Product> {
    let mut product_path = None;
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if key == "product" {
            product_path = Some(value.to_string());
        } else if let Some((_, field)) = key.split_once("/props/") {
            // Keys are prefixed with the product's own path; the suffix after
            // `/props/` is the field name and is unique within a file.
            fields.insert(field, value);
        }
    }

    let raw = product_path?;
    let path = ProductPath::parse(&raw).ok()?;
    Some(Product {
        path,
        prop_name,
        requires: split_list(fields.get("requiresRegexp").copied(), ','),
        requires_versions: split_list_keep_empty(fields.get("requiresVersionRegexp").copied(), ','),
        product_requires: fields.get("productRequires").map(|s| s.to_string()),
        includes: split_list(fields.get("includeRegexp").copied(), ':'),
        product_code: fields.get("sagProductCode").map(|s| s.to_string()),
        container: fields.get("containerProductId").map(|s| s.to_string()),
    })
}

/// Like [`split_list`] but keeps empty entries, because `requiresVersionRegexp`
/// is positionally aligned with `requiresRegexp` and a hole means "no constraint".
fn split_list_keep_empty(value: Option<&str>, sep: char) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|v| v.split(sep))
        .map(|s| s.trim().to_string())
        .collect()
}

/// `requiresRegexp` is comma-separated, `includeRegexp` is colon-separated.
fn split_list(value: Option<&str>, sep: char) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|v| v.split(sep))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TN_PROP: &str = "\
product=e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer

e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/props/requiresRegexp=e2ei/11/.*/.*/SCGCommon,e2ei/11/.*/.*/PIECore
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/props/includeRegexp=e2ei/11/.*/.*/integrationServer:e2ei/11/.*/.*/TNSspm
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/props/sagProductCode=TNS
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/props/containerProductId=integrationServer
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/jars/x/props/md5=ignored
";

    #[test]
    fn parses_a_real_prop_file() {
        let p = parse_prop(TN_PROP, "TNServer".into()).expect("product line present");
        assert_eq!(p.path.component, "TNServer");
        assert_eq!(p.path.group, "TradingNetworks");
        assert_eq!(p.path.code(), "TN");
        assert_eq!(p.path.version(), "12.1.0.0.139");
        assert_eq!(
            p.requires,
            ["e2ei/11/.*/.*/SCGCommon", "e2ei/11/.*/.*/PIECore"]
        );
        assert_eq!(
            p.includes,
            ["e2ei/11/.*/.*/integrationServer", "e2ei/11/.*/.*/TNSspm"]
        );
        assert_eq!(p.product_code.as_deref(), Some("TNS"));
        assert!(p.product_requires.is_none());
        assert_eq!(p.container.as_deref(), Some("integrationServer"));
    }

    #[test]
    fn rejects_paths_with_the_wrong_shape() {
        assert!(ProductPath::parse("e2ei/11/TN_12.1/TradingNetworks").is_err());
        assert!(ProductPath::parse("").is_err());
    }

    #[test]
    fn a_prop_without_a_product_line_is_skipped() {
        assert!(parse_prop("# nothing here\nfoo=bar\n", "Stub".into()).is_none());
    }
}
