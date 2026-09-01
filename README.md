# wm-installer-mcp

Two MCP servers that install, provision and patch IBM webMethods 12.1 **without
the shipped installer, without Update Manager, and without the Eclipse p2
director** — a pair of Rust binaries with no JVM anywhere in the path.

## Which installs is this for, and what does it change

The answer differs by runtime, and the difference is worth getting straight
before any of the numbers below mean anything.

| your install | has a p2 profile? | what this changes |
|---|---|---|
| **Integration Server** | **no** | download, install, instance creation, database schemas, fixes — all planned before they run |
| **Microservices Runtime** | **no** | same |
| **Trading Networks / EDI / AS2** (they run *inside* an IS instance) | **no** | same |
| **My webMethods Server** | yes | the above, **plus** profile provisioning in 1.2 s instead of 30.7 s |
| **Platform Manager, Command Central** | yes | same |
| **Trading Networks Portal UI** (an MWS application) | yes | same |

Measured on the 12.1 catalogue: a `PIECore` selection closes to 29 products and
`PIECore` + `MSC` to 33 — **neither pulls in a single product that needs a p2
profile**. Adding `TNPortal` takes it to 58, eleven of them bringing MWS and OSGI
along.

So a headless B2B runtime — Trading Networks, the EDI module and AS2 on an
Integration Server, which is what usually goes to production — never touches p2
at all. For those installs the gain here is speed and, more to the point,
*plannability*. p2 is the story only once My webMethods Server, Platform Manager
or the TN web console is in the picture.

## Measured

Everything below was measured on a real installation against IBM's real
download and fix services, not estimated.

| | shipped tooling | here |
|---|---|---|
| B2B install, download included | — | **191 s** (0.90 GB, 138 artifacts) |
| Trading Networks schema, empty database → 85 tables | — | **0.23 s** |
| download 6 applicable fixes | — | 79 s, sha256-verified |
| provision a Platform Manager profile *(MWS-class installs only)* | 30.7 s (p2 director) | **1.2 s** |
| peak memory to do it | 401 MB | 56 MB |

The p2 comparison is head-to-head: the same twelve feature roots, the same 35
repositories, the vendor's own director on one side and this on the other. The
two agree on **496 of 498 bundles** and on **every** `started` flag.

## Why bother

The shipped installer is a Java wizard. Driving it unattended means a script
file, a JVM, a pseudo-terminal, and an Update Manager that exits `211` in
silence when a lock file survives a previous run. None of that is amenable to
automation, and none of it tells you what it is about to do before it does it.

These servers expose the same operations as MCP tools an agent can call, and
every destructive one **defaults to a dry run that reports the plan first**.

## The p2 problem

**This section applies to MWS-class installs only** — My webMethods Server,
Platform Manager, Command Central, the TN Portal UI. If you are installing
Integration Server, Microservices Runtime, or a headless Trading Networks / EDI
/ AS2 stack, you have no p2 profile and can skip it. The only substantive panel
those declare is `ISMultiInstancePanel`, the Integration Server instance, which
is Ant-driven rather than p2 and which `instance_create` already replaces.

Platform Manager, My webMethods Server and the Command Central runtimes each
boot from an **Eclipse p2 profile** — an OSGi framework plus a `bundles.info`
saying which of several hundred bundles is installed, at what version, at which
start level, and whether it is started. Computing that list is the job of the
**p2 director**, and in the general case it is genuinely hard: version ranges,
LDAP environment filters, optional and greedy flags, singletons, and an
objective that prefers a minimal consistent set. In this metadata **4,113
requirements have more than one provider**. p2 uses a SAT solver because it
needs one.

That costs **30.7 seconds and 401 MB of peak RSS per profile** — and the
director does not run once. It runs per profile, again when the product mix
changes, and again for every fix that touches a profile. It is also why the
process is opaque: there is no way to ask the director what it is about to do
without letting it do it.

**But a webMethods installation is not the general case.** The repositories ship
together, the feature graph is closed and vendor-curated, and `feature.xml`
names its plugins at exact versions. The general solver is doing more work than
the actual job requires. Walk the feature graph, close the OSGi wiring greedily,
and you land on the same answer: **496 of 498 bundles identical to the
director's**, every `started` flag identical, in **1.2 s and 56 MB**. The profile
it builds boots and serves.

