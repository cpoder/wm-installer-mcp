# wm-installer-mcp

Two MCP servers for IBM webMethods 12.1: download and install products, create
Integration Server instances, provision Eclipse p2 profiles, create database
schemas, and find and apply fixes — driven from an agent, without the setup
wizard or Update Manager's console.

Written in Rust. Where the product ships tooling, that tooling does the work;
these servers plan it, order it, run it and report on it.

## What each step uses

| step | performed by |
|---|---|
| talk to IBM's download centre | native — three wire protocols |
| download and install products | native — signed BM archives, sha256-verified |
| create an Integration Server instance | the shipped `IntegrationServer/instances/is_instance.sh` |
| provision a p2 profile | the shipped p2 director, run from `install/profile` |
| copy a profile to another machine | native — a 3 MB archive of a 218 MB directory |
| create database schemas | the shipped `common/db/bin/dbConfigurator.sh` |
| find, fetch and apply fixes | native — IBM's fix service, and the fix recipe |

## Tools

**`wm-installer-mcp`**

| tool | does |
|---|---|
| `sdc_releases` | releases this IBM account is entitled to |
| `sdc_catalog` | fetch and cache a release's product tree |
| `catalog_search` | find products and their exact versioned paths |
| `inventory_read` | read an installed webMethods home |
| `plan_resolve` | close a product selection over its prerequisites |
| `native_plan` | price a selection: artifacts, size, install panels declared |
| `native_install` | download and install it |
| `instance_create` | create an Integration Server instance |
| `profile_provision` | provision a p2 profile with the shipped director |
| `profile_capture` / `profile_replay` | carry a profile between installations |
| `database_plan` / `database_configure` | plan and create database schemas |
| `script_generate` / `script_validate` | write and check an unattended installer script |
| `image_build` / `install_run` | drive the shipped installer |
| `job_status` | poll a running job |
| `diagnose_log` | explain a failed run |

**`wm-sum-mcp`**

| tool | does |
|---|---|
| `fixes_inventory` | build the document that describes an installation to IBM |
| `fixes_available` | ask IBM which fixes apply |
| `fixes_download` | fetch them, verified against the published sha256 |
| `fix_inspect` | read a fix archive's recipe |
| `fix_apply` | apply it: extract, delete, OSGi cache, profile `bundles.info` |
| `fixes_installed` | list what is already patched |
| `fixes_parse_metadata` | parse a p2 fix metadata archive |
| `fix_script_generate` / `fix_run` | drive Update Manager unattended |
| `sum_locks` | clear the stale lock behind Update Manager's silent `211` |
| `sum_result` | decode `bin/result.json` |
| `job_status`, `diagnose_log` | poll and explain |

## Dry runs and defaults

Every tool that changes anything defaults to a dry run, which names each setting
and where its value came from:

```console
$ instance_create wm_home=/opt/webmethods name=demo

dry run: would create instance demo. Put the settings below to the user,
confirm or amend them, then call again with apply=true.

  name             demo                                             you asked for it
  primary_port     5555                                             default
  secure_port      5543                                             default
  diagnostic_port  9999                                             default
  jmx_port         8075                                             default
  admin_password   the password set when the product was installed   default
  database         embedded                                         default
  bind_address     every interface                                  default
```

The server instructs the agent to put that list to the user, take corrections,
and only then call again with `apply: true`.

## Progress

A job publishes `progress.json` beside its log. `job_status` returns it as
structured data with a one-line summary:

```text
native-3078062-1788386815569-0: downloading — 43% (350 MB of 812 MB), 1m 32s elapsed, about 2m 01s left
```

`--watch` draws it at a terminal, redrawing in place:

```console
$ wm-installer-mcp --watch native-3078062-1788386815569-0
  native-3078062-1788386815569-0

  █████████████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░   43%

  phase      downloading
  step       61 of 125
  fetched    350 MB of 812 MB  (3.80 MB/s)
  elapsed    1m 32s
  remaining  about 2m 01s

  BM_OSGiMigration-UNIX-Any
```

```console
  native-3078062-1788386815569-0

  ████████████████████████████████████████████████  100%

  phase      tooling jars
  step       80 of 80
  fetched    816 MB of 816 MB  (3.78 MB/s)
  elapsed    3m 36s
  state      done

  ZSLOSGIInstallMessages-ALL-Any

  installed 125 artifact(s) and 81 jar(s), 816 MB
```

