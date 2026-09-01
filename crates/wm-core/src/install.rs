//! Placing a product tree on disk, without the shipped installer.
//!
//! An artifact — a "BM" — is a signed JAR whose entries are already rooted at
//! the installation directory, plus two pieces of metadata: `META-INF/` holding
//! the signature, and `___comment_block` naming the module, its version, and the
//! Unix mode of every file it carries. Installing one is therefore: fetch,
//! verify against the digest the product tree declared, unpack everything that
//! is not metadata, and apply the recorded modes.
//!
//! # What this does not do
//!
//! Products also declare **install panels** — Java classes the shipped installer
//! runs at named stages (`PostProdSelect`, `PostFileCopy`). They create
//! Integration Server instances, seed the administrator password, write wrapper
//! configuration. They are compiled code inside each product's own resource
//! jars, so file placement is reproducible here and those actions are not; see
//! [`crate::tree::ProductTree::panels_for`]. A plan reports which selected
//! products declare panels so the gap is visible before anything is written,
//! rather than discovered on a server that does not start.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::sdc::{self, Session};
use crate::tree::{Artifact, ProductTree};
use crate::{Error, Result};

/// Entries that describe the artifact rather than belong to the installation.
const METADATA_ENTRIES: &[&str] = &["META-INF/", "___comment_block"];

/// Entry listing symbolic links the module wants created.
///
/// Each line is `<link> <target>`, the target relative to the link's own
/// directory: `common/security/openssl/lib64/libssl.so libssl-wm.so.3`. Written
/// out as a plain file instead, the links are missing and the libraries they
/// stand in for cannot be found by name.
const SYMLINK_ENTRY: &str = "___symlinks";

/// What an installation of a given selection would involve.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    /// Products to install.
    pub products: Vec<String>,
    /// Artifacts to fetch, deduplicated.
    pub artifacts: Vec<PlannedArtifact>,
    /// Total bytes to download.
    pub download_bytes: u64,
    /// Total bytes once unpacked.
    pub expanded_bytes: u64,
    /// Selected products that declare install panels this crate cannot run.
    pub products_with_panels: Vec<ProductPanels>,
}

/// One artifact in a plan.
#[derive(Debug, Clone, Serialize)]
pub struct PlannedArtifact {
    /// Artifact name.
    pub name: String,
    /// Repository-relative path.
    pub repository_path: String,
    /// Expected sha256.
    pub sha256: Option<String>,
    /// Download size.
    pub compressed_size: Option<u64>,
}

/// A product whose post-copy actions are Java panels.
#[derive(Debug, Clone, Serialize)]
pub struct ProductPanels {
    /// Product path.
    pub product: String,
    /// Panel names declared by the product.
    pub panels: Vec<String>,
}

/// Build a plan for `products` against `tree`.
pub fn plan(tree: &ProductTree, products: &[String]) -> Plan {
    let selected = tree.artifacts_for_selection(products.iter().map(String::as_str));
    let download_bytes = selected.iter().filter_map(|a| a.compressed_size).sum();
    let expanded_bytes = selected.iter().filter_map(|a| a.expanded_size).sum();
    let artifacts = selected
        .iter()
        .map(|a| PlannedArtifact {
            name: a.name.clone(),
            repository_path: a.repository_path.clone(),
            sha256: a.sha256.clone(),
            compressed_size: a.compressed_size,
        })
        .collect();
    let products_with_panels = products
        .iter()
        .filter_map(|p| {
            let panels = tree.panels_for(p);
            (!panels.is_empty()).then(|| ProductPanels {
                product: p.clone(),
                panels: panels.to_vec(),
            })
        })
        .collect();
    Plan {
        products: products.to_vec(),
        artifacts,
        download_bytes,
        expanded_bytes,
        products_with_panels,
    }
}

