//! Database components, natively.
//!
//! Trading Networks does not run without its schemas, and the shipped
//! `common/db/bin/dbConfigurator.sh` is a shell wrapper around
//! `com.webmethods.dcc.cli.Main` — a JVM, a classpath of a dozen jars, and a
//! set of JDBC drivers. Everything it needs is on disk in a form that does not
//! require any of that:
//!
//! ```text
//! common/db/<product>/<component>/config.json          name, code, versions
//! common/db/<product>/<component>/scripts/<v>/<db>/    the create set
//! common/db/<product>/<component>/scripts/<a>-<b>/<db>/  a migration
//! ```
//!
//! A component is installed by running the create set for the wanted database,
//! then walking migrations forward to the newest reachable version, then
//! recording an `INSTALL` event in `COMPONENT_EVENT` — the table whose
//! `INSTALLED_COMPONENT` view is how every webMethods product asks what level
//! its schema is at.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};

/// The tracking component, which must exist before anything can be recorded.
pub const TRACKER: &str = "ComponentTracker";

/// One database component as the product ships it.
#[derive(Debug, Clone, Serialize)]
pub struct Component {
    /// Directory name of the owning product, e.g. `TradingNetworks`.
    pub product: String,
    /// Component name, e.g. `TradingNetworks`.
    pub name: String,
    /// Short code recorded in `COMPONENT_EVENT.COMPONENT_CD`, e.g. `TNS`.
    pub code: String,
    /// Where its `config.json` and `scripts/` live.
    pub root: PathBuf,
    /// Versions the component declares, oldest first.
    pub versions: Vec<String>,
    /// Components that must be installed before this one.
    pub preinstall: Vec<String>,
    /// Components that must be installed after this one.
    pub postinstall: Vec<String>,
}

/// What installing one component will do.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub component: String,
    pub code: String,
    pub database: String,
    /// The version whose create set is run first.
    pub create_from: String,
    /// Migration directories to run, in order.
    pub migrations: Vec<String>,
    /// The version the component ends at, and what is recorded.
    pub target: String,
    /// Every script file, in execution order.
    pub scripts: Vec<PathBuf>,
}

/// Find every database component in an installation.
pub fn discover(wm_home: &Path) -> Result<Vec<Component>> {
    let root = wm_home.join("common").join("db");
    let mut out = Vec::new();
    let Ok(products) = fs::read_dir(&root) else {
        return Err(Error::Malformed(format!(
            "{} has no database components; install DatabaseComponentConfigurator first",
            root.display()
        )));
    };
    for product in products.flatten() {
        if !product.path().is_dir() {
            continue;
        }
        let product_name = product.file_name().to_string_lossy().to_string();
        let Ok(components) = fs::read_dir(product.path()) else {
            continue;
        };
        for component in components.flatten() {
            let config = component.path().join("config.json");
            if !config.is_file() {
                continue;
            }
            let Ok(text) = fs::read_to_string(&config) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let name = value["name"].as_str().unwrap_or_default().to_string();
            let code = value["code"].as_str().unwrap_or_default().to_string();
            if name.is_empty() || code.is_empty() {
                continue;
            }
            let entries = value["versions"].as_array().cloned().unwrap_or_default();
            let versions = entries
                .iter()
                .filter_map(|v| v["version"].as_str().map(str::to_string))
                .collect();
            // Dependencies are declared per version, but the versions in
            // `config.json` do not line up with the script directories — Trading
            // Networks stops at 10.4 here while its scripts reach 12.0. Taking
            // the union is the conservative reading, and matches the data:
            // where a component declares dependencies at several versions, it
            // declares the same ones.
            let (preinstall, postinstall) = collect_dependencies(&entries);
            out.push(Component {
                product: product_name.clone(),
                name,
                code,
                root: component.path(),
                versions,
                preinstall,
                postinstall,
            });
        }
    }
    out.sort_by(|a, b| (&a.product, &a.name).cmp(&(&b.product, &b.name)));
    Ok(out)
}

/// Union of the `preinstall` and `postinstall` names across every version.
fn collect_dependencies(versions: &[serde_json::Value]) -> (Vec<String>, Vec<String>) {
    let mut pre = BTreeSet::new();
    let mut post = BTreeSet::new();
    for version in versions {
        for (key, sink) in [("preinstall", &mut pre), ("postinstall", &mut post)] {
            if let Some(names) = version["dependencies"][key].as_array() {
                sink.extend(names.iter().filter_map(|n| n.as_str().map(str::to_string)));
            }
        }
    }
    (pre.into_iter().collect(), post.into_iter().collect())
}

