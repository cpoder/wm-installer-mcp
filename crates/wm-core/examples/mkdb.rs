fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dsn = std::env::args().nth(1).unwrap();
    let name = std::env::args().nth(2).unwrap_or_else(|| "wmtn".into());
    let mut c = postgres::Client::connect(&dsn, postgres::NoTls)?;
    let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {name}"));
    c.batch_execute(&format!("CREATE DATABASE {name}"))?;
    println!("database {name} created");
    Ok(())
}
