# The webMethods installer's unattended interface

Reverse-engineered from `sagInstaller.jar` 12.1.0.0.123 (`com.wm.distman.install`,
`com.webmethods.installer`), decompiled with CFR, and checked against a live
12.1 installation.

## Entry point

`IBM_webM_Install_Linux_x64.bin` is a shell script with a gzipped tar appended
after a `PAYLOAD:` marker. It extracts to `$SAGINSTALLERDIR` (default
`$TMPDIR/saginstaller.$$`) and runs:

```
$SAGINSTALLERDIR/jvm/bin/java -cp sagInstaller.jar \
    com.wm.distman.install.DistManInstallMain <args>
```

Setting `SAGINSTALLERDIR` keeps the extracted tree, including the bundled JVM —
useful for reproducing a JVM-level failure in seconds rather than by re-running
an install.

There are two argument dialects. The classic one is `-`-prefixed and parsed by
`CommandLineArgs`; a newer picocli layer adds the `create` and `list`
subcommands with `--`-prefixed options for container images.

## The classic switches

Extracted from `CommandLineArgs`:

```
-console -installDir -readScript -writeScript -editScript
-readImage -writeImage -imageContents -imagePlatform -validateImage
-readImageScript -writeImageScript -editImageScript
-products -dumpProductTree -queryInstall
-URLBase -server -repoURL -user -pass
-adminPassword -masterPassword -acceptInnovationRelease
-scriptAutoDependencies -scriptErrorExit -scriptErrorInteract -scriptVar -scriptNoExit
-debug -debugLvl -debugFile -debugOut -debugErr -maxLogSize
-proxyHost -proxyPort -proxyUser -proxyPass -socksProxyHost -socksProxyPort
-SSLcert -SSLkey -SSLcacert -verifyFileSignature -sha256sum -md5sum
-skipDiskCheck -skipExternalJarCheck -skipLockedFilesCheck -skipSHA256Check -skipWriteCheck
-jdkHome -jreHome -javaProperties -uninstallEveryInstall -dummyInstall
```

`-debug` is deprecated and equivalent to `-debugLvl <n> -debugErr`. It writes
diagnostics to **stderr**, so `installer ... -debug 3 | tee install.log` produces
a log containing the failure line and nothing else. Use `-debugLvl verbose
-debugFile <path> -maxLogSize 20M`.

## The script

A Java `.properties` file. Values may contain `$NAME$` placeholders, substituted
from the environment when the script is read, which keeps credentials out of the
file. Keys whose name contains `pass`/`Pass`/`Password`/`Pwd` are stored
encrypted with a `@secure@` prefix when the installer writes a script itself.

```properties
InstallDir=/opt/webmethods
LicenseAgree=Accept
adminPassword=$WM_ADMIN_PASSWORD$
ServerURL=https://sdc.webmethods.io/cgi-bin/dataservewebM121.cgi
Username=$WM_EMPOWER_USER$
Password=$WM_EMPOWER_KEY$
InstallProducts=e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer,...
```

`DistManUtils.isScriptValid` requires, in this order:

1. `InstallDir`;
2. `InstallProducts` or `InstallLocProducts`;
3. either `Username` **and** `Password` **and** `ServerURL`, or an image file —
   spelled `ImageFile` or `imageFile`, both accepted.

Passing `-readImage` on the command line does **not** satisfy rule 3: the image
must be named in the script itself.

`adminPassword` is not part of `isScriptValid`, but since 12.1 the installer
exits with code 30 without it. `AdministratorPasswordUtil.validatePassword` only
rejects an empty value; the length and complexity rules come from the individual
products' password scripts and therefore fail later, not sooner.

`imageFile` **and** `imagePlatform` together put the installer into image-write
mode even without `-writeImage`.

## Product identifiers

`InstallProducts` takes versioned paths of exactly five segments:

```
e2ei/11/<CODE>_<VERSION>/<GROUP>/<COMPONENT>
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer
```

