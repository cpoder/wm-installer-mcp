# wm-installer-mcp

Two MCP servers that plan, install, provision and patch IBM webMethods 12.1 from
an agent — a pair of Rust binaries.

## What is replaced, and what is not

The target is **the installer** — the Java setup wizard and Update Manager's
console. Those are what you cannot automate, cannot ask what they are about to
do, and cannot drive from an agent.

The target is **not the product's own tooling**. `dbConfigurator.sh`, the p2
director and `is_instance.sh` are installed *by* the installer and are part of
the product: current, supported, and drivable from a command line. An earlier
version of this reimplemented them, which was a mistake — a schema or a profile
IBM's tooling did not create is one IBM will not support, and the divergence was
real, not hypothetical: a native database path recorded a component level as
`12.0` where the shipped configurator writes `v12.0`, and a natively-built p2
profile leaves a registry a later Update Manager run misreads.

So the line is: **replace the installer, and drive what the installer installs.**
Replacing the installer includes doing everything it does — which is more than
unpacking archives. It lays down `install/jars/`, including its own jar as
`DistMan.jar`, because the product's tooling puts those on its classpath.

## What that means in practice

| the work | who does it | what this adds |
|---|---|---|
| download from IBM | **native** — there is no local tool to drive | three wire protocols, entitlement-checked, every artifact sha256-verified before it is written |
| install products | **native** — unpacking signed BM archives | prerequisite closure, dry-run plan |
| create a p2 profile | **the shipped p2 director** | finds the bootable runtime, builds the command, reports it first |
| copy a profile elsewhere | **native** — it is a file copy, not a solve | 3 MB archive instead of a 218 MB directory, 0.1 s |
| create database schemas | **the shipped `dbConfigurator.sh`** | prerequisite closure, topological order, the exact commands first |
| create an IS instance | **the shipped `is_instance.sh`** (Ant) | a dry run with the command, passwords masked |
| find and fetch fixes | **native** — IBM's fix service, over HTTP | inventory, applicability, verified download |
| apply a fix | **native** — the recipe is declarative | dry run, profile `bundles.info` rewrite, backups |

## Measured

Each figure below was taken today, against IBM's real services and a real
installation, with the current code — the one that drives the product's own
tooling. Nothing here is an estimate.

| | | runs |
|---|---|---|
| provision an SPM profile with the shipped p2 director | **21–27 s**, 498 bundles | 2 |
| create an IS instance with the shipped `is_instance.sh` | **5–7 s** | 2 |
| Trading Networks schema with the shipped `dbConfigurator.sh` | **5 s**, 3 components in dependency order | 1 |
| copy a provisioned profile to another machine | **0.1 s** (3 MB archive) | 1 |
| find and download 6 applicable fixes from IBM | **79 s**, sha256-verified | 1 |
| B2B install from nothing, download included | **3m 36s – 4m 21s** — 816 MB, 125 artifacts, 81 tooling jars | 3 |

Ranges are the spread actually observed, not error bars; the run count is
there so you can weigh them. Two runs of the same profile provisioning came out
at 21.4 s and 27.3 s on the same machine, which is worth knowing before anyone
treats any of these as a benchmark.

The install figures are genuine cold ones: empty artifact cache, empty
destination, `TNServer` + `EDIINT` + `PIECore` + `integrationServer` closing to
54 products. Three runs came out at 3m 36s, 3m 57s and 4m 21s — the spread is
download throughput, which varied between 2.6 and 3.8 MB/s. An earlier version
of this file claimed **191 s**; that was taken against a partly-warm cache and
never measured what it said. The honest figure is slower, and the install also
does more now — it lays down the 81 tooling jars it used to skip.

One cold run before this one failed part-way, at `Modes::read`, with an
incomplete deflate stream. It did not reproduce, and the obvious explanations do
not hold: the bytes had passed their sha256 before being written, the filesystem
had 780 GB free, and no other job shared the cache. It is recorded here rather
than quietly dropped, and the error message now names the archive and its size
so a recurrence has something to go on.

