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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::catalog::ProductPath;
use crate::inventory::Inventory;
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

/// A declared path that is not where the manifest put it.
#[derive(Debug, Clone, Serialize)]
pub struct Absent {
    /// The path, as the manifest declares it.
    pub path: String,
    /// A file in the same directory that supersedes it, when there is one.
    ///
    /// `Some` means a fix replaced it: `com.webmethods.tps.apache.ant.feature_
    /// 12.1.0.0000-0280.jar` is gone and `…_12.1.0.0001-0731.jar` is beside it.
    /// That is the Update Manager doing its job, not damage.
    pub superseded_by: Option<String>,
}

/// One artifact's manifest, checked against the disk.
#[derive(Debug, Clone, Serialize)]
pub struct ArtifactCheck {
    /// The `.contents` basename, which is the artifact's name.
    pub artifact: String,
    /// The product path the manifest names.
    pub product: Option<String>,
    /// Version the manifest records.
    pub version: Option<String>,
    /// How many paths it lists.
    pub declared: usize,
    /// Directory the paths were resolved against, relative to the
    /// installation. Empty for the usual case of the installation root.
    pub base: String,
    /// Declared paths that are not there.
    pub absent: Vec<Absent>,
    /// Of those, how many a newer file supersedes.
    pub superseded: usize,
    /// Of those, how many nothing accounts for.
    pub unexplained: usize,
    /// Whether not one declared path is present.
    pub never_written: bool,
}

/// Everything an installation claims, checked against what it has.
#[derive(Debug, Clone, Serialize)]
pub struct Verification {
    /// The installation.
    pub wm_home: PathBuf,
    /// Manifests read.
    pub artifacts: usize,
    /// Paths declared across all of them.
    pub declared: usize,
    /// Declared paths that are not there, for whatever reason.
    pub absent: usize,
    /// Of those, superseded by a newer file in the same directory.
    pub superseded: usize,
    /// Of those, unaccounted for.
    pub unexplained: usize,
    /// Artifacts of which nothing at all was written. The signal that matters.
    pub never_written: Vec<ArtifactCheck>,
    /// Artifacts with absences nothing accounts for, worst first.
    pub incomplete: Vec<ArtifactCheck>,
    /// Manifests that could not be read at all.
    pub unreadable: Vec<String>,
}

impl Verification {
    /// Whether anything was found that an applied fix does not explain.
    pub fn is_sound(&self) -> bool {
        self.never_written.is_empty() && self.unexplained == 0 && self.unreadable.is_empty()
    }
}

