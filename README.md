# wm-installer-mcp

Two MCP servers for IBM webMethods 12.1: download and install products, create
Integration Server instances, provision Eclipse p2 profiles, create database
schemas, and find and apply fixes — driven from an agent, without the setup
wizard or Update Manager's console.

Installing into an installation that already exists is incremental: the
selection is subtracted from what is on disk, and a product the target carries
under another version is reported rather than overwritten.

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
| `install_verify` | check what it claims against what is on its disk |
| `plan_resolve` | close a product selection over its prerequisites |
| `native_plan` | price a selection: artifacts, size, install panels declared |
| `native_install` | download and install it |
| `instance_create` | create an Integration Server instance |
| `instance_update` | copy newly installed packages into an existing instance |
| `profile_provision` | provision a p2 profile with the shipped director |
| `profile_capture` / `profile_replay` | carry a profile between installations |
| `database_plan` / `database_configure` | plan and create database schemas |
| `script_generate` / `script_validate` | write and check an unattended installer script |
| `image_build` / `install_run` | drive the shipped installer |
| `job_status` | poll a running job; a failure comes back with its cause and log |
| `diagnose_log` | explain a failed run |
| `installer_check` | whether the download centre has outgrown the local installer |
| `install_register` / `install_list` / `install_show` / `install_forget` | name an installation and see what is in it |
| `config_show` / `config_set` | where everything lives, and the defaults |
| `credential_set` / `credential_list` / `credential_remove` | the encrypted credential store |

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

The tools that change this server's own configuration rather than an
installation — the registry, the defaults, the credential store — take effect
immediately. Every value in them is supplied by the caller, so there is nothing
for a dry run to disclose. The two that destroy something, `install_forget` and
`credential_remove`, take `confirm` instead.

## Installations and defaults

An installation is registered under a short name, and every tool that takes a
`wm_home` or an `install_dir` then accepts that name where the path used to go.

```console
$ install_register name=b2b wm_home=/opt/webmethods release=12.1

registered b2b -> /opt/webmethods (165 products, 4 runtime(s), 19 fix readme(s))

$ catalog_search install=b2b query=deployer

4 of 394 products match "deployer"; 3 already installed, 1 available to add.
searched:
  /opt/webmethods/install/products (165 products installed)
  ~/.wm-mcp/catalog/webM121-LNXAMD64.tree (394 products available)
```

A record holds what the installation does not say about itself — the release,
platform and download centre it was built from, the Update Manager home that
patches it, the selection that was asked for before the dependency closure — and
a dated snapshot of its component list. What is *installed* is read live from the
installation's own `install/products/*.prop` every time, because that stays
correct when a fix is applied or someone runs the shipped wizard; the snapshot is
the fallback for a path that is not mounted, and `install_show` reports the drift
between the two rather than hiding it.

Two directories, with different lifetimes:

| | | |
|---|---|---|
| state | `~/.wm-mcp`, moved by `WM_STATE_DIR` | product trees, artifacts, job logs — a cache, and the one that grows to gigabytes |
| config | `~/.wm-mcp/config`, moved by `WM_CONFIG_DIR` | the registry, the defaults, the credential store |

Config deliberately does not follow `WM_STATE_DIR`. That variable gets pointed at
scratch paths and working trees, and an entitlement key must not follow a cache
into either. `config_show` prints both, which is also the answer to "where did
that job's log go".

## Incremental installs

`native_install` reads the target before it unpacks anything.

```console
$ native_plan release=12.1 products='["Deployer","acdl"]' install=b2b

16 of 65 products to install (49 already present at these versions),
17 artifacts, 58 MB to download — against the whole closure's 957 MB
```

Three outcomes, and the middle one is the point:

- **already present at the same version** — not re-fetched.
- **present under a different version** — reported as *not performed*. A `.prop`
  file records the version a product was installed at, never its fix level, so a
  product Update Manager has patched still reports its base version. Unpacking a
  catalogue version over it replaces corrected files with base-version copies —
  including when the catalogue version is the newer of the two — and nothing
  afterwards records that the fix level dropped. The path that raises a patched
  product is Update Manager. `force: true` overwrites from the catalogue instead,
  and says what it is undoing.
