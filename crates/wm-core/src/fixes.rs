//! Discovering fixes without Update Manager.
//!
//! Update Manager asks IBM what applies to an installation by posting an
//! inventory of it. The answer is a p2 metadata archive — `content.jar`
//! containing `content.xml` — listing one unit per applicable fix, with its
//! target product, size and prerequisites.
//!
//! Both halves are reproducible here. The inventory is read off the
//! installation's own `.prop` files; the request is a plain authenticated POST:
//!
//! ```text
//! POST /services/sum-repository-service/repositories/<fixRepo>/fixes?showAll=<bool>
//! X-IBM-wMSUM-P2-SCHEMA: WM
//! { "envVariables": { "platform": "LNXAMD64", "platformGroup": ["LNXAMD64"], … },
//!   "installedProducts": [ { "productId": "TNS", "version": "12.1.0.0.139", … } ],
//!   "installedFixes": [], "installedSupportPatches": [] }
//! ```
//!
//! The asymmetry between `platform` (a string) and `platformGroup` (an array) is
//! not a typo: the service rejects the request either way round.

use std::path::Path;

use serde::Serialize;

use crate::catalog::Catalog;
use crate::sdc::Session;
use crate::{Error, Result};

/// An installation, as the fix service wants to hear about it.
#[derive(Debug, Clone, Serialize)]
pub struct Inventory {
    /// Platform, e.g. `LNXAMD64`.
    pub platform: String,
    /// Host name reported to the service.
    pub hostname: String,
    /// Update Manager version claimed by the client.
    pub update_manager_version: String,
    /// Installed products.
    pub products: Vec<InventoryProduct>,
    /// Fix ids already applied.
    pub installed_fixes: Vec<String>,
}

/// One installed product in an inventory.
///
/// The field names are the service's, not ours: it keys on `productId`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryProduct {
    /// Product code, e.g. `TNS`.
    pub product_id: String,
    /// Version, e.g. `12.1.0.0.139`.
    pub version: String,
    /// Component name.
    pub display_name: String,
}

impl Inventory {
    /// Build an inventory by reading an installation.
    pub fn read(install_dir: &Path, platform: &str) -> Result<Self> {
        let catalog = Catalog::load(install_dir)?;
        let products = catalog
            .iter()
            .map(|p| InventoryProduct {
                // The service keys on the product code, not the versioned path.
                product_id: p
                    .product_code
                    .clone()
                    .unwrap_or_else(|| p.path.component.clone()),
                version: p.path.version().to_string(),
                display_name: p.path.component.clone(),
            })
            .collect();
        Ok(Self {
            platform: platform.to_string(),
            hostname: hostname(),
            update_manager_version: "12.0.0.0008".to_string(),
            products,
            installed_fixes: Vec::new(),
        })
    }

    /// The request body the fix service expects.
    pub fn to_request(&self) -> serde_json::Value {
        serde_json::json!({
            "envVariables": {
                "platform": self.platform,
                "platformGroup": [self.platform],
                "UpdateManagerVersion": self.update_manager_version,
                "Hostname": self.hostname,
            },
            "installedProducts": self.products,
            "installedFixes": self.installed_fixes,
            "installedSupportPatches": [],
        })
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "wm-native".to_string())
}

/// One fix offered for an installation.
#[derive(Debug, Clone, Serialize)]
pub struct Fix {
    /// p2 unit id, e.g. `wMFix.TPS.SharedBundles`.
    pub id: String,
    /// Fix version, e.g. `12.1.0.0003-0779`.
    pub version: String,
    /// Product code the fix targets, e.g. `TPS`.
    pub product_code: Option<String>,
    /// Target product unit, e.g. `wMProduct.TPS_12.1.0`.
    pub target_product: Option<String>,
    /// Human name.
    pub display_name: Option<String>,
    /// Display group.
    pub display_group: Option<String>,
    /// Download size in bytes.
    pub size: Option<u64>,
    /// Release date, as reported.
    pub release_date: Option<String>,
    /// Fixes this one requires first.
    pub requires: Option<String>,
    /// Minimum Update Manager build the shipped tool would demand.
    pub requires_sum_build: Option<String>,
}

