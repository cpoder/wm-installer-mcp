# Install panels, and which of them are now native

Placing files is only half of an installation. Each product also declares
**install panels** — Java classes the shipped installer runs at named stages:

```properties
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/panels=TNServerConfigPanel,TNServerInstallPanel
…/panels/TNServerInstallPanel/props/class=com.webmethods.tn.install.TNServerInstallPanel
…/panels/TNServerInstallPanel/props/stage=PostFileCopy
…/panels/TNServerConfigPanel/props/stage=PostProdSelect
```

They live in each product's resource jars, which is why a native installation
that only unpacks modules leaves a tree that is complete but inert. In a 12.1
B2B selection, 21 of 84 products declare panels.

Not all of them matter equally. The ones that decide whether anything runs
concern the Integration Server instance; the rest configure products *inside* an
instance that already exists, or notify components that may not be installed.

## Integration Server instance creation — reimplemented

The panel does not do the work itself: it drives
`IntegrationServer/instances/is_instance.xml`, an Ant build shipped with the
product. Reading it is what makes the step reproducible — `create` is a sequence
of file operations plus a single call into Java, for the password hash.

| Ant target | Reproduced by | Notes |
|---|---|---|
| `extractTemplate` | `instance::create` | unpacks `instances/template.zip`, dropping `*.bat` and `support/**` on Unix, then `chmod 755` on every `.sh` |
| `copyCorePackages` | `instance::create` | copies the packages named in `is_core_packages.properties`, falling back to the list hard-coded in the Ant file |
| `createServerCnfFile` | `instance::create` | writes `config/server.cnf` with the ports; the `{0} {1} {2}` placeholders in `watt.server.compile` are positional and survive verbatim |
| `createAdminPassFile` | `password::hash` | see below |
| `createSetEnvInstance-sh` | `instance::create` | `bin/setenv_instance.sh`, mode 755 |
| `invoke-instance-manager` | mostly | token substitution and the wrapper configuration are reproduced (below); the OSGi profile work is not — see the limits |
| `createJDBCPoolAlias` | not done | needs a database that does not exist yet |
| `notifyAPIGatewayOnInstanceCreate`, `notifyAgileAppsOnInstanceCreate` | not done | poke products that may not be installed |

### The administrator password

`createAdminPassFile` runs `com.wm.security.UpdateInstanceKey`, which writes two
files. The hash comes from `PasswordUtil.getPasswordHash_v2`:

* the material hashed is `"Administrator" + password + "SAG"` — the account name
  is a prefix and `SAG` a fixed suffix, so the same password hashes differently
  for a different account;
* 16 random bytes of salt;
* PBKDF2-HMAC-SHA256, **600 000 iterations**, a 256-bit key;
* the file holds `{PBKDF2-HmacSHA256_2}` followed by base64(key ‖ salt) — 85
  bytes in total.

One detail is easy to get backwards. The original reads

```java
new PBEKeySpec(password, salt, workFactor, iterations)
```

as though `iterations` (256) were the iteration count, but Java's signature is
`(password, salt, iterationCount, keyLength)`. The iteration count is
`workFactor` = 600 000 and `iterations` is the key length *in bits*. Read the
other way round it produces a plausible-looking hash the server rejects.

Verified two ways: the format matches the `installerKeyFile` a real installer run
left in `IntegrationServer/conf`, and the product's own
`PasswordUtil.checkPassword` accepts a hash produced by `wm_core::password`.

The shipped installer writes the pair to `IntegrationServer/conf`, which a later
instance inherits; the Ant file writes it into the instance. `instance::create`
does both, and never overwrites an existing installation-wide file.

### Template tokens

The instance template is not ready to run. Its scripts and wrapper
configuration ship with `{{TOKEN}}` placeholders that the shipped instance
manager fills in, and the Ant file does not touch:

| Token | Resolves to |
|---|---|
| `INSTALL_AREA` | `<wm_home>/IntegrationServer/instances/<name>` |
| `ROOT_PATH` + `SECURITY_LIB_DIR` | `<wm_home>/common` + `security/ssx/lib` |
| `JAVA_EXEC`, `JAVA_EXEC_PATH` | the bundled JVM's `bin/java` |
| `WRAPPER_EXEC`, `WRAPPER_EXEC_VER`, `WRAPPER_LIB` | `common/bin/wrapper-<v>`, the version, `common/lib/tw-<v>` |
| `STARTUP_JAR` | `<wm_home>/IntegrationServer/lib/wm-isproxy.jar` |
| `SERVICE_NAME`, `SERVICE_DISP_NAME`, `PRODUCT_DISPLAY_NAME`, `SERVICE_DESCR` | from the launcher script's own name and the Integration Server release |
| `LIBPATH`, `EXPORT_LD_LIB` | the `LD_LIBRARY_PATH` assignment and its export |