- **absent** — installed.

`cargo run -p wm-core --example plan_delta -- <tree> <wm-home> <seed…>` performs
the same subtraction against a cached tree, with no credentials and no network.

## Installing a product is not the last step

A product that ships Integration Server packages lays them down under
`IntegrationServer/packages`, which is the *repository*. An instance loads
`IntegrationServer/instances/<name>/packages`, and nothing copies between the
two. So a successful install leaves the server answering `Unknown package` for
something the installation visibly contains, and nothing anywhere says a step is
missing.

```console
$ instance_update install=b2b

instance default is missing 3 package(s) the installation carries:
  WmBrokerDeployer
  WmDeployer
  WmDeployerResource

Call again with packages=[…] to copy specific ones, or packages=["all"] for
every non-core package. Nothing was changed.
```

`instance_update` drives the shipped `is_instance.sh update`. Called without a
package list it only reports the difference. A package is counted as present
only when it carries a `manifest.v3` — the file Integration Server reads to know
a package at all — so a directory left behind by a half-finished copy is
reported as missing, which is what it is.

A package the repository does not hold is refused before the script runs, and
the refusal looks for it elsewhere:

```console
$ instance_update install=b2b packages='["WmDeployerResource"]'

a package is not in /opt/webmethods/IntegrationServer/packages: WmDeployerResource.
…
But it does ship, published by another package for distribution rather than held
in the repository:
  /opt/webmethods/IntegrationServer/packages/WmDeployer/pub/WmDeployerResource.zip
A package that arrives this way is not installed by is_instance.sh at all.
```

## Claimed is not installed

`install/products/*.prop` records that a product was placed, which is what an
incremental plan trusts. `install/bms/*.contents` records every path that was
written, and `install_verify` reads it back.

```console
$ install_verify install=b2b

/opt/webmethods: 287 artifact(s), 38212 declared path(s).
685 absent — 305 superseded by a newer file, 380 unaccounted for.
```

A declared file being absent is usually correct, so the report classifies rather
than counts. Applying a fix deletes files and puts newer ones in their place and
nobody rewrites the manifest afterwards: on the installation this was built
against, `com.webmethods.osgi.agent.profile_12.1.0.0000-0497` is gone with
`…_12.1.0.0002-0579` beside it, and `org-eclipse-jgit-ssh-jsch-6.3.0.jar` with
`-7.4.0.jar`. Counting those as faults would report a correctly patched
installation as broken in sixteen places.

One finding is conclusive: an artifact of which *nothing* was written and nothing
accounts for the absence — a product recorded as installed that is not there, and
one an incremental plan will therefore skip. The rest are absences nothing
accounts for, which are worth a look and are not a verdict.

Two details the format forced. A manifest is written in either of two shapes —
the shipped installer's, with a `timestamp=` header and an octal mode before
every path, or a native install's, with a blank line and bare paths — and reading
only the second reports every file of the first as missing. And a few manifests
declare paths relative to `IntegrationServer/` rather than to the installation
root, saying so nowhere, so the base is chosen by where the files actually are.

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
- **An incremental plan trusts the installation's own record.** A product counts
  as present because `install/products/*.prop` says so; nothing opens a file to
  check. The one partial run measured is reassuring rather than otherwise — a
  shipped-installer run that failed part-way on 2026-09-18 wrote a `.prop` for
  each of the three products it had placed and for none of the thirteen it had
  not — but that is one observation, not a guarantee. Verifying an installation
  against the paths its `install/bms/*.contents` files name is not implemented.