/// Check an installation against the manifests it carries.
///
/// A native install leaves `install/bms/<artifact>.contents` listing every path
/// it wrote, and the shipped installer does the same. That is a complete
/// statement of what the installation contained *when it was installed*, and
/// until now nothing read it back: a plan trusted `install/products/*.prop` —
/// which says a product is *claimed* — and no tool could tell a finished install
/// from one whose files a failed run had never written.
///
/// # Why a declared path being absent is usually correct
///
/// Applying a fix deletes files and puts newer ones in their place, and no one
/// rewrites the manifest afterwards. On the first real installation this was run
/// against, every one of the sampled absences was of that kind:
/// `com.webmethods.osgi.agent.profile_12.1.0.0000-0497` gone with
/// `…_12.1.0.0002-0579` beside it, `org-eclipse-jgit-ssh-jsch-6.3.0.jar` gone
/// with `-7.4.0.jar` beside it. Reporting those as faults would tell an operator
/// that a correctly patched installation is broken in sixteen places.
///
/// So an absence is classified rather than counted. A file that a same-named,
/// differently-versioned neighbour supersedes is reported as superseded. Only
/// two things are ever called wrong: an artifact of which *nothing* was written,
/// and an absence with no replacement to account for it.
///
/// Reading is all this does. It opens no archive and contacts nothing, so it
/// works on a stopped installation with no credentials. It checks presence, not
/// content: the manifests carry no sizes or digests, so a truncated file passes.
pub fn verify(wm_home: &Path, only: Option<&str>) -> Result<Verification> {
    let dir = wm_home.join("install").join("bms");
    let entries = fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))?;
    let mut incomplete: Vec<ArtifactCheck> = Vec::new();
    let mut never_written: Vec<ArtifactCheck> = Vec::new();
    let mut unreadable = Vec::new();
    let (mut artifacts, mut declared_total) = (0usize, 0usize);
    let (mut absent_total, mut superseded_total, mut unexplained_total) = (0usize, 0usize, 0usize);

    let needle = only.map(str::to_lowercase);
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().is_none_or(|e| e != "contents") {
            continue;
        }
        let name = path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Ok(text) = fs::read_to_string(&path) else {
            unreadable.push(name);
            continue;
        };
        let manifest = Manifest::parse(&text);
        // Filter on the manifest as well as the filename: a caller narrowing to
        // "Deployer" means the product, and the artifact carrying it is called
        // BM_Deployer-ALL-Any.
        if let Some(needle) = &needle {
            let hit = name.to_lowercase().contains(needle)
                || manifest
                    .product
                    .as_ref()
                    .is_some_and(|p| p.to_lowercase().contains(needle));
            if !hit {
                continue;
            }
        }
        artifacts += 1;
        declared_total += manifest.files.len();

        let base = resolve_base(wm_home, &manifest.files);
        let root = wm_home.join(&base);
        let absent: Vec<Absent> = manifest
            .files
            .iter()
            .filter(|relative| !root.join(relative).exists())
            .map(|relative| Absent {
                superseded_by: superseding_neighbour(&root, relative),
                path: relative.clone(),
            })
            .collect();
        let superseded = absent.iter().filter(|a| a.superseded_by.is_some()).count();
        let unexplained = absent.len() - superseded;
        absent_total += absent.len();
        superseded_total += superseded;
        unexplained_total += unexplained;

        let check = ArtifactCheck {
            artifact: name,
            product: manifest.product,
            version: manifest.version,
            declared: manifest.files.len(),
            base: base.to_string_lossy().into_owned(),
            // Nothing present *and* nothing accounting for it. A small artifact
            // whose every file a fix replaced is absent in full and perfectly
            // healthy, so counting absences alone would condemn it.
            never_written: !manifest.files.is_empty() && unexplained == manifest.files.len(),
            superseded,
            unexplained,
            absent,
        };
        if check.never_written {
            never_written.push(check);
        } else if unexplained > 0 {
            incomplete.push(check);
        }
    }
    let worst_first = |a: &ArtifactCheck, b: &ArtifactCheck| {
        b.unexplained
            .cmp(&a.unexplained)
            .then_with(|| a.artifact.cmp(&b.artifact))
    };
    incomplete.sort_by(worst_first);
    never_written.sort_by(worst_first);
    unreadable.sort();
    Ok(Verification {
        wm_home: wm_home.to_path_buf(),
        artifacts,
        declared: declared_total,
        absent: absent_total,
        superseded: superseded_total,
        unexplained: unexplained_total,
        never_written,
        incomplete,
        unreadable,
    })
}

/// Directories a manifest's paths may be relative to.
///
/// Almost every manifest is relative to the installation root. A few are not:
/// `BM_WmSAP-ALL-Any#2` declares `packages/WmSAP/…` for files that are at
/// `IntegrationServer/packages/WmSAP/…`, and nothing in the manifest says so.
/// Rather than assume, the base is chosen by which one the files are actually
/// under.
const CANDIDATE_BASES: &[&str] = &["", "IntegrationServer"];

/// Pick the base directory `files` are relative to, on the evidence.
///
/// Sampled rather than exhaustive: a manifest can list several thousand paths
/// and the answer is the same after fifty.
fn resolve_base(wm_home: &Path, files: &[String]) -> PathBuf {
    let sample: Vec<&String> = files.iter().take(50).collect();
    if sample.is_empty() {
        return PathBuf::new();
    }
    let hits = |base: &str| {
        let root = wm_home.join(base);
        sample
            .iter()
            .filter(|relative| root.join(relative.as_str()).exists())
            .count()
    };
    let best = CANDIDATE_BASES
        .iter()
        .max_by_key(|base| hits(base))
        .copied()
        .unwrap_or("");
    // Only move off the installation root when the evidence is clear; a
    // manifest of which nothing was written scores zero everywhere and must
    // keep the root, or its report would name a directory it never used.
    if hits(best) > hits("") {
        PathBuf::from(best)
    } else {
        PathBuf::new()
    }
}

