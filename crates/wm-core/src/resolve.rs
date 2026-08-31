//! Computing a profile's bundle set without a p2 director.
//!
//! p2 resolves over `content.xml`, whose requirements and capabilities are
//! deliberately loose — `java.package` ranges with many providers, singletons,
//! environment filters — which is why it needs a SAT solver. But that document
//! is p2's *verification* layer. The *definition* layer is each feature's own
//! `feature.xml`, and it is exact:
//!
//! ```xml
//! <feature id="com.webmethods.plm.spm.asset.feature" version="12.1.0.0000-0417">
//!   <requires>
//!     <import feature="com.webmethods.repository.lar.registry"
//!             version="12.1.0.0000-0000" match="greaterOrEqual"/>
//!   </requires>
//!   <plugin id="com.webmethods.plm.spm.asset.management" version="12.1.0.0000-0417"/>
//! </feature>
//! ```
//!
//! Plugins are named at one exact version; feature imports carry a floor. That
//! is a graph traversal with a deterministic tie-break rather than a search.
//!
//! Two passes follow it. Environment filters drop the fragments meant for
//! another platform — only a handful of entries carry one. Then a repair pass
//! reads each selected bundle's own `MANIFEST.MF` and adds a provider for any
//! `Import-Package` or `Require-Bundle` nothing in the set satisfies; over an
//! almost-closed set each has an obvious answer, which is what makes this
//! tractable where a search from nothing is not.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::profile::Bundle;
use crate::Result;

/// Target environment, matched against feature entries' `os`/`ws`/`arch`.
#[derive(Debug, Clone, Serialize)]
pub struct Environment {
    /// `linux`, `win32`, `macosx`.
    pub os: String,
    /// Windowing system, `gtk` on Linux.
    pub ws: String,
    /// `x86_64`.
    pub arch: String,
}

impl Default for Environment {
    fn default() -> Self {
        Self {
            os: "linux".into(),
            ws: "gtk".into(),
            arch: "x86_64".into(),
        }
    }
}

/// A plugin as a feature names it.
#[derive(Debug, Clone)]
pub struct PluginRef {
    id: String,
    version: String,
    os: Option<String>,
    ws: Option<String>,
    arch: Option<String>,
}

impl PluginRef {
    /// Whether this entry applies to `env`. An absent attribute means "any".
    fn applies(&self, env: &Environment) -> bool {
        let matches = |attr: &Option<String>, actual: &str| {
            attr.as_ref()
                .is_none_or(|v| v.split(',').any(|part| part.trim() == actual))
        };
        matches(&self.os, &env.os) && matches(&self.ws, &env.ws) && matches(&self.arch, &env.arch)
    }
}

/// One feature.
#[derive(Debug, Clone)]
pub struct Feature {
    /// Feature id.
    pub id: String,
    /// Feature version.
    pub version: String,
    plugins: Vec<PluginRef>,
    /// Other features it pulls in, by id.
    requires: Vec<String>,
}

/// Every feature found in an installation's repositories.
#[derive(Debug, Clone, Default)]
pub struct FeatureIndex {
    features: BTreeMap<(String, String), Feature>,
    by_id: BTreeMap<String, Vec<String>>,
}

impl FeatureIndex {
    /// Read every `features/*.jar` under `common/runtime/bundles`.
    pub fn load(wm_home: &Path) -> Result<Self> {
        let mut index = Self::default();
        let root = wm_home.join("common").join("runtime").join("bundles");
        let Ok(groups) = fs::read_dir(&root) else {
            return Ok(index);
        };
        for group in groups.flatten() {
            let features = group.path().join("eclipse").join("features");
            let Ok(entries) = fs::read_dir(&features) else {
                continue;
            };
            for entry in entries.flatten() {
                if let Some(feature) = read_feature(&entry.path()) {
                    index
                        .by_id
                        .entry(feature.id.clone())
                        .or_default()
                        .push(feature.version.clone());
                    index
                        .features
                        .insert((feature.id.clone(), feature.version.clone()), feature);
                }
            }
        }
        Ok(index)
    }

    /// Number of features indexed.
    pub fn len(&self) -> usize {
        self.features.len()
    }

    /// Whether nothing was found.
    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }

    /// The highest version of `id`.
    fn newest(&self, id: &str) -> Option<&Feature> {
        let versions = self.by_id.get(id)?;
        let best = versions
            .iter()
            .max_by(|a, b| version_key(a).cmp(&version_key(b)))?;
        self.features.get(&(id.to_string(), best.clone()))
    }
}

/// Read one `feature.xml` out of a feature jar.
fn read_feature(jar: &Path) -> Option<Feature> {
    let file = fs::File::open(jar).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut xml = String::new();
    zip.by_name("feature.xml")
        .ok()?
        .read_to_string(&mut xml)
        .ok()?;

    let start = xml.find("<feature ")?;
    let header = &xml[start..start + xml[start..].find('>')?];
    let id = attribute(header, "id")?;
    let version = attribute(header, "version")?;

    let mut plugins = Vec::new();
    for element in elements(&xml, "<plugin ") {
        let (Some(id), Some(version)) = (attribute(&element, "id"), attribute(&element, "version"))
        else {
            continue;
        };
        plugins.push(PluginRef {
            id,
            version,
            os: attribute(&element, "os"),
            ws: attribute(&element, "ws"),
            arch: attribute(&element, "arch"),
        });
    }

    // `import` states a dependency, `includes` a nested feature; both bring the
    // named feature into the profile.
    let mut requires: Vec<String> = elements(&xml, "<import ")
        .iter()
        .filter_map(|e| attribute(e, "feature"))
        .collect();
    requires.extend(
        elements(&xml, "<includes ")
            .iter()
            .filter_map(|e| attribute(e, "id")),
    );

    Some(Feature {
        id,
        version,
        plugins,
        requires,
    })
}

