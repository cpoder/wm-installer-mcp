//! Parse a product tree document and report what it contains.
//!
//! Usage: `cargo run -p wm-core --example parse_tree -- <tree-file> [seed…]`

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: parse_tree <tree-file> [seed…]")?;
    let seeds: Vec<String> = args.collect();

    let text = std::fs::read_to_string(&path)?;
    let tree = wm_core::tree::ProductTree::parse(&text)?;
    println!("products : {}", tree.product_count());
    println!("artifacts: {}", tree.artifacts().len());

    let total: u64 = tree
        .artifacts()
        .iter()
        .filter_map(|a| a.compressed_size)
        .sum();
    println!(
        "download : {:.1} GB if everything were taken",
        total as f64 / 1e9
    );

    let missing = tree
        .artifacts()
        .iter()
        .filter(|a| a.sha256.is_none())
        .count();
    println!("artifacts without sha256: {missing}");

    if seeds.is_empty() {
        return Ok(());
    }
    let catalog = tree.catalog();
    let paths: Vec<String> = seeds
        .iter()
        .filter_map(|s| catalog.path_of(s).map(|p| p.raw.clone()))
        .collect();
    println!("\nseeds resolved: {}/{}", paths.len(), seeds.len());
    let resolution = wm_core::deps::resolve(&catalog, &paths, true)?;
    println!("closure  : {} products", resolution.len());
    println!("complete : {}", resolution.is_complete());
    for u in &resolution.unsatisfied {
        println!(
            "  unsatisfied {:?} required by {}",
            u.pattern, u.required_by
        );
    }
    let paths = resolution.paths();
    let selected = tree.artifacts_for_selection(paths.iter().map(String::as_str));
    let bytes: u64 = selected.iter().filter_map(|a| a.compressed_size).sum();
    println!(
        "artifacts: {}, {:.2} GB to download",
        selected.len(),
        bytes as f64 / 1e9
    );
    Ok(())
}
