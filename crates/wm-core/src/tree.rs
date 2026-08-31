//! The product tree served by the download centre.
//!
//! One flat `key=value` document describes an entire release for one platform:
//! every product, its prerequisites, and every downloadable artifact with its
//! size and digests. It is the same dialect as the `.prop` files an
//! installation carries, but keyed by a deeper path:
//!
//! ```text
//! e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/props/requiresRegexp=…
//! e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/children=TNServer-LNXAMD64-Any
//! e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/TNServer-LNXAMD64-Any/BM_TNSWmTN-ALL-Any/props/sha256=…
//! ```
//!
//! Parsing it removes the need for a reference installation: the exact
//! versioned product paths, which cannot be guessed, come straight from IBM.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::catalog::{Catalog, Product, ProductPath};
use crate::Result;

/// What kind of thing an artifact is.
///
/// The two live in different places in the repository and serve different
/// purposes: a module carries the product's files, a resource jar carries the
/// shipped installer's own panels and message bundles. A native installation
/// needs the first and not the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A module ("BM"), under `<release>/bms/<name>.zip`.
    Module,
    /// A resource jar, under `<release>/jars/<name>.jar`.
    ResourceJar,
}

/// One downloadable artifact.
#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    /// Module or resource jar.
    pub kind: ArtifactKind,
    /// Product this artifact belongs to.
    pub product: String,
    /// Platform variant node, e.g. `TNServer-LNXAMD64-Any`.
    pub variant: String,
    /// Artifact name, e.g. `BM_TNSWmTN-ALL-Any`.
    pub name: String,
    /// Path within the repository, ready for download.
    pub repository_path: String,
    /// Expected sha256, lowercase hex.
    pub sha256: Option<String>,
    /// Expected md5, lowercase hex.
    pub md5: Option<String>,
    /// Size on the wire.
    pub compressed_size: Option<u64>,
    /// Size once unpacked.
    pub expanded_size: Option<u64>,
    /// Artifact version.
    pub version: Option<String>,
}

/// A parsed product tree.
#[derive(Debug, Clone, Default)]
pub struct ProductTree {
    products: Vec<Product>,
    artifacts: Vec<Artifact>,
    /// Install panels declared per product, which this crate does not run.
    panels: BTreeMap<String, Vec<String>>,
    /// Every `props` field of every product, verbatim, so an installation can
    /// be made self-describing without re-deriving what IBM already said.
    raw: BTreeMap<String, BTreeMap<String, String>>,
}

impl ProductTree {
    /// Parse a tree document.
    pub fn parse(text: &str) -> Result<Self> {
        // node path -> field -> value, for `<node>/props/<field>=<value>` lines.
        let mut props: BTreeMap<&str, BTreeMap<&str, &str>> = BTreeMap::new();
        let mut panels: BTreeMap<String, Vec<String>> = BTreeMap::new();

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if let Some((node, field)) = key.rsplit_once("/props/") {
                props.entry(node).or_default().insert(field, value);
            } else if let Some(product) = key.strip_suffix("/panels") {
                panels.insert(
                    product.to_string(),
                    value
                        .split(',')
                        .map(str::trim)
                        .map(str::to_string)
                        .collect(),
                );
            }
        }

        let mut products = Vec::new();
        let mut artifacts = Vec::new();