impl Fix {
    /// Update Manager's label for the fix, `<id>_<version>`.
    pub fn label(&self) -> String {
        format!("{}_{}", self.id, self.version)
    }
}

/// Ask the service which fixes apply to `inventory`.
///
/// `fix_repository` comes from the sandbox description — `prodRepo_WM` for a
/// webMethods-branded 12.1 installation. `show_all` widens the answer from
/// "what you are missing" to "everything published".
pub fn available(
    session: &Session,
    fix_repository: &str,
    inventory: &Inventory,
    show_all: bool,
) -> Result<Vec<Fix>> {
    let content = session.fix_metadata(fix_repository, &inventory.to_request(), show_all)?;
    parse_content_jar(&content)
}

/// Extract `content.xml` from a p2 metadata archive and parse it.
pub fn parse_content_jar(bytes: &[u8]) -> Result<Vec<Fix>> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| Error::Malformed(format!("fix metadata is not an archive: {e}")))?;
    let mut xml = String::new();
    {
        let mut entry = archive
            .by_name("content.xml")
            .map_err(|e| Error::Malformed(format!("no content.xml in the fix metadata: {e}")))?;
        use std::io::Read as _;
        entry
            .read_to_string(&mut xml)
            .map_err(|e| Error::Malformed(format!("content.xml unreadable: {e}")))?;
    }
    Ok(parse_content_xml(&xml))
}

/// Parse the `<unit>` elements of a p2 `content.xml`.
///
/// The document is machine-generated and shallow — units carrying a flat list of
/// properties — so it is scanned directly rather than through a parser: one
/// dependency fewer on a tool meant to be dropped onto a server.
pub fn parse_content_xml(xml: &str) -> Vec<Fix> {
    let mut fixes = Vec::new();
    let mut rest = xml;

    while let Some(start) = rest.find("<unit ") {
        let after = &rest[start..];
        let Some(header_end) = after.find('>') else {
            break;
        };
        let header = &after[..header_end];
        let body_end = after.find("</unit>").unwrap_or(after.len());
        let body = &after[..body_end];
        rest = &after[body_end.min(after.len())..];
        // Guard against a malformed document leaving `rest` unchanged.
        if rest.len() >= after.len() {
            rest = &after[header_end..];
        }

        let (Some(id), Some(version)) = (attribute(header, "id"), attribute(header, "version"))
        else {
            continue;
        };
        // Only fix units matter; p2 repositories also carry product units.
        if property(body, "com.webmethods.wm.type.fix").as_deref() != Some("true") {
            continue;
        }
        fixes.push(Fix {
            id,
            version,
            product_code: property(body, "com.webmethods.wm.fix.productCode"),
            target_product: property(body, "com.webmethods.wm.fix.targetProduct"),
            display_name: property(body, "com.webmethods.wm.fix.displayName"),
            display_group: property(body, "com.webmethods.wm.fix.displayGroupName"),
            size: property(body, "com.webmethods.wm.fix.size").and_then(|s| s.parse().ok()),
            release_date: property(body, "com.webmethods.wm.fix.releaseDate"),
            requires: property(body, "com.webmethods.wm.fix.requireFix"),
            requires_sum_build: property(body, "com.webmethods.wm.fix.requireSUMBuild"),
        });
    }
    fixes
}

/// Value of `name='…'` in an element header.
fn attribute(element: &str, name: &str) -> Option<String> {
    let needle = format!("{name}='");
    let start = element.find(&needle)? + needle.len();
    let end = element[start..].find('\'')? + start;
    Some(unescape(&element[start..end]))
}

/// Value of a `<property name='…' value='…'/>` inside a unit body.
fn property(body: &str, name: &str) -> Option<String> {
    let needle = format!("name='{name}' value='");
    let start = body.find(&needle)? + needle.len();
    let end = body[start..].find('\'')? + start;
    Some(unescape(&body[start..end]))
}