/// A file beside `relative` that looks like a newer version of it.
///
/// Fix artifacts are versioned in their names — `…_12.1.0.0000-0497`,
/// `…-6.3.0.jar` — so two names match when they agree on everything but the
/// version: the same stem before it, and the same tail after it. The tail is
/// what keeps a `.jar` from being superseded by a `.txt` that shares a prefix,
/// and comparing file extensions instead does not work, because a version
/// containing dots *is* the extension as far as any split on `.` can tell.
fn superseding_neighbour(root: &Path, relative: &str) -> Option<String> {
    let path = root.join(relative);
    let directory = path.parent()?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let stem = version_stem(&name)?;
    let tail = version_tail(&name, stem);
    fs::read_dir(directory)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|candidate| {
            candidate != &name
                && version_stem(candidate)
                    .is_some_and(|other| other == stem && version_tail(candidate, other) == tail)
        })
}

/// What follows the version in a name whose stem is `stem`.
///
/// `…profile_12.1.0.0000-0497` yields `""` and `mina-core-2.2.5.jar` yields
/// `jar`: everything version-shaped is dropped, and whatever the name ends with
/// remains.
fn version_tail<'a>(name: &'a str, stem: &str) -> &'a str {
    name[stem.len()..].trim_start_matches(|c: char| c.is_ascii_digit() || "._-".contains(c))
}

/// The part of a file name before its version, or `None` if it has none.
///
/// Splits at the **first** `_` or `-` followed by a digit, because a version is
/// several such segments and only the first begins it:
/// `com.webmethods.osgi.agent.profile_12.1.0.0000-0497` yields
/// `com.webmethods.osgi.agent.profile`, which is what it shares with the
/// `…_12.1.0.0002-0579` a fix left in its place. Cutting at the last one instead
/// yields `…profile_12.1.0.0000`, which the replacement cannot match — and every
/// replaced file is then reported as unexplained.
///
/// A name with no such separator has no version to differ in, so it cannot be
/// superseded.
fn version_stem(name: &str) -> Option<&str> {
    let bytes = name.as_bytes();
    bytes
        .iter()
        .enumerate()
        .find(|(i, byte)| {
            (**byte == b'_' || **byte == b'-') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
        })
        .map(|(i, _)| &name[..i])
}

/// A parsed `.contents` file.
///
/// Two shapes are in the wild, and an installation of any age carries both. The
/// shipped installer writes `name=`, `version=` and `timestamp=` headers with no
/// blank line, then one `<octal mode> <path>` line per file:
///
/// ```text
/// name=e2ei/11/IS_12.1.0.0.938/integrationServer/PIECore/…/BM_…
/// version=12.1.0.0.938
/// timestamp=1776417008
/// 0755 IntegrationServer/packages/WmRoot/assets.json
/// ```
///
/// A native install writes the headers, a blank line, then bare paths. Reading
/// only the second shape reports every file of the first as missing — 22 745 of
/// them on the installation this was first run against, none of them absent.
struct Manifest {
    product: Option<String>,
    version: Option<String>,
    files: Vec<String>,
}

impl Manifest {
    fn parse(text: &str) -> Self {
        let mut product = None;
        let mut version = None;
        let mut files = Vec::new();
        // Headers only count until the first line that is not one. After that a
        // file may legitimately be named `version=something`.
        let mut in_headers = true;
        for line in text.lines() {
            if in_headers {
                if line.trim().is_empty() {
                    in_headers = false;
                    continue;
                }
                if let Some(value) = line.strip_prefix("name=") {
                    product = Some(value.trim().to_string());
                    continue;
                }
                if let Some(value) = line.strip_prefix("version=") {
                    version = Some(value.trim().to_string());
                    continue;
                }
                if line.starts_with("timestamp=") {
                    continue;
                }
                in_headers = false;
            }
            let path = strip_mode(line.trim_end());
            if !path.trim().is_empty() {
                files.push(path.to_string());
            }
        }
        Self {
            product,
            version,
            files,
        }
    }
}