        for (node, fields) in &props {
            let segments: Vec<&str> = node.split('/').collect();
            match segments.len() {
                // e2ei/11/<release>/<group>/<component>
                5 => {
                    if let Ok(path) = ProductPath::parse(node) {
                        let component = path.component.clone();
                        products.push(Product {
                            path,
                            prop_name: component,
                            requires: split(fields.get("requiresRegexp").copied(), ','),
                            requires_versions: split_keep_empty(
                                fields.get("requiresVersionRegexp").copied(),
                                ',',
                            ),
                            product_requires: fields.get("productRequires").map(|s| s.to_string()),
                            includes: split(fields.get("includeRegexp").copied(), ':'),
                            product_code: fields.get("sagProductCode").map(|s| s.to_string()),
                            container: fields.get("containerProductId").map(|s| s.to_string()),
                        });
                    }
                }
                // …/<component>/<variant>/<artifact> or …/<component>/jars/<jar>
                7 => {
                    // Only artifact nodes carry a digest; the variant node
                    // itself just lists jars.
                    if !fields.contains_key("sha256") && !fields.contains_key("md5") {
                        continue;
                    }
                    let product = segments[..5].join("/");
                    let release_prefix = segments[..3].join("/");
                    let kind = if segments[5] == "jars" {
                        ArtifactKind::ResourceJar
                    } else {
                        ArtifactKind::Module
                    };
                    let repository_path = match kind {
                        ArtifactKind::ResourceJar => {
                            crate::sdc::jar_path(&release_prefix, segments[6])
                        }
                        ArtifactKind::Module => {
                            let Some(path) = crate::sdc::artifact_path(node) else {
                                continue;
                            };
                            path
                        }
                    };
                    artifacts.push(Artifact {
                        kind,
                        product,
                        variant: segments[5].to_string(),
                        name: segments[6].to_string(),
                        repository_path,
                        sha256: fields.get("sha256").map(|s| s.to_lowercase()),
                        md5: fields.get("md5").map(|s| s.to_lowercase()),
                        compressed_size: fields.get("compressed_size").and_then(|s| s.parse().ok()),
                        expanded_size: fields.get("expanded_size").and_then(|s| s.parse().ok()),
                        version: fields.get("version").map(|s| s.to_string()),
                    });
                }
                _ => {}
            }
        }

        let raw = props
            .iter()
            .filter(|(node, _)| node.split('/').count() == 5)
            .map(|(node, fields)| {
                let owned = fields
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                (node.to_string(), owned)
            })
            .collect();
        Ok(Self {
            products,
            artifacts,
            panels,
            raw,
        })
    }

    /// Products, as a catalogue usable with [`crate::deps::resolve`].
    pub fn catalog(&self) -> Catalog {
        Catalog::from_products(self.products.iter().cloned())
    }

    /// Every artifact in the tree.
    pub fn artifacts(&self) -> &[Artifact] {
        &self.artifacts
    }

    /// Artifacts belonging to one product.
    pub fn artifacts_for(&self, product: &str) -> Vec<&Artifact> {
        self.artifacts
            .iter()
            .filter(|a| a.product == product)
            .collect()
    }

    /// Modules for a whole selection, deduplicated by repository path.
    ///
    /// Two products can name the same module; downloading it twice is waste, and
    /// unpacking it twice risks writing the same file from two sources. Resource
    /// jars are excluded: they exist for the shipped installer's own wizard.
    pub fn artifacts_for_selection<'a>(
        &'a self,
        products: impl IntoIterator<Item = &'a str>,
    ) -> Vec<&'a Artifact> {
        self.select(products, ArtifactKind::Module)
    }

    /// Artifacts of one kind for a selection, deduplicated by repository path.
    pub fn select<'a>(
        &'a self,
        products: impl IntoIterator<Item = &'a str>,
        kind: ArtifactKind,
    ) -> Vec<&'a Artifact> {
        let wanted: std::collections::BTreeSet<&str> = products.into_iter().collect();
        let mut seen = std::collections::BTreeSet::new();
        self.artifacts
            .iter()
            .filter(|a| a.kind == kind)
            .filter(|a| wanted.contains(a.product.as_str()))
            .filter(|a| seen.insert(a.repository_path.clone()))
            .collect()
    }

    /// Install panels declared by a product.
    ///
    /// These are Java classes the shipped installer runs at defined stages, and
    /// are the one part of an installation this crate cannot reproduce natively
    /// — see [`crate::install`].
    pub fn panels_for(&self, product: &str) -> &[String] {
        self.panels
            .get(product)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The declared `props` of one product, verbatim.
    pub fn props_for(&self, product: &str) -> Option<&BTreeMap<String, String>> {
        self.raw.get(product)
    }

    /// Number of products.
    pub fn product_count(&self) -> usize {
        self.products.len()
    }
}