/// Order a selection so every component is installed after what it needs.
///
/// The components are not independent: `TradingNetworksArchive` declares
/// `preinstall: [TradingNetworks]`, `MywebMethodsServer` needs `TaskEngine` and
/// `CommonDirectoryServices` before it and `CentralConfiguration` after. Running
/// them in the order they were asked for happens to work only when the caller
/// already knew the order.
///
/// `preinstall` names a prerequisite, so it is pulled into the selection.
/// `postinstall` only constrains ordering between components already selected —
/// it says "if you install this too, install it after me", not "install it".
pub fn order<'a>(available: &'a [Component], wanted: &[String]) -> Result<Vec<&'a Component>> {
    let by_name: BTreeMap<&str, &Component> =
        available.iter().map(|c| (c.name.as_str(), c)).collect();

    // Pull in prerequisites, transitively.
    let mut selected: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = wanted.iter().cloned().collect();
    while let Some(name) = queue.pop_front() {
        let Some(component) = by_name.get(name.as_str()) else {
            return Err(Error::Malformed(format!(
                "no database component named {name}"
            )));
        };
        if !selected.insert(name.clone()) {
            continue;
        }
        for prerequisite in &component.preinstall {
            queue.push_back(prerequisite.clone());
        }
    }
    // The tracker holds everyone else's level, so nothing can be recorded
    // without it.
    if by_name.contains_key(TRACKER) {
        selected.insert(TRACKER.to_string());
    }

    // Edges: prerequisite -> dependent.
    let mut incoming: BTreeMap<&str, BTreeSet<&str>> = selected
        .iter()
        .map(|n| (n.as_str(), BTreeSet::new()))
        .collect();
    for name in &selected {
        let component = by_name[name.as_str()];
        for prerequisite in &component.preinstall {
            if selected.contains(prerequisite) {
                incoming
                    .entry(name.as_str())
                    .or_default()
                    .insert(prerequisite.as_str());
            }
        }
        for dependent in &component.postinstall {
            if selected.contains(dependent) {
                incoming
                    .entry(dependent.as_str())
                    .or_default()
                    .insert(name.as_str());
            }
        }
        if name != TRACKER && selected.contains(TRACKER) {
            incoming.entry(name.as_str()).or_default().insert(TRACKER);
        }
    }

    // Kahn, taking ready components in name order so the result is stable.
    let mut out = Vec::new();
    while !incoming.is_empty() {
        let Some(next) = incoming
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(name, _)| *name)
            .next()
        else {
            let stuck: Vec<&str> = incoming.keys().copied().collect();
            return Err(Error::Malformed(format!(
                "database components depend on each other in a cycle: {}",
                stuck.join(", ")
            )));
        };
        incoming.remove(next);
        for deps in incoming.values_mut() {
            deps.remove(next);
        }
        out.push(by_name[next]);
    }
    Ok(out)
}

/// The databases a component ships scripts for.
pub fn databases(component: &Component) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Ok(sets) = fs::read_dir(component.root.join("scripts")) else {
        return out;
    };
    for set in sets.flatten() {
        let Ok(kinds) = fs::read_dir(set.path()) else {
            continue;
        };
        for kind in kinds.flatten() {
            if kind.path().is_dir() {
                out.insert(kind.file_name().to_string_lossy().to_string());
            }
        }
    }
    out
}