/// Every bundle jar the installation carries, by id and normalised version.
#[derive(Debug, Clone, Default)]
pub struct BundleIndex {
    jars: BTreeMap<(String, String), PathBuf>,
}

impl BundleIndex {
    /// Index `common/runtime/bundles/*/eclipse/plugins/*.jar`.
    pub fn load(wm_home: &Path) -> Self {
        let mut jars = BTreeMap::new();
        let root = wm_home.join("common").join("runtime").join("bundles");
        let Ok(groups) = fs::read_dir(&root) else {
            return Self { jars };
        };
        for group in groups.flatten() {
            let plugins = group.path().join("eclipse").join("plugins");
            let Ok(entries) = fs::read_dir(&plugins) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(stem) = name.strip_suffix(".jar") else {
                    continue;
                };
                let Some((id, version)) = stem.rsplit_once('_') else {
                    continue;
                };
                jars.entry((id.to_string(), normalise_version(version)))
                    .or_insert(path);
            }
        }
        Self { jars }
    }

    /// Number of jars indexed.
    pub fn len(&self) -> usize {
        self.jars.len()
    }

    /// Whether nothing was found.
    pub fn is_empty(&self) -> bool {
        self.jars.is_empty()
    }

    /// Look a plugin up, tolerating a truncated version.
    ///
    /// Features write OSGi versions short — `version="1.84"` for a jar named
    /// `bcprov_1.84.0.jar` — so both sides are padded to `major.minor.micro`
    /// before comparison.
    pub fn find(&self, id: &str, version: &str) -> Option<&PathBuf> {
        self.jars.get(&(id.to_string(), normalise_version(version)))
    }

    /// The jar file name for a plugin, if present.
    pub fn jar_name(&self, id: &str, version: &str) -> Option<String> {
        self.find(id, version)
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    }

    /// Every indexed (id, version) pair.
    pub fn entries(&self) -> impl Iterator<Item = (&(String, String), &PathBuf)> {
        self.jars.iter()
    }
}

/// Start levels declared by p2 touchpoint instructions.
///
/// Not a resolution input: `content.xml` carries
/// `setStartLevel(startLevel:n)` and `markStarted(started:b)` per unit, and the
/// overwhelming majority of bundles take the framework default instead.
#[derive(Debug, Clone, Default)]
pub struct StartLevels {
    by_id: BTreeMap<String, (String, String)>,
    /// Fragments, which the framework cannot start.
    fragments: BTreeSet<String>,
}

impl StartLevels {
    /// Collect start levels and started flags from every source the product uses.
    ///
    /// There are four, and all four matter:
    ///
    /// 1. `configure.<bundle>` units in a repository `content.xml`;
    /// 2. `META-INF/p2.inf` inside a bundle jar, declaring its own level;
    /// 3. `p2.inf` inside a *feature* jar, declaring synthetic `configure.<bundle>`
    ///    units for bundles that carry no metadata of their own;
    /// 4. the fragment rule — a fragment is never started, because the
    ///    framework has nothing to start.
    ///
    /// Everything else takes the product default of level 4, started.
    pub fn load(wm_home: &Path, bundles: &BundleIndex) -> Self {
        let mut by_id = BTreeMap::new();
        let root = wm_home.join("common").join("runtime").join("bundles");

        if let Ok(groups) = fs::read_dir(&root) {
            for group in groups.flatten() {
                let eclipse = group.path().join("eclipse");
                if let Ok(xml) = fs::read_to_string(eclipse.join("content.xml")) {
                    for (id, level, started) in parse_touchpoints(&xml) {
                        by_id.insert(id, (level, started));
                    }
                }
                // Feature jars declare configure units for their own bundles.
                if let Ok(features) = fs::read_dir(eclipse.join("features")) {
                    for feature in features.flatten() {
                        let path = feature.path();
                        if path.extension().is_none_or(|e| e != "jar") {
                            continue;
                        }
                        if let Some(text) = read_entry(&path, "p2.inf") {
                            for (id, level, started) in parse_feature_p2inf(&text) {
                                by_id.insert(id, (level, started));
                            }
                        }
                    }
                }
            }
        }

        // A bundle's own p2.inf outranks a feature's claim about it.
        for ((id, _), path) in bundles.entries() {
            let Some(text) = read_entry(path, "META-INF/p2.inf") else {
                continue;
            };
            if let Some((level, started)) = parse_configure(&text) {
                by_id.insert(id.clone(), (level, started));
            }
        }

        let mut fragments = BTreeSet::new();
        for ((id, _), path) in bundles.entries() {
            if is_fragment(path) {
                fragments.insert(id.clone());
            }
        }
        Self { by_id, fragments }
    }

