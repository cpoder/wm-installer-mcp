//! What Update Manager has recorded as installed.
//!
//! Update Manager keeps its own p2 profile, named `self`, under
//! `install/fix/profile/org.eclipse.equinox.p2.engine/profileRegistry/self.profile/`.
//! Every run that changes the fix level writes a new generation,
//! `<timestamp>.profile.gz`, and leaves the older ones in place so that Revert
//! can go back to them. Each generation is a gzipped XML document: the profile's
//! properties, one `<unit>` per installed fix copied verbatim from the download
//! centre's metadata, and an `<iusProperties>` block carrying the
//! `installTimestamp` of each.
//!
//! That file is the single source of truth for "View installed fixes", for
//! the inventory Update Manager sends to IBM, and for the filtering of what IBM
//! offers next. Readmes and backups are side effects. A fix applied by any
//! other means — including [`crate::fix::apply`] — is not in it, and Update
//! Manager will treat that fix as missing until something writes it there.
//!
//! Reading the registry is safe and cheap; this module only reads.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Error, Result};

/// Where the registry lives, relative to an installation.
pub const REGISTRY_DIR: &str =
    "install/fix/profile/org.eclipse.equinox.p2.engine/profileRegistry/self.profile";

/// One fix Update Manager considers installed.
#[derive(Debug, Clone, Serialize)]
pub struct InstalledFix {
    /// Unit id, e.g. `wMFix.TPS.SharedBundles`.
    pub id: String,
    /// Fix version, e.g. `12.1.0.0001-0731`.
    pub version: String,
    /// `com.webmethods.wm.fix.displayName`.
    pub display_name: Option<String>,
    /// `com.webmethods.wm.fix.productCode`, e.g. `TPS`.
    pub product_code: Option<String>,
    /// `com.webmethods.wm.fix.targetProduct`, e.g. `wMProduct.TPS_12.1.0`.
    pub target_product: Option<String>,
    /// `com.webmethods.fix.empowerFixId`, the download centre's identifier.
    pub empower_id: Option<String>,
    /// `com.webmethods.wm.fix.p2.repositories`, the repositories it refreshed.
    pub p2_repositories: Vec<String>,
    /// What kind of unit this is; the inventory sent to IBM reports support
    /// patches apart from fixes.
    pub kind: FixKind,
    /// `installTimestamp`, milliseconds since the epoch.
    pub installed_at_millis: Option<u64>,
    /// The same instant, RFC 3339 in UTC.
    pub installed_at: Option<String>,
}

/// The three kinds of unit Update Manager records, told apart by the
/// `com.webmethods.wm.type.*` properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixKind {
    /// An ordinary fix (`com.webmethods.wm.type.fix`).
    Fix,
    /// An emergency fix (`com.webmethods.wm.type.emfix`): the EDI module's
    /// per-standard fixes are recorded this way.
    EmergencyFix,
    /// A support patch — diagnostic collector, test patch, hotfix
    /// (`com.webmethods.wm.type.customdiagnoser`).
    SupportPatch,
}

impl InstalledFix {
    /// Whether the inventory sent to IBM lists this under
    /// `installedSupportPatches` rather than `installedFixes`.
    pub fn is_support_patch(&self) -> bool {
        self.kind == FixKind::SupportPatch
    }
}

/// The registry as it stands: the newest generation, parsed.
#[derive(Debug, Clone, Serialize)]
pub struct Registry {
    /// The generation file that was read.
    pub path: PathBuf,
    /// Its timestamp, which is also its file name.
    pub timestamp: u64,
    /// How many generations the registry holds, this one included.
    pub generations: usize,
    /// `webm_install_dir`, the installation this profile describes.
    pub install_dir: Option<String>,
    /// Installed fixes, by id.
    pub fixes: Vec<InstalledFix>,
}

impl Registry {
    /// The recorded fix with this id, if any.
    pub fn get(&self, id: &str) -> Option<&InstalledFix> {
        self.fixes.iter().find(|f| f.id == id)
    }
}

