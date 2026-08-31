//! Print an Integration Server administrator password hash.
//!
//! Usage: `cargo run -p wm-core --example hash_password -- <password> [user]`

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let password = args
        .next()
        .ok_or("usage: hash_password <password> [user]")?;
    let user = args
        .next()
        .unwrap_or_else(|| wm_core::password::DEFAULT_USER.to_string());
    let complaints = wm_core::password::complaints(&password);
    if !complaints.is_empty() {
        eprintln!(
            "note: the product scripts would object: {}",
            complaints.join(", ")
        );
    }
    println!("{}", wm_core::password::hash(&user, &password)?);
    Ok(())
}
