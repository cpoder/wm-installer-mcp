# Applying fixes without Update Manager — verified

Run end to end against a throwaway 12.1 B2B installation, using the real IBM
fix service and real fix archives.

## The flow, measured

| step | result |
|---|---|
| `fixes_inventory` / `fixes_available` | IBM returns **6 fixes** applicable to the installation (12.1, 67 products), 0.17 GB |
| `fixes_download` | 6 archives, **0.17 GB in 79 s**, each verified against the sha256 the repository publishes |
| `fix_inspect` | reads the recipe: e.g. *Platform Manager Shared 12.1.0 FIX 1*, 28 entries, 1 phase |
| `fix_apply` (dry run) | 28 files would be written, 0 deleted |
| `fix_apply` | 28 files written |

`fix_apply` defaults to a dry run, which is the right first call: a fix expects
its runtimes stopped.

## Both directions verified

A fix lands in the p2 repository under `common/runtime/bundles/<group>/eclipse`.
That is only half the job — a fix that reaches the repository but never reaches
the runtime is useless. Both paths were checked.

**Fix first, then provision.** After applying `wMFix.CCShared_12.1.0.0001-0556`,
the repository held both `…cc.spm.abe_12.1.0.0000-0417.jar` and
`…_12.1.0.0001-0556.jar`. Provisioning a Platform Manager profile with the
lightweight resolver then selected the fixed build:

```text
com.webmethods.plm.cc.spm.abe          12.1.0.0001-0556
com.webmethods.plm.cc.spm.common       12.1.0.0001-0556
com.webmethods.plm.cc.spm.remote.core  12.1.0.0001-0556
com.webmethods.plm.cc.spm.util         12.1.0.0001-0556
```

**Provision first, then fix.** Applying
`wMFix.TPS.SharedBundles_12.1.0.0003-0779` to an installation that already had a
profile rewrote that profile in place — **151 files written, 22 bundles replaced
in profiles**:

```text
before   com.webmethods.tps.extra.jre.packages   12.1.0.0000-0280
after    com.webmethods.tps.extra.jre.packages   12.1.0.0003-0779
```

Afterwards the profile had **0 dangling `bundles.info` entries** out of 459, the
replacement jars were physically present in `plugins/`, and the previous list
was kept as `bundles.info.before-fix`.

## Still not covered

Actions in a fix recipe that require a p2 director are reported as *not
performed* rather than silently skipped. None of the six fixes above needed one.