/// The five predefined XML entities, which p2 does use in display names.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// One entry of a p2 artifact repository.
#[derive(Debug, Clone, Serialize)]
pub struct FixArtifact {
    /// p2 classifier: `binary`, `osgi.bundle`, `readme`, …
    pub classifier: String,
    /// Artifact id, matching a fix unit id.
    pub id: String,
    /// Artifact version.
    pub version: String,
    /// Download size in bytes.
    pub size: Option<u64>,
    /// Expected sha256, lowercase hex.
    pub sha256: Option<String>,
}

impl FixArtifact {
    /// Repository-relative path, from the repository's own mapping rules.
    ///
    /// p2 declares these as templates in `artifacts.xml`; the four the fix
    /// repository publishes are stable and encoded here rather than
    /// interpreted, since evaluating an LDAP filter to choose between four
    /// known cases would be ceremony.
    pub fn path(&self) -> String {
        match self.classifier.as_str() {
            "org.eclipse.update.feature" => {
                format!("features/{}_{}.jar", self.id, self.version)
            }
            "osgi.bundle" => format!("plugins/{}_{}.jar", self.id, self.version),
            "readme" => format!("readme/{}_{}_readme.txt", self.id, self.version),
            // `binary`, and anything new, is served unadorned.
            _ => format!("binary/{}_{}", self.id, self.version),
        }
    }
}

/// Parse the p2 artifact index (`artifacts.jar`).
pub fn parse_artifact_index(bytes: &[u8]) -> Result<Vec<FixArtifact>> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| Error::Malformed(format!("artifact index is not an archive: {e}")))?;
    let mut xml = String::new();
    {
        let mut entry = archive
            .by_name("artifacts.xml")
            .map_err(|e| Error::Malformed(format!("no artifacts.xml in the index: {e}")))?;
        use std::io::Read as _;
        entry
            .read_to_string(&mut xml)
            .map_err(|e| Error::Malformed(format!("artifacts.xml unreadable: {e}")))?;
    }
    Ok(parse_artifact_xml(&xml))
}

/// Parse the `<artifact>` elements of a p2 `artifacts.xml`.
pub fn parse_artifact_xml(xml: &str) -> Vec<FixArtifact> {
    let mut artifacts = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<artifact ") {
        let after = &rest[start..];
        let Some(header_end) = after.find('>') else {
            break;
        };
        let header = &after[..header_end];
        let body_end = after.find("</artifact>").unwrap_or(header_end);
        let body = &after[..body_end.max(header_end)];
        rest = &after[header_end..];

        let (Some(classifier), Some(id), Some(version)) = (
            attribute(header, "classifier"),
            attribute(header, "id"),
            attribute(header, "version"),
        ) else {
            continue;
        };
        artifacts.push(FixArtifact {
            classifier,
            id,
            version,
            size: property(body, "download.size").and_then(|s| s.parse().ok()),
            sha256: property(body, "download.sha256").map(|s| s.to_lowercase()),
        });
    }
    artifacts
}

/// The artifacts belonging to one fix, by id and version.
pub fn artifacts_of<'a>(index: &'a [FixArtifact], fix: &Fix) -> Vec<&'a FixArtifact> {
    index
        .iter()
        .filter(|a| a.id == fix.id && a.version == fix.version)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = "\
<?xml version='1.0' encoding='UTF-8' standalone='yes'?>
<repository name='prodRepo_WM' type='…' version='1.0.0'>
    <units size='2'>
        <unit id='wMFix.TPS.SharedBundles' version='12.1.0.0003-0779' singleton='false'>
            <properties size='5'>
                <property name='com.webmethods.wm.type.fix' value='true'/>
                <property name='com.webmethods.wm.fix.size' value='53840981'/>
                <property name='com.webmethods.wm.fix.productCode' value='TPS'/>
                <property name='com.webmethods.wm.fix.targetProduct' value='wMProduct.TPS_12.1.0'/>
                <property name='com.webmethods.wm.fix.displayGroupName' value='Shared &amp; Bundles'/>
            </properties>
        </unit>
        <unit id='wMProduct.TPS' version='12.1.0'>
            <properties size='1'>
                <property name='com.webmethods.wm.type.fix' value='false'/>
            </properties>
        </unit>
    </units>
