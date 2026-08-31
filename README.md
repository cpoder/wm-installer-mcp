# wm-installer-mcp

Two MCP servers that install, provision and patch IBM webMethods 12.1 **without
the shipped installer, without Update Manager, and without the Eclipse p2
director** — a pair of Rust binaries with no JVM anywhere in the path.

Everything below was measured on a real installation against IBM's real
download and fix services, not estimated.

| | shipped tooling | here |
|---|---|---|
| B2B install, download included | — | **191 s** (0.90 GB, 138 artifacts) |
| provision a Platform Manager profile | 30.7 s (p2 director) | **1.2 s** |
| peak memory to do it | 401 MB | 56 MB |
| Trading Networks schema, empty database → 85 tables | — | **0.23 s** |
| download 6 applicable fixes | — | 79 s, sha256-verified |

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
- `database_plan`, `database_configure` — install database schemas by running
  the product's own SQL, replacing `dbConfigurator.sh`.
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
- **The schema is never generated.** Tables come from the product's own `.sql`
  files, read from disk at run time. Nothing about a schema is compiled into
  these binaries, so a fix that ships new SQL is picked up with no code change.
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