/// Outcome of fetching one artifact.
#[derive(Debug, Clone, Serialize)]
pub struct Fetched {
    /// Artifact name.
    pub name: String,
    /// Where it was cached.
    pub path: PathBuf,
    /// Bytes on disk.
    pub size: u64,
    /// Whether it was already in the cache and verified.
    pub from_cache: bool,
}

/// Download an artifact into `cache_dir`, verifying its digest.
///
/// A cached copy whose digest already matches is reused: the release is
/// immutable, so re-fetching gigabytes to reach the same bytes is pure cost.
/// A cached copy that does *not* match is replaced rather than trusted.
pub fn fetch(
    session: &mut Session,
    cgi: &str,
    repository: &str,
    artifact: &Artifact,
    cache_dir: &Path,
) -> Result<Fetched> {
    fs::create_dir_all(cache_dir).map_err(|e| Error::io(cache_dir, e))?;
    let path = cache_dir.join(format!("{}.zip", artifact.name));

    if path.is_file() {
        let bytes = fs::read(&path).map_err(|e| Error::io(&path, e))?;
        if digest_matches(artifact, &bytes) {
            return Ok(Fetched {
                name: artifact.name.clone(),
                path,
                size: bytes.len() as u64,
                from_cache: true,
            });
        }
    }

    let bytes = session.download(cgi, repository, &artifact.repository_path)?;
    if !digest_matches(artifact, &bytes) {
        return Err(Error::Exec(format!(
            "{} failed verification: expected sha256 {}, got {}",
            artifact.name,
            artifact.sha256.as_deref().unwrap_or("<none declared>"),
            sdc::sha256_hex(&bytes)
        )));
    }
    fs::write(&path, &bytes).map_err(|e| Error::io(&path, e))?;
    Ok(Fetched {
        name: artifact.name.clone(),
        path,
        size: bytes.len() as u64,
        from_cache: false,
    })
}

/// Whether `bytes` match whichever digests the tree declared.
fn digest_matches(artifact: &Artifact, bytes: &[u8]) -> bool {
    if let Some(expected) = &artifact.sha256 {
        return sdc::sha256_hex(bytes) == *expected;
    }
    if let Some(expected) = &artifact.md5 {
        return sdc::md5_hex(bytes) == *expected;
    }
    // Nothing to check against: refuse rather than silently accept.
    false
}

/// What unpacking one artifact wrote.
#[derive(Debug, Clone, Serialize)]
pub struct Unpacked {
    /// Artifact name.
    pub name: String,
    /// Files written, relative to the installation directory.
    pub files: Vec<String>,
    /// Directories created.
    pub directories: usize,
    /// Entries skipped because they describe the artifact, not the product.
    pub metadata_entries: usize,
    /// Symbolic links created from the module's `___symlinks` manifest.
    pub symlinks: Vec<String>,
}

/// Unpack a fetched artifact into `install_dir`.
///
/// Entry paths come from a signed archive but are still treated as untrusted:
/// anything escaping the installation directory is refused rather than
/// normalised, because a path that needs normalising is a path worth looking at.
pub fn unpack(archive: &Path, install_dir: &Path, modes: &Modes) -> Result<Unpacked> {
    let file = fs::File::open(archive).map_err(|e| Error::io(archive, e))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| {
        Error::Exec(format!(
            "{} is not readable as an archive: {e}",
            archive.display()
        ))
    })?;

    let name = archive
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut files = Vec::new();
    let mut directories = 0usize;
    let mut metadata_entries = 0usize;
    let mut symlinks = Vec::new();

    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|e| Error::Exec(format!("cannot read entry {index} of {name}: {e}")))?;
        let entry_name = entry.name().to_string();

        if METADATA_ENTRIES
            .iter()
            .any(|m| entry_name.starts_with(m) || entry_name == *m)
        {
            metadata_entries += 1;
            continue;
        }
        if entry_name == SYMLINK_ENTRY {
            let mut manifest = String::new();
            entry.read_to_string(&mut manifest).map_err(|e| {
                Error::Exec(format!("cannot read {SYMLINK_ENTRY} from {name}: {e}"))
            })?;
            symlinks.extend(create_symlinks(&manifest, install_dir)?);
            metadata_entries += 1;
            continue;
        }
        let Some(relative) = safe_path(&entry_name) else {
            return Err(Error::Exec(format!(
                "{name} contains an entry that escapes the installation directory: {entry_name:?}"
            )));
        };
        let target = install_dir.join(&relative);

        if entry_name.ends_with('/') {
            fs::create_dir_all(&target).map_err(|e| Error::io(&target, e))?;
            directories += 1;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| Error::Exec(format!("cannot read {entry_name} from {name}: {e}")))?;
        fs::write(&target, &bytes).map_err(|e| Error::io(&target, e))?;
        apply_mode(&target, modes.mode_of(&entry_name));
        files.push(relative.to_string_lossy().into_owned());
    }

    Ok(Unpacked {
        name,
        files,
        directories,
        metadata_entries,
        symlinks,
    })
}

