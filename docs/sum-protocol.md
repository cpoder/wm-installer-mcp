# Update Manager's unattended interface

Reverse-engineered from the `com.webmethods.fixinstall.*` OSGi bundles of
Update Manager 12.0.0.0008, decompiled with CFR, and checked against a live
installation.

## Scripts supply the values; the terminal still supplies the page turns

Update Manager is commonly automated by allocating a pseudo-terminal and matching
each prompt with a regular expression, then writing the answer. That works until
a prompt is reworded, and it encodes the whole wizard flow in the driver.

Most of it is avoidable. `AbstractFixApplication` accepts `-readScript <file>`,
and `ScriptingSession` reads that file as a Java `.properties` list whose keys are
the names of the wizard's own `UserInput` fields. Every value the wizard asks for
has a key, so no prompt has to be recognised.

What a script does **not** remove is the terminal. Measured on 12.0.0.0008 with
`action=View installed fixes` and `installDir` both supplied:

* stdin closed — the wizard reaches `Product directory (full path):
  [/home/cpo/wm12] ?`, having taken the value from the script, then hits EOF at
  the navigation prompt and aborts: `Error Received: Terminating IBM webMethods
  Update Manager exit code:-1`, exit 255.
* 40 newlines on a plain pipe — identical failure. Piped answers are ignored.
* 40 newlines through `script -qec` — also fails. The newlines are echoed at the
  top of the transcript: they were consumed before the page existed, leaving
  stdin at EOF by the time the prompt was drawn.

So the answers must arrive **on a terminal, and after the prompt is on screen**.
The remaining driver is small, though, precisely because the values come from the
script: there is nothing to match, only pages to advance. `runner::run_console`
gives the child a pty via `script(1)`, watches the transcript, and sends one empty
line each time the output has been quiet — accepting the displayed default, which
at the navigation prompt is `N` for Next.

## Command line

`<sum_home>/bin/UpdateManagerCMD.sh` forwards to `SUMlauncher.jar`, which passes
the arguments through to the Equinox application. From `AppArgs`:

```
-readScript -empowerUser -empowerPass
-installFromImage -installFromCache -createImage -imagePlatform
-viewInstalledFixes -viewAvailableFixes -viewImageFixes -createInventory
-installSP -spKey -server -propertiesFile -useSSL
-clearCache -clearProfileMetadata -selfUpdate -skipJVMCheck -overInstall
-fixBackupCleanup -forceOSGiUninstall -disableProductsStartup
-verifyFileSignature -showAll -showEOM -debug
-proxyHost -proxyPort -proxyUsername -proxyPassword -proxyProtocol -nonProxyHosts
```

These flags are not all standalone actions. Verified against 12.0.0.0008,
`-viewInstalledFixes` on its own still launches the interactive wizard: with
stdin closed it walks the main menu on defaults and answers nothing, exiting 0
having listed nothing. The reliable path for *every* action is `-readScript` with
the corresponding `action=` value — which is what this server does, generating a
scratch script even for a read-only listing.

## The script

```properties
action=Install fixes from Empower
installDir=/opt/webmethods
selectedFixes=IBM webMethods Adapter 6.5 for MQ Fix 53
imageFile=/images/fixes.zip
imagePlatform=LNXAMD64
empowerUser=someone@example.com
```

Keys, from the `UserInput` registrations: `action`, `installDir`,
`selectedFixes`, `imageFile`, `imagePlatform`, `createEmpowerImage`,
`empowerUser`, `empowerPwd`, `installSP`, `diagnoserKey`, `scriptConfirm`,
`updateConfirm`, `saveCredentials`, `useSSL`, `backupDeletePeriod`,
`periodType`, and the `proxy*` family.

`action` holds the wizard's **display label**, not an identifier. The literals
come from `App_Actionname_*` in the core bundle's messages:

| Meaning | Value |
|---|---|
| Install from IBM | `Install fixes from Empower` |
| Install from image | `Install fixes from image` |
| Install from cache | `Install fixes from cache` |
| Build or extend an image | `Create or add fixes to fix image` |
| List installed fixes | `View installed fixes` |
| List available fixes | `View available fixes` |
| Write the inventory | `Create inventory` |
| Uninstall | `Uninstall fixes` |
| Roll back | `Revert` |
| Delete backups | `fixBackupCleanup` |

### Batches

`batch=true` turns the file into several sessions. Every key is prefixed with a
digit and a dot; `ScriptingSession` reads the prefix with
`Integer.parseInt(name.substring(0, 1))`, so the prefix is a **single digit** and
a batch cannot exceed nine steps.

```properties
batch=true
1.action=View installed fixes
1.installDir=/opt/webmethods
2.action=Create inventory
2.installDir=/opt/webmethods
```

### Passwords in scripts

`empowerPwd` must be encrypted. A plaintext value fails with *"The value of
'empowerPwd' password is not encrypted or in plain text"*. Passing
`-empowerUser` / `-empowerPass` on the command line avoids the question, which is
what this server does — and it references the key by environment-variable name so
the value never reaches the job's wrapper script on disk.

## Results

`<sum_home>/bin/result.json` holds the outcome of the last run, one section per
component:

```json
{"Launcher":{"exitCode":"175","detailedMessage":"<base64>","exception":""},
 "Client": {"exitCode":"25", "detailedMessage":"<base64>","exception":"<base64>"}}
```

`detailedMessage` and `exception` are base64, so reading the file directly tells
you nothing. Decoded, the exception is the full Java stack trace — for an
authentication failure, `SDAuthClient.getTokens` → `RetrieveTokenException`.

`<sum_home>/bin/exit_code.txt` carries the launcher's code. 16 means a self-update
is in progress and the launcher expects to be re-run; 170 and 0 are success.

## Locks

A previous run leaves `<sum_home>/bin/.lock` and
`<sum_home>/UpdateManager/SumAlreadyRunning.lock`. While either exists the next
run exits **211 and prints nothing**. Clearing them, with no Update Manager
process running, is the fix.

## Images

An image built with no fixes selected is not empty — it is a launcher-only image
of about 120 MB. Update Manager warns *"By not selecting any fix ... will create
only launcher image"* and proceeds, so the mistake is only visible later.

## Layout

```
<sum_home>/bin/UpdateManagerCMD.sh   launcher
<sum_home>/bin/result.json           last run, base64-encoded fields
<sum_home>/bin/config.properties     component versions
<sum_home>/UpdateManager/conf/       sum.cnf, proxy.cnf, privacy.cnf
<sum_home>/UpdateManager/profile/    Equinox profile, com.webmethods.fixinstall.* bundles
<sum_home>/UpdateManager/repository/ downloaded fixes
```