/// Work out how to bring a component to its newest version for one database.
///
/// A create set exists only at a handful of versions — for PostgreSQL, Trading
/// Networks ships exactly one, at 10.1 — so the rest of the distance is covered
/// by migrations. Where both a long chain and a direct jump exist
/// (`10.1-10.1.fix1…` and `10.1-10.3`), the shortest path is taken: the jump is
/// there precisely so it can be.
pub fn plan(component: &Component, database: &str) -> Result<Plan> {
    let scripts = component.root.join("scripts");
    let mut creates: Vec<String> = Vec::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    let Ok(entries) = fs::read_dir(&scripts) else {
        return Err(Error::Malformed(format!(
            "{} has no scripts directory",
            component.root.display()
        )));
    };
    for entry in entries.flatten() {
        let set = entry.file_name().to_string_lossy().to_string();
        // A set that ships nothing for this database is not a usable step.
        if !entry.path().join(database).is_dir() {
            continue;
        }
        match set.split_once('-') {
            Some((from, to)) => edges.push((from.to_string(), to.to_string())),
            None => creates.push(set),
        }
    }
    if creates.is_empty() {
        return Err(Error::Malformed(format!(
            "{} ships no {database} create scripts",
            component.name
        )));
    }
    creates.sort_by_key(|a| version_key(a));
    let create_from = creates.last().cloned().unwrap_or_default();

    // Shortest path from the create version to every version it can reach.
    let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (from, to) in &edges {
        adjacency.entry(from).or_default().push(to);
    }
    let mut previous: BTreeMap<String, String> = BTreeMap::new();
    let mut queue = VecDeque::from([create_from.clone()]);
    let mut seen = BTreeSet::from([create_from.clone()]);
    while let Some(current) = queue.pop_front() {
        for next in adjacency.get(current.as_str()).into_iter().flatten() {
            if seen.insert(next.to_string()) {
                previous.insert(next.to_string(), current.clone());
                queue.push_back(next.to_string());
            }
        }
    }
    let target = seen
        .iter()
        .max_by(|a, b| version_key(a).cmp(&version_key(b)))
        .cloned()
        .unwrap_or_else(|| create_from.clone());

    // Walk the predecessors back from the target to rebuild the chain.
    let mut chain = Vec::new();
    let mut cursor = target.clone();
    while let Some(from) = previous.get(&cursor) {
        chain.push(format!("{from}-{cursor}"));
        cursor = from.clone();
    }
    chain.reverse();

    let mut files = collect_scripts(&scripts.join(&create_from).join(database))?;
    for step in &chain {
        files.extend(collect_scripts(&scripts.join(step).join(database))?);
    }

    Ok(Plan {
        component: component.name.clone(),
        code: component.code.clone(),
        database: database.to_string(),
        create_from,
        migrations: chain,
        target,
        scripts: files,
    })
}

/// The `.sql` files of one script directory, in name order, dropping `drop.sql`.
fn collect_scripts(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_lowercase();
        // `drop.sql` is the uninstall half of the same directory. Running it
        // here would undo the create it sits beside.
        if path.is_file() && name.ends_with(".sql") && name != "drop.sql" {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Split a script into statements.
///
/// Statement separators inside string literals, dollar-quoted bodies and
/// comments are not separators. Getting this wrong does not fail loudly — it
/// sends half a statement to the server and reports a syntax error pointing at
/// the wrong place.
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = sql.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut dollar_tag: Option<String> = None;

    while let Some(c) = chars.next() {
        if let Some(tag) = &dollar_tag {
            current.push(c);
            if c == '$' && current.ends_with(tag) {
                dollar_tag = None;
            }
            continue;
        }
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(c);
            }
            '-' if !in_single && !in_double && chars.peek() == Some(&'-') => {
                for rest in chars.by_ref() {
                    if rest == '\n' {
                        break;
                    }
                }
                current.push('\n');
            }
            '/' if !in_single && !in_double && chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = ' ';
                for rest in chars.by_ref() {
                    if previous == '*' && rest == '/' {
                        break;
                    }
                    previous = rest;
                }
                current.push(' ');
            }
            '$' if !in_single && !in_double => {
                // A dollar-quoted body: `$$ … $$` or `$tag$ … $tag$`.
                let mut tag = String::from("$");
                while let Some(&next) = chars.peek() {
                    tag.push(next);
                    chars.next();
                    if next == '$' {
                        break;
                    }
                    if !next.is_alphanumeric() && next != '_' {
                        break;
                    }
                }
                current.push_str(&tag);
                if tag.ends_with('$') && tag.len() >= 2 {
                    dollar_tag = Some(tag);
                }
            }
            ';' if !in_single && !in_double => {
                push_statement(&mut out, &current);
                current.clear();
            }
            _ => current.push(c),
        }
    }
    push_statement(&mut out, &current);
    out
}

/// Keep a statement, having dropped any line that is a bare `/`.
///
/// Several of the shipped PostgreSQL scripts terminate a function body with a
/// lone `/` on its own line — an Oracle-ism that survived the port. Because it
/// carries no `;` of its own it is not a separate statement: it lands at the
/// front of whatever follows, and PostgreSQL rejects the lot. A `/` that means
/// division always shares its line with operands, so a line that is nothing but
/// a slash is safe to discard.
fn push_statement(out: &mut Vec<String>, current: &str) {
    let kept: Vec<&str> = current.lines().filter(|line| line.trim() != "/").collect();
    let trimmed = kept.join("\n");
    let trimmed = trimmed.trim();
    if trimmed.is_empty() {
        return;
    }
    out.push(trimmed.to_string());
}