/// Where the registry of `wm_home` lives.
pub fn registry_dir(wm_home: &Path) -> PathBuf {
    wm_home.join(REGISTRY_DIR)
}

/// Read the newest generation of the registry.
///
/// `None` when there is no registry at all: an installation Update Manager
/// never touched, which is also what a native installation looks like. A
/// registry that exists but cannot be read is an error, not `None` — the
/// difference between "nothing is recorded" and "I could not tell" matters to
/// a caller deciding whether a fix is already applied.
pub fn read(wm_home: &Path) -> Result<Option<Registry>> {
    let dir = registry_dir(wm_home);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(&dir, e)),
    };
    let mut generations: Vec<(u64, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let stamp = name.strip_suffix(".profile.gz")?.parse::<u64>().ok()?;
            Some((stamp, path))
        })
        .collect();
    if generations.is_empty() {
        return Ok(None);
    }
    generations.sort();
    let count = generations.len();
    let (timestamp, path) = generations.pop().expect("at least one generation");
    let xml = read_gzipped(&path)?;
    let (properties, fixes) = parse_profile(&xml);
    Ok(Some(Registry {
        path,
        timestamp,
        generations: count,
        install_dir: properties.get("webm_install_dir").cloned(),
        fixes,
    }))
}

fn read_gzipped(path: &Path) -> Result<String> {
    let file = fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let mut text = String::new();
    flate2::read::GzDecoder::new(file)
        .read_to_string(&mut text)
        .map_err(|e| {
            Error::Malformed(format!("{} is not a gzipped profile: {e}", path.display()))
        })?;
    Ok(text)
}

/// Parse one generation: the profile's own properties and its fix units.
///
/// The document is machine-written and shallow, so it is scanned directly,
/// the way [`crate::fixes::parse_content_xml`] scans the download centre's
/// metadata. The units come first and the `installTimestamp` of each lives in
/// a separate `<iusProperties>` block, joined here by `(id, version)`.
pub fn parse_profile(
    xml: &str,
) -> (
    std::collections::BTreeMap<String, String>,
    Vec<InstalledFix>,
) {
    let mut properties = std::collections::BTreeMap::new();
    // The profile's own properties: everything before the first unit.
    let head = xml.split("<units").next().unwrap_or("");
    let mut rest = head;
    while let Some(start) = rest.find("<property ") {
        let after = &rest[start..];
        let end = after.find("/>").unwrap_or(after.len());
        let element = &after[..end];
        if let (Some(name), Some(value)) = (attribute(element, "name"), attribute(element, "value"))
        {
            properties.insert(name, value);
        }
        rest = &after[end.min(after.len())..];
        if rest.len() >= after.len() {
            break;
        }
    }

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
        if rest.len() >= after.len() {
            rest = &after[header_end..];
        }
        let (Some(id), Some(version)) = (attribute(header, "id"), attribute(header, "version"))
        else {
            continue;
        };
        let typed = |kind: &str| {
            property(body, &format!("com.webmethods.wm.type.{kind}")).as_deref() == Some("true")
        };
        let kind = if typed("customdiagnoser") {
            FixKind::SupportPatch
        } else if typed("emfix") {
            FixKind::EmergencyFix
        } else if typed("fix") {
            FixKind::Fix
        } else {
            // Product units and anything else the profile carries.
            continue;
        };
        fixes.push(InstalledFix {
            id,
            version,
            display_name: property(body, "com.webmethods.wm.fix.displayName"),
            product_code: property(body, "com.webmethods.wm.fix.productCode"),
            target_product: property(body, "com.webmethods.wm.fix.targetProduct"),
            empower_id: property(body, "com.webmethods.fix.empowerFixId"),
            p2_repositories: property(body, "com.webmethods.wm.fix.p2.repositories")
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().trim_end_matches('/').to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            kind,
            installed_at_millis: None,
            installed_at: None,
        });
    }

    // Then the per-unit properties, which carry when each was installed.
    if let Some(start) = xml.find("<iusProperties") {
        let mut rest = &xml[start..];
        while let Some(start) = rest.find("<iuProperties ") {
            let after = &rest[start..];
            let Some(header_end) = after.find('>') else {
                break;
            };
            let header = &after[..header_end];
            let body_end = after.find("</iuProperties>").unwrap_or(after.len());
            let body = &after[..body_end];
            rest = &after[body_end.min(after.len())..];
            if rest.len() >= after.len() {
                rest = &after[header_end..];
            }
            let (Some(id), Some(version)) = (attribute(header, "id"), attribute(header, "version"))
            else {
                continue;
            };
            let Some(millis) =
                property(body, "installTimestamp").and_then(|v| v.parse::<u64>().ok())
            else {
                continue;
            };
            if let Some(fix) = fixes
                .iter_mut()
                .find(|f| f.id == id && f.version == version)
            {
                fix.installed_at_millis = Some(millis);
                fix.installed_at = Some(rfc3339_utc(millis));
            }
        }
    }
    fixes.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.version.cmp(&b.version)));
    (properties, fixes)
}