Every value is discovered from the installation — the JVM directory, the wrapper
version and the launcher's name all vary between releases. Left unsubstituted
the failure is opaque: `startup.sh` sources `{{INSTALL_AREA}}/bin/custom_setenv.sh`,
finds nothing, and reports only *"No such file"*.

`custom_wrapper.conf.template` needs more than tokens. It is a **sample**: it
carries Windows example paths (`c:\webMethods`, an instance called `i1`) and
omits the dozen settings the instance manager appends. Shipped as-is, the
wrapper resolves its working directory to `c:\webMethods\...\i1` and stops
before the JVM starts. `instance::create` rewrites the sample paths and appends
the working directory, console flush, service parameters, log4j configuration,
JMX port and the two extra classpath and library entries.

Two mode details also matter, and neither is in the Ant file. The template
stores every entry as `0644`; the Ant chmods `bin/*.sh`, but the service-wrapper
launcher has no extension, so it stays unreadable as a program and `startup.sh`
reports nothing but *Permission denied*.

### Verified

A native installation of Integration Server 12.1 — downloaded from IBM,
unpacked, instance created, all without a JVM or the shipped installer — starts
and serves:

```text
Running IBM webMethods Integration Server 12.1 (default)...
wrapper  | Java Service Wrapper Standard Edition 64-bit 3.5.60
…
listening on 5555, 9999, 8075

$ curl -u 'Administrator:…' http://localhost:5555/invoke/wm.server/ping
HTTP 200
$ curl -u 'Administrator:wrong' http://localhost:5555/invoke/wm.server/ping
HTTP 401
```

The password accepted there was hashed by `wm_core::password`, never by the
product.

Two complaints appear in the log and are not instance defects: `Component
directory not found for DCC:ComponentTracker` (that product was not in the
selection) and `keystore 'DEFAULT_IS_KEYSTORE' not found` (the HTTPS listener
has no keystore yet — post-install configuration, not instance creation).

## Symbolic links

Not a panel, but the same class of omission. Modules carry a `___symlinks`
manifest — `<link> <target>` per line, the target relative to the link's own
directory:

```text
common/security/openssl/lib64/libssl.so libssl-wm.so.3
common/security/openssl/lib64/libcrypto.so libcrypto-wm.so.3
```

Unpacked as an ordinary file, the links are simply missing and the libraries
cannot be found under the names that reference them. `install::unpack` now reads
the manifest and creates them, refusing an absolute target or one containing
`..`.

## Superseded: instance creation now drives the shipped script

The section above reimplemented what `is_instance.xml` performs through Ant.
That was the wrong call for the same reason it was wrong for the database
configurator: `IntegrationServer/instances/is_instance.sh`, the Ant in
`common/lib/ant` and the XML itself all ship with the product, and an instance
the product's own tooling created is one it recognises afterwards. Driving it
takes 6.8 s.

Two things only surfaced by driving it:

**The instance manager needs the installer's jars.** `is_instance.xml` forks
`com.webmethods.is.instance.InstanceManager` with `install/jars/DistMan.jar`,
`CustomInstall-ALL-Any.jar`, `wMInstTools` and the rest on its classpath. A
native install used to skip those as "resources for the shipped installer's
wizard"; it now installs them. `DistMan.jar` is the exception, and it is not a
limit: it *is* `sagInstaller.jar`, the installer's own jar, which the installer
lays down under that name. Replacing the installer includes doing that, so
`native_install` takes an `installer_jar` and installs it. Verified end to end —
an installation built entirely without the vendor installer, in which the
vendor's own `is_instance.sh` then creates an instance in 5.2 s.

**The native path skipped that fork entirely**, which is why it appeared to work
without any of it. Whatever `InstanceManager` does — profile registration, most
likely — the reimplementation never did.

The native builder is still reachable as `instance_create` with `native: true`,
for installations that have no `DistMan.jar`. It is documented as producing an
instance IBM's tooling did not create.

## What is still not native

**Eclipse p2 profiles — now native.** Superseded: the lightweight resolver in
`crates/wm-core/src/resolve.rs` computes a profile's bundle set without p2, and
the profile it produces starts and serves. See `docs/lightweight-resolver.md`
for the measurements (1.2 s against the p2 director's 30.7 s, 496 of 498
bundles identical) and for the four things that decide correctness.

**Database schemas — now native.** Superseded: `database_plan` and
`database_configure` install a component's create set and migration chain and
record the `INSTALL` event, with no JVM and no JDBC. See
`docs/database-components.md`. Execution is PostgreSQL only; the plan is
computed for every engine.

**Product-level panels.** `TNServerConfigPanel` and its kind configure a product
inside an instance. They are worth taking one at a time, against a real
requirement, rather than in bulk.