    /// Start level and started flag for a bundle.
    ///
    /// A bundle nothing says anything about takes the product default: started,
    /// at level 4.
    pub fn for_bundle(&self, id: &str) -> (String, String) {
        // The system bundle is the framework itself: it is already running by
        // the time the configurator reads this file, so it is listed at -1.
        if id == "org.eclipse.osgi" {
            return ("-1".to_string(), "true".to_string());
        }
        if self.fragments.contains(id) {
            let level = self
                .by_id
                .get(id)
                .map(|(l, _)| l.clone())
                .unwrap_or_else(|| "4".to_string());
            return (level, "false".to_string());
        }
        self.by_id
            .get(id)
            .cloned()
            .unwrap_or_else(|| ("4".to_string(), "true".to_string()))
    }
}

/// Read one entry out of a jar, if it has it.
fn read_entry(jar: &Path, name: &str) -> Option<String> {
    let file = fs::File::open(jar).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut text = String::new();
    zip.by_name(name).ok()?.read_to_string(&mut text).ok()?;
    Some(text)
}

/// Does this jar declare a `Fragment-Host`?
fn is_fragment(jar: &Path) -> bool {
    read_entry(jar, "META-INF/MANIFEST.MF")
        .map(|text| parse_manifest_headers(&text).contains_key("Fragment-Host"))
        .unwrap_or(false)
}

/// `instructions.configure=...setStartLevel(startLevel:N); markStarted(started:B);`
fn parse_configure(text: &str) -> Option<(String, String)> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("instructions.configure="))?;
    let level = between(line, "setStartLevel(startLevel:", ")");
    let started = between(line, "markStarted(started:", ")");
    if level.is_none() && started.is_none() {
        return None;
    }
    Some((
        level
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "4".into()),
        started
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "true".into()),
    ))
}

/// A feature's `p2.inf` declares synthetic units keyed by index:
/// `units.4.id=configure.<bundle>` with a matching
/// `units.4.instructions.configure=...`. The two are joined on that index.
fn parse_feature_p2inf(text: &str) -> Vec<(String, String, String)> {
    // Logical lines: a trailing backslash continues onto the next.
    let mut joined = Vec::new();
    let mut current = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        match line.strip_suffix('\\') {
            Some(head) => current.push_str(head),
            None => {
                current.push_str(line);
                if !current.is_empty() {
                    joined.push(std::mem::take(&mut current));
                }
            }
        }
    }
    if !current.is_empty() {
        joined.push(current);
    }

    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let mut configures: BTreeMap<String, String> = BTreeMap::new();
    for line in &joined {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(rest) = key.trim().strip_prefix("units.") else {
            continue;
        };
        let Some((index, field)) = rest.split_once('.') else {
            continue;
        };
        match field {
            "id" => {
                if let Some(bundle) = value.trim().strip_prefix("configure.") {
                    ids.insert(index.to_string(), bundle.to_string());
                }
            }
            "instructions.configure" => {
                configures.insert(index.to_string(), value.to_string());
            }
            _ => {}
        }
    }

    ids.into_iter()
        .filter_map(|(index, bundle)| {
            let configure = configures.get(&index)?;
            let level = between(configure, "setStartLevel(startLevel:", ")")
                .or_else(|| between(configure, "setStartLevel(startLevel :", ")"))
                .or_else(|| between(configure, "setStartLevel(startLevel: ", ")"));
            let started = between(configure, "markStarted(started:", ")")
                .or_else(|| between(configure, "markStarted(started: ", ")"));
            if level.is_none() && started.is_none() {
                return None;
            }
            Some((
                bundle,
                level
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| "4".into()),
                started
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| "true".into()),
            ))
        })
        .collect()
}

/// Extract start levels from the `configure.<bundle>` units of a `content.xml`.
///
/// The start level does not live on the bundle's own installable unit. p2 emits
/// a separate configuration unit named `configure.<symbolic name>` whose
/// `configure` instruction carries `setStartLevel` and `markStarted` — and
/// whose `unconfigure` instruction carries the *opposite* values, so the two
/// must not be confused.
fn parse_touchpoints(xml: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<unit ") {
        let after = &rest[start..];
        let Some(header_end) = after.find('>') else {
            break;
        };
        let header = &after[..header_end];
        // A self-closing unit has no body, and searching for `</unit>` would
        // run into the *next* unit and attribute its instructions here.
        let body = if header.trim_end().ends_with('/') {
            header
        } else {
            match after.find("</unit>") {
                Some(body_end) => &after[..body_end],
                None => header,
            }
        };
        rest = &after[header_end..];

        let Some(id) = attribute(header, "id") else {
            continue;
        };
        let Some(bundle) = id.strip_prefix("configure.") else {
            continue;
        };
        let Some(configure) = between(body, "<instruction key='configure'>", "</instruction>")
            .or_else(|| between(body, "<instruction key=\"configure\">", "</instruction>"))
        else {
            continue;
        };
        let level = between(&configure, "setStartLevel(startLevel:", ")");
        let started = between(&configure, "markStarted(started:", ")");
        if level.is_some() || started.is_some() {
            out.push((
                bundle.to_string(),
                level
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| "4".into()),
                started
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| "false".into()),
            ));
        }
    }
    out
}

/// What the resolver produced.
#[derive(Debug, Clone, Serialize)]
pub struct Resolution {
    /// Bundles, ready for `bundles.info`.
    pub bundles: Vec<Bundle>,
    /// Features traversed.
    pub features: usize,
    /// Plugins a feature names that no jar provides.
    pub unresolved_plugins: Vec<String>,
    /// Feature imports that match no feature.
    pub unresolved_features: Vec<String>,
    /// Bundles added by the repair pass, with the import that pulled them in.
    pub repaired: Vec<String>,
    /// Imports nothing in the installation satisfies.
    pub unsatisfied_imports: Vec<String>,
}

