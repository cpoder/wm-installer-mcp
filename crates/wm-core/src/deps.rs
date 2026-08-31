//! Transitive prerequisite closure over the product catalogue.
//!
//! The installer does **not** complete a selection for you. `-writeImage`
//! embeds exactly the paths listed in `InstallProducts`, and the later install
//! then refuses with:
//!
//! ```text
//! Cannot install some selected products because products they require
//! do not exist in the image, local machine, or selected installer server.
//! ```
//!
//! So the closure has to be computed before the image is built.
//!
//! # `requiresRegexp` is not a regular expression
//!
//! The name is misleading and the difference matters. `DistManUtils.productsMatch`
//! splits both the pattern and the candidate path on `/`, requires the same
//! number of segments, and then compares segment by segment: a pattern segment
//! matches only if it is *literally equal* to the path segment, or is exactly
//! `*` or exactly `.*`. No other regex syntax is honoured.
//!
//! Treating these patterns as anchored regexes — the obvious reading of the
//! name — gets the common `e2ei/11/.*/.*/SCGCommon` form right by coincidence
//! and silently drops the `e2ei/11/*/*/WISSharedLibs` form, because as a regex
//! `/*` means "zero or more slashes". The prerequisite then goes missing from
//! the image and the failure surfaces an hour later.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::catalog::Catalog;
use crate::Result;

/// Products every installation needs but that nothing declares as a
/// prerequisite, so the closure alone will never pull them in.
///
/// The first two are refused explicitly at startup — *"Infrastructure > License
/// Agreement and Infrastructure > Java Package must exist in the installation
/// image or the target directory"* — and the third is the custom-install
/// component the console mode drives.
pub const MANDATORY_COMPONENTS: &[&str] = &["sjp", "license", "CustomInstall"];

/// Whether `path` satisfies `pattern`, with the installer's own semantics.
///
/// See the module documentation: whole-segment equality, with `*` and `.*` as
/// the only wildcards.
pub fn pattern_matches(pattern: &str, path: &str) -> bool {
    let pattern_segments: Vec<&str> = pattern.split('/').collect();
    let path_segments: Vec<&str> = path.split('/').collect();
    if pattern_segments.len() != path_segments.len() {
        return false;
    }
    pattern_segments
        .iter()
        .zip(&path_segments)
        .all(|(p, s)| *p == "*" || *p == ".*" || p == s)
}

/// Whether `version` satisfies a `requiresVersionRegexp` expression.
///
/// The grammar is `eq|gt|gte|lt|lte` followed by a dotted version, combined with
/// `&&` and `||`. An empty expression means "no constraint".
pub fn version_matches(expr: &str, version: &str) -> bool {
    let expr = expr.trim();
    if expr.is_empty() {
        return true;
    }
    if version.is_empty() {
        return false;
    }
    // `&&` binds tighter than `||` in the installer's recursive split, which
    // splits on the first `&&` it finds before looking for `||`.
    if let Some((left, right)) = expr.split_once("&&") {
        return version_matches(left, version) && version_matches(right, version);
    }
    if let Some((left, right)) = expr.split_once("||") {
        return version_matches(left, version) || version_matches(right, version);
    }
    // Order matters: `gte` must be tested before `gt`.
    let (ordering, bound) = if let Some(rest) = expr.strip_prefix("gte") {
        (
            &[std::cmp::Ordering::Greater, std::cmp::Ordering::Equal][..],
            rest,
        )
    } else if let Some(rest) = expr.strip_prefix("gt") {
        (&[std::cmp::Ordering::Greater][..], rest)
    } else if let Some(rest) = expr.strip_prefix("lte") {
        (
            &[std::cmp::Ordering::Less, std::cmp::Ordering::Equal][..],
            rest,
        )
    } else if let Some(rest) = expr.strip_prefix("lt") {
        (&[std::cmp::Ordering::Less][..], rest)
    } else if let Some(rest) = expr.strip_prefix("eq") {
        (&[std::cmp::Ordering::Equal][..], rest)
    } else {
        return false;
    };
    ordering.contains(&compare_versions(version, bound.trim()))
}

