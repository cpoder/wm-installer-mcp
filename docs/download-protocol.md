# The IBM webMethods download protocol

Everything the shipped installer and Update Manager do over the network, written
down. Reverse-engineered from `sagInstaller.jar` 12.1.0.0.123 and the
`com.webmethods.fixinstall.*` bundles of Update Manager 12.0.0.0008, then
verified end to end against `sdc.webmethods.io` with a real entitlement.

Three protocols are involved and they do not share credentials — which is the
single fact that makes this look impenetrable from the outside.

## 1. REST API — the catalogue

Base `https://sdc.webmethods.io/services/`.

```http
POST /services/auth
Content-Type: application/json

{"username": "you@example.com", "password": "<entitlement key>"}
```

Answers an OAuth bearer token (`access_token`, `expires_in` 3600, plus a
`refresh_token` good for ten hours). Everything else in this section is
`Authorization: Bearer …`.

| Endpoint | Returns |
|---|---|
| `sd-access-service/entitlements/suites?installerVersion=…` | the releases this account may install |
| `sd-repository-service/v1/repositories/sandboxes` | every sandbox, with its repository URL |
| `sd-repository-service/v1/repositories/sandboxes/<sandbox>` | one sandbox, including its `fixRepository` |
| `sd-access-service/entitlements/products?sandbox=…&platform=…` | the **product tree** |

The product tree is the important one. `Accept: application/octet-stream`, and
the answer for 12.1 on `LNXAMD64` is 2.1 MB of flat `key=value` lines describing
**394 products and 1184 artifacts**, each with size, md5 and sha256, plus 662
prerequisite declarations. It is the same dialect as the `.prop` files an
installation carries, so no reference installation is needed — and it is more
complete than one: a 12.1 tree on disk has no descriptor for webMethods Flat
File, while the served catalogue does.

Names: a release's CGI URL `…/cgi-bin/dataservewebM121.cgi` yields the repository
`dataservewebM121` by stripping `cgi-bin/` and `.cgi`, and the sandbox `webM121`
by then stripping `dataserve`.

## 2. Protocol G — the handshake

The artifact repository refuses both the bearer token and the account's own
credentials. It wants a short-lived grant, obtained from the release's CGI:

```http
POST https://sdc.webmethods.io/cgi-bin/dataservewebM121.cgi?G
Authorization: Basic base64(<account lowercased>:<entitlement key>)
Cookie: SD_SERVER_ENVIRONMENT_VERSION=1
Content-Type: application/x-www-form-urlencoded

locale=en_US&buildNo=123
```

```text
OK,a=u26_242_46358:<password>
```

The `a=` field carries the credentials for sections 3 and 4. The account name is
lower-cased before encoding — `DSConnect` does this and the server enforces it.

## 3. Artifacts — the products

`Authorization: Basic` with the grant from section 2.

An artifact listed in the tree at

```text
e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/TNServer-LNXAMD64-Any/BM_TNSWmTN-ALL-Any
```

is fetched from

```text
https://sdc.webmethods.io/dataservewebM121/e2ei/11/TN_12.1.0.0.139/bms/BM_TNSWmTN-ALL-Any.zip
```

Only the first three segments and the last matter: the group, the component and
the platform variant are metadata, not location. Resource jars — the shipped
wizard's own panels and message bundles, which a native install does not need —
live under `<release>/jars/<name>.jar` instead.

A module is a signed JAR whose entries are already rooted at the installation
directory, plus `META-INF/` and a `___comment_block` naming the module, its
version, and the Unix mode of every file it carries. Installing one is: fetch,
check the sha256 the tree declared, unpack everything that is not metadata,
apply the recorded modes.

## 4. Fixes

Which fixes apply is a question about an installation, so it is asked with one:

```http
POST /services/sum-repository-service/repositories/prodRepo_WM/fixes?showAll=false
Authorization: Bearer …
X-IBM-wMSUM-P2-SCHEMA: WM

{"envVariables": {"platform": "LNXAMD64", "platformGroup": ["LNXAMD64"],
                  "UpdateManagerVersion": "12.0.0.0008", "Hostname": "…"},
 "installedProducts": [{"productId": "TNS", "version": "12.1.0.0.139",
                        "displayName": "TNServer"}],
 "installedFixes": [], "installedSupportPatches": []}
```

`platform` is a string and `platformGroup` an array; the service rejects the
request either way round, which is worth knowing before spending an afternoon on
it. `productId` is the product *code* (`TNS`), not the versioned path.

The answer is a p2 metadata archive: `content.jar` holding `content.xml`, one
`<unit>` per fix carrying `com.webmethods.wm.fix.*` properties — target product,
size, release date, prerequisite fixes, minimum Update Manager build.

Fix binaries come from a standard p2 artifact repository at
`https://sdc.webmethods.io/updates/<fixRepository>/`, again with the protocol-G
credentials. `artifacts.jar` holds `artifacts.xml`: 16 000-odd entries with
`download.size` and `download.sha256`, and four mapping rules —

```text
binary                      -> ${repoUrl}/binary/${id}_${version}
osgi.bundle                 -> ${repoUrl}/plugins/${id}_${version}.jar
org.eclipse.update.feature  -> ${repoUrl}/features/${id}_${version}.jar
readme                      -> ${repoUrl}/readme/${id}_${version}_readme.txt
```

## What is not reimplemented

**Install panels.** Products declare Java classes the shipped installer runs at
named stages (`PostProdSelect`, `PostFileCopy`) — `TNServerInstallPanel`,
`ISLauncherInstallPanel` and so on. They create Integration Server instances,
seed the administrator password, write wrapper configuration. They are compiled
code inside each product's resource jars, so file placement is reproducible and
those actions are not. A plan reports which selected products declare panels, so
the gap is visible before anything is written. In a 12.1 B2B selection that is 21
products out of 84.

**Applying a fix.** Downloading and verifying is done; the provisioning itself is
p2 — bundle replacement, profile rewriting, `com.webmethods.fixinstall.actions` —
and remains Update Manager's job.

## Measured

Against a live entitlement, 12.1 / LNXAMD64:

| | |
|---|---|
| catalogue | 394 products, 1184 artifacts, 7.9 GB in full |
| a B2B selection (IS, TN, EDI, AS2, MQ, JDBC, MWS, DCC, SPM) | 20 seeds → 84 products after closure, 177 artifacts, **1.13 GB** |
| installed | 1.7 GB on disk, every artifact sha256-verified |
| `tncore.jar` from that install | byte-identical to the same file installed by the Java installer |
| fixes offered for that installation | 6, 0.17 GB |