/// Compute the bundle set for a set of root features.
pub fn resolve(
    features: &FeatureIndex,
    bundles: &BundleIndex,
    levels: &StartLevels,
    filters: &PlatformFilters,
    roots: &[String],
    env: &Environment,
) -> Resolution {
    let mut seen_features: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = roots.to_vec();
    // (symbolic name, version) -> jar name. Keyed on both, because a
    // profile legitimately installs several builds of the same bundle
    // when consumers ask for disjoint version ranges.
    let mut chosen: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut unresolved_plugins = Vec::new();
    let mut unresolved_features = Vec::new();

    while let Some(id) = queue.pop() {
        if !seen_features.insert(id.clone()) {
            continue;
        }
        let Some(feature) = features.newest(&id) else {
            unresolved_features.push(id);
            continue;
        };
        for plugin in &feature.plugins {
            // A plugin can be excluded two ways: by an attribute on the
            // feature entry, or by an LDAP filter in the repository metadata.
            if !plugin.applies(env) || !filters.admits(&plugin.id, env) {
                continue;
            }
            match bundles.jar_name(&plugin.id, &plugin.version) {
                Some(jar) => {
                    let version = jar_version(&jar).map(|(_, v)| v).unwrap_or_default();
                    chosen.insert((plugin.id.clone(), version), jar);
                }
                None => unresolved_plugins.push(format!("{}_{}", plugin.id, plugin.version)),
            }
        }
        queue.extend(feature.requires.iter().cloned());
    }

    let (repaired, unsatisfied_imports) = repair(&mut chosen, bundles, filters, env);

    let mut out: Vec<Bundle> = chosen
        .iter()
        .map(|((id, version), jar)| {
            let version = version.clone();
            let (start_level, started) = levels.for_bundle(id);
            Bundle {
                name: id.clone(),
                version,
                jar: jar.clone(),
                location: format!("plugins/{jar}"),
                start_level,
                started,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));

    Resolution {
        bundles: out,
        features: seen_features.len(),
        unresolved_plugins,
        unresolved_features,
        repaired,
        unsatisfied_imports,
    }
}

/// Add providers for imports the selected set does not satisfy.
///
/// The set arriving here is nearly closed, so each unsatisfied import usually
/// has one provider. Where several export the same package, the one whose
/// symbolic name is already present wins, then the highest version — a rule, not
/// a search.
/// Pull in whatever the chosen bundles need but the feature graph did not name.
///
/// This is the whole of the lightweight alternative to a p2 solve. It is not a
/// solver: there is no minimality objective and no backtracking. It closes the
/// wiring graph greedily, and the one thing it must get right — the thing that
/// decides whether the framework comes up — is the OSGi version range. An
/// import of `[1.79.0,1.80.0)` is not satisfied by the newest build on disk.
fn repair(
    chosen: &mut BTreeMap<(String, String), String>,
    bundles: &BundleIndex,
    filters: &PlatformFilters,
    env: &Environment,
) -> (Vec<String>, Vec<String>) {
    // package -> bundles exporting it at a given package version.
    let mut exporters: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
    let mut manifests: BTreeMap<(String, String), Manifest> = BTreeMap::new();
    for ((id, version), path) in bundles.entries() {
        let Some(manifest) = read_manifest(path) else {
            continue;
        };
        for export in &manifest.exports {
            exporters.entry(export.name.clone()).or_default().push((
                id.clone(),
                version.clone(),
                export.version.clone(),
            ));
        }
        manifests.insert((id.clone(), version.clone()), manifest);
    }

    let mut repaired = Vec::new();
    let mut unsatisfied = Vec::new();
    let mut refused: BTreeSet<String> = BTreeSet::new();
    let mut changed = true;
    while changed {
        changed = false;

        // What the current selection exports, package by package.
        let mut exported: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (id, version) in chosen.keys() {
            let Some(manifest) = manifests.get(&(id.clone(), normalise_version(version))) else {
                continue;
            };
            for export in &manifest.exports {
                exported
                    .entry(export.name.clone())
                    .or_default()
                    .push(export.version.clone());
            }
        }
        let installed: BTreeSet<(String, String)> = chosen
            .keys()
            .map(|(id, v)| (id.clone(), normalise_version(v)))
            .collect();

        let pending: Vec<(String, String)> = chosen.keys().cloned().collect();
        'outer: for (id, version) in &pending {
            let Some(manifest) = manifests.get(&(id.clone(), normalise_version(version))) else {
                continue;
            };

            // Require-Bundle names a bundle outright, so there is no package to
            // match — only the range on the bundle version itself.
            for required in &manifest.requires {
                if installed
                    .iter()
                    .any(|(bid, bver)| bid == &required.name && required.range.contains(bver))
                {
                    continue;
                }
                let pick = bundles
                    .entries()
                    .filter(|((bid, bver), _)| {
                        bid == &required.name
                            && required.range.contains(bver)
                            && filters.admits(bid, env)
                    })
                    .max_by(|((_, a), _), ((_, b), _)| version_key(a).cmp(&version_key(b)))
                    .map(|((bid, bver), _)| (bid.clone(), bver.clone()));
                match pick {
                    Some((bid, bver)) => {
                        if let Some(jar) = bundles.jar_name(&bid, &bver) {
                            let key = jar_version(&jar)
                                .map(|(_, v)| (bid.clone(), v))
                                .unwrap_or((bid.clone(), bver));
                            if chosen.insert(key, jar).is_none() {
                                repaired.push(format!("{} (required by {id})", required.name));
                                changed = true;
                                break 'outer;
                            }
                        }
                    }
                    None => {
                        refused.insert(format!("{} (required by {id})", required.name));
                    }
                }
            }

            for import in &manifest.imports {
                let satisfied = exported
                    .get(&import.name)
                    .is_some_and(|versions| versions.iter().any(|v| import.range.contains(v)));
                if satisfied {
                    continue;
                }
                let Some(candidates) = exporters.get(&import.name) else {
                    refused.insert(format!("{} (imported by {id})", import.name));
                    continue;
                };
                let pick = candidates
                    .iter()
                    .filter(|(pid, _, pkg_version)| {
                        import.range.contains(pkg_version) && filters.admits(pid, env)
                    })
                    // Prefer the lowest build that satisfies the range: a
                    // narrow range is a compatibility statement, and reaching
                    // past it is what drags in a second, unwanted family.
                    .min_by(|a, b| version_key(&a.1).cmp(&version_key(&b.1)));
                match pick {
                    Some((pid, pver, _)) => {
                        if let Some(jar) = bundles.jar_name(pid, pver) {
                            let key = jar_version(&jar)
                                .map(|(_, v)| (pid.clone(), v))
                                .unwrap_or((pid.clone(), pver.clone()));
                            if chosen.insert(key, jar).is_none() {
                                repaired.push(format!("{pid} (exports {}, for {id})", import.name));
                                changed = true;
                                break 'outer;
                            }
                        }
                    }
                    None => {
                        refused.insert(format!("{} (imported by {id})", import.name));
                    }
                }
            }
        }
    }
    unsatisfied.extend(refused);
    unsatisfied.sort();
    unsatisfied.dedup();
    (repaired, unsatisfied)
}