/// Compare dotted versions segment by segment, numerically where possible.
///
/// Missing trailing segments count as zero, so `9.12.0.0.293` is greater than
/// `9.12` rather than incomparable.
fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let mut l = left.split('.');
    let mut r = right.split('.');
    loop {
        match (l.next(), r.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (a, b) => {
                let a = a.unwrap_or("0");
                let b = b.unwrap_or("0");
                let ordering = match (a.parse::<u64>(), b.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    _ => a.cmp(b),
                };
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

/// Why a product ended up in the resolved set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Explicitly asked for by the caller.
    Seed,
    /// Added because [`MANDATORY_COMPONENTS`] lists it.
    Mandatory,
    /// Pulled in to satisfy a prerequisite of another product.
    Prerequisite,
}

/// One resolved product and how it got there.
#[derive(Debug, Clone, Serialize)]
pub struct Resolved {
    /// Versioned product path.
    pub path: String,
    /// Why it is in the set.
    pub origin: Origin,
    /// For [`Origin::Prerequisite`], the product that required it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_by: Option<String>,
}

/// A prerequisite pattern that nothing in the catalogue satisfies.
///
/// This is the interesting failure: it means the reference installation does not
/// contain the prerequisite either, so the resulting image will be incomplete
/// however carefully the seeds were chosen.
#[derive(Debug, Clone, Serialize)]
pub struct Unsatisfied {
    /// The pattern as declared.
    pub pattern: String,
    /// The version constraint, when one was declared alongside.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_constraint: Option<String>,
    /// The product that declared it.
    pub required_by: String,
    /// Whether some path matched the pattern but failed the version constraint,
    /// which is a different problem from nothing matching at all.
    pub version_rejected: bool,
}

/// A product whose metadata this resolver cannot fully honour.
#[derive(Debug, Clone, Serialize)]
pub struct Caveat {
    /// The product concerned.
    pub product: String,
    /// What is unhandled.
    pub note: String,
}

/// Outcome of a closure.
#[derive(Debug, Clone, Serialize)]
pub struct Resolution {
    /// Every product to install, ordered by path.
    pub products: Vec<Resolved>,
    /// Seeds that are not in the catalogue at all.
    pub unknown_seeds: Vec<String>,
    /// Prerequisite patterns with no match.
    pub unsatisfied: Vec<Unsatisfied>,
    /// Metadata this resolver does not implement.
    pub caveats: Vec<Caveat>,
}

impl Resolution {
    /// The paths only, ready for `InstallProducts`.
    pub fn paths(&self) -> Vec<String> {
        self.products.iter().map(|p| p.path.clone()).collect()
    }

    /// Total product count.
    pub fn len(&self) -> usize {
        self.products.len()
    }

    /// Whether nothing resolved.
    pub fn is_empty(&self) -> bool {
        self.products.is_empty()
    }

    /// Whether the closure is safe to feed to the installer.
    pub fn is_complete(&self) -> bool {
        self.unknown_seeds.is_empty() && self.unsatisfied.is_empty()
    }
}

/// Compute the prerequisite closure of `seeds` against `catalog`.
///
/// `seeds` are versioned product paths. When `include_mandatory` is set, the
/// components in [`MANDATORY_COMPONENTS`] are added first — they are part of
/// every viable selection, and leaving them out is the most common way to build
/// an image the installer then refuses to open.
pub fn resolve(catalog: &Catalog, seeds: &[String], include_mandatory: bool) -> Result<Resolution> {
    let mut origins: BTreeMap<String, (Origin, Option<String>)> = BTreeMap::new();
    let mut queue: Vec<String> = Vec::new();
    let mut unknown_seeds = Vec::new();

    for seed in seeds {
        if catalog.get(seed).is_none() {
            unknown_seeds.push(seed.clone());
        }
        origins.entry(seed.clone()).or_insert((Origin::Seed, None));
        queue.push(seed.clone());
    }

    if include_mandatory {
        for component in MANDATORY_COMPONENTS {
            let Some(path) = catalog.path_of(component) else {
                continue;
            };
            origins
                .entry(path.raw.clone())
                .or_insert((Origin::Mandatory, None));
            queue.push(path.raw.clone());
        }
    }

    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut unsatisfied = Vec::new();
    let mut caveats = Vec::new();

    while let Some(current) = queue.pop() {
        if !visited.insert(current.clone()) {
            continue;
        }
        let Some(product) = catalog.get(&current) else {
            continue;
        };

        if product.product_requires.is_some() {
            // The installer lets `productRequires` override the regexp lists.
            // Nothing in a 12.1 tree uses it; say so rather than resolving a
            // selection that quietly ignores it.
            caveats.push(Caveat {
                product: current.clone(),
                note: "declares productRequires, which overrides requiresRegexp in the \
                       installer and is not resolved here"
                    .to_string(),
            });
        }

        for (index, pattern) in product.requires.iter().enumerate() {
            let constraint = product
                .requires_versions
                .get(index)
                .map(String::as_str)
                .unwrap_or("");
            let candidates: Vec<&str> = catalog
                .paths()
                .filter(|p| pattern_matches(pattern, p))
                .collect();
            let accepted: Vec<&str> = candidates
                .iter()
                .copied()
                .filter(|p| match catalog.get(p) {
                    Some(product) => version_matches(constraint, product.path.version()),
                    None => false,
                })
                .collect();

            if accepted.is_empty() {
                unsatisfied.push(Unsatisfied {
                    pattern: pattern.clone(),
                    version_constraint: (!constraint.is_empty()).then(|| constraint.to_string()),
                    required_by: current.clone(),
                    version_rejected: !candidates.is_empty(),
                });
                continue;
            }
            for matched in accepted {
                origins
                    .entry(matched.to_string())
                    .or_insert((Origin::Prerequisite, Some(current.clone())));
                queue.push(matched.to_string());
            }
        }
    }

    let products = origins
        .into_iter()
        .map(|(path, (origin, required_by))| Resolved {
            path,
            origin,
            required_by,
        })
        .collect();

    Ok(Resolution {
        products,
        unknown_seeds,
        unsatisfied,
        caveats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Product, ProductPath};

    fn product(path: &str, requires: &[&str]) -> Product {
        Product {
            path: ProductPath::parse(path).expect("valid test path"),
            prop_name: path.rsplit('/').next().unwrap().to_string(),
            requires: requires.iter().map(|s| s.to_string()).collect(),
            requires_versions: Vec::new(),
            product_requires: None,
            includes: Vec::new(),
            product_code: None,
            container: None,
        }
    }

    fn catalog() -> Catalog {
        Catalog::from_products([
            product(
                "e2ei/11/TN_12.1/TradingNetworks/TNServer",
                &["e2ei/11/.*/.*/PIECore"],
            ),
            product(
                "e2ei/11/IS_12.1/integrationServer/PIECore",
                &["e2ei/11/.*/.*/OSGI"],
            ),
            product("e2ei/11/OSGI_12.1/Platform/OSGI", &[]),
            product("e2ei/11/SJP_21.0/Infrastructure/sjp", &[]),
            product("e2ei/11/TPL_12.1/License/license", &[]),
            product("e2ei/11/WIR_12.1/Infrastructure/CustomInstall", &[]),
            product("e2ei/11/X_1.0/G/Lonely", &["e2ei/11/.*/.*/DoesNotExist"]),
            // The bare-`*` form, which a regex reading of the pattern misses.
            product(
                "e2ei/11/SSX_12.1/API/SSX_TLS",
                &["e2ei/11/*/*/WISSharedLibs"],
            ),
            product("e2ei/11/WIS_12.1/Infrastructure/WISSharedLibs", &[]),
        ])
    }

    #[test]
    fn patterns_are_segment_wise_not_regex() {
        // Both wildcard spellings the installer accepts.
        assert!(pattern_matches(
            "e2ei/11/*/*/WISSharedLibs",
            "e2ei/11/WIS_12.1/Infra/WISSharedLibs"
        ));
        assert!(pattern_matches(
            "e2ei/11/.*/.*/SCGCommon",
            "e2ei/11/WSC_12.1/SCG/SCGCommon"
        ));
        // Segment counts must agree.
        assert!(!pattern_matches(
            "e2ei/11/*/SCGCommon",
            "e2ei/11/WSC_12.1/SCG/SCGCommon"
        ));
        // A partial wildcard is not a wildcard: only a whole `*` segment is.
        assert!(!pattern_matches(
            "e2ei/11/WSC.*/SCG/SCGCommon",
            "e2ei/11/WSC_12.1/SCG/SCGCommon"
        ));
        // Literal segments still have to match.
        assert!(!pattern_matches(
            "e2ei/11/*/*/Other",
            "e2ei/11/WIS_12.1/Infra/WISSharedLibs"
        ));
    }

    #[test]
    fn the_bare_star_form_resolves() {
        let seeds = vec!["e2ei/11/SSX_12.1/API/SSX_TLS".to_string()];
        let r = resolve(&catalog(), &seeds, false).expect("closure");
        assert!(
            r.paths().iter().any(|p| p.ends_with("/WISSharedLibs")),
            "e2ei/11/*/*/WISSharedLibs must resolve; got {:?}",
            r.paths()
        );
        assert!(r.is_complete());
    }

    #[test]
    fn version_expressions_follow_the_installer_grammar() {
        assert!(version_matches("", "9.12"));
        assert!(version_matches("gte9.12", "9.12.0.0.293"));
        assert!(version_matches("gte9.12", "9.12"));
        assert!(!version_matches("gt9.12", "9.12"));
        assert!(version_matches("eq9.12", "9.12.0.0.0"));
        assert!(version_matches("lte10.0", "9.12"));
        assert!(!version_matches("lt9.12", "9.12"));
        assert!(version_matches("gte9.0&&lt10.0", "9.12"));
        assert!(!version_matches("gte9.0&&lt10.0", "10.1"));
        assert!(version_matches("lt9.0||gte12.0", "12.1"));
        assert!(!version_matches("nonsense", "9.12"));
    }

    #[test]
    fn a_version_constraint_can_reject_every_candidate() {
        let mut ssx = product(
            "e2ei/11/SSX_12.1/API/SSX_TLS",
            &["e2ei/11/*/*/WISSharedLibs"],
        );
        ssx.requires_versions = vec!["gte99.0".into()];
        let catalog = Catalog::from_products([
            ssx,
            product("e2ei/11/WIS_12.1/Infrastructure/WISSharedLibs", &[]),
        ]);
        let seeds = vec!["e2ei/11/SSX_12.1/API/SSX_TLS".to_string()];
        let r = resolve(&catalog, &seeds, false).expect("closure");
        assert_eq!(r.unsatisfied.len(), 1);
        assert!(
            r.unsatisfied[0].version_rejected,
            "a candidate matched but failed the version"
        );
    }

    #[test]
    fn pulls_prerequisites_transitively() {
        let seeds = vec!["e2ei/11/TN_12.1/TradingNetworks/TNServer".to_string()];
        let r = resolve(&catalog(), &seeds, false).expect("closure");
        let paths = r.paths();
        assert!(
            paths.iter().any(|p| p.ends_with("/PIECore")),
            "direct prerequisite"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("/OSGI")),
            "transitive prerequisite"
        );
        assert_eq!(paths.len(), 3);
        assert!(r.is_complete());
    }

    #[test]
    fn injects_the_mandatory_base() {
        let seeds = vec!["e2ei/11/OSGI_12.1/Platform/OSGI".to_string()];
        let r = resolve(&catalog(), &seeds, true).expect("closure");
        for component in MANDATORY_COMPONENTS {
            assert!(
                r.paths()
                    .iter()
                    .any(|p| p.ends_with(&format!("/{component}"))),
                "{component} must be injected"
            );
        }
    }

    #[test]
    fn reports_prerequisites_nothing_satisfies() {
        let seeds = vec!["e2ei/11/X_1.0/G/Lonely".to_string()];
        let r = resolve(&catalog(), &seeds, false).expect("closure");
        assert!(!r.is_complete());
        assert_eq!(r.unsatisfied.len(), 1);
        assert!(!r.unsatisfied[0].version_rejected);
    }

    #[test]
    fn reports_seeds_outside_the_catalog() {
        let seeds = vec!["e2ei/11/NOPE_1.0/G/Nope".to_string()];
        let r = resolve(&catalog(), &seeds, false).expect("closure");
        assert_eq!(r.unknown_seeds, ["e2ei/11/NOPE_1.0/G/Nope"]);
        assert!(!r.is_complete());
    }

    #[test]
    fn flags_product_requires_as_unhandled() {
        let mut p = product("e2ei/11/A_1/G/A", &[]);
        p.product_requires = Some("something".into());
        let catalog = Catalog::from_products([p]);
        let r = resolve(&catalog, &["e2ei/11/A_1/G/A".to_string()], false).expect("closure");
        assert_eq!(r.caveats.len(), 1);
    }

    #[test]
    fn a_cycle_terminates() {
        let cyclic = Catalog::from_products([
            product("e2ei/11/A_1/G/A", &["e2ei/11/.*/.*/B"]),
            product("e2ei/11/B_1/G/B", &["e2ei/11/.*/.*/A"]),
        ]);
        let seeds = vec!["e2ei/11/A_1/G/A".to_string()];
        let r = resolve(&cyclic, &seeds, false).expect("closure");
        assert_eq!(r.len(), 2);
    }
}