</repository>";

    #[test]
    fn parses_fix_units_and_ignores_product_units() {
        let fixes = parse_content_xml(XML);
        assert_eq!(fixes.len(), 1, "only the fix unit counts");
        let fix = &fixes[0];
        assert_eq!(fix.id, "wMFix.TPS.SharedBundles");
        assert_eq!(fix.version, "12.1.0.0003-0779");
        assert_eq!(fix.product_code.as_deref(), Some("TPS"));
        assert_eq!(fix.size, Some(53_840_981));
        assert_eq!(fix.label(), "wMFix.TPS.SharedBundles_12.1.0.0003-0779");
    }

    #[test]
    fn unescapes_entities_in_values() {
        let fixes = parse_content_xml(XML);
        assert_eq!(fixes[0].display_group.as_deref(), Some("Shared & Bundles"));
    }

    #[test]
    fn an_empty_or_broken_document_yields_nothing() {
        assert!(parse_content_xml("").is_empty());
        assert!(parse_content_xml("<repository><unit id='x'").is_empty());
        assert!(parse_content_xml("<unit >no attributes</unit>").is_empty());
    }

    const ARTIFACTS: &str = "\
<repository name='prodRepo_WM'>
    <artifacts size='2'>
        <artifact classifier='binary' id='wMFix.SPM' version='12.1.0.0001-0556'>
            <properties size='2'>
                <property name='download.size' value='1786736'/>
                <property name='download.sha256' value='ABCDEF'/>
            </properties>
        </artifact>
        <artifact classifier='readme' id='wMFix.SPM' version='12.1.0.0001-0556'>
            <properties size='1'>
                <property name='download.size' value='3911'/>
            </properties>
        </artifact>
    </artifacts>
</repository>";

    #[test]
    fn parses_the_artifact_index_and_maps_paths() {
        let index = parse_artifact_xml(ARTIFACTS);
        assert_eq!(index.len(), 2);
        assert_eq!(index[0].path(), "binary/wMFix.SPM_12.1.0.0001-0556");
        assert_eq!(index[0].size, Some(1_786_736));
        assert_eq!(
            index[0].sha256.as_deref(),
            Some("abcdef"),
            "digests are normalised"
        );
        assert_eq!(
            index[1].path(),
            "readme/wMFix.SPM_12.1.0.0001-0556_readme.txt"
        );
    }

    #[test]
    fn maps_the_other_p2_classifiers() {
        let bundle = FixArtifact {
            classifier: "osgi.bundle".into(),
            id: "com.example".into(),
            version: "1.0.0".into(),
            size: None,
            sha256: None,
        };
        assert_eq!(bundle.path(), "plugins/com.example_1.0.0.jar");
        let feature = FixArtifact {
            classifier: "org.eclipse.update.feature".into(),
            ..bundle
        };
        assert_eq!(feature.path(), "features/com.example_1.0.0.jar");
    }

    #[test]
    fn finds_the_artifacts_of_one_fix() {
        let index = parse_artifact_xml(ARTIFACTS);
        let fix = Fix {
            id: "wMFix.SPM".into(),
            version: "12.1.0.0001-0556".into(),
            product_code: None,
            target_product: None,
            display_name: None,
            display_group: None,
            size: None,
            release_date: None,
            requires: None,
            requires_sum_build: None,
        };
        assert_eq!(artifacts_of(&index, &fix).len(), 2);
    }

    #[test]
    fn the_request_body_uses_the_shape_the_service_demands() {
        let inventory = Inventory {
            platform: "LNXAMD64".into(),
            hostname: "host".into(),
            update_manager_version: "12.0.0.0008".into(),
            products: vec![InventoryProduct {
                product_id: "TNS".into(),
                version: "12.1.0.0.139".into(),
                display_name: "TNServer".into(),
            }],
            installed_fixes: Vec::new(),
        };
        let body = inventory.to_request();
        // platform is a string, platformGroup an array; the service rejects
        // either one written the other way.
        assert!(body["envVariables"]["platform"].is_string());
        assert!(body["envVariables"]["platformGroup"].is_array());
        assert_eq!(body["installedProducts"][0]["productId"], "TNS");
    }
}