fn split(value: Option<&str>, sep: char) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|v| v.split(sep))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn split_keep_empty(value: Option<&str>, sep: char) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|v| v.split(sep))
        .map(|s| s.trim().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = "\
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/props=requiresRegexp,sagProductCode
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/props/requiresRegexp=e2ei/11/*/*/PIECore
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/props/sagProductCode=TNS
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/children=TNServer-LNXAMD64-Any
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/panels=TNServerInstallPanel
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/TNServer-LNXAMD64-Any/props=jars
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/TNServer-LNXAMD64-Any/props/jars=TNSInstallPanels-ALL-Any
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/TNServer-LNXAMD64-Any/BM_TNSWmTN-ALL-Any/props/sha256=1751A6
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/TNServer-LNXAMD64-Any/BM_TNSWmTN-ALL-Any/props/md5=536D60
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/TNServer-LNXAMD64-Any/BM_TNSWmTN-ALL-Any/props/compressed_size=6537400
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/TNServer-LNXAMD64-Any/BM_TNSWmTN-ALL-Any/props/expanded_size=18666448
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/jars/TNSInstallMessages-ALL-Any/props/sha256=9E49DD
e2ei/11/IS_12.1.0.0.938/integrationServer/PIECore/props=sagProductCode
e2ei/11/IS_12.1.0.0.938/integrationServer/PIECore/props/sagProductCode=PIE
";

    #[test]
    fn parses_products_and_artifacts() {
        let tree = ProductTree::parse(TREE).expect("parse");
        assert_eq!(tree.product_count(), 2);
        assert_eq!(tree.artifacts().len(), 2, "one module and one resource jar");

        let artifact = tree
            .artifacts()
            .iter()
            .find(|a| a.kind == ArtifactKind::Module)
            .expect("module present");
        assert_eq!(artifact.name, "BM_TNSWmTN-ALL-Any");
        assert_eq!(artifact.variant, "TNServer-LNXAMD64-Any");
        assert_eq!(
            artifact.repository_path,
            "e2ei/11/TN_12.1.0.0.139/bms/BM_TNSWmTN-ALL-Any.zip"
        );
        // Digests are normalised so comparison never depends on case.
        assert_eq!(artifact.sha256.as_deref(), Some("1751a6"));
        assert_eq!(artifact.compressed_size, Some(6_537_400));
    }

    #[test]
    fn the_variant_node_is_not_mistaken_for_an_artifact() {
        let tree = ProductTree::parse(TREE).expect("parse");
        assert!(tree
            .artifacts()
            .iter()
            .all(|a| a.sha256.is_some() || a.md5.is_some()));
    }

    #[test]
    fn resource_jars_are_told_apart_and_addressed_differently() {
        let tree = ProductTree::parse(TREE).expect("parse");
        let jar = tree
            .artifacts()
            .iter()
            .find(|a| a.kind == ArtifactKind::ResourceJar)
            .expect("jar present");
        assert_eq!(
            jar.repository_path,
            "e2ei/11/TN_12.1.0.0.139/jars/TNSInstallMessages-ALL-Any.jar"
        );
        // A selection to install must not drag the wizard's own jars along.
        let selected =
            tree.artifacts_for_selection(["e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer"]);
        assert!(selected.iter().all(|a| a.kind == ArtifactKind::Module));
    }

    #[test]
    fn yields_a_catalog_the_resolver_can_use() {
        let tree = ProductTree::parse(TREE).expect("parse");
        let catalog = tree.catalog();
        let seeds = vec!["e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer".to_string()];
        let resolution = crate::deps::resolve(&catalog, &seeds, false).expect("closure");
        assert!(resolution.paths().iter().any(|p| p.ends_with("/PIECore")));
        assert!(resolution.is_complete());
    }

    #[test]
    fn deduplicates_artifacts_across_a_selection() {
        let tree = ProductTree::parse(TREE).expect("parse");
        let selected =
            tree.artifacts_for_selection(["e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer"]);
        assert_eq!(selected.len(), 1);
        assert!(tree
            .artifacts_for_selection(["e2ei/11/IS_12.1.0.0.938/integrationServer/PIECore"])
            .is_empty());
    }

    #[test]
    fn records_install_panels_without_running_them() {
        let tree = ProductTree::parse(TREE).expect("parse");
        assert_eq!(
            tree.panels_for("e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer"),
            ["TNServerInstallPanel"]
        );
    }
}