Progress is measured in bytes rather than steps: artifacts differ in size by two
orders of magnitude. The expected total revises itself upward, because the
product tree declares no size for resource jars.

## Measured

Against IBM's real services and a real installation.

| | | runs |
|---|---|---|
| B2B install from nothing, download included | **3m 36s – 4m 21s** — 816 MB, 125 artifacts, 81 tooling jars | 3 |
| provision an SPM profile | **21–27 s**, 498 bundles | 2 |
| create an IS instance | **5–7 s** | 2 |
| Trading Networks schema, 3 components in dependency order | **5 s** | 1 |
| copy a provisioned profile to another machine | **0.1 s** (3 MB archive) | 1 |
| find and download 6 applicable fixes | **79 s**, sha256-verified | 1 |

Ranges are the spread observed across the stated number of runs, not error bars.
Install time varies with download throughput, which ranged from 2.6 to 3.8 MB/s.

## Which installs have a p2 profile

| install | p2 profile |
|---|---|
| Integration Server | no |
| Microservices Runtime | no |
| Trading Networks, EDI, AS2 — they run inside an IS instance | no |
| My webMethods Server | yes |
| Platform Manager, Command Central | yes |
| Trading Networks Portal UI — an MWS application | yes |

Measured on the 12.1 catalogue: a `PIECore` selection closes to 29 products and
`PIECore` + `MSC` to 33, neither needing a p2 profile. Adding `TNPortal` takes it
to 58, eleven of them bringing MWS and OSGI along.

## Limits

- **`install/jars/DistMan.jar` is the installer's own jar** (`sagInstaller.jar`),
  not a catalogue product, and `is_instance.xml` puts it on the instance
  manager's classpath. Pass `installer_jar` to `native_install` to lay it down.
  Without it, `instance_create` says so and `native: true` builds the instance
  directly instead — an instance that works, but that IBM's tooling did not
  create.
- **`database_configure` implements one action**: create at the base version,
  migrate to the newest, per component. `com.webmethods.dcc.cli.Main` also offers
  `--action`, `--fromVersion`, `--export`/`--import` and `--runCatalog`, which
  are not wrapped. Connection, admin-account and tablespace flags are passed
  through.
- **`fix_apply` does not rewrite the p2 profile registry.** It changes
  `bundles.info` and the jars, which is what the runtime reads. An installation
  you intend to hand back to Update Manager should be patched by Update Manager.
  Recipe actions needing a p2 director are reported as *not performed*, never
  silently skipped.
- **`profile_provision` needs `install/profile`**, the installer's bootstrap p2
  runtime. `common/runtime/bundles/platform/eclipse` holds the launcher and the
  director but has no `config.ini`, so running from there fails.
- **Two product panels are not covered**: `TNServerConfigPanel` and
  `PortalStartConfiguratorSerenity`, which configure Trading Networks inside an
  instance.
- **Developed and verified on Linux.** CI builds macOS and Windows binaries; the
  Update Manager path uses a Unix pseudo-terminal and is Unix-only.
- **Unofficial.** Not an IBM product, no support. It talks to IBM services with
  your own entitlement credentials.

## Credentials

Read from `WM_EMPOWER_USER` and `WM_EMPOWER_KEY`, never written to disk.
Generated job wrappers reference them by variable name.

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

## Reference

- [`download-protocol.md`](docs/download-protocol.md) — the three protocols
  `sdc.webmethods.io` speaks.
- [`installer-protocol.md`](docs/installer-protocol.md) — the installer's script
  format and validation rules.
- [`install-panels.md`](docs/install-panels.md) — what each install panel does.
- [`p2-profiles.md`](docs/p2-profiles.md) — profile structure, capture and replay.
- [`database-components.md`](docs/database-components.md) — how a schema is
  assembled from create sets and migrations.
- [`sum-protocol.md`](docs/sum-protocol.md) — Update Manager's script format and
  failure modes.
- [`fixes-verified.md`](docs/fixes-verified.md) — the fix flow, end to end.
- [`lightweight-resolver.md`](docs/lightweight-resolver.md) — an experiment:
  computing a profile's bundle set without the p2 director. Not a supported path.

Two things worth knowing if you work with this metadata:

**`requiresRegexp` is not a regex.** Dependency patterns are matched
segment-by-segment. Treating them as regexes silently drops real dependencies.

**A fragment is never started.** Marking fragments and framework extensions as
started in `bundles.info` leaves the framework idle with no HTTP connector and no
error naming the cause.

## License

MIT
