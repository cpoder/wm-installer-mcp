# Eclipse p2 profiles, and how updates are applied without Update Manager

My webMethods Server and Platform Manager do not run from an installed
directory the way Integration Server does. They run from an **Eclipse p2
profile** — `profiles/MWS_default`, `profiles/SPM` — provisioned by a p2
director from local repositories.

## What a profile is

```text
profiles/SPM/
  eclipse.ini                                  two lines
  plugins/                                     494 jars, copied in
  configuration/config.ini                     generated framework properties
  configuration/org.eclipse.equinox.simpleconfigurator/bundles.info
                                               500 lines: name,version,location,startLevel,started
  p2/org.eclipse.equinox.p2.engine/profileRegistry/minimal.profile/*.profile.gz
                                               653 installable units, one generation per change
```

The runtime reads `config.ini` and `bundles.info`. The registry under `p2/` is
read by p2 itself, when provisioning.

Each product declares its inputs rather than its output:

```properties
e2ei/11/SPM_12.1.0.0.417/Platform/SPM/props/osgiBundlesRepos=spm,cc-shared
e2ei/11/SPM_12.1.0.0.417/Platform/SPM/props/osgiProfileNames=SPM
```

and the installer records what it ran with, in `install/profiles/SPM.data`:

```properties
featuresList=com.webmethods.plm.spm.is.feature.feature.group,…
repositoriesList=file:/…/common/runtime/bundles/spm/eclipse/,…
```

The bundle groups under `common/runtime/bundles/*/eclipse/` are ordinary p2
repositories — `content.xml`, `artifacts.xml`, `features/`, `plugins/`. Creating
a profile means running a director over them.

## Creating a profile from scratch is a solver problem

This is not implemented, and the measurements are why. Against the real 12.1
metadata — 35 repositories, 1202 available units, a reference SPM profile of 653
— a series of closures from the 12 declared root features gives:

| Approach | Result |
|---|---|
| follow every requirement, all providers | 1088 units (435 too many) |
| honour `greedy='false'` and `optional` | unchanged |
| follow only the feature graph (`org.eclipse.equinox.p2.iu`) | 749 (116 extra, 20 missing) |
| exclude `*.feature.jar`, take highest in range, then satisfy capabilities | 325–1088 depending on the tie-break |

Every one of the 653 units *is* present in the local repositories, so nothing is
missing from the inputs. What is missing is p2's resolution: version ranges,
LDAP environment filters, singleton constraints, and an objective function that
prefers a minimal consistent set. p2 uses a SAT solver for exactly this reason,
and 4113 of the requirements in this metadata have more than one provider.

A closure that is nearly right is not useful here. A profile with a hundred extra
bundles usually starts, and then produces classloading behaviour that diverges
from every other installation — a worse outcome than not building it.

This matters less than it sounds for a B2B runtime: Integration Server, Trading
Networks, the EDI module and AS2 all run inside an Integration Server instance,
which **is** created natively (see `install-panels.md`). Platform Manager serves
fix management and Command Central; My webMethods Server provides the Trading
Networks web console.

## Capture and replay, instead of solving

A profile is deterministic for a given release and product selection, so it does
not have to be *computed* on every host — it can be built once and laid down
again. That is `wm_core::profile`.

What makes the capture small is that the bundles are already on the target.
Every jar in a profile's `plugins/` comes from the installation's own
repositories under `common/runtime/bundles/*/eclipse/plugins/`, and those arrive
with the products. So the capture carries the bundle list and the configuration,
and the replay copies the jars locally:

| Profile | On disk | Captured |
|---|---|---|
| `SPM` | 213 MB | **3.2 MB** |
| `MWS_default` | 460 MB | **3.4 MB** |

Three details decide whether the result actually runs.

**Not every jar is in `bundles.info`.** The framework, its extensions, the
launcher and the bootstrap hooks live in `plugins/` but are named by
`config.ini`. Replaying only what `bundles.info` lists leaves nine jars behind
and a profile that cannot start, so the capture records the whole directory.

**Not every bundle belongs in `plugins/`.** Thirteen are referenced in place
under `../../common/runtime/bundles/…`. Their location is carried verbatim and
they are not copied.

**Per-run state is not part of a profile.** A lock, a wrapper anchor, a pid or a
status file describes a process on the machine the capture came from; carried
along, they make a replayed profile look like it is already running and the
launcher declines to start. File modes are recorded per file rather than
guessed — marking everything executable is untidy, and missing the wrapper
launcher makes the profile unstartable.

**Paths are named two ways.** Absolute paths become `{{WM_HOME}}` and
`{{PROFILE_DIR}}`, but `config.ini` also reaches its framework extensions
*relatively* — `../../../../../../profiles/SPM/plugins/…` — which encodes the
profile's own name. Tokenising only the absolute form leaves a replayed profile
loading another profile's hooks, so `profiles/<name>/` becomes
`{{PROFILE_NAME}}` as well.

### Verified — the profile runs

`MWS_default` captured and replayed under another name in the same installation,
then started:

