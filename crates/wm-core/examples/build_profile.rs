//! Build a profile whose bundle set comes from the resolver, not from a capture.
//!
//! The configuration is taken from a capture — `config.ini`, `jaas.config` and
//! the rest are product contributions, not something the feature graph
//! describes — but `bundles.info`, the list the OSGi framework actually
//! installs from, is computed. Starting the result is the test of the resolver.
//!
//! Usage: `cargo run -p wm-core --example build_profile -- <capture> <wm_home> <name> <roots-from>`

use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let capture = std::path::PathBuf::from(
        args.next()
            .ok_or("usage: build_profile <capture> <wm_home> <name> <roots-profile>")?,
    );
    let wm_home = std::path::PathBuf::from(args.next().ok_or("missing wm_home")?);
    let name = args.next().ok_or("missing profile name")?;
    let roots_from = args.next().ok_or("missing roots profile")?;

    let overall = Instant::now();

    // Configuration and the framework jars come from the capture.
    let laid = Instant::now();
    let replayed = wm_core::profile::replay(&capture, &wm_home, Some(&name), false)?;
    println!(
        "laid down {} config file(s) and {} jar(s) in {:.1}s",
        replayed.files,
        replayed.bundles,
        laid.elapsed().as_secs_f64()
    );

    // The bundle list is computed.
    let data = wm_home
        .join("install")
        .join("profiles")
        .join(format!("{roots_from}.data"));
    let text = std::fs::read_to_string(&data)?;
    let roots: Vec<String> = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("featuresList="))
        .ok_or("no featuresList")?
        .split(',')
        .map(|f| f.trim().trim_end_matches(".feature.group").to_string())
        .filter(|f| !f.is_empty())
        .collect();

    let resolving = Instant::now();
    let features = wm_core::resolve::FeatureIndex::load(&wm_home)?;
    let bundles = wm_core::resolve::BundleIndex::load(&wm_home);
    let levels = wm_core::resolve::StartLevels::load(&wm_home, &bundles);
    let filters = wm_core::resolve::PlatformFilters::load(&wm_home);
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

    // Any computed bundle the capture did not bring, copy in.
    let profile_dir = wm_home.join("profiles").join(&name);
    let plugins = profile_dir.join("plugins");
    let mut added = 0;
    for bundle in &resolution.bundles {
        let target = plugins.join(&bundle.jar);
        if target.exists() {
            continue;
        }
        let Some((id, version)) = bundle
            .jar
            .strip_suffix(".jar")
            .and_then(|s| s.rsplit_once('_'))
        else {
            continue;
        };
        if let Some(source) = bundles.find(id, version) {
            std::fs::copy(source, &target)?;
            added += 1;
        }
    }
    println!("added {added} jar(s) the capture did not carry");

    // Replace the list the framework installs from.
    let info =
        profile_dir.join("configuration/org.eclipse.equinox.simpleconfigurator/bundles.info");
    std::fs::write(
        &info,
        wm_core::profile::render_bundles_info(&resolution.bundles),
    )?;
    println!(
        "wrote {} with {} entries; total {:.1}s",
        info.display(),
        resolution.bundles.len(),
        overall.elapsed().as_secs_f64()
    );
    Ok(())
}