/// Create the links a `___symlinks` manifest asks for.
///
/// An existing entry is replaced: re-installing a module must converge on the
/// same tree rather than fail because the link is already there.
fn create_symlinks(manifest: &str, install_dir: &Path) -> Result<Vec<String>> {
    let mut created = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((link, target)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let target = target.trim();
        let Some(relative) = safe_path(link.trim()) else {
            return Err(Error::Exec(format!(
                "symlink manifest names a path outside the installation: {link:?}"
            )));
        };
        // The target is resolved beside the link, so it must stay relative and
        // must not climb out of the tree.
        if target.is_empty() || Path::new(target).is_absolute() || target.contains("..") {
            return Err(Error::Exec(format!(
                "symlink {link:?} has an unusable target {target:?}"
            )));
        }
        let path = install_dir.join(&relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        #[cfg(unix)]
        {
            let _ = fs::remove_file(&path);
            std::os::unix::fs::symlink(target, &path).map_err(|e| Error::io(&path, e))?;
            created.push(format!("{} -> {target}", relative.display()));
        }
        #[cfg(not(unix))]
        {
            // Windows needs a privilege for symlinks; copying the file the link
            // names is closer to the intent than failing the install.
            let source = path.parent().map(|p| p.join(target));
            if let Some(source) = source.filter(|s| s.is_file()) {
                fs::copy(&source, &path).map_err(|e| Error::io(&path, e))?;
                created.push(format!("{} (copied from {target})", relative.display()));
            }
        }
    }
    Ok(created)
}

/// Unix modes recorded in an artifact's `___comment_block`.
///
/// The archive format carries no usable permission bits of its own, so the
/// module lists them separately. Executables that lose their bit produce a
/// server that installs cleanly and will not start.
#[derive(Debug, Clone, Default)]
pub struct Modes {
    entries: std::collections::BTreeMap<String, u32>,
    /// Module name from the header.
    pub module: Option<String>,
    /// Module version from the header.
    pub version: Option<String>,
}

impl Modes {
    /// Read the modes from an artifact archive.
    pub fn read(archive: &Path) -> Result<Self> {
        let file = fs::File::open(archive).map_err(|e| Error::io(archive, e))?;
        let mut zip = zip::ZipArchive::new(file)
            .map_err(|e| Error::Exec(format!("{} unreadable: {e}", archive.display())))?;
        let Ok(mut entry) = zip.by_name("___comment_block") else {
            return Ok(Self::default());
        };
        let mut text = String::new();
        // Name the archive and its size. A failure here has been seen once,
        // was not reproducible, and could not be attributed: the bytes had
        // already passed their sha256 before being written, the filesystem had
        // 780 GB free, and no other job shared the cache. Without the archive
        // name in the message there was nothing left to investigate with.
        entry.read_to_string(&mut text).map_err(|e| {
            let size = fs::metadata(archive).map(|m| m.len()).unwrap_or(0);
            Error::Exec(format!(
                "comment block of {} ({size} bytes) unreadable: {e}",
                archive.display()
            ))
        })?;
        Ok(Self::parse(&text))
    }

    /// Parse a comment block.
    pub fn parse(text: &str) -> Self {
        let mut modes = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("MODULE:") {
                modes.module = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("VERSION:") {
                modes.version = Some(rest.trim().to_string());
            } else if let Some((mode, path)) = line.split_once(' ') {
                if let Ok(bits) = u32::from_str_radix(mode.trim(), 8) {
                    modes.entries.insert(path.trim().to_string(), bits);
                }
            }
        }
        modes
    }

    /// Mode for one entry, defaulting to a plain read-write file.
    pub fn mode_of(&self, entry: &str) -> u32 {
        self.entries.get(entry).copied().unwrap_or(0o644)
    }
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    // Best effort: a file placed with the wrong bits is recoverable, a failed
    // install because chmod was refused is not worth it.
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: u32) {}

/// Reject an archive entry that would write outside the installation directory.
fn safe_path(entry: &str) -> Option<PathBuf> {
    let candidate = Path::new(entry);
    let mut clean = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            // `..`, a leading `/`, or a Windows prefix all mean the entry is
            // not relative to the installation directory.
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!clean.as_os_str().is_empty()).then_some(clean)
}