- **The shipped installer cannot currently install anything.** IBM's code-signing
  certificate for the 12.1 modules expired on 2026-08-28, and the installer's
  `SignatureValidator` checks it against the current date. Every module is
  refused with `Verification failed: validity check failed`, the mirror step
  fails, and the run ends in a partial installation with `Provisioning step
  command exited with exit code 5`. The modules do carry an RFC 3161 timestamp
  token, which is what normally keeps a signature valid past its certificate's
  expiry. Nothing on the affected machine fixes this. `native_install` is
  unaffected, but not because it performs the same check by another route: it
  verifies each artifact against the sha256 the authenticated catalogue
  declares, and does not validate the JAR signature chain at all. That is a
  different trust model, and worth saying so to whoever signs off the
  installation. `diagnose_log` and `job_status` report it as
  `installer-signature-validity`.
- **There is no way to fetch an installer binary.** `installer_check` reads the
  local `.bin`'s version and compares it against the service level the catalogue
  declares, and `install_run` refuses to start a binary a generation behind
  rather than spending a minute discovering it. Getting a newer one is manual:
  the `.bin` is not in the product tree, and Passport Advantage and Fix Central
  authenticate an IBMid rather than an entitlement key.
- **The key is visible in the process list while Update Manager runs.**
  `UpdateManagerCMD.sh` takes it as `-empowerPass`, the only non-interactive
  way it accepts one, and a JVM's arguments are readable in `/proc` by any
  process of the same user for as long as it lives. The job's wrapper unsets
  the variable before Update Manager starts, so that is the one place left.
- **Developed and verified on Linux.** CI builds macOS and Windows binaries; the
  Update Manager path uses a Unix pseudo-terminal and is Unix-only.
- **Unofficial.** Not an IBM product, no support. It talks to IBM services with
  your own entitlement credentials.

## Credentials

`WM_EMPOWER_USER` and `WM_EMPOWER_KEY` still work and still win. What is new is
somewhere else to put them, because the entitlement key is not a password: it is
a bearer token that downloads against the account until it is revoked, and the
usual homes for it — an MCP client's configuration file, a shell export, a file
under `/tmp` — are all in clear.

Where the key goes when a job runs: the installer reads it from its
environment through the script's `$WM_EMPOWER_KEY$` placeholder; Update Manager
takes it as a command-line argument, and the job's wrapper unsets the variable
once the argument is expanded, because Update Manager copies its whole
environment into `UpdateManager/logs/debug/*.log`, mode 644. Neither job's
wrapper ever holds the value. What remains visible is the argument in the
process list for the length of an Update Manager run.

```console
$ credential_set name=empower.key value=…

stored empower.key in ~/.wm-mcp/config/credentials.enc (sealed with a 0600 key
file beside it: safe to back up or copy, but readable by anything running as
this user. Set WM_MCP_PASSPHRASE to take the key off disk entirely.)
```

AES-256-GCM, mode 0600 in a 0700 directory. Two ways to seal it, and it is worth
being exact about what each buys:

| | key | protects against |
|---|---|---|
| passphrase | PBKDF2-HMAC-SHA256 over `$WM_MCP_PASSPHRASE`, 600 000 iterations | anyone with the disk, the backup or the file |
| key file (default) | 32 random bytes in `key`, beside the store | disclosure — a `cat`, a screen share, a backup tarball, a directory copied into a repository. **Not** against anything running as you |

The key-file mode is the default because the alternative on an unattended
installation host is no store at all. Neither mode is a secrets manager; a site
that has one should keep the passphrase in it and let this hold the rest.

Values are never returned by a tool, never written into a generated installer or
Update Manager script, and never put in a job wrapper. The `$NAME$` convention is
unchanged: a script names the variable, the product substitutes it at read time,
and a detached job receives the value through its own environment while the
wrapper on disk holds only the name.

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

With the credential store filled, the `env` blocks come out entirely:

```json
{
  "mcpServers": {
    "wm-installer": { "command": "/path/to/wm-installer-mcp" },
    "wm-sum": { "command": "/path/to/wm-sum-mcp" }
  }
}
```

Both servers read the same store and the same registry, so an installation
registered through one is named the same way in the other.

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