/// Compare versions like `10.5.fix10` — numerically, and with `fixN` ordered
/// after the release it patches.
fn version_key(version: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for part in version.split('.') {
        match part.strip_prefix("fix") {
            Some(n) => {
                // `10.5.fix2` sorts after `10.5` and before `10.7`.
                out.push(n.parse().unwrap_or(0));
            }
            None => out.push(part.parse().unwrap_or(0)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_split_on_real_separators_only() {
        let sql = "CREATE TABLE a (x VARCHAR(8) DEFAULT 'a;b');\n\
                   -- a comment with ; in it\n\
                   INSERT INTO a VALUES ('x');\n\
                   /* block ; comment */\n\
                   SELECT 1";
        let parts = split_statements(sql);
        assert_eq!(parts.len(), 3);
        assert!(parts[0].contains("'a;b'"));
        assert!(parts[0].starts_with("CREATE TABLE"));
        assert!(parts[1].starts_with("INSERT"));
        assert_eq!(parts[2], "SELECT 1");
    }

    #[test]
    fn dollar_quoted_bodies_are_one_statement() {
        let sql = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN; RETURN 1; END; $$ LANGUAGE plpgsql;\nSELECT 2;";
        let parts = split_statements(sql);
        assert_eq!(parts.len(), 2, "got {parts:#?}");
        assert!(parts[0].contains("RETURN 1"));
        assert_eq!(parts[1], "SELECT 2");
    }

    #[test]
    fn a_lone_slash_is_not_a_statement() {
        // Several shipped PostgreSQL scripts end a function body with an
        // Oracle-style `/` line, which PostgreSQL rejects outright.
        let sql = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql;\n/\n\nSELECT 2;";
        assert_eq!(
            split_statements(sql),
            vec![
                "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql",
                "SELECT 2"
            ]
        );
    }

    fn component(name: &str, pre: &[&str], post: &[&str]) -> Component {
        Component {
            product: "P".into(),
            name: name.into(),
            code: name.chars().take(3).collect::<String>().to_uppercase(),
            root: PathBuf::from("/nowhere"),
            versions: vec!["1.0".into()],
            preinstall: pre.iter().map(|s| s.to_string()).collect(),
            postinstall: post.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn prerequisites_are_pulled_in_and_ordered_first() {
        let available = vec![
            component(TRACKER, &[], &[]),
            component("TradingNetworks", &[], &[]),
            component("TradingNetworksArchive", &["TradingNetworks"], &[]),
        ];
        // Asking for the archive alone must still install what it needs.
        let ordered = order(&available, &["TradingNetworksArchive".into()]).unwrap();
        let names: Vec<&str> = ordered.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [TRACKER, "TradingNetworks", "TradingNetworksArchive"]
        );
    }

    #[test]
    fn postinstall_orders_but_does_not_pull_in() {
        let available = vec![
            component(TRACKER, &[], &[]),
            component("ISInternal", &[], &["DistributedLocking"]),
            component("DistributedLocking", &[], &[]),
        ];
        // Asked for on its own, ISInternal does not drag the other in.
        let alone = order(&available, &["ISInternal".into()]).unwrap();
        assert_eq!(
            alone.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            [TRACKER, "ISInternal"]
        );
        // Asked for together, the constraint decides the order regardless of
        // how they were listed.
        let both = order(
            &available,
            &["DistributedLocking".into(), "ISInternal".into()],
        )
        .unwrap();
        assert_eq!(
            both.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            [TRACKER, "ISInternal", "DistributedLocking"]
        );
    }

    #[test]
    fn a_dependency_cycle_is_reported_not_hung_on() {
        let available = vec![component("A", &["B"], &[]), component("B", &["A"], &[])];
        let error = order(&available, &["A".into()]).unwrap_err().to_string();
        assert!(error.contains("cycle"), "got {error}");
    }

    #[test]
    fn an_unknown_component_is_named() {
        let available = vec![component("A", &[], &[])];
        let error = order(&available, &["Nope".into()]).unwrap_err().to_string();
        assert!(error.contains("Nope"), "got {error}");
    }

    #[test]
    fn versions_order_fixes_after_the_release_they_patch() {
        let mut vs = ["10.5.fix10", "10.5", "10.5.fix2", "10.7", "9.12"];
        vs.sort_by_key(|a| version_key(a));
        assert_eq!(vs, ["9.12", "10.5", "10.5.fix2", "10.5.fix10", "10.7"]);
    }
}

/// Where to install a schema.
#[derive(Debug, Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
}

impl Target {
    fn connection_string(&self) -> String {
        format!(
            "host={} port={} dbname={} user={} password={}",
            self.host, self.port, self.database, self.user, self.password
        )
    }
}

/// What applying a plan actually did.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    pub component: String,
    pub code: String,
    pub from: Option<String>,
    pub to: String,
    pub scripts: usize,
    pub statements: usize,
    /// True when the schema was already at the target and nothing ran.
    pub skipped: bool,
}

/// Open a connection.
pub fn connect(target: &Target) -> Result<postgres::Client> {
    postgres::Client::connect(&target.connection_string(), postgres::NoTls)
        .map_err(|e| Error::Exec(format!("cannot connect to {}: {e}", target.host)))
}

/// Read the level of every installed component.
///
/// Returns nothing at all when the tracking table does not exist yet, which is
/// the normal state of an empty database rather than an error.
pub fn installed(client: &mut postgres::Client) -> Result<BTreeMap<String, String>> {
    let exists: bool = client
        .query_one("SELECT to_regclass('component_event') IS NOT NULL", &[])
        .map_err(|e| Error::Exec(format!("cannot inspect the database: {e}")))?
        .get(0);
    if !exists {
        return Ok(BTreeMap::new());
    }
    let rows = client
        .query(
            "SELECT component_cd, component_level FROM installed_component",
            &[],
        )
        .map_err(|e| Error::Exec(format!("cannot read installed_component: {e}")))?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
        .collect())
}

/// Run a plan against a database and record the result.
///
/// Each script runs in its own transaction, so a failure leaves the database at
/// the last script that fully succeeded rather than half-way through one — and
/// the error names the file and the statement, which is the thing the shipped
/// configurator does not tell you.
pub fn apply(client: &mut postgres::Client, plan: &Plan, already: Option<&str>) -> Result<Applied> {
    if already == Some(plan.target.as_str()) {
        return Ok(Applied {
            component: plan.component.clone(),
            code: plan.code.clone(),
            from: already.map(str::to_string),
            to: plan.target.clone(),
            scripts: 0,
            statements: 0,
            skipped: true,
        });
    }
    if let Some(level) = already {
        return Err(Error::Exec(format!(
            "{} is already installed at {level}; migrating an existing schema in place is not \
             supported here — take a backup and run the migration steps deliberately",
            plan.component
        )));
    }

    let mut statements = 0usize;
    for script in &plan.scripts {
        let sql = fs::read_to_string(script).map_err(|e| Error::io(script.clone(), e))?;
        let mut transaction = client
            .transaction()
            .map_err(|e| Error::Exec(format!("cannot begin a transaction: {e}")))?;
        for statement in split_statements(&sql) {
            transaction.batch_execute(&statement).map_err(|e| {
                Error::Exec(format!(
                    "{}: {}\n  while running: {}",
                    script.display(),
                    describe(&e),
                    first_line(&statement)
                ))
            })?;
            statements += 1;
        }
        transaction
            .commit()
            .map_err(|e| Error::Exec(format!("{}: cannot commit: {e}", script.display())))?;
    }

    record(client, &plan.code, &plan.component, &plan.target)?;

    Ok(Applied {
        component: plan.component.clone(),
        code: plan.code.clone(),
        from: None,
        to: plan.target.clone(),
        scripts: plan.scripts.len(),
        statements,
        skipped: false,
    })
}

/// Write the `INSTALL` event every webMethods product reads to learn the level.
pub fn record(
    client: &mut postgres::Client,
    code: &str,
    description: &str,
    level: &str,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO component_event \
             (component_cd, component_desc, component_event, component_level) \
             VALUES ($1, $2, 'INSTALL', $3)",
            &[&code, &description, &level],
        )
        .map_err(|e| Error::Exec(format!("cannot record the install of {code}: {e}")))?;
    Ok(())
}

/// Render a database error usefully.
///
/// `postgres::Error` displays as a bare "db error"; everything that identifies
/// the problem — the server's message, the position, the hint — is in the
/// `DbError` behind it.
fn describe(error: &postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => {
            let mut text = format!("{}: {}", db.severity(), db.message());
            if let Some(detail) = db.detail() {
                text.push_str(&format!(" ({detail})"));
            }
            if let Some(hint) = db.hint() {
                text.push_str(&format!(" [hint: {hint}]"));
            }
            text
        }
        None => error.to_string(),
    }
}

fn first_line(statement: &str) -> String {
    let line = statement
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let line = line.trim();
    if line.len() > 90 {
        format!("{}…", &line[..90])
    } else {
        line.to_string()
    }
}