/// Record what an artifact placed, in the form the installer uses.
///
/// `install/bms/<artifact>.contents` is how an installation remembers which
/// files came from which module; without it, later tooling — including the
/// shipped Update Manager — cannot reason about the tree.
pub fn write_contents(install_dir: &Path, artifact: &Artifact, unpacked: &Unpacked) -> Result<()> {
    let dir = install_dir.join("install").join("bms");
    fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
    let path = dir.join(format!("{}.contents", artifact.name));

    let mut text = String::new();
    text.push_str(&format!(
        "name={}/{}/{}\n",
        artifact.product, artifact.variant, artifact.name
    ));
    if let Some(version) = &artifact.version {
        text.push_str(&format!("version={version}\n"));
    }
    text.push('\n');
    for file in &unpacked.files {
        text.push_str(file);
        text.push('\n');
    }
    fs::write(&path, text).map_err(|e| Error::io(&path, e))
}

/// Record a product in `install/products/<component>.prop`.
///
/// The shipped installer writes one of these per product, and every later
/// tool — dependency resolution, Update Manager, this crate's own catalogue
/// reader — treats them as the record of what is installed. An installation
/// without them is a directory of files that nothing can reason about.
pub fn write_prop(install_dir: &Path, product: &str, tree: &ProductTree) -> Result<PathBuf> {
    let component = product.rsplit('/').next().unwrap_or(product);
    let dir = install_dir.join("install").join("products");
    fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
    let path = dir.join(format!("{component}.prop"));

    let mut text = String::new();
    text.push_str(&format!("\nproduct={product}\n\n"));
    if let Some(props) = tree.props_for(product) {
        let names: Vec<&str> = props.keys().map(String::as_str).collect();
        text.push_str(&format!("{product}/props={}\n", names.join(",")));
        for (field, value) in props {
            text.push_str(&format!("{product}/props/{field}={value}\n"));
        }
    }
    // Every artifact of a product shares its platform variant, so the first
    // one names the child node the installer records.
    if let Some(artifact) = tree.artifacts_for(product).first() {
        text.push_str(&format!("{product}/children={}\n", artifact.variant));
    }
    fs::write(&path, text).map_err(|e| Error::io(&path, e))?;
    Ok(path)
}

