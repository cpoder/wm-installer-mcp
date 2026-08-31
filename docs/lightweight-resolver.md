# The lightweight resolver

## When p2 is your problem, and when it is not

Not every webMethods installation has a p2 profile. Measured on the 12.1
catalogue, an Integration Server selection (`PIECore`) closes to 29 products and
an IS + Microservices Runtime selection (`PIECore` + `MSC`) to 33 — **neither
pulls in a single product that needs one**. The only substantive install panel
either declares is `ISMultiInstancePanel`, the Integration Server instance, which
is Ant-driven rather than p2 and is reimplemented in `instance.rs`.

This is worth stating plainly because Trading Networks, the EDI module and AS2
all run *inside* an Integration Server instance. A headless B2B runtime never
touches p2.

p2 arrives with **My webMethods Server, Platform Manager and the Command Central
runtimes** — and therefore with the Trading Networks Portal UI, which is an MWS
application. Adding `TNPortal` to that same selection takes the closure from 29
products to 58, eleven of them bringing MWS and OSGI along. Everything below is
about that case.

## What p2 is, and why it is the bottleneck there

Platform Manager, My webMethods Server and the Command Central runtimes are not
plain Java processes. Each is an OSGi framework booting from an **Eclipse p2
profile**: a `configuration/` directory, a `plugins/` directory of several
hundred jars, and a `bundles.info` listing which bundle is installed, at what
version, at which start level, and whether it is started. Getting that list
right is the whole job — the framework does exactly what it says and nothing
else.

Computing that list is what the **p2 director** does, and it is a genuinely hard
problem *in general*. Installable units declare requirements with version
ranges, LDAP environment filters, optional and greedy flags, and singleton
constraints; a requirement may have many providers; and the director is expected
to return a *minimal* consistent set. In this metadata, **4,113 requirements
have more than one provider**. p2 uses a SAT solver, and it needs one.

The cost is not theoretical. On the reference installation, one profile costs
**30.7 seconds and 401 MB of peak RSS** — and the director does not run once. It
runs per profile, again when the product mix changes, and again for every fix
that touches a profile. That is the bulk of "the installer takes forever", and
it is also why the process is opaque: there is no way to ask the director what
it is about to do without letting it do it.

## Why it can be replaced here — and what that costs

p2 solves the general case: arbitrary repositories, arbitrary constraints,
contributed by parties who never met. A webMethods installation is not the
general case. The repositories ship together, the feature graph is closed and
vendor-curated, and `feature.xml` — the *definition* layer, as opposed to
`content.xml`, the *verification* layer where the ambiguity lives — names its
plugins at exact versions.

So the general solver is doing more work than the actual job requires. Walk the
feature graph, then close the OSGi wiring greedily. No SAT, no backtracking, no
minimality objective.

The bargain is explicit: **this is not a solver, and it carries no guarantee on
a product mix nobody has provisioned before.** It agrees with the director on
the profiles measured here. On a genuinely novel mix, run the vendor tool once
and capture the result — which takes 30 seconds, once, ever.

## Measured against the director

Same twelve SPM feature roots, same 35 repositories, the vendor's own director
on one side and this on the other:

| | p2 director | this resolver |
|---|---|---|
| wall clock | **30.7 s** | **1.2 s** |
| peak RSS | 401 MB | 56 MB |
| bundles computed | 498 | 501 |
| identical to the other | — | 496 of 498 |
| `started` flags differing | — | 0 of 513 |
| start levels differing | — | 2 of 513 |

The profile built from its output **starts and serves**: the Tomcat connector
binds ten seconds after `startup.sh` returns, `GET /spm/` answers 401, and
`GET /spm/inventory/products` with credentials returns HTTP 200 listing 149
installed products. Zero unresolved modules.

## The four things that actually decide correctness

Everything below was found by building a profile, starting it, and reading why
it did not come up. Each one was silent — nothing warned, the framework just
sat idle at its start level with no connector.

### 1. Version ranges, and more than one build of a bundle

`Import-Package: org.bouncycastle.asn1;version="[1.79.0,1.80.0)"` is not
satisfied by the newest `bcprov` on disk. Taking the newest build left five
BouncyCastle bundles unresolvable at boot. Two consequences:

- candidates must be filtered by the declared range, and the *lowest* build
  inside it preferred — a narrow range is a compatibility statement, and
  reaching past it drags in a second, unwanted family (the castor / axiom /
  jettison extras, 20 of them, all vanished when this landed);
- the selection is keyed on `(symbolic name, version)`, not on name. A profile
  legitimately installs `bcprov` twice, at 1.79.0 and 1.84.0, because different
  consumers ask for disjoint ranges.

### 2. Start levels live in four places, none of them the bundle's own unit

`content.xml` describes `com.webmethods.osgi.console` with no touchpoint data at
all. The start level is in a *separate* installable unit named
`configure.com.webmethods.osgi.console`, whose `configure` instruction carries
`setStartLevel` and `markStarted` — and whose `unconfigure` instruction carries
the opposite values, so reading the first `setStartLevel(...)` in the unit is
wrong half the time.

The four sources, in increasing precedence:

1. the product default: level 4, started;
2. `configure.<bundle>` units in a repository `content.xml`;
3. `p2.inf` inside a **feature** jar, declaring synthetic units keyed by index —
   `units.4.id=configure.<bundle>` joined to
   `units.4.instructions.configure=...` on that index, with whitespace that
   varies (`startLevel :2`, `startLevel: 2`, `startLevel:2` all occur);
4. `META-INF/p2.inf` inside the bundle jar itself.

Plus one rule that is not metadata at all: **a fragment is never started**,
because the framework has nothing to start. Tested against the reference
profile, `started = true unless Fragment-Host` is right for 489 of 490 bundles.
Getting this wrong marked 46 fragments and framework extensions as started.

### 3. Platform constraints are on the requirement edge

`feature.xml` lists `com.webmethods.plm.sd.introspect.custom.w32` with no `os`
or `arch` attribute whatsoever. The constraint is in the repository metadata, as
an LDAP filter on the feature group's requirement:

```xml
<required name='com.webmethods.plm.sd.introspect.custom.w64'>
  <filter>(&amp;(osgi.arch=x86_64)(osgi.os=win32))</filter>
</required>
```

Without evaluating it, a Linux profile quietly collects Windows binaries. Only
`osgi.os` / `osgi.ws` / `osgi.arch` filters are environment constraints —
`org.eclipse.update.install.sources` and its siblings are p2's own provisioning
switches, and treating those as constraints excludes most of the product.

### 4. What a capture may not throw away

Unrelated to the resolver, but it cost the longest debugging session here.
`configuration/tomcat` was treated as regenerated state. Only `work/` under it
is; `conf/` and `resources/` beside it are product content. Dropping them cost
the profile its `server.xml`, Tomcat substituted a stock one carrying an AJP
connector, that connector failed on `secretRequired="true"` with no secret, and
the exception aborted the start-level dispatch for every bundle behind it. The
symptom was a framework sitting idle with no HTTP connector and no error that
named the cause.

## What it does not do

The five bundles it adds beyond p2 are framework extensions and the launcher,
named in `config.ini` rather than installed from `bundles.info`; they are inert.
The one it omits is `com.webmethods.osgi.p2.actions`, needed only by p2 itself.
The one genuine disagreement is a version pick between two `nirvana.um.impl`
builds where p2 prefers a `-SNAPSHOT`.

Two start levels still differ from the reference (`org.eclipse.equinox.cm` and
`com.webmethods.osgi.audit.api`); neither prevented the profile from starting.