/// An OSGi version range: `[1.79.0,1.80.0)`, or a bare floor `1.79.0`.
#[derive(Debug, Clone, Default)]
pub struct VersionRange {
    floor: Option<String>,
    floor_inclusive: bool,
    ceiling: Option<String>,
    ceiling_inclusive: bool,
}

impl VersionRange {
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        let (floor_inclusive, ceiling_inclusive) = (!text.starts_with('('), !text.ends_with(')'));
        let inner = text
            .trim_start_matches(['[', '('])
            .trim_end_matches([']', ')']);
        match inner.split_once(',') {
            Some((low, high)) => Self {
                floor: non_empty(low),
                floor_inclusive,
                ceiling: non_empty(high),
                ceiling_inclusive,
            },
            // A bare version is a floor with no ceiling, per the OSGi spec.
            None => Self {
                floor: non_empty(inner),
                floor_inclusive: true,
                ceiling: None,
                ceiling_inclusive: false,
            },
        }
    }

    pub fn contains(&self, version: &str) -> bool {
        let v = version_key(version);
        if let Some(floor) = &self.floor {
            let f = version_key(floor);
            if v < f || (v == f && !self.floor_inclusive) {
                return false;
            }
        }
        if let Some(ceiling) = &self.ceiling {
            let c = version_key(ceiling);
            if v > c || (v == c && !self.ceiling_inclusive) {
                return false;
            }
        }
        true
    }
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn jar_version(jar: &str) -> Option<(String, String)> {
    let stem = jar.strip_suffix(".jar")?;
    let (id, version) = stem.rsplit_once('_')?;
    Some((id.to_string(), version.to_string()))
}

/// The OSGi headers a bundle declares.
struct Manifest {
    exports: Vec<Clause>,
    imports: Vec<Clause>,
    /// `Require-Bundle`: symbolic names, which name a bundle outright rather
    /// than a package.
    requires: Vec<Clause>,
}

/// One clause of an OSGi header: a name plus the version constraint on it.
#[derive(Debug, Clone)]
struct Clause {
    name: String,
    /// For an export, the single version the package is offered at; empty when
    /// the header omits it, which OSGi reads as 0.0.0.
    version: String,
    /// For an import or a require, the range the consumer will accept.
    range: VersionRange,
}

fn read_manifest(jar: &Path) -> Option<Manifest> {
    let file = fs::File::open(jar).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut text = String::new();
    zip.by_name("META-INF/MANIFEST.MF")
        .ok()?
        .read_to_string(&mut text)
        .ok()?;
    let headers = parse_manifest_headers(&text);
    Some(Manifest {
        exports: header_clauses(headers.get("Export-Package"), false, "version"),
        imports: header_clauses(headers.get("Import-Package"), true, "version"),
        requires: header_clauses(headers.get("Require-Bundle"), true, "bundle-version"),
    })
}

/// Split a header into clauses, keeping the version attribute.
///
/// `drop_optional` discards clauses marked `resolution:=optional`: the
/// framework does not fail on those, so pulling in a bundle to satisfy one is
/// how a closure balloons.
fn header_clauses(header: Option<&String>, drop_optional: bool, attr: &str) -> Vec<Clause> {
    let Some(header) = header else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for clause in split_clauses(header) {
        if drop_optional && clause.contains("resolution:=optional") {
            continue;
        }
        let mut parts = clause.split(';').map(str::trim);
        let Some(name) = parts.next().filter(|n| !n.is_empty()) else {
            continue;
        };
        let mut version = String::new();
        for part in parts {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            if key.trim_end_matches(':') == attr {
                version = value.trim().trim_matches('"').to_string();
            }
        }
        out.push(Clause {
            name: name.trim_matches('"').to_string(),
            version: if version.is_empty() {
                "0.0.0".to_string()
            } else {
                version.clone()
            },
            range: VersionRange::parse(&version),
        });
    }
    out
}

