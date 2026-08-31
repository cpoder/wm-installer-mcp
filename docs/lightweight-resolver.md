# The lightweight resolver

An alternative to the Eclipse p2 director for computing a profile's bundle set.
It is not a solver: no SAT, no backtracking, no minimality objective. It walks
the feature graph and then closes the OSGi wiring greedily.

Measured against the real p2 director on the same twelve SPM feature roots and
the same 35 repositories:

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