The bargain is explicit, and it is the whole reason this is a *lightweight*
resolver rather than a replacement: **no SAT, no backtracking, no minimality
objective, and therefore no guarantee on a product mix nobody has provisioned
before.** For that case, run the vendor tool once and capture the result — 30
seconds, once, ever. For every case after that, 1.2 seconds.

The surprise was where the difficulty actually lived. Not in the closure — in
the metadata. Start levels are in four different places and never in the
bundle's own installable unit. Platform constraints sit on the requirement
*edge* in `content.xml` and are absent from `feature.xml` entirely. A profile
legitimately installs the same bundle at two versions at once. And a fragment is
never started, which nothing anywhere says. Each of those failed **silently** —
a framework idling with no HTTP connector and no error naming the cause.
[`docs/lightweight-resolver.md`](docs/lightweight-resolver.md) has the full
account.

## What it does

**`wm-installer-mcp`** — 18 tools

- `sdc_releases`, `sdc_catalog`, `catalog_search` — talk to IBM's download
  centre directly. Three separate wire protocols, all implemented natively.
- `native_plan`, `native_install` — close a product selection over its
  prerequisites, then download and install it. No installer binary.
- `instance_create` — create an Integration Server instance, which is what the
  wizard's `ISMultiInstancePanel` does.
- `profile_capture`, `profile_replay` — carry an Eclipse p2 profile between
  installations as a ~3 MB archive instead of a 218 MB directory.
- `database_plan`, `database_configure` — **run the product's own table-creation
  scripts**, replacing `dbConfigurator.sh`. See below.
- `script_generate`, `script_validate`, `install_run`, `image_build` — drive the
  shipped installer too, when you want to.
- `inventory_read`, `plan_resolve`, `diagnose_log`, `job_status`.

**`wm-sum-mcp`** — 13 tools

- `fixes_inventory`, `fixes_available`, `fixes_download` — ask IBM which fixes
  apply to an installation and fetch them, each verified against the sha256 the
  repository publishes.
- `fix_inspect`, `fix_apply` — read a fix's recipe and apply it natively:
  extract, delete, OSGi cache actions, and the profile `bundles.info` rewrite.
- `fixes_installed`, `fix_script_generate`, `fix_run`, `sum_locks`,
  `sum_result`, `diagnose_log`, `job_status` — drive Update Manager when needed,
  including clearing the stale lock behind its silent `211`.

## Database schemas: the product's scripts, executed — never reinvented

Trading Networks does not run without its schemas, and `dbConfigurator.sh` is a
shell wrapper around `com.webmethods.dcc.cli.Main` — a JVM, a dozen jars and a
set of JDBC drivers. `database_configure` replaces the wrapper, **not the SQL**.

The distinction matters, so to be unambiguous: **no DDL is compiled into these
binaries.** Every table, index, view and constraint comes from the product's own
`.sql` files, read from `common/db/` at run time and executed as shipped. The
schema stays versioned and patchable by IBM: a fix that ships new scripts is
picked up with no code change here. The only statement the binary composes
itself is the `INSERT` recording the install in `COMPONENT_EVENT` — the row
whose `INSTALLED_COMPONENT` view is how every webMethods product asks what level
its schema is at.

What it does add is the part `dbConfigurator.sh` performs in Java and never
shows you:

- **choosing the scripts.** Create sets exist at only a handful of versions —
  for Trading Networks on PostgreSQL, exactly one, at 10.1. The remaining
  distance to 12.0 is 21 migrations. So it runs the newest create set, then the
  *shortest* path through the migration graph.
- **ordering the components.** They are not independent: `TradingNetworksArchive`
  declares `preinstall: [TradingNetworks]`, `MywebMethodsServer` needs
  `TaskEngine` and `CommonDirectoryServices` before it and `CentralConfiguration`
  after. Prerequisites are pulled in and everything is topologically ordered, so
  asking for one component installs what it needs.
- **saying so first.** `database_plan` reports the create set, the migration
  chain and the script count without touching the database, for *any* engine.

Measured, from an empty PostgreSQL: ComponentTracker, TradingNetworks and
TradingNetworksArchive — 47 scripts, 408 statements, **0.23 s**, 85 tables.
Re-running is a no-op.