```text
captured MWS_default: 595 bundles, 61 config file(s), 3.4 MB   (profile is 460 MB)
replayed: 599 bundle(s) resolved, 61 file(s)

16:55:08  web application contexts deployed, Jetty serving
16:55:30  PortalServlet handling requests
          GET http://localhost:8585/  ->  HTTP 200
          <title>My webMethods: My webMethods Login Page</title>
16:55:53  shutdown requested
16:55:54  2 ERROR lines — an in-flight request whose commandManager had just
          been deactivated by that shutdown
```

Two error lines in a 9 700-line run, both one second after the stop began. The
replayed profile boots, resolves every bundle, deploys its web applications and
serves its UI.

Structural comparison, capturing `SPM` and replaying it under another name:

```text
captured SPM: 498 bundles, 97 config file(s), 3.2 MB
replayed: 494 bundle(s) resolved, 13 referenced in place

jar set identical:      YES
bundles.info identical: YES
config.ini identical after renaming the profile: YES
stale references to the source profile: 0
```

A bundle the capture names that the target does not carry is reported, not
guessed at: the profile would not start, and saying so is more useful than
handing back a partial one.

## Applying an update, however, is not a solver problem

A fix is the same shape as a product module — a signed JAR rooted at the
installation directory — plus two pieces of metadata that make it a recipe.

`META-INF/MANIFEST.MF`:

```text
Display-Fix-Name: Platform Manager 12.1.0 FIX 1
Fix-Name: wMFix.SPM
P2-Repositories: common/runtime/bundles/spm/eclipse
Require-SUM-Build: 11.0.0.0003-0257
```

`META-INF/instructions.txt`, numbered phases of `;`-separated actions, continued
across lines with a trailing backslash:

```text
install.phase3=osgiShutdown(profile:SPM);
install.phase4=delete(file:PlatformManager/migrate/lib);
install.phase5=extract(include:PlatformManager/**/*);\
osgiCleanCache(profiles:SPM);
```

The vocabulary, from Update Manager's own action bundles: `extract`, `copy`,
`move`, `delete`, `replace`, `backup`, `jar`, `update`, and the OSGi family
`osgiShutdown`, `osgiCleanCache`, `osgiInstall`, `osgiInstallIU`,
`osgiUninstall`, `osgiUninstallIU`, `osgiUpdate`, `osgiPublish`,
`osgiPlatformInstall`, `osgiPlatformUninstall`, `p2`.

`wm_core::fix` performs the file actions and the profile update:

1. entries under the `P2-Repositories` paths are unpacked, refreshing the
   repository the fix targets;
2. `extract` unpacks its glob, `delete` removes its path;
3. for every profile in the installation, each bundle listed in `bundles.info`
   that the fix ships a **newer build of** is copied into `plugins/` and its
   line rewritten. The previous `bundles.info` is kept beside it, and the
   superseded jar is left in place so a rollback needs no download;
4. `osgiCleanCache` removes the framework caches so the runtime re-reads its
   bundles.

Replacing a bundle already in a profile is not a resolve — the set stays the
same, one member changes version. Anything the fix ships that the profile does
**not** already carry is deliberately left alone: adding an installable unit is
the director's job, and is reported rather than guessed.

`osgiShutdown` is reported, not performed: a runtime may be under a service
manager, and stopping it midway through is worse than declining. `fix_apply`
defaults to a dry run and warns when a profile looks like it is running.

### Measured

Against Platform Manager 12.1.0 Fix 1 and a real installation:

```text
dry run: 94 file(s) would be written, 1 deleted,
         39 bundle(s) replaced in profiles, 3 cache(s) cleared; 1 warning(s)
  SPM com.webmethods.plm.spm.configuration 12.1.0.0000-0417 -> 12.1.0.0001-0556
  …
warning: profile SPM looks like it is running; stop it before applying
```

## A lighter resolver — built, and it starts

The section above was written before the thing existed. It does now, it works,
and the design sketched here turned out to be right about the shape and wrong
about the difficulty.

Right about the shape: `content.xml` is a *verification* layer, where the
ambiguity lives; `feature.xml` is the *definition* layer and is exact. Walking
the feature graph and then closing the OSGi wiring greedily does replace the
solve. Measured head to head against the real p2 director on the same twelve
roots and the same 35 repositories: **1.2 s against 30.7 s**, 56 MB against
401 MB, agreeing on **496 of 498 bundles** and on **every** `started` flag. The
profile it builds serves `/spm/inventory/products` with HTTP 200.

Wrong about the difficulty. The sketch expected the work to be in the closure —
"a repair pass would close them". The closure was the easy half. What actually
decided whether the framework came up was metadata archaeology: OSGi version
ranges and the fact that a profile installs one bundle at two versions at once;
start levels that live in four places and never in the bundle's own unit;
platform constraints that sit on the requirement *edge* in `content.xml` and are
absent from `feature.xml` entirely; and the unwritten rule that a fragment is
never started. Each of those failed silently — a framework idling with no HTTP
connector and no error naming the cause.

The full account, with the measurements, is in
[`lightweight-resolver.md`](lightweight-resolver.md).

## What is still Update Manager's

* the `osgi*IU` actions, which add or remove installable units;
* rewriting the p2 profile registry. A fix applied here changes `bundles.info`
  and the jars, which is what the runtime reads; the registry still describes
  the previous generation, so a later Update Manager run will see the profile as
  unpatched. For an installation managed entirely by these tools that is
  harmless; for one you intend to hand back to Update Manager, it is not.
