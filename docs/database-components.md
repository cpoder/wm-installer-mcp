# Database components

Trading Networks does not run without its schemas. This is how they get created,
and — more importantly — who creates them.

## The product's own configurator does the work

`common/db/bin/dbConfigurator.sh` is a shell wrapper around
`com.webmethods.dcc.cli.Main`. It ships with the installation, along with its
Java classes, its JDBC drivers and a JVM at `jvm/jvm`. It runs from the command
line and self-documents with `--help`.

An earlier version of this reimplemented it: read the shipped `.sql` files,
split them into statements, execute them over a native PostgreSQL driver. That
worked, and it was still the wrong call, for two reasons.

**Support.** A schema IBM's tooling did not create is one IBM will not support.
That alone settles it.

**Fidelity, demonstrated.** Running both against the same product and diffing
the result, the native path recorded the component level as `12.0` where the
shipped configurator writes `v12.0` — same column, different format, and a
product comparing levels would see a mismatch. That bug existed only because the
work was duplicated, and it surfaced only because the two were compared.

There is a third reason that is merely practical: the shipped scripts use a lone
`/` as a PL/SQL block terminator for Oracle and DB2 (373 and 308 occurrences)
and `GO` as a batch separator for SQL Server (1192 occurrences), while the
PostgreSQL scripts want that same `/` discarded. Reimplementing means a
statement splitter and a driver per engine, forever. Driving the vendor's tool
means every engine webMethods supports works on day one.

## What is added around it

The configurator takes one component at a time and expects you to know which,
in what order. That is where the value is, and it comes from metadata already on
disk:

```text
common/db/<product>/<component>/config.json           name, code, versions, dependencies
common/db/<product>/<component>/scripts/<v>/<db>/     a create set
common/db/<product>/<component>/scripts/<a>-<b>/<db>/ a migration
```

**Prerequisites, pulled in.** Components are not independent.
`TradingNetworksArchive` declares `preinstall: [TradingNetworks]`;
`MywebMethodsServer` needs `TaskEngine` and `CommonDirectoryServices` before it
and `CentralConfiguration` after; `ISInternal` wants `DistributedLocking` after.
Eleven declarations across a 12.1 tree. Ask for `TradingNetworksArchive` alone
and you get `ComponentTracker`, `TradingNetworks`, then the archive — in that
order, because `preinstall` is a prerequisite and `postinstall` is an ordering
constraint between components already selected.

**`ComponentTracker` first, always.** It *is* the `COMPONENT_EVENT` table and the
`INSTALLED_COMPONENT` view over it — how every webMethods product asks what
level its schema is at. Nothing can be recorded before it exists.

**The plan, before anything runs.** `database_plan` reports, for **any** engine,
which create set applies and how many migrations follow — for Trading Networks
on PostgreSQL, create at 10.1 then 21 migrations to 12.0. It reads metadata and
touches no database. `database_configure` defaults to a dry run that prints the
exact commands, password masked.

## Measured

`database_configure` asking only for `TradingNetworksArchive`, against a live
PostgreSQL:

```text
configured: 3 component(s) on postgresql
  ComponentTracker         complete -> 10.4
  TradingNetworks          complete -> 12.0
  TradingNetworksArchive   complete -> 12.0
```

**5 seconds**, and `installed_component` afterwards reads `CTR v10.4`,
`TNA v12.0`, `TNS v12.0` — the vendor's format, because the vendor's tool wrote
it.

## What is not wrapped

`com.webmethods.dcc.cli.Main` accepts more than this drives: `--action` other
than create, `--fromVersion`, `--export` / `--import`, and `--runCatalog`. The
connection, admin-account and tablespace flags (`--adminUser`,
`--tablespaceForData`, `--bufferpool` and the rest) are passed through where
given. Drop, export and explicit migration are deliberately not wrapped — they
are destructive or stateful enough that the shipped CLI is the right place to
call them from.
