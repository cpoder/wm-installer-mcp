//! The shipped installer binary itself, and whether the catalogue has outgrown it.
//!
//! The download centre refuses an installer client that is too old, and it does
//! so late: the run authenticates, downloads the whole product list, and only
//! then prints
//!
//! ```text
//! Your Installer Client is version 12.1.0.0.123 033026. The server requires
//! changes introduced in Installer Client version 12.1.0.1.153 or later.
//! ```
//!
//! before exiting 1. Nothing about the selection causes it and nothing on the
//! machine changes — an installer that worked last month fails today because the
//! server moved. The cost is a minute per attempt and a failure that reads like
//! a network problem.
//!
//! Both halves of the check are available without asking anyone. The binary is a
//! self-extracting shell script whose header declares `VERSION="…"` in its first
//! few kilobytes, and the product tree carries the installer's own infrastructure
//! product, `WIR`, at the version the release expects — a 12.1 tree lists
//! `WIR_12.1.0.0.123` and `WIR_12.1.0.1.157`, and `12.1.0.0.123` is exactly the
//! version the old binary reports of itself.
//!
//! # What is compared, and what is not
//!
//! Only the **service level** — the first four components, `12.1.0.1` against
//! `12.1.0.0`. The server's complaint is about "changes introduced in" a
//! service level, and the binary that satisfies it in practice is
//! `12.1.0.1.153` while the catalogue declares `12.1.0.1.157`: a strict
//! comparison of the full version would reject a client that works. A build
//! number behind within the same service level is reported and not refused.
//!
//! # What this cannot do
//!
//! Fetch a newer installer. The `.bin` is not in the product tree — `WIR`
//! carries the installer's panel jars and its introspection module, not the
//! self-extracting client — and it is distributed through Passport Advantage
//! and Fix Central, which authenticate an IBMid rather than an entitlement key.
//! Neither is a protocol this crate speaks. The check therefore names the
//! version needed and where it comes from, and points at the native install
//! path, which has no client version to be out of date.

use std::cmp::Ordering;
use std::io::Read as _;
use std::path::Path;

use serde::Serialize;

use crate::catalog::Catalog;
use crate::{Error, Result};

/// Product code of the installer's own infrastructure product.
const INSTALLER_PRODUCT_CODE: &str = "WIR";

/// How much of the binary to read looking for its version.
///
/// The declaration sits in the shell header, within the first few hundred
/// bytes. Reading 64 KiB is generous and keeps the whole check under a
/// millisecond against a 69 MB file.
const HEADER_BYTES: usize = 64 * 1024;

/// What a version check found.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// Version the binary declares, when it declares one.
    pub local: Option<String>,
    /// Newest installer version the catalogue declares.
    pub catalog: Option<String>,
    /// Whether the local binary predates the catalogue's service level.
    pub outdated: bool,
    /// Whether it is behind only by build number within the same service level.
    pub behind_build: bool,
}

impl Check {
    /// One line stating the finding, or `None` when there is nothing to say.
    pub fn warning(&self) -> Option<String> {
        let local = self.local.as_deref()?;
        let catalog = self.catalog.as_deref()?;
        if self.outdated {
            Some(format!(
                "this installer is {local} and the {catalog} catalogue expects a \
                 {}.x client: the download centre rejects it after fetching the product \
                 list, with \"the server requires changes introduced in Installer Client \
                 version … or later\". Get a current client from Passport Advantage or Fix \
                 Central, or use native_install, which does not run this binary.",
                service_level(catalog)
            ))
        } else if self.behind_build {
            Some(format!(
                "this installer is {local} and the catalogue declares {catalog}. Same \
                 service level, so the download centre should accept it; a failure naming \
                 the Installer Client version means it did not."
            ))
        } else {
            None
        }
    }
}

/// Read the version a shipped installer binary declares of itself.
///
/// `Ok(None)` means the file was read and carries no `VERSION="…"` line, which
/// is not an error: it may be a binary from another generation, and a check that
/// cannot be made must not stop a run that would otherwise work.
pub fn local_version(bin: &Path) -> Result<Option<String>> {
    let mut file = std::fs::File::open(bin).map_err(|e| Error::io(bin, e))?;
    let mut head = vec![0u8; HEADER_BYTES];
    let read = file.read(&mut head).map_err(|e| Error::io(bin, e))?;
    head.truncate(read);
    let text = String::from_utf8_lossy(&head);
    Ok(parse_version_declaration(&text))
}

