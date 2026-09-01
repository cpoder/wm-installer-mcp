//! Read the comment block of every archive in a directory, the way install does.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args().nth(1).ok_or("usage: check_modes <dir>")?;
    let mut total = 0usize;
    let mut bad = Vec::new();
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "zip") {
            continue;
        }
        total += 1;
        if let Err(e) = wm_core::install::Modes::read(&path) {
            bad.push((
                path.file_name().unwrap().to_string_lossy().to_string(),
                e.to_string(),
            ));
        }
    }
    println!("{total} archives, {} unreadable", bad.len());
    for (name, why) in bad.iter().take(10) {
        println!("  {name}: {why}");
    }
    Ok(())
}
