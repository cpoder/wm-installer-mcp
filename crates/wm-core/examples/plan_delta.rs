//! Price a selection against an installation that already exists.
//!
//! The same subtraction `native_plan` and `native_install` perform, run against
//! a cached product tree and a real installation. No credentials and no
//! network: it is the way to check what an incremental install would touch
//! before letting one run.
//!
//! Usage:
//! `cargo run -p wm-core --example plan_delta -- <tree-file> <wm-home> <seed…>`

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let tree_path = args
        .next()
        .ok_or("usage: plan_delta <tree-file> <wm-home> <seed…>")?;
    let wm_home = args
        .next()
        .ok_or("usage: plan_delta <tree-file> <wm-home> <seed…>")?;
    let seeds: Vec<String> = args.collect();
    if seeds.is_empty() {
        return Err("name at least one product".into());
    }

    let tree = wm_core::tree::ProductTree::parse(&std::fs::read_to_string(&tree_path)?)?;
    let catalog = tree.catalog();
    let paths: Vec<String> = seeds
        .iter()
        .filter_map(|s| {
            catalog
                .get(s)
                .map(|_| s.clone())
                .or_else(|| catalog.path_of(s).map(|p| p.raw.clone()))
        })
        .collect();
    println!("seeds resolved : {}/{}", paths.len(), seeds.len());

    let resolution = wm_core::deps::resolve(&catalog, &paths, true)?;
    let closure = resolution.paths();
    let whole = wm_core::install::plan(&tree, &closure);
    println!(
        "closure        : {} products, {} artifacts, {}",
        closure.len(),
        whole.artifacts.len(),
        wm_core::progress::human_bytes(whole.download_bytes)
    );

    let inventory = wm_core::inventory::Inventory::read(std::path::Path::new(&wm_home))?;
    println!(
        "installed      : {} products, {} fix readme(s)",
        inventory.products.len(),
        inventory.fixes.len()
    );

    let delta = wm_core::install::delta(&closure, &inventory);
    let incremental = wm_core::install::plan(&tree, &delta.to_install);
    println!(
        "already there  : {} products",
        delta.already_installed.len()
    );
    println!(
        "to install     : {} products, {} artifacts, {}",
        delta.to_install.len(),
        incremental.artifacts.len(),
        wm_core::progress::human_bytes(incremental.download_bytes)
    );
    for product in &delta.to_install {
        println!("  + {product}");
    }
    if !delta.version_changes.is_empty() {
        println!(
            "\nNOT performed  : {} product(s) present under a different version",
            delta.version_changes.len()
        );
        for change in &delta.version_changes {
            println!(
                "  ! {} installed {}, catalogue {}",
                change.component, change.installed_version, change.catalog_version
            );
        }
    }
    Ok(())
}