fn parse_manifest_headers(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut key: Option<String> = None;
    let mut value = String::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix(' ') {
            value.push_str(rest);
            continue;
        }
        if let Some(k) = key.take() {
            map.insert(k, value.trim().to_string());
            value.clear();
        }
        if let Some((k, v)) = line.split_once(':') {
            key = Some(k.trim().to_string());
            value = v.trim().to_string();
        }
    }
    if let Some(k) = key {
        map.insert(k, value.trim().to_string());
    }
    map
}

fn split_clauses(header: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in header.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ',' if !quoted => {
                out.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// Pad an OSGi version to `major.minor.micro`, keeping any qualifier.
pub fn normalise_version(version: &str) -> String {
    let mut numeric = Vec::new();
    let mut rest = version;
    while numeric.len() < 3 {
        let (head, tail) = match rest.split_once('.') {
            Some((h, t)) => (h, Some(t)),
            None => (rest, None),
        };
        if head.parse::<u64>().is_err() {
            break;
        }
        numeric.push(head.to_string());
        match tail {
            Some(t) => rest = t,
            None => {
                rest = "";
                break;
            }
        }
    }
    if numeric.is_empty() {
        return version.to_string();
    }
    while numeric.len() < 3 {
        numeric.push("0".to_string());
    }
    let base = numeric.join(".");
    if rest.is_empty() {
        base
    } else {
        format!("{base}.{rest}")
    }
}

fn version_key(v: &str) -> Vec<u64> {
    v.split(['.', '-'])
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

/// Value of an attribute, accepting either quoting style.
///
/// `feature.xml` is written with double quotes and `content.xml` with single
/// ones, and both are read here.
fn attribute(element: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{name}={quote}");
        if let Some(index) = element.find(&needle) {
            let start = index + needle.len();
            if let Some(offset) = element[start..].find(quote) {
                return Some(element[start..start + offset].to_string());
            }
        }
    }
    None
}

/// All elements starting with `open`, up to their closing bracket.
fn elements(xml: &str, open: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(open) {
        let after = &rest[start..];
        let Some(end) = after.find('>') else { break };
        out.push(after[..end].to_string());
        rest = &after[end..];
    }
    out
}

fn between(text: &str, open: &str, close: &str) -> Option<String> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(text[start..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_padded_to_three_parts() {
        // A feature writes 1.84 for a jar named 1.84.0.
        assert_eq!(normalise_version("1.84"), "1.84.0");
        assert_eq!(normalise_version("1.84.0"), "1.84.0");
        assert_eq!(normalise_version("2"), "2.0.0");
        // Qualifiers survive, in either spelling.
        // A full version passes through unchanged, qualifier included.
        assert_eq!(normalise_version("12.1.0.0002-0579"), "12.1.0.0002-0579");
        assert_eq!(normalise_version("1.5.500.v20250306"), "1.5.500.v20250306");
        // Only the truncation is repaired, so distinct qualifiers stay distinct.
        assert_ne!(
            normalise_version("1.5.500.v20250306"),
            normalise_version("1.5.500.v20250306-1127")
        );
    }

    #[test]
    fn environment_filters_drop_other_platforms() {
        let env = Environment::default();
        let any = PluginRef {
            id: "a".into(),
            version: "1".into(),
            os: None,
            ws: None,
            arch: None,
        };
        assert!(any.applies(&env));
        let win = PluginRef {
            os: Some("win32".into()),
            ..clone_ref(&any)
        };
        assert!(!win.applies(&env));
        let linux = PluginRef {
            os: Some("linux,macosx".into()),
            ..clone_ref(&any)
        };
        assert!(linux.applies(&env));
    }

    fn clone_ref(p: &PluginRef) -> PluginRef {
        PluginRef {
            id: p.id.clone(),
            version: p.version.clone(),
            os: p.os.clone(),
            ws: p.ws.clone(),
            arch: p.arch.clone(),
        }
    }

    #[test]
    fn osgi_headers_split_outside_quotes_only() {
        let header = "org.a;version=\"[1.0,2.0)\",org.b,org.c;resolution:=optional";
        assert_eq!(split_clauses(header).len(), 3);
        // Optional imports are not requirements.
        let clauses = header_clauses(Some(&header.to_string()), true, "version");
        let names: Vec<&str> = clauses.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["org.a", "org.b"]);
        // The version attribute survives the split, as the range to match on.
        assert!(clauses[0].range.contains("1.5.0"));
        assert!(!clauses[0].range.contains("2.0.0"));
        // An unversioned clause accepts anything.
        assert!(clauses[1].range.contains("99.0.0"));
        // Exports keep everything.
        assert_eq!(
            header_clauses(Some(&header.to_string()), false, "version").len(),
            3
        );
    }

    #[test]
    fn attributes_are_read_in_either_quoting_style() {
        // feature.xml uses double quotes, content.xml single ones.
        assert_eq!(
            attribute("<plugin id=\"a\" version=\"1\"", "id").as_deref(),
            Some("a")
        );
        assert_eq!(
            attribute("<unit id='b' version='2'", "version").as_deref(),
            Some("2")
        );
        assert_eq!(attribute("<unit id='b'", "missing"), None);
    }

    #[test]
    fn touchpoints_are_read_per_unit() {
        let xml = "<unit id='configure.a' version='1'><touchpointData>\
                   <instruction key='configure'>setStartLevel(startLevel:2);markStarted(started:true);\
                   </instruction></touchpointData></unit>\
                   <unit id='configure.b' version='1'><touchpointData>\
                   <instruction key='configure'>markStarted(started:false);\
                   </instruction></touchpointData></unit>";
        let found = parse_touchpoints(xml);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0], ("a".into(), "2".into(), "true".into()));
        // A unit that only marks started keeps the default level.
        assert_eq!(found[1], ("b".into(), "4".into(), "false".into()));
    }

    #[test]
    fn a_bundle_with_no_touchpoint_takes_the_framework_default() {
        let levels = StartLevels::default();
        assert_eq!(
            levels.for_bundle("anything"),
            ("4".to_string(), "true".to_string())
        );
    }

    #[test]
    fn manifest_continuation_lines_are_joined() {
        let text = "Bundle-SymbolicName: com.example\n\
                    Import-Package: org.foo;version=\"[1.0,\n \
                    2.0)\",org.bar\n\
                    \n";
        let headers = parse_manifest_headers(text);
        let clauses = header_clauses(headers.get("Import-Package"), true, "version");
        let names: Vec<&str> = clauses.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["org.foo", "org.bar"]);
    }

    #[test]
    fn version_ranges_follow_osgi_bracket_semantics() {
        let range = VersionRange::parse("[1.79.0,1.80.0)");
        assert!(range.contains("1.79.0"));
        assert!(range.contains("1.79.9"));
        assert!(!range.contains("1.80.0"));
        assert!(!range.contains("1.78.1"));
        // A bare version is a floor, not an exact match.
        let floor = VersionRange::parse("1.79.0");
        assert!(floor.contains("1.84.0"));
        assert!(!floor.contains("1.78.1"));
        // An absent range accepts anything.
        assert!(VersionRange::parse("").contains("0.0.1"));
    }

    #[test]
    fn start_levels_come_from_the_configure_unit_not_the_bundle_unit() {
        let xml = "\
<unit id='com.example.thing' version='1.0.0'>\
<provides size='1'><provided namespace='osgi.bundle' name='com.example.thing'/></provides>\
</unit>\
<unit id='configure.com.example.thing' version='1.0.0'>\
<touchpointData size='1'><instructions size='2'>\
<instruction key='configure'>setStartLevel(startLevel:2); markStarted(started:true);</instruction>\
<instruction key='unconfigure'>setStartLevel(startLevel:-1); markStarted(started:false);</instruction>\
</instructions></touchpointData></unit>";
        let levels = parse_touchpoints(xml);
        // The bundle unit contributes nothing; the configure unit contributes
        // the level, and the *unconfigure* values must not leak in.
        assert_eq!(
            levels,
            vec![(
                "com.example.thing".to_string(),
                "2".to_string(),
                "true".to_string()
            )]
        );
    }

    #[test]
    fn feature_p2inf_units_are_joined_on_their_index() {
        let text = "\
units.4.id=configure.com.webmethods.osgi.config.store.props\n\
units.4.instructions.configure=setStartLevel(startLevel :2); markStarted(started: true);\n\
units.4.instructions.unconfigure=setStartLevel(startLevel: -1); markStarted(started: false);\n\
units.7.id=configure.com.example.other\n\
units.7.instructions.configure=setStartLevel(startLevel:5);\n";
        let mut levels = parse_feature_p2inf(text);
        levels.sort();
        assert_eq!(
            levels,
            vec![
                (
                    "com.example.other".to_string(),
                    "5".to_string(),
                    "true".to_string()
                ),
                (
                    "com.webmethods.osgi.config.store.props".to_string(),
                    "2".to_string(),
                    "true".to_string()
                ),
            ]
        );
    }

    #[test]
    fn platform_filters_are_read_from_the_requirement_edge() {
        let xml = "<requires size='2'>\
                   <required namespace='org.eclipse.equinox.p2.iu' name='thing.w64'>\
                   <filter>(&amp;(osgi.arch=x86_64)(osgi.os=win32))</filter>\
                   </required>\
                   <required namespace='org.eclipse.equinox.p2.iu' name='thing.any'>\
                   <filter>(org.eclipse.update.install.sources=true)</filter>\
                   </required></requires>";
        let found = parse_requirement_filters(xml);
        // Only the environment filter is a platform constraint; p2's own
        // provisioning switches must not exclude anything.
        assert_eq!(
            found,
            vec![(
                "thing.w64".to_string(),
                "(&(osgi.arch=x86_64)(osgi.os=win32))".to_string()
            )]
        );
    }

    #[test]
    fn a_nested_requirement_does_not_lend_its_filter_to_its_parent() {
        let xml = "<required name='outer'>\
                   <required name='inner'><filter>(osgi.os=win32)</filter></required>\
                   </required>";
        let found = parse_requirement_filters(xml);
        assert_eq!(
            found,
            vec![("inner".to_string(), "(osgi.os=win32)".to_string())]
        );
    }

    #[test]
    fn ldap_filters_evaluate_against_the_environment() {
        let linux = Environment::default();
        assert!(!ldap_matches(
            "(&(osgi.arch=x86_64)(osgi.os=win32))",
            &linux
        ));
        assert!(ldap_matches("(&(osgi.arch=x86_64)(osgi.os=linux))", &linux));
        assert!(ldap_matches("(!(osgi.os=win32))", &linux));
        assert!(!ldap_matches("(!(osgi.os=linux))", &linux));
        assert!(ldap_matches(
            "(|(osgi.os=aix)(&(osgi.arch=x86_64)(osgi.os=linux)))",
            &linux
        ));
        assert!(!ldap_matches("(|(osgi.os=aix)(osgi.os=macosx))", &linux));
        // An unmodelled key is not a constraint.
        assert!(ldap_matches(
            "(org.eclipse.update.install.sources=true)",
            &linux
        ));
    }

    #[test]
    fn windows_fragments_are_kept_off_a_linux_profile() {
        let mut by_name = BTreeMap::new();
        by_name.insert(
            "thing.w64".to_string(),
            "(&(osgi.arch=x86_64)(osgi.os=win32))".to_string(),
        );
        let filters = PlatformFilters { by_name };
        let linux = Environment::default();
        assert!(!filters.admits("thing.w64", &linux));
        // A bundle nothing constrains is admitted.
        assert!(filters.admits("thing", &linux));
    }
}