/// Drop the leading `0755 ` the shipped installer records, if there is one.
///
/// Split on the first space only: a recorded path may contain spaces, and the
/// mode never does.
fn strip_mode(line: &str) -> &str {
    let Some((mode, rest)) = line.split_once(' ') else {
        return line;
    };
    let is_mode = mode.len() == 4 && mode.bytes().all(|b| (b'0'..=b'7').contains(&b));
    if is_mode {
        rest
    } else {
        line
    }
}

/// What a selection means for an installation that already exists.
///
/// The native install path unpacked every artifact of the closure
/// unconditionally. Into an empty directory that is correct. Into an existing
/// installation it is not: a product reinstalled at its base version overwrites
/// files that Update Manager has since patched, so the installation silently
/// loses fix level — no error, no entry in a log, and nothing that would show up
/// until something misbehaves months later.
///
/// Splitting the selection against what is on disk is what makes the difference
/// visible. `install/products/*.prop` already carries the exact versioned path
/// of everything installed, which is the same identifier the catalogue uses, so
/// the comparison is an equality and not a heuristic.
///
/// Two things it deliberately does not claim.
///
/// That a matching version means matching *files*: a `.prop` records the version
/// a product was installed at, and a product Update Manager has since patched
/// still reports that same version — the fix level lives in `updateReadmes`,
/// which [`Inventory`] reads separately. Matching versions therefore only
/// justify leaving a product alone, which is what happens; they never justify
/// writing over it.
///
/// And that a `.prop` means the product is *complete*. Nothing here opens a
/// single file: a product counts as present because the installation says so.
/// The one partial run measured — the shipped installer failing part-way on
/// 2026-09-18 — argues the record is trustworthy rather than the reverse, since
/// it wrote a `.prop` for each of the three products it had actually placed and
/// for none of the thirteen it had not. What is untested is the case where the
/// record outruns the files. Verifying that every path an
/// `install/bms/*.contents` names is present is a different question, and not
/// one this answers.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Delta {
    /// Selected products already present at exactly this version. No work.
    pub already_installed: Vec<String>,
    /// Selected products present at a *different* version — the dangerous case.
    pub version_changes: Vec<VersionChange>,
    /// Selected products not present at all. This is the actual install.
    pub to_install: Vec<String>,
}

/// A selected product the installation already carries under another version.
#[derive(Debug, Clone, Serialize)]
pub struct VersionChange {
    /// Versioned path as the catalogue gives it.
    pub product: String,
    /// Versioned path as the installation carries it.
    pub installed: String,
    /// Component name, the two paths' common identity.
    pub component: String,
    /// Version on disk.
    pub installed_version: String,
    /// Version the catalogue would lay down.
    pub catalog_version: String,
}

impl Delta {
    /// Whether anything at all would be written.
    pub fn is_empty(&self) -> bool {
        self.to_install.is_empty() && self.version_changes.is_empty()
    }

    /// Everything that would be written: the additions, plus the version
    /// changes, which are only performed when the caller forces them.
    pub fn forced(&self) -> Vec<String> {
        let mut all = self.to_install.clone();
        all.extend(self.version_changes.iter().map(|c| c.product.clone()));
        all.sort();
        all
    }
}