**One path, not the whole tool.** `com.webmethods.dcc.cli.Main` accepts a good
deal more than is covered here — its own option strings list `--action`,
`--fromVersion`, `--export` / `--import`, `--adminUser` / `--adminPassword`, and
`--tablespaceDir` / `--tablespaceForData` / `--tablespaceForIndex` /
`--tablespaceForBlob` / `--bufferpool`. What `database_configure` implements is
*create at the base version, migrate to the newest, record it*. No drop, no
export or import, no database user or schema creation, no explicit
`--fromVersion`. Migrating a schema that is already installed at a different
level refuses explicitly rather than guessing. The tablespace and bufferpool
options are Oracle and DB2 placement concerns, and so moot while execution is
PostgreSQL-only.

## The interesting parts

Four write-ups in [`docs/`](docs/), each the residue of something that failed
silently first:

- [`download-protocol.md`](docs/download-protocol.md) — IBM's download centre
  speaks three non-overlapping protocols depending on what you ask for.
- [`lightweight-resolver.md`](docs/lightweight-resolver.md) — replacing the p2
  director. Version ranges, start levels that live in four different places, and
  platform constraints that are on the requirement edge rather than the bundle.
- [`database-components.md`](docs/database-components.md) — how a schema is
  actually assembled: one create set, then a shortest path through a migration
  graph.
- [`fixes-verified.md`](docs/fixes-verified.md) — applying fixes both before and
  after a profile exists.

Two findings worth pulling out, because they are the kind of thing that costs a
day:

**`requiresRegexp` is not a regex.** The product's dependency patterns look like
regular expressions and are named as if they were. They are matched
segment-by-segment. Treating them as regexes silently drops real dependencies —
`e2ei/11/*/*/WISSharedLibs` matches nothing.

**A fragment is never started.** Nothing in the p2 metadata says so, but marking
fragments and framework extensions as started in `bundles.info` leaves the
framework sitting idle with no HTTP connector and no error naming the cause. The
rule `started = true unless Fragment-Host` is correct for 489 of 490 bundles in
a reference profile.

## Limits

Stated plainly, because the measurements above are only worth what the
caveats are.

- **Databases: PostgreSQL only for execution.** The *plan* is computed for
  Oracle, SQL Server, DB2, MySQL and Sybase — `database_plan` will show you the
  create set and migration chain for any of them. Running it is PostgreSQL only,
  and that is not a driver away: the shipped scripts use a lone `/` as a PL/SQL
  block terminator for Oracle and DB2 (373 and 308 occurrences), and `GO` as a
  batch separator for SQL Server (1192 occurrences). The same character that
  must be *discarded* in their PostgreSQL scripts must be *executed* in their
  Oracle ones. Each engine needs its own statement splitter and its own driver.
- **Two product panels are not reimplemented**: `TNServerConfigPanel` and
  `PortalStartConfiguratorSerenity`, which configure Trading Networks *inside* an
  instance. Everything else a 12.1 B2B selection declares is either replaced or
  cosmetic (licence acceptance, language pack choice).
- **Fix actions needing a p2 director** are reported as *not performed*, never
  silently skipped. None of the six fixes tested needed one.
- **The lightweight resolver is not a solver.** No SAT, no backtracking, no
  minimality objective. It agrees with the p2 director on the profiles measured
  here; it is not proven to agree on a product mix nobody has provisioned before.
  For that case, run the vendor tool once and capture the result.
- **Developed and verified on Linux.** CI builds macOS and Windows binaries, but
  the Update Manager path uses a Unix pseudo-terminal and is Unix-only; the
  native paths are portable.
- **Unofficial.** This is not an IBM product and carries no support. It talks to
  IBM services with your own entitlement credentials and installs software you
  are separately licensed for.

## Credentials

Referenced from the environment — `WM_EMPOWER_USER` and `WM_EMPOWER_KEY` — and
never written to disk. Generated job wrappers reference them by variable name,
so a key never lands in a script.

## Build

```sh
cargo build --release
./target/release/wm-installer-mcp    # stdio, speaks MCP
./target/release/wm-sum-mcp
```

Pre-built binaries for Linux, macOS and Windows are attached to each
[release](../../releases), with checksums.

## Client configuration

```json
{
  "mcpServers": {
    "wm-installer": {
      "command": "/path/to/wm-installer-mcp",
      "env": {
        "WM_EMPOWER_USER": "you@example.com",
        "WM_EMPOWER_KEY": "…"
      }
    },
    "wm-sum": {
      "command": "/path/to/wm-sum-mcp",
      "env": {
        "WM_EMPOWER_USER": "you@example.com",
        "WM_EMPOWER_KEY": "…"
      }
    }
  }
}
```

## License

MIT