/// Value of `name='…'` in an element header.
fn attribute(element: &str, name: &str) -> Option<String> {
    let needle = format!(" {name}='");
    let start = element.find(&needle)? + needle.len();
    let end = element[start..].find('\'')? + start;
    Some(unescape(&element[start..end]))
}

/// Value of a `<property name='…' value='…'/>` inside a body.
fn property(body: &str, name: &str) -> Option<String> {
    let needle = format!("name='{name}' value='");
    let start = body.find(&needle)? + needle.len();
    let end = body[start..].find('\'')? + start;
    Some(unescape(&body[start..end]))
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Milliseconds since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Proleptic Gregorian, the civil-from-days computation, so the crate needs no
/// calendar dependency for one timestamp.
pub fn rfc3339_utc(millis: u64) -> String {
    let seconds = millis / 1000;
    let days = (seconds / 86_400) as i64;
    let rem = seconds % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    const PROFILE: &str = "<?xml version='1.0' encoding='UTF-8'?>\n\
<?profile version='1.0.0'?>\n\
<profile id='self' timestamp='1784564816049'>\n\
  <properties size='2'>\n\
    <property name='webm_install_dir' value='/opt/webmethods'/>\n\
    <property name='regular' value='true'/>\n\
  </properties>\n\
  <units size='4'>\n\
    <unit id='wMFix.TPS.SharedBundles' version='12.1.0.0001-0731' singleton='false'>\n\
      <properties size='5'>\n\
        <property name='com.webmethods.wm.type.fix' value='true'/>\n\
        <property name='com.webmethods.wm.fix.p2.repositories' value='common/runtime/bundles/ext'/>\n\
        <property name='com.webmethods.wm.fix.displayName' value='Shared Bundles SharedBundles 12.1 Fix 1'/>\n\
        <property name='com.webmethods.wm.fix.productCode' value='TPS'/>\n\
        <property name='com.webmethods.fix.empowerFixId' value='TPS_12.1_SharedBundles_Fix1'/>\n\
      </properties>\n\
    </unit>\n\
    <unit id='wMProduct.TPS' version='12.1.0.0000-0000'>\n\
      <properties size='1'>\n\
        <property name='com.webmethods.wm.type.product' value='true'/>\n\
      </properties>\n\
    </unit>\n\
    <unit id='wMFix.EDIVDA' version='9.12.0.0003-0001'>\n\
      <properties size='2'>\n\
        <property name='com.webmethods.wm.type.customdiagnoser' value='false'/>\n\
        <property name='com.webmethods.wm.type.emfix' value='true'/>\n\
      </properties>\n\
    </unit>\n\
    <unit id='wMFix.Diag' version='1.0.0.0001-0001'>\n\
      <properties size='2'>\n\
        <property name='com.webmethods.wm.type.fix' value='false'/>\n\
        <property name='com.webmethods.wm.type.customdiagnoser' value='true'/>\n\
      </properties>\n\
    </unit>\n\
  </units>\n\
  <iusProperties size='2'>\n\
    <iuProperties id='wMFix.TPS.SharedBundles' version='12.1.0.0001-0731'>\n\
      <properties size='1'>\n\
        <property name='installTimestamp' value='1782481602033'/>\n\
      </properties>\n\
    </iuProperties>\n\
    <iuProperties id='wMFix.Diag' version='1.0.0.0001-0001'>\n\
      <properties size='1'>\n\
        <property name='installTimestamp' value='1782520219465'/>\n\
      </properties>\n\
    </iuProperties>\n\
  </iusProperties>\n\
</profile>\n";

    #[test]
    fn reads_fixes_their_properties_and_when_they_were_installed() {
        let (properties, fixes) = parse_profile(PROFILE);
        assert_eq!(
            properties.get("webm_install_dir").map(String::as_str),
            Some("/opt/webmethods")
        );
        // The product unit is not a fix; the diagnoser is a support patch.
        assert_eq!(fixes.len(), 3);
        let shared = fixes
            .iter()
            .find(|f| f.id == "wMFix.TPS.SharedBundles")
            .unwrap();
        assert_eq!(shared.version, "12.1.0.0001-0731");
        assert_eq!(shared.product_code.as_deref(), Some("TPS"));
        assert_eq!(
            shared.empower_id.as_deref(),
            Some("TPS_12.1_SharedBundles_Fix1")
        );
        assert_eq!(
            shared.p2_repositories,
            vec!["common/runtime/bundles/ext".to_string()]
        );
        assert_eq!(shared.kind, FixKind::Fix);
        assert_eq!(shared.installed_at_millis, Some(1782481602033));
        assert_eq!(shared.installed_at.as_deref(), Some("2026-06-26T13:46:42Z"));
        let diag = fixes.iter().find(|f| f.id == "wMFix.Diag").unwrap();
        assert_eq!(diag.kind, FixKind::SupportPatch);
        let em = fixes.iter().find(|f| f.id == "wMFix.EDIVDA").unwrap();
        assert_eq!(em.kind, FixKind::EmergencyFix);
    }

    #[test]
    fn timestamps_render_as_rfc3339() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_784_564_816_049), "2026-07-20T16:26:56Z");
    }

    #[test]
    fn the_newest_generation_wins_and_an_absent_registry_is_none() {
        let home = std::env::temp_dir().join(format!(
            "wm-fixregistry-{}-{}",
            std::process::id(),
            std::thread::current().id().as_u64_hack()
        ));
        let _ = fs::remove_dir_all(&home);
        assert!(read(&home).unwrap().is_none());

        let dir = registry_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        // An older generation listing nothing, and a newer one with the fix.
        for (stamp, xml) in [
            (
                1_000_000_000_000u64,
                "<profile id='self'><units size='0'/></profile>",
            ),
            (1_784_564_816_049u64, PROFILE),
        ] {
            let file = fs::File::create(dir.join(format!("{stamp}.profile.gz"))).unwrap();
            let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            gz.write_all(xml.as_bytes()).unwrap();
            gz.finish().unwrap();
        }
        // A stray file that is not a generation is ignored.
        fs::write(dir.join(".lock"), b"").unwrap();

        let registry = read(&home).unwrap().expect("a registry");
        assert_eq!(registry.timestamp, 1_784_564_816_049);
        assert_eq!(registry.generations, 2);
        assert_eq!(registry.fixes.len(), 3);
        assert!(registry.get("wMFix.TPS.SharedBundles").is_some());
        let _ = fs::remove_dir_all(&home);
    }

    /// `ThreadId` has no stable accessor; its `Debug` form does.
    trait ThreadIdHack {
        fn as_u64_hack(&self) -> String;
    }
    impl ThreadIdHack for std::thread::ThreadId {
        fn as_u64_hack(&self) -> String {
            format!("{self:?}")
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect()
        }
    }
}