/// Products already recorded in an installation.
pub fn installed_products(install_dir: &Path) -> BTreeSet<String> {
    let dir = install_dir.join("install").join("products");
    let Ok(entries) = fs::read_dir(&dir) else {
        return BTreeSet::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "prop" {
                return None;
            }
            let text = fs::read_to_string(&path).ok()?;
            text.lines()
                .find_map(|l| l.trim().strip_prefix("product=").map(str::to_string))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_entries_that_escape_the_target() {
        assert!(safe_path("../etc/passwd").is_none());
        assert!(safe_path("/etc/passwd").is_none());
        assert!(safe_path("a/../../b").is_none());
        assert_eq!(
            safe_path("install/x.zip"),
            Some(PathBuf::from("install/x.zip"))
        );
        assert_eq!(safe_path("./install/x"), Some(PathBuf::from("install/x")));
        assert!(safe_path("").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn creates_the_links_a_manifest_asks_for() {
        let dir = std::env::temp_dir().join(format!("wm-links-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let manifest = "common/lib64/libssl.so libssl-wm.so.3\n\n# comment\n";
        let made = create_symlinks(manifest, &dir).expect("links");
        assert_eq!(made.len(), 1);
        let link = dir.join("common/lib64/libssl.so");
        assert_eq!(
            fs::read_link(&link).expect("link"),
            Path::new("libssl-wm.so.3")
        );
        // Re-running must converge, not fail on an existing link.
        create_symlinks(manifest, &dir).expect("idempotent");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_manifest_that_points_outside_the_installation() {
        let dir = std::env::temp_dir().join("wm-links-never");
        assert!(create_symlinks("../escape lib.so\n", &dir).is_err());
        assert!(create_symlinks("ok/link /etc/passwd\n", &dir).is_err());
        assert!(create_symlinks("ok/link ../../etc/passwd\n", &dir).is_err());
    }

    #[test]
    fn parses_a_comment_block() {
        let modes = Modes::parse(
            "MODULE: BM_TNSServerConfiguration-ALL-Any\n\
             VERSION: 12.1.0.0.139\n\
             DATE: 1775481028(Mon Apr 06 13:10:28 UTC 2026)\n\
             0755 install/configurations/TNServer.zip\n",
        );
        assert_eq!(
            modes.module.as_deref(),
            Some("BM_TNSServerConfiguration-ALL-Any")
        );
        assert_eq!(modes.version.as_deref(), Some("12.1.0.0.139"));
        assert_eq!(modes.mode_of("install/configurations/TNServer.zip"), 0o755);
        // Anything unlisted gets a conservative default.
        assert_eq!(modes.mode_of("not/listed"), 0o644);
    }

    #[test]
    fn a_missing_digest_fails_verification() {
        let artifact = Artifact {
            kind: crate::tree::ArtifactKind::Module,
            product: "e2ei/11/A_1/G/C".into(),
            variant: "C-LNXAMD64-Any".into(),
            name: "BM_X".into(),
            repository_path: "e2ei/11/A_1/bms/BM_X.zip".into(),
            sha256: None,
            md5: None,
            compressed_size: None,
            expanded_size: None,
            version: None,
        };
        assert!(!digest_matches(&artifact, b"anything"));
    }

    #[test]
    fn verifies_against_sha256_then_md5() {
        let mut artifact = Artifact {
            kind: crate::tree::ArtifactKind::Module,
            product: "e2ei/11/A_1/G/C".into(),
            variant: "C-LNXAMD64-Any".into(),
            name: "BM_X".into(),
            repository_path: "e2ei/11/A_1/bms/BM_X.zip".into(),
            sha256: Some(sdc::sha256_hex(b"abc")),
            md5: None,
            compressed_size: None,
            expanded_size: None,
            version: None,
        };
        assert!(digest_matches(&artifact, b"abc"));
        assert!(!digest_matches(&artifact, b"abd"));

        artifact.sha256 = None;
        artifact.md5 = Some(sdc::md5_hex(b"abc"));
        assert!(digest_matches(&artifact, b"abc"));
    }
}
