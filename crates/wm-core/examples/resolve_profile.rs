//! Compute a profile's bundle set from the feature graph and compare it to one
//! already installed.
//!
//! Usage: `cargo run -p wm-core --example resolve_profile -- <wm_home> <profile>`
//!
//! Roots come from `install/profiles/<profile>.data`, which the shipped
//! installer writes with the feature list it provisioned from.

use std::collections::BTreeSet;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let wm_home = std::path::PathBuf::from(
        args.next()
            .ok_or("usage: resolve_profile <wm_home> <profile>")?,
    );
    let name = args.next().ok_or("missing profile name")?;

    let data = wm_home
        .join("install")
        .join("profiles")
        .join(format!("{name}.data"));
    let text = std::fs::read_to_string(&data)?;
    let roots: Vec<String> = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("featuresList="))
        .ok_or("no featuresList in the profile data")?
        .split(',')
        .map(|f| f.trim().trim_end_matches(".feature.group").to_string())
        .filter(|f| !f.is_empty())
        .collect();
    println!("roots: {}", roots.len());

    let started = Instant::now();
    let features = wm_core::resolve::FeatureIndex::load(&wm_home)?;
    let bundles = wm_core::resolve::BundleIndex::load(&wm_home);
    let levels = wm_core::resolve::StartLevels::load(&wm_home, &bundles);
    let filters = wm_core::resolve::PlatformFilters::load(&wm_home);
    let indexed = started.elapsed();
    println!(
        "indexed {} features and {} jars in {:.2}s",
        features.len(),
        bundles.len(),
        indexed.as_secs_f64()
    );

    let resolving = Instant::now();
    let resolution = wm_core::resolve::resolve(
        &features,
        &bundles,
        &levels,
        &filters,
        &roots,
        &wm_core::resolve::Environment::default(),
    );
    println!(
        "resolved {} features -> {} bundles in {:.2}s",
        resolution.features,
        resolution.bundles.len(),
        resolving.elapsed().as_secs_f64()
    );
    println!("  repaired: {}", resolution.repaired.len());
    println!(
        "  unresolved plugin refs: {}",
        resolution.unresolved_plugins.len()
    );
    println!(
        "  unresolved feature imports: {}",
        resolution.unresolved_features.len()
    );
    println!(
        "  unsatisfied imports: {}",
        resolution.unsatisfied_imports.len()
    );

    // Compare with what is installed.
    let profile_dir = wm_home.join("profiles").join(&name);
    let info =
        profile_dir.join("configuration/org.eclipse.equinox.simpleconfigurator/bundles.info");
    let Ok(existing) = std::fs::read_to_string(&info) else {
        println!("no installed profile to compare against");
        return Ok(());
    };
    let reference: BTreeSet<String> = wm_core::profile::parse_bundles(&existing)
        .into_iter()
        .map(|b| b.jar)
        .collect();
    let computed: BTreeSet<String> = resolution.bundles.iter().map(|b| b.jar.clone()).collect();

    println!(
        "\nagainst the installed profile: {} bundles",
        reference.len()
    );
    println!("  extra:   {}", computed.difference(&reference).count());
    println!("  missing: {}", reference.difference(&computed).count());
    for jar in computed.difference(&reference).take(8) {
        println!("    extra:   {jar}");
    }
    for jar in reference.difference(&computed).take(8) {
        println!("    missing: {jar}");
    }
    Ok(())
}