## Watching it work

A cold install takes about four minutes. Until recently the only feedback was
the tail of a log, which tells a person little and gives an agent nothing it can
render. A job now publishes `progress.json` beside its log, and there are two
ways to read it.

**At a terminal**, `--watch` draws the job and redraws in place:

```console
$ wm-installer-mcp --watch native-3078062-1788386815569-0
  BM_OSGiMigration-ALL-Any
  native-3078062-1788386815569-0

  █████████████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░   43%

  phase      downloading
  step       61 of 125
  fetched    350 MB of 812 MB  (3.80 MB/s)
  elapsed    1m 32s
  remaining  about 2m 01s

  BM_OSGiMigration-UNIX-Any
```

and when it lands:

```console
  ████████████████████████████████████████████████  100%

  phase      tooling jars
  step       80 of 80
  fetched    816 MB of 816 MB  (3.78 MB/s)
  elapsed    3m 36s
  state      done

  ZSLOSGIInstallMessages-ALL-Any

  installed 125 artifact(s) and 81 jar(s), 816 MB
```

**From an agent**, `job_status` returns the same figures as structured data, and
its one-line summary is written to be relayed verbatim:

```text
native-3078062-1788386815569-0: downloading — 43% (350 MB of 812 MB), 1m 32s elapsed, about 2m 01s left
```

Progress is measured in bytes rather than steps, because artifacts differ in
size by two orders of magnitude and a step count runs far ahead of the work. The
total is an estimate that revises itself: the product tree declares no size for
resource jars, so the real figure overshoots the plan, and the bar corrects
rather than reading 816 MB of 812 MB.

## Defaults are shown, not assumed

Every tool that changes anything defaults to a dry run, and the dry run names
each setting and where its value came from:

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

The server's instructions tell the agent to put that list to the user, take any
corrections, and only then call again with `apply: true`. Ports, instance names
and install locations all have defaults that are reasonable and frequently wrong
for a given site, and the user is the only one who knows which.

## Which installs is this for

The p2 story only applies to some of them, and that is worth getting straight.

| your install | has a p2 profile? |
|---|---|
| **Integration Server** | **no** |
| **Microservices Runtime** | **no** |
| **Trading Networks / EDI / AS2** (they run *inside* an IS instance) | **no** |
| My webMethods Server | yes |
| Platform Manager, Command Central | yes |
| Trading Networks Portal UI (an MWS application) | yes |

Measured on the 12.1 catalogue: a `PIECore` selection closes to 29 products and
`PIECore` + `MSC` to 33 — **neither pulls in a single product that needs a p2
profile**. Adding `TNPortal` takes it to 58, eleven of them bringing MWS and OSGI
along. A headless B2B runtime — the thing that usually goes to production —
never touches p2 at all.

## Tools

**`wm-installer-mcp`**

- `sdc_releases`, `sdc_catalog`, `catalog_search` — IBM's download centre,
  natively. Three separate wire protocols.
- `native_plan`, `native_install` — close a selection over its prerequisites,
  then download and install it.
- `profile_provision` — create a p2 profile **with the shipped director**, run
  from the installer's own bootstrap runtime at `install/profile`.
- `profile_capture`, `profile_replay` — carry a director-produced profile to
  another machine as a ~3 MB archive, p2 registry included.
- `database_plan`, `database_configure` — **run the product's own
  `dbConfigurator.sh`**, with prerequisites resolved and ordered first.
- `instance_create` — create an Integration Server instance by running the
  shipped `is_instance.sh`.
- `script_generate`, `script_validate`, `install_run`, `image_build` — drive the
  shipped installer, when you want it.
- `inventory_read`, `plan_resolve`, `diagnose_log`, `job_status`.

**`wm-sum-mcp`**