/// Platform constraints, which live on the *requirement edge* rather than on
/// the bundle.
///
/// A `feature.xml` lists `com.webmethods.plm.sd.introspect.custom.w32` with no
/// os or arch attribute at all; the constraint is in the repository metadata,
/// as an LDAP filter on the feature group's requirement:
/// `<required name='…w64'><filter>(&(osgi.arch=x86_64)(osgi.os=win32))</filter></required>`.
/// Without reading it, a Linux profile quietly collects Windows binaries.
#[derive(Debug, Default)]
pub struct PlatformFilters {
    by_name: BTreeMap<String, String>,
}

impl PlatformFilters {
    pub fn load(wm_home: &Path) -> Self {
        let mut by_name = BTreeMap::new();
        let root = wm_home.join("common").join("runtime").join("bundles");
        let Ok(groups) = fs::read_dir(&root) else {
            return Self { by_name };
        };
        for group in groups.flatten() {
            let content = group.path().join("eclipse").join("content.xml");
            let Ok(xml) = fs::read_to_string(&content) else {
                continue;
            };
            for (name, filter) in parse_requirement_filters(&xml) {
                by_name.insert(name, filter);
            }
        }
        Self { by_name }
    }

    /// Does this bundle belong on this platform?
    pub fn admits(&self, name: &str, env: &Environment) -> bool {
        match self.by_name.get(name) {
            Some(filter) => ldap_matches(filter, env),
            None => true,
        }
    }
}

