//! Install database components into a live database.
//!
//! Usage: `db_apply <wm_home> <db> <host:port/database> <user> <password> <component…>`

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let home = std::path::PathBuf::from(args.next().ok_or("missing wm_home")?);
    let database = args.next().ok_or("missing database kind")?;
    let dsn = args.next().ok_or("missing host:port/database")?;
    let user = args.next().ok_or("missing user")?;
    let password = args.next().ok_or("missing password")?;
    let wanted: Vec<String> = args.collect();

    let (hostport, dbname) = dsn
        .split_once('/')
        .ok_or("dsn must be host:port/database")?;
    let (host, port) = hostport.split_once(':').unwrap_or((hostport, "5432"));
    let target = wm_core::database::Target {
        host: host.to_string(),
        port: port.parse()?,
        database: dbname.to_string(),
        user,
        password,
    };

    let components = wm_core::database::discover(&home)?;
    let mut client = wm_core::database::connect(&target)?;
    let installed = wm_core::database::installed(&mut client)?;
    println!("already installed: {} component(s)", installed.len());

    let order = wm_core::database::order(&components, &wanted)?;

    let started = std::time::Instant::now();
    for component in order {
        let plan = wm_core::database::plan(component, &database)?;
        let already = installed.get(&plan.code).map(String::as_str);
        let t = std::time::Instant::now();
        let applied = wm_core::database::apply(&mut client, &plan, already)?;
        if applied.skipped {
            println!("  {:<24} already at {}", applied.component, applied.to);
        } else {
            println!(
                "  {:<24} {} -> {:<10} {:>3} script(s), {:>4} statement(s) in {:.2}s",
                applied.component,
                plan.create_from,
                applied.to,
                applied.scripts,
                applied.statements,
                t.elapsed().as_secs_f64()
            );
        }
    }
    println!("total {:.2}s", started.elapsed().as_secs_f64());
    Ok(())
}
