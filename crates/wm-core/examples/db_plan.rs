//! Show what installing every database component would do.
//!
//! Usage: `cargo run -p wm-core --example db_plan -- <wm_home> <database>`

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let home = std::path::PathBuf::from(args.next().ok_or("usage: db_plan <wm_home> <database>")?);
    let database = args.next().unwrap_or_else(|| "postgresql".into());

    for component in wm_core::database::discover(&home)? {
        let kinds = wm_core::database::databases(&component);
        match wm_core::database::plan(&component, &database) {
            Ok(p) => println!(
                "{:<28} {:<5} create {:<8} + {:>2} migration(s) -> {:<10} {:>3} script(s)",
                p.component,
                p.code,
                p.create_from,
                p.migrations.len(),
                p.target,
                p.scripts.len()
            ),
            Err(e) => println!(
                "{:<28} {:<5} -- {e}  (ships: {})",
                component.name,
                component.code,
                kinds.into_iter().collect::<Vec<_>>().join(",")
            ),
        }
    }
    Ok(())
}