There is no way to guess the build number, so the identifiers have to come from
somewhere. A reference installation is the practical source: the installer writes
`<WM_HOME>/install/products/<Component>.prop` for every product it deployed, and
each declares its own path plus its prerequisites.

## Prerequisites: `requiresRegexp` is not a regular expression

```properties
product=e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer
<path>/props/requiresRegexp=e2ei/11/.*/.*/SCGCommon,e2ei/11/.*/.*/PIECore
<path>/props/requiresVersionRegexp=gte9.12
<path>/props/includeRegexp=e2ei/11/.*/.*/integrationServer:e2ei/11/.*/.*/TNSspm
```

`DistManUtils.productsMatch` splits both the pattern and the candidate on `/`,
requires **the same number of segments**, and compares segment by segment. A
pattern segment matches only when it is literally equal to the path segment, or
is exactly `*` or exactly `.*`. Nothing else in regex syntax is honoured.

This matters. Treating the patterns as anchored regexes — the obvious reading of
the name — handles the common `e2ei/11/.*/.*/SCGCommon` form by coincidence and
silently drops `e2ei/11/*/*/WISSharedLibs`, because as a regex `/*` means "zero
or more slashes". In a 12.1 catalogue exactly one pattern uses that spelling, and
the prerequisite it names is a real one; a regex-based resolver leaves it out of
the image and the installation fails an hour later.

`requiresVersionRegexp` is positionally aligned with `requiresRegexp`. Its
grammar, per `DistManUtils.versionMatch`, is `eq|gt|gte|lt|lte` followed by a
dotted version, combined with `&&` and `||`.

`productRequires`, when present, **overrides** `requiresRegexp` and
`optionalRegexp`. No product in a 12.1 installation uses it.

## What the closure cannot find

The installer does not complete a selection when writing an image: `-writeImage`
embeds exactly what `InstallProducts` names, and the install then reports
*"products they require do not exist in the image, local machine, or selected
installer server"*.

Two categories escape the closure entirely:

* **Undeclared mandatory products.** Nothing requires Infrastructure > License
  Agreement (`license`), Infrastructure > Java Package (`sjp`) or
  `CustomInstall`, yet the installer refuses to start without the first two:
  *"must exist in the installation image or the target directory"*.
* **Products with no `.prop` file.** A 12.1 installation carries no descriptor
  for webMethods Flat File even though the package is installed, so its path
  (`e2ei/11/WFF_10.7.0.0.30/IntegrationServer/WFF`) has to be supplied literally.
  It is a hard dependency of the EDI module: without it `WmEDI` stays unloaded
  and Integration Server reports "possibly due to circular dependencies", which
  points nowhere near the cause.

## Images

```bash
installer -console -readScript s.script -writeImage out.zip -imagePlatform LNXAMD64
installer -console -validateImage out.zip     # "is not missing any files and all checksums match"
installer -console -imageContents  out.zip
installer -console -readImage out.zip -readScript from-image.script
```

Platforms: `LNXAMD64`, `W64`, `AIX`, `SOLAMD64`, `LNXS390X`. The build downloads
before assembling, so `$TMPDIR` needs roughly twice the final image size.

## The bundled JVM

The installer ships IBM Semeru 21 / OpenJ9. On a host whose CPUID is reported
inconsistently — typically a hypervisor presenting a processor sub-type older
than the feature flags it also advertises — OpenJ9's two CPU-detection APIs
disagree and the JIT aborts before any installer code runs. The javacore shows
`Number of loaded classes 157` and the trace point `JIT: Fatal Crash in the JIT`.

`TR_DisableCPUDetectionTest=1` disables only that consistency check and is read
directly by the JIT, so a plain `export` suffices. Fallbacks, in order:
`SAG_JAVA_OPTIONS=-Xshareclasses:none`, then adding `-Xint`. The real fix is on
the hypervisor. The installed servers run their own OpenJ9 on the same CPU and
need the same setting in `custom_wrapper.conf`, which fixes do not overwrite.