- `fixes_inventory`, `fixes_available`, `fixes_download` — ask IBM which fixes
  apply and fetch them, each verified against the published sha256.
- `fix_inspect`, `fix_apply` — read a fix's recipe and apply it: extract, delete,
  OSGi cache actions, profile `bundles.info` rewrite. Dry run by default.
- `fixes_installed`, `fix_script_generate`, `fix_run`, `sum_locks`,
  `sum_result`, `diagnose_log`, `job_status` — drive Update Manager, including
  clearing the stale lock behind its silent `211`.

Every destructive tool **defaults to a dry run that reports the plan first**.
That is the single most useful thing here, and none of the shipped tools do it.

## Reading

- [`download-protocol.md`](docs/download-protocol.md) — IBM's download centre
  speaks three non-overlapping protocols depending on what you ask for.
- [`database-components.md`](docs/database-components.md) — how a schema is
  assembled, and why the SQL is the vendor's to run.
- [`p2-profiles.md`](docs/p2-profiles.md) — what a profile is, and capture/replay.
- [`lightweight-resolver.md`](docs/lightweight-resolver.md) — **an experiment,
  not a supported path.** Replacing the p2 director's solve with a graph walk:
  1.2 s against 30.7 s, agreeing on 496 of 498 bundles. Kept because what it
  took to get there is the best documentation of the metadata that exists.
- [`fixes-verified.md`](docs/fixes-verified.md), [`install-panels.md`](docs/install-panels.md),
  [`installer-protocol.md`](docs/installer-protocol.md), [`sum-protocol.md`](docs/sum-protocol.md).

Two findings worth pulling out:

**`requiresRegexp` is not a regex.** The product's dependency patterns look like
regular expressions and are named as if they were. They are matched
segment-by-segment. Treating them as regexes silently drops real dependencies.

**A fragment is never started.** Nothing in the p2 metadata says so, but marking
fragments and framework extensions as started in `bundles.info` leaves the
framework idle with no HTTP connector and no error naming the cause.

## Limits

- **`install/jars/DistMan.jar` is the installer's own jar**, not a catalogue
  product — it is `sagInstaller.jar`, laid down under that name, and
  `is_instance.xml` puts it on the classpath of the instance manager it forks.
  Pass `installer_jar` to `native_install` and it is installed like anything
  else; without it, `instance_create` says so by name and `native: true` builds
  the instance directly instead — an instance that works, but that IBM's tooling
  did not create.
- **Fix application is native**, because a fix recipe is a declarative list of
  extract and delete actions rather than a program. Actions needing a p2
  director are reported as *not performed*, never silently skipped. It does
  **not** rewrite the p2 profile registry, so an installation you intend to hand
  back to Update Manager should be patched by Update Manager.
- **Two product panels are not reimplemented**: `TNServerConfigPanel` and
  `PortalStartConfiguratorSerenity`, which configure Trading Networks inside an
  instance.
- **`database_configure` implements one action**: create at the base version,
  migrate to the newest, per component. `com.webmethods.dcc.cli.Main` also
  offers `--action`, `--fromVersion`, `--export`/`--import`, `--adminUser` and
  the tablespace and bufferpool placement flags; those are passed through where
  they exist as inputs, but drop, export and explicit migration are not wrapped.
- **`profile_provision` needs `install/profile`**, the installer's own bootstrap
  p2 runtime. `common/runtime/bundles/platform/eclipse` holds the launcher and
  the director but has no `config.ini`, so running from it fails.
- **Developed and verified on Linux.** CI builds macOS and Windows binaries; the
  Update Manager path uses a Unix pseudo-terminal and is Unix-only.
- **Unofficial.** Not an IBM product, no support. It talks to IBM services with
  your own entitlement credentials.

## Credentials

Referenced from the environment — `WM_EMPOWER_USER` and `WM_EMPOWER_KEY` — never
written to disk. Generated job wrappers reference them by variable name, so a key
never lands in a script.

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
