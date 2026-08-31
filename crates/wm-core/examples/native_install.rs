//! End-to-end native install of one product, with no Java installer involved.
//!
//! Usage:
//!   `WM_EMPOWER_USER=… WM_EMPOWER_KEY=… cargo run -p wm-core --example native_install -- \
//!        <release> <component> <install-dir>`

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let release_wanted = args
        .next()
        .ok_or("usage: native_install <release> <component> <dir>")?;
    let component = args.next().ok_or("missing component")?;
    let install_dir = PathBuf::from(args.next().ok_or("missing install dir")?);

    let user = std::env::var("WM_EMPOWER_USER")?;
    let key = std::env::var("WM_EMPOWER_KEY")?;

    println!("authenticating…");
    let mut session = wm_core::sdc::Session::login(wm_core::sdc::DEFAULT_HOST, &user, &key)?;

    let releases = session.releases()?;
    let release = releases
        .iter()
        .find(|r| r.release == release_wanted)
        .ok_or_else(|| format!("no entitlement for release {release_wanted}"))?;
    let sandbox = release.sandbox().ok_or("release has no sandbox")?;
    let repository = release.repository().ok_or("release has no repository")?;
    let cgi = release.cgi().ok_or("release has no CGI")?.to_string();
    println!(
        "release {} -> sandbox {sandbox}, repository {repository}",
        release.release
    );

    println!("fetching the product tree…");
    let text = session.product_tree(&sandbox, "LNXAMD64")?;
    let tree = wm_core::tree::ProductTree::parse(&text)?;
    println!(
        "  {} products, {} artifacts",
        tree.product_count(),
        tree.artifacts().len()
    );

    let catalog = tree.catalog();
    let path = catalog
        .path_of(&component)
        .ok_or_else(|| format!("no product {component}"))?;
    let seeds = vec![path.raw.clone()];
    let plan = wm_core::install::plan(&tree, &seeds);
    println!(
        "plan: {} artifact(s), {:.2} MB to download",
        plan.artifacts.len(),
        plan.download_bytes as f64 / 1e6
    );
    for p in &plan.products_with_panels {
        println!(
            "  note: {} declares install panels {:?}",
            p.product, p.panels
        );
    }

    let cache = install_dir.join(".cache");
    std::fs::create_dir_all(&install_dir)?;
    for artifact in tree.artifacts_for_selection(seeds.iter().map(String::as_str)) {
        let fetched = wm_core::install::fetch(&mut session, &cgi, &repository, artifact, &cache)?;
        let modes = wm_core::install::Modes::read(&fetched.path)?;
        let unpacked = wm_core::install::unpack(&fetched.path, &install_dir, &modes)?;
        wm_core::install::write_contents(&install_dir, artifact, &unpacked)?;
        println!(
            "  {} {:>9} bytes {} -> {} file(s), {} dir(s)",
            if fetched.from_cache {
                "cached "
            } else {
                "fetched"
            },
            fetched.size,
            artifact.name,
            unpacked.files.len(),
            unpacked.directories
        );
    }
    println!("done: {}", install_dir.display());
    Ok(())
}