/// Split `products` against what `installed` already carries.
///
/// Identity is `(group, component, product code)` — the version is deliberately
/// not part of it, because telling a reinstall from an upgrade is the entire
/// point. An installation with no products at all yields everything to install,
/// which is the fresh case and needs no special handling by the caller.
pub fn delta(products: &[String], installed: &Inventory) -> Delta {
    // Exact paths first: the cheap answer for the common case.
    let present: BTreeSet<&str> = installed.products.iter().map(|p| p.path.as_str()).collect();
    // Then by identity, for products carried at some other version.
    let by_identity: BTreeMap<(&str, &str, &str), &crate::inventory::InstalledProduct> = installed
        .products
        .iter()
        .map(|p| ((p.group.as_str(), p.component.as_str(), p.code.as_str()), p))
        .collect();

    let mut delta = Delta::default();
    for product in products {
        if present.contains(product.as_str()) {
            delta.already_installed.push(product.clone());
            continue;
        }
        let Ok(path) = ProductPath::parse(product) else {
            // Not a well-formed versioned path: nothing on disk can match it by
            // identity, so treat it as new rather than dropping it.
            delta.to_install.push(product.clone());
            continue;
        };
        match by_identity.get(&(path.group.as_str(), path.component.as_str(), path.code())) {
            Some(existing) => delta.version_changes.push(VersionChange {
                product: product.clone(),
                installed: existing.path.clone(),
                component: path.component.clone(),
                installed_version: existing.version.clone(),
                catalog_version: path.version().to_string(),
            }),
            None => delta.to_install.push(product.clone()),
        }
    }
    delta
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

    /// An installation carrying exactly `paths`.
    fn installed(paths: &[&str]) -> Inventory {
        Inventory {
            wm_home: PathBuf::from("/opt/webmethods"),
            products: paths
                .iter()
                .map(|raw| {
                    let path = ProductPath::parse(raw).expect("test path");
                    crate::inventory::InstalledProduct {
                        component: path.component.clone(),
                        group: path.group.clone(),
                        code: path.code().to_string(),
                        version: path.version().to_string(),
                        path: raw.to_string(),
                    }
                })
                .collect(),
            runtimes: Vec::new(),
            fixes: Vec::new(),
        }
    }

    const IS_938: &str = "e2ei/11/IS_12.1.0.0.938/integrationServer/integrationServer";
    const IS_940: &str = "e2ei/11/IS_12.1.0.0.940/integrationServer/integrationServer";
    const DEPLOYER: &str = "e2ei/11/DEP_12.1.0.0.42/Deployer/Deployer";

    /// An installation carrying one manifest, with `present` of its paths on disk.
    fn with_manifest(label: &str, declared: &[&str], present: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "wm-verify-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        let bms = root.join("install").join("bms");
        fs::create_dir_all(&bms).unwrap();
        let mut text = String::from(
            "name=e2ei/11/DEP_12.1.0.0.1560/IntegrationServer/Deployer/x/BM_Deployer-ALL-Any\n\
             version=12.1.0.0.1560\n\n",
        );
        for path in declared {
            text.push_str(path);
            text.push('\n');
        }
        fs::write(bms.join("BM_Deployer-ALL-Any.contents"), text).unwrap();
        for path in present {
            let file = root.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, "x").unwrap();
        }
        root
    }

    #[test]
    fn a_complete_installation_verifies() {
        let files = ["IntegrationServer/packages/WmDeployer/bin/Deployer.sh"];
        let root = with_manifest("ok", &files, &files);
        let report = verify(&root, None).unwrap();
        assert!(report.is_sound());
        assert_eq!(
            (report.artifacts, report.declared, report.absent),
            (1, 1, 0)
        );
        assert!(report.incomplete.is_empty() && report.never_written.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_absence_is_named_with_the_product_that_claims_it() {
        let declared = [
            "IntegrationServer/packages/WmDeployer/bin/Deployer.sh",
            "IntegrationServer/packages/WmDeployer/code/Gone.class",
        ];
        let root = with_manifest("gap", &declared, &declared[..1]);
        let report = verify(&root, None).unwrap();
        assert!(!report.is_sound());
        assert_eq!((report.absent, report.unexplained), (1, 1));
        let [check] = &report.incomplete[..] else {
            panic!("expected one incomplete artifact");
        };
        assert_eq!(check.artifact, "BM_Deployer-ALL-Any");
        assert_eq!(check.declared, 2);
        assert_eq!(
            check
                .absent
                .iter()
                .map(|a| a.path.as_str())
                .collect::<Vec<_>>(),
            vec![declared[1]]
        );
        assert!(check.product.as_deref().is_some_and(|p| p.contains("DEP_")));
        assert_eq!(check.version.as_deref(), Some("12.1.0.0.1560"));
        // Something was written, so it is not the conclusive finding.
        assert!(!check.never_written);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_filter_matches_the_product_and_not_only_the_artifact_name() {
        let files = ["IntegrationServer/packages/WmDeployer/bin/Deployer.sh"];
        let root = with_manifest("filter", &files, &files);
        // `Deployer` is in both; `DEP_12.1` is only in the product path, which
        // is what a caller reading a plan would have to hand.
        assert_eq!(verify(&root, Some("dep_12.1")).unwrap().artifacts, 1);
        assert_eq!(verify(&root, Some("deployer")).unwrap().artifacts, 1);
        assert_eq!(verify(&root, Some("trading")).unwrap().artifacts, 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_a_fix_replaced_is_not_reported_as_a_fault() {
        // The exact shape measured on a real installation: the manifest names
        // the version installed, a fix left a newer one in its place.
        let declared =
            ["common/runtime/bundles/x/com.webmethods.osgi.agent.profile_12.1.0.0000-0497"];
        let root = with_manifest("fix", &declared, &[]);
        let bundles = root.join("common/runtime/bundles/x");
        fs::create_dir_all(&bundles).unwrap();
        fs::write(
            bundles.join("com.webmethods.osgi.agent.profile_12.1.0.0002-0579"),
            "x",
        )
        .unwrap();
        let report = verify(&root, None).unwrap();
        assert_eq!(
            (report.absent, report.superseded, report.unexplained),
            (1, 1, 0)
        );
        assert!(
            report.is_sound(),
            "a patched installation must read as sound"
        );
        assert!(report.incomplete.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_artifact_entirely_replaced_by_a_fix_is_not_called_never_written() {
        // One declared file, gone, with its replacement beside it. Counting
        // absences alone makes this "nothing was written", which is the
        // strongest thing the report can say and would be wrong.
        let declared = ["common/runtime/bundles/y/com.webmethods.tps.feature_12.1.0.0000-0280.jar"];
        let root = with_manifest("whole", &declared, &[]);
        let bundles = root.join("common/runtime/bundles/y");
        fs::create_dir_all(&bundles).unwrap();
        fs::write(
            bundles.join("com.webmethods.tps.feature_12.1.0.0001-0731.jar"),
            "x",
        )
        .unwrap();
        let report = verify(&root, None).unwrap();
        assert!(report.never_written.is_empty(), "{report:?}");
        assert!(report.is_sound());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_artifact_that_was_never_written_is_the_conclusive_finding() {
        let declared = [
            "IntegrationServer/packages/WmDeployer/bin/Deployer.sh",
            "IntegrationServer/packages/WmDeployer/code/Gone.class",
        ];
        let root = with_manifest("never", &declared, &[]);
        let report = verify(&root, None).unwrap();
        assert!(!report.is_sound());
        let [check] = &report.never_written[..] else {
            panic!(
                "expected one never-written artifact: {:?}",
                report.never_written
            );
        };
        assert_eq!(check.declared, 2);
        assert_eq!(check.unexplained, 2);
        // It belongs in one list, not both.
        assert!(report.incomplete.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_version_stem_cuts_at_the_start_of_the_version() {
        // Cutting at the last separator instead reports every replaced file as
        // unexplained, which is how this was first got wrong.
        assert_eq!(
            version_stem("com.webmethods.osgi.agent.profile_12.1.0.0000-0497"),
            Some("com.webmethods.osgi.agent.profile")
        );
        assert_eq!(version_stem("mina-core-2.2.5.jar"), Some("mina-core"));
        assert_eq!(
            version_stem("org-eclipse-jgit-ssh-jsch-6.3.0.jar"),
            Some("org-eclipse-jgit-ssh-jsch")
        );
        // Nothing version-shaped: it cannot be superseded, so it has no stem.
        assert_eq!(version_stem("Help_Basics.html"), None);
        assert_eq!(version_stem("assets.json"), None);
    }

    #[test]
    fn a_base_directory_is_chosen_on_the_evidence() {
        // One real manifest declares `packages/WmSAP/…` for files that live
        // under `IntegrationServer/`, and says so nowhere.
        let declared = ["packages/WmSAP/code/Adapter.class"];
        let root = with_manifest("base", &declared, &[]);
        let under = root.join("IntegrationServer/packages/WmSAP/code");
        fs::create_dir_all(&under).unwrap();
        fs::write(under.join("Adapter.class"), "x").unwrap();
        let report = verify(&root, None).unwrap();
        assert!(report.is_sound(), "{report:?}");
        assert_eq!(report.absent, 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn both_manifest_shapes_are_read() {
        // Verbatim shape of a manifest the shipped installer wrote: a third
        // header, no blank line, and a mode in front of every path.
        let vendor = "name=e2ei/11/IS_12.1.0.0.938/integrationServer/PIECore/x/BM_Core\n\
                      version=12.1.0.0.938\n\
                      timestamp=1776417008\n\
                      0755 IntegrationServer/packages/WmRoot/assets.json\n\
                      0644 IntegrationServer/packages/WmRoot/a file with spaces.txt\n";
        let manifest = Manifest::parse(vendor);
        assert_eq!(manifest.version.as_deref(), Some("12.1.0.0.938"));
        assert_eq!(
            manifest.files,
            vec![
                "IntegrationServer/packages/WmRoot/assets.json",
                "IntegrationServer/packages/WmRoot/a file with spaces.txt",
            ]
        );

        // And what a native install writes: blank line, bare paths.
        let native = "name=p\nversion=1\n\nIntegrationServer/packages/WmRoot/assets.json\n";
        assert_eq!(
            Manifest::parse(native).files,
            vec!["IntegrationServer/packages/WmRoot/assets.json"]
        );
    }

    #[test]
    fn a_path_is_not_mistaken_for_a_mode() {
        // Four octal digits and a space is a mode; anything else is the path.
        assert_eq!(strip_mode("0755 a/b"), "a/b");
        assert_eq!(strip_mode("0999 a/b"), "0999 a/b");
        assert_eq!(strip_mode("075 a/b"), "075 a/b");
        assert_eq!(strip_mode("dir with space/file"), "dir with space/file");
        assert_eq!(strip_mode("a/b"), "a/b");
    }

    #[test]
    fn headers_stop_at_the_blank_line() {
        // An installed file whose own name begins `version=` must be checked,
        // not swallowed as a header.
        let manifest = Manifest::parse("name=p\nversion=1\n\nversion=oddly-named-file\na/b\n");
        assert_eq!(manifest.product.as_deref(), Some("p"));
        assert_eq!(manifest.version.as_deref(), Some("1"));
        assert_eq!(manifest.files, vec!["version=oddly-named-file", "a/b"]);
    }

    #[test]
    fn a_product_already_there_at_the_same_version_is_not_reinstalled() {
        let delta = delta(
            &[IS_938.to_string(), DEPLOYER.to_string()],
            &installed(&[IS_938]),
        );
        assert_eq!(delta.already_installed, vec![IS_938]);
        assert_eq!(delta.to_install, vec![DEPLOYER]);
        assert!(delta.version_changes.is_empty());
    }

    #[test]
    fn a_different_version_is_reported_rather_than_silently_overwritten() {
        // The case that costs fix level: the catalogue's base version laid down
        // over files Update Manager has patched. It must never land in
        // `to_install`, which is what actually gets unpacked.
        let delta = delta(&[IS_940.to_string()], &installed(&[IS_938]));
        assert!(delta.to_install.is_empty());
        assert!(delta.already_installed.is_empty());
        let [change] = &delta.version_changes[..] else {
            panic!(
                "expected one version change, got {:?}",
                delta.version_changes
            );
        };
        assert_eq!(change.component, "integrationServer");
        assert_eq!(change.installed_version, "12.1.0.0.938");
        assert_eq!(change.catalog_version, "12.1.0.0.940");
    }

    #[test]
    fn an_empty_installation_leaves_the_selection_whole() {
        let delta = delta(&[IS_938.to_string(), DEPLOYER.to_string()], &installed(&[]));
        assert_eq!(delta.to_install, vec![IS_938, DEPLOYER]);
        assert!(delta.version_changes.is_empty());
    }

    #[test]
    fn the_same_component_under_another_code_is_a_different_product() {
        // `integrationServer` under IS and under some other product code are
        // not the same thing; matching on the component name alone would drop a
        // genuine install.
        let other = "e2ei/11/MSC_12.1.0.0.938/integrationServer/integrationServer";
        let delta = delta(&[other.to_string()], &installed(&[IS_938]));
        assert_eq!(delta.to_install, vec![other]);
        assert!(delta.version_changes.is_empty());
    }

    #[test]
    fn forcing_installs_the_version_changes_too_and_nothing_else() {
        let delta = delta(
            &[IS_940.to_string(), DEPLOYER.to_string(), IS_938.to_string()],
            &installed(&[IS_938]),
        );
        // IS_938 is already there and stays out even when forcing: rewriting a
        // product with the identical version only risks undoing a fix.
        assert_eq!(delta.forced(), vec![DEPLOYER, IS_940]);
    }

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