/// Collect `name -> filter` for requirements carrying an environment filter.
///
/// Only `osgi.os` / `osgi.ws` / `osgi.arch` filters are environment
/// constraints. The others — `org.eclipse.update.install.sources` and friends —
/// are p2's own provisioning switches, and treating them as constraints would
/// exclude most of the product.
fn parse_requirement_filters(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<required ") {
        let after = &rest[start..];
        rest = &after[1..];
        // The span of this requirement ends where the next one begins, so a
        // nested requirement cannot lend its filter to its parent.
        let span_end = after[1..]
            .find("<required ")
            .map(|i| i + 1)
            .unwrap_or(after.len());
        let span = &after[..span_end];
        let Some(header_end) = span.find('>') else {
            continue;
        };
        let Some(name) = attribute(&span[..header_end], "name") else {
            continue;
        };
        let Some(filter) = between(span, "<filter>", "</filter>") else {
            continue;
        };
        let filter = filter.replace("&amp;", "&").trim().to_string();
        if filter.contains("osgi.os") || filter.contains("osgi.ws") || filter.contains("osgi.arch")
        {
            out.push((name, filter));
        }
    }
    out
}

/// Evaluate the small subset of LDAP filter syntax p2 uses for environments:
/// `&`, `|`, `!` and equality on `osgi.os` / `osgi.ws` / `osgi.arch`.
fn ldap_matches(filter: &str, env: &Environment) -> bool {
    let filter = filter.trim();
    let Some(inner) = filter
        .strip_prefix('(')
        .and_then(|f| f.strip_suffix(')'))
        .map(str::trim)
    else {
        return true;
    };
    match inner.chars().next() {
        Some('&') => split_top_level(&inner[1..])
            .iter()
            .all(|c| ldap_matches(c, env)),
        Some('|') => split_top_level(&inner[1..])
            .iter()
            .any(|c| ldap_matches(c, env)),
        Some('!') => !split_top_level(&inner[1..])
            .iter()
            .all(|c| ldap_matches(c, env)),
        _ => match inner.split_once('=') {
            Some((key, value)) => match key.trim() {
                "osgi.os" => value.trim() == env.os,
                "osgi.ws" => value.trim() == env.ws,
                "osgi.arch" => value.trim() == env.arch,
                // A key this evaluator does not model is not an environment
                // constraint, so it must not exclude anything.
                _ => true,
            },
            None => true,
        },
    }
}

/// Split a run of sibling `(...)` groups at depth zero.
fn split_top_level(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '(' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    out.push(text[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    out
}
