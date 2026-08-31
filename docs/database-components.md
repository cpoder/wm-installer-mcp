# Database components, natively

Trading Networks does not run without its schemas. The shipped
`common/db/bin/dbConfigurator.sh` is a shell wrapper around
`com.webmethods.dcc.cli.Main`: a JVM, a classpath of a dozen jars, and a set of
JDBC drivers. Everything it needs is already on disk in a form that requires
none of that.

## The layout

```text
common/db/<product>/<component>/config.json           name, code, versions
common/db/<product>/<component>/scripts/<v>/<db>/     a create set
common/db/<product>/<component>/scripts/<a>-<b>/<db>/ a migration
```

A directory name without a dash is a create set; with a dash it is a migration
from one version to another. Both exist per database — `postgresql`, `oracle`,
`sqlserver`, `db2`, `mysql`, `sybase` — and a component ships create sets for
only some of them.

## How a component is installed

Create sets exist at a handful of versions only. For PostgreSQL, Trading
Networks ships exactly one, at **10.1**; the remaining distance to **12.0** is
21 migrations. So the algorithm is: run the newest create set for the wanted
database, then walk migrations forward to the newest reachable version.

Where both a long chain and a direct jump exist (`10.1-10.1.fix1…` alongside
`10.1-10.3`), the **shortest** path is taken — the jump is in the distribution
precisely so it can be.

Then the result is recorded. `COMPONENT_EVENT` holds one row per install, and
the `INSTALLED_COMPONENT` view over it is how every webMethods product asks
what level its schema is at:

```sql
INSERT INTO component_event (component_cd, component_desc, component_event, component_level)
VALUES ('TNS', 'TradingNetworks', 'INSTALL', '12.0');
```

`ComponentTracker` — which *is* that table and view — must therefore be
installed before anything else, and is, always, first.

## Measured

Against a live PostgreSQL, from an empty database:

| | |
|---|---|
| ComponentTracker | 1 script, 2 statements |
| TradingNetworks | create 10.1 + 21 migrations, 25 scripts, 347 statements |
| TradingNetworksArchive | create 10.1 + 21 migrations, 22 scripts, 59 statements |
| **total** | **0.23 s**, 85 tables |

`installed_component` then reports `TNS 12.0`, `TNA 12.0`, `CTR 10.4` — which is
what the product reads.

## Two things the scripts do that a naive splitter gets wrong

**A lone `/`.** Several shipped PostgreSQL scripts terminate a function body
with an Oracle-style `/` on its own line. It carries no `;`, so it does not form
a statement of its own — it lands at the front of whatever follows and
PostgreSQL rejects the pair. A `/` meaning division always shares its line with
operands, so a line that is nothing but a slash is dropped.

**Dollar-quoted bodies.** `CREATE FUNCTION … AS $$ BEGIN … RETURN 1; END; $$`
contains semicolons that are not separators, as do string literals and both
comment forms. Splitting naively sends half a statement to the server and gets
back a syntax error pointing at the wrong place.

## What is covered

The plan is computed for **any** database — `database_plan` reports the create
set and migration chain for oracle, sqlserver, db2 and the rest, and names the
databases a component does not support. Execution is **PostgreSQL only**: the
other engines need their own drivers.

Two components in a 12.1 tree, `Reporting` and `Staging`, ship no PostgreSQL
scripts at all — only db2, oracle and sqlserver. That is the distribution's
choice, not a gap here.

## Tools

- `database_plan` — what would be installed, and how, without touching anything.
- `database_configure` — do it. Defaults to a dry run; one transaction per
  script, so a failure stops at the last script that fully succeeded and the
  error names the file and the offending statement. Re-running is a no-op.
