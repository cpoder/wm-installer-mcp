//! List the fixes IBM offers for an installation, without Update Manager.
//!
//! Usage: `WM_EMPOWER_USER=… WM_EMPOWER_KEY=… \
//!          cargo run -p wm-core --example list_fixes -- <release> <install-dir> [all]`

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let release_wanted = args
        .next()
        .ok_or("usage: list_fixes <release> <install-dir> [all]")?;
    let install_dir = args.next().ok_or("missing install dir")?;
    let show_all = args.next().as_deref() == Some("all");

    let user = std::env::var("WM_EMPOWER_USER")?;
    let key = std::env::var("WM_EMPOWER_KEY")?;
    let session = wm_core::sdc::Session::login(wm_core::sdc::DEFAULT_HOST, &user, &key)?;

    let releases = session.releases()?;
    let release = releases
        .iter()
        .find(|r| r.release == release_wanted)
        .ok_or_else(|| format!("no entitlement for release {release_wanted}"))?;
    let sandbox = release.sandbox().ok_or("release names no sandbox")?;
    let fix_repository = session
        .fix_repository(&sandbox)?
        .ok_or("sandbox publishes no fix repository")?;
    println!("sandbox {sandbox} -> fix repository {fix_repository}");

    let inventory =
        wm_core::fixes::Inventory::read(std::path::Path::new(&install_dir), "LNXAMD64")?;
    println!("inventory: {} products", inventory.products.len());

    let fixes = wm_core::fixes::available(&session, &fix_repository, &inventory, show_all)?;
    println!(
        "{} fix(es) offered{}",
        fixes.len(),
        if show_all { " (all published)" } else { "" }
    );
    let total: u64 = fixes.iter().filter_map(|f| f.size).sum();
    for fix in &fixes {
        println!(
            "  {:<40} {:<22} {:>9} MB  {}",
            fix.id,
            fix.version,
            fix.size.unwrap_or(0) / 1_000_000,
            fix.display_group.as_deref().unwrap_or("")
        );
    }
    println!("total {:.2} GB", total as f64 / 1e9);
    Ok(())
}