/// Pull `VERSION="…"` out of the self-extracting script's header.
fn parse_version_declaration(text: &str) -> Option<String> {
    let rest = text.split("VERSION=\"").nth(1)?;
    let value = rest.split('"').next()?.trim();
    (!value.is_empty() && value.chars().next()?.is_ascii_digit()).then(|| value.to_string())
}

/// The newest installer version `catalog` declares.
pub fn catalog_version(catalog: &Catalog) -> Option<String> {
    catalog
        .iter()
        .filter(|p| p.path.code() == INSTALLER_PRODUCT_CODE)
        .map(|p| p.path.version().to_string())
        .max_by(|a, b| compare(a, b))
}

/// Compare a binary against what a catalogue expects.
pub fn check(bin: Option<&Path>, catalog: Option<&Catalog>) -> Result<Check> {
    let local = match bin {
        Some(path) => local_version(path)?,
        None => None,
    };
    let catalog = catalog.and_then(catalog_version);
    let (outdated, behind_build) = match (&local, &catalog) {
        (Some(local), Some(catalog)) => (
            compare(&service_level(local), &service_level(catalog)) == Ordering::Less,
            compare(local, catalog) == Ordering::Less
                && service_level(local) == service_level(catalog),
        ),
        _ => (false, false),
    };
    Ok(Check {
        local,
        catalog,
        outdated,
        behind_build,
    })
}

/// The first four components of a version: everything but the build number.
fn service_level(version: &str) -> String {
    version.split('.').take(4).collect::<Vec<_>>().join(".")
}

/// Order two dotted versions numerically, component by component.
///
/// String order is wrong here in the way that matters: `12.1.0.0.99` sorts after
/// `12.1.0.0.123` lexically, and the build numbers are exactly where the
/// comparison has to be right.
fn compare(a: &str, b: &str) -> Ordering {
    let parts = |v: &str| -> Vec<u64> {
        v.split('.')
            .map(|p| p.trim().parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parts(a), parts(b));
    let len = a.len().max(b.len());
    for i in 0..len {
        match a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0)) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_version_out_of_a_self_extracting_header() {
        let header = "#!/bin/sh\nPRODUCT=\"IBM webMethods\"\nVERSION=\"12.1.0.1.153\"\nexit 0\n";
        assert_eq!(
            parse_version_declaration(header).as_deref(),
            Some("12.1.0.1.153")
        );
    }

    #[test]
    fn a_header_without_a_version_is_not_an_error() {
        assert_eq!(parse_version_declaration("#!/bin/sh\nexit 0\n"), None);
        // A value that is not a version is not one either: the same token could
        // reasonably appear as VERSION="latest" in something else entirely.
        assert_eq!(parse_version_declaration("VERSION=\"latest\""), None);
    }

    #[test]
    fn build_numbers_compare_numerically_not_lexically() {
        assert_eq!(compare("12.1.0.0.99", "12.1.0.0.123"), Ordering::Less);
        assert_eq!(compare("12.1.0.1.157", "12.1.0.1.157"), Ordering::Equal);
        // A missing component reads as zero, so 12.1 and 12.1.0.0.0 are one.
        assert_eq!(compare("12.1", "12.1.0.0.0"), Ordering::Equal);
    }

    #[test]
    fn an_older_service_level_is_outdated() {
        let check = Check {
            local: Some("12.1.0.0.123".into()),
            catalog: Some("12.1.0.1.157".into()),
            outdated: compare(
                &service_level("12.1.0.0.123"),
                &service_level("12.1.0.1.157"),
            ) == Ordering::Less,
            behind_build: false,
        };
        assert!(check.outdated);
        assert!(check.warning().is_some_and(|w| w.contains("12.1.0.1")));
    }

    #[test]
    fn a_lower_build_at_the_same_service_level_is_not_outdated() {
        // The binary that actually satisfies the server is 12.1.0.1.153 while
        // the catalogue declares 12.1.0.1.157. Refusing that one would block a
        // working install, which is worse than the failure being prevented.
        assert_eq!(service_level("12.1.0.1.153"), service_level("12.1.0.1.157"));
        assert_eq!(
            compare(
                &service_level("12.1.0.1.153"),
                &service_level("12.1.0.1.157")
            ),
            Ordering::Equal
        );
    }

    #[test]
    fn nothing_to_compare_means_no_finding() {
        let check = check(None, None).unwrap();
        assert!(!check.outdated);
        assert_eq!(check.warning(), None);
    }
}
