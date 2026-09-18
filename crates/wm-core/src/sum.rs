//! Update Manager: unattended scripts, command line, locks and results.
//!
//! Update Manager refuses to run without a TTY when driven interactively, which
//! is why it is so often automated by screen-scraping a pseudo-terminal. It does
//! not have to be: `AbstractFixApplication` accepts `-readScript <file>`, where
//! the file is a Java `.properties` list whose keys are the names of the wizard's
//! own `UserInput` fields. Everything the console wizard asks for has a key.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::Serialize;
use serde_json::Value;

use crate::{Error, Result};

/// What Update Manager should do.
///
/// The script stores the action's *display* string, because the wizard matches
/// the value against the labels of its action combo. The literals below are the
/// `App_Actionname_*` entries of `com.webmethods.fixinstall.core` messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Download and install fixes from IBM.
    InstallFromEmpower,
    /// Install fixes from a previously built image.
    InstallFromImage,
    /// Install fixes already downloaded into the local cache.
    InstallFromCache,
    /// Build or extend a fix image.
    CreateImage,
    /// List the fixes installed in a product directory.
    ViewInstalled,
    /// List the fixes IBM offers for a product directory.
    ViewAvailable,
    /// Write the product/fix inventory.
    CreateInventory,
    /// Remove installed fixes.
    Uninstall,
    /// Roll back to the previous state.
    Revert,
    /// Delete fix backups.
    DeleteBackup,
}

impl Action {
    /// The literal written to `action=` in a script.
    pub fn label(self) -> &'static str {
        match self {
            Self::InstallFromEmpower => "Install fixes from Empower",
            Self::InstallFromImage => "Install fixes from image",
            Self::InstallFromCache => "Install fixes from cache",
            Self::CreateImage => "Create or add fixes to fix image",
            Self::ViewInstalled => "View installed fixes",
            Self::ViewAvailable => "View available fixes",
            Self::CreateInventory => "Create inventory",
            Self::Uninstall => "Uninstall fixes",
            Self::Revert => "Revert",
            Self::DeleteBackup => "fixBackupCleanup",
        }
    }

    /// Parse an action from its snake_case name.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "install_from_empower" => Self::InstallFromEmpower,
            "install_from_image" => Self::InstallFromImage,
            "install_from_cache" => Self::InstallFromCache,
            "create_image" => Self::CreateImage,
            "view_installed" => Self::ViewInstalled,
            "view_available" => Self::ViewAvailable,
            "create_inventory" => Self::CreateInventory,
            "uninstall" => Self::Uninstall,
            "revert" => Self::Revert,
            "delete_backup" => Self::DeleteBackup,
            _ => return None,
        })
    }

    /// Whether this action contacts IBM and therefore needs credentials.
    pub fn needs_credentials(self) -> bool {
        matches!(
            self,
            Self::InstallFromEmpower | Self::ViewAvailable | Self::CreateImage
        )
    }
}

/// One unattended Update Manager step.
#[derive(Debug, Clone, Serialize)]
pub struct FixStep {
    /// What to do.
    pub action: Action,
    /// The webMethods installation to act on (`installDir`).
    pub install_dir: String,
    /// Fixes to select, as Update Manager displays them.
    ///
    /// Empty does **not** mean "all applicable". For the install actions
    /// Update Manager answers *"Empty selectedFixes found in the silent script
    /// file. Performing only self-update"* and installs nothing; for
    /// [`Action::CreateImage`] it warns *"By not selecting any fix ... will
    /// create only launcher image"* and produces an image with no fixes in it.
    /// Both are caught by [`FixStep::validate`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub selected_fixes: Vec<String>,
    /// Image path for the image-based actions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_file: Option<String>,
    /// Image platform, e.g. `LNXAMD64`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_platform: Option<String>,
    /// IBM account (`empowerUser`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub empower_user: Option<String>,
    /// Entitlement key (`empowerPwd`).
    ///
    /// Update Manager expects this *encrypted*: reading a plaintext value fails
    /// with "The value of 'empowerPwd' password is not encrypted or in plain
    /// text". Use the product's password-encryption utility, or pass credentials
    /// on the command line instead — see [`SumCommand`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub empower_password_encrypted: Option<String>,
    /// Any other `UserInput` key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, String>,
}

impl FixStep {
    /// The key/value pairs this step contributes to a script.
    fn entries(&self) -> Vec<(String, String)> {
        let mut entries = vec![
            ("action".to_string(), self.action.label().to_string()),
            ("installDir".to_string(), self.install_dir.clone()),
        ];
        if !self.selected_fixes.is_empty() {
            entries.push(("selectedFixes".into(), self.selected_fixes.join(",")));
        }
        if let Some(v) = &self.image_file {
            entries.push(("imageFile".into(), v.clone()));
        }
        if let Some(v) = &self.image_platform {
            entries.push(("imagePlatform".into(), v.clone()));
        }
        if let Some(v) = &self.empower_user {
            entries.push(("empowerUser".into(), v.clone()));
        }
        if let Some(v) = &self.empower_password_encrypted {
            entries.push(("empowerPwd".into(), v.clone()));
        }
        for (k, v) in &self.extra {
            entries.push((k.clone(), v.clone()));
        }
        entries
    }

    /// Problems that would make Update Manager stop or do nothing useful.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.install_dir.trim().is_empty() {
            problems.push("installDir is empty".to_string());
        }
        let image_actions = matches!(self.action, Action::InstallFromImage | Action::CreateImage);
        if image_actions && self.image_file.is_none() {
            problems.push(format!("{:?} needs imageFile", self.action));
        }
        if self.action == Action::CreateImage && self.image_platform.is_none() {
            problems.push("create_image needs imagePlatform, e.g. LNXAMD64".to_string());
        }
        if self.action == Action::CreateImage && self.selected_fixes.is_empty() {
            problems.push(
                "no fixes selected: Update Manager would build a launcher-only image".to_string(),
            );
        }
        let needs_fixes = matches!(
            self.action,
            Action::InstallFromEmpower
                | Action::InstallFromImage
                | Action::InstallFromCache
                | Action::Uninstall
        );
        if needs_fixes && self.selected_fixes.is_empty() {
            problems.push(format!(
                "no fixes selected: with an empty selectedFixes Update Manager performs only \
                 its self-update and does not {} anything. Name them as fixes_available lists \
                 them",
                if self.action == Action::Uninstall {
                    "uninstall"
                } else {
                    "install"
                }
            ));
        }
        if self.action.needs_credentials()
            && self.empower_user.is_none()
            && !self.extra.contains_key("empowerUser")
        {
            problems.push(
                "action contacts IBM but no empowerUser is set (pass credentials on the \
                 command line if you do not want them in the script)"
                    .to_string(),
            );
        }
        problems
    }
}

/// A complete script: one step, or several run in sequence.
#[derive(Debug, Clone, Serialize)]
pub struct FixScript {
    /// Steps, in order.
    pub steps: Vec<FixStep>,
}

impl FixScript {
    /// A single-step script.
    pub fn single(step: FixStep) -> Self {
        Self { steps: vec![step] }
    }

    /// Render as a `.properties` file.
    ///
    /// More than one step switches on `batch=true` and prefixes every key with
    /// `<n>.`, which is how `ScriptingSession` splits a batch into sub-sessions.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.steps.len() > 1 {
            out.push_str("batch=true\n");
            for (index, step) in self.steps.iter().enumerate() {
                // ScriptingSession reads the prefix as a single digit.
                let n = index + 1;
                for (key, value) in step.entries() {
                    let _ = writeln!(out, "{n}.{key}={value}");
                }
            }
        } else if let Some(step) = self.steps.first() {
            for (key, value) in step.entries() {
                let _ = writeln!(out, "{key}={value}");
            }
        }
        out
    }

    /// Write the script to `path`.
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        fs::write(path, self.render()).map_err(|e| Error::io(path, e))
    }

    /// Validate every step, plus the batch-size limit.
    pub fn validate(&self) -> Vec<String> {
        let mut problems: Vec<String> = Vec::new();
        if self.steps.is_empty() {
            problems.push("script has no steps".to_string());
        }
        if self.steps.len() > 9 {
            problems.push(
                "more than 9 steps: batch prefixes are a single digit, so later steps \
                 would collide"
                    .to_string(),
            );
        }
        for (index, step) in self.steps.iter().enumerate() {
            problems.extend(
                step.validate()
                    .into_iter()
                    .map(|p| format!("step {}: {p}", index + 1)),
            );
        }
        problems
    }
}

/// A resolved Update Manager invocation.
#[derive(Debug, Clone, Serialize)]
pub struct SumCommand {
    /// The program to run, `<sum_home>/bin/UpdateManagerCMD.sh`.
    pub program: PathBuf,
    /// Arguments, in order.
    pub args: Vec<String>,
}

impl SumCommand {
    /// Build a `-readScript` invocation.
    ///
    /// Credentials are passed as arguments rather than written into the script:
    /// the script wants them encrypted, the command line does not, and a
    /// short-lived argument is easier to keep out of a repository than a file.
    pub fn read_script(sum_home: &Path, script: &Path, credentials: Option<(&str, &str)>) -> Self {
        let mut args = vec!["-readScript".to_string(), script.display().to_string()];
        if let Some((user, password)) = credentials {
            args.push("-empowerUser".into());
            args.push(user.to_string());
            args.push("-empowerPass".into());
            args.push(password.to_string());
        }
        Self {
            program: sum_home.join("bin").join("UpdateManagerCMD.sh"),
            args,
        }
    }

    /// Build a read-only inventory invocation that needs no credentials.
    pub fn view_installed(sum_home: &Path, install_dir: &Path) -> Self {
        Self {
            program: sum_home.join("bin").join("UpdateManagerCMD.sh"),
            args: vec![
                "-viewInstalledFixes".to_string(),
                "-installDir".to_string(),
                install_dir.display().to_string(),
            ],
        }
    }
}

/// A lock file of Update Manager's.
///
/// Update Manager exits with 211 and says nothing when one of its own is
/// present, which is a common cause of "it worked yesterday". It takes a POSIX
/// lock on the file while it runs, so a lock file can be told apart from a
/// running Update Manager: [`StaleLock::held`] says whether a live process
/// still holds it, in which case deleting the file would let two runs write
/// the same installation.
#[derive(Debug, Clone, Serialize)]
pub struct StaleLock {
    /// Path of the lock file.
    pub path: PathBuf,
    /// Whether a live process holds a lock on it right now. False for a file
    /// left behind by a run that is over.
    pub held: bool,
}

/// Find the lock files a previous Update Manager run may have left: the two
/// under its own home, and the one it puts in the installation it works on.
pub fn stale_locks(sum_home: &Path, install_dir: Option<&Path>) -> Vec<StaleLock> {
    let mut candidates = vec![
        sum_home.join("bin").join(".lock"),
        sum_home
            .join("UpdateManager")
            .join("SumAlreadyRunning.lock"),
    ];
    if let Some(dir) = install_dir {
        candidates.push(dir.join("SumWorksOnThisDir.lock"));
    }
    candidates
        .into_iter()
        .filter(|p| p.exists())
        .map(|path| StaleLock {
            held: lock_held(&path),
            path,
        })
        .collect()
}

/// Whether another process holds a POSIX lock on `path`.
///
/// Java's `FileChannel.tryLock`, which Update Manager uses, is an `fcntl`
/// record lock on Linux; `F_GETLK` reports the conflicting lock without
/// taking one. A file this process cannot open is reported as not held.
#[cfg(unix)]
fn lock_held(path: &Path) -> bool {
    use std::os::unix::io::AsRawFd as _;
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return false;
    };
    let mut probe = libc::flock {
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    // SAFETY: F_GETLK reads the descriptor's lock state into `probe`, a
    // properly initialised struct that outlives the call.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut probe) };
    rc == 0 && probe.l_type != libc::F_UNLCK as libc::c_short
}

#[cfg(not(unix))]
fn lock_held(_path: &Path) -> bool {
    false
}

/// One section of `bin/result.json`.
#[derive(Debug, Clone, Serialize)]
pub struct ResultSection {
    /// Section name, `Launcher` or `Client`.
    pub name: String,
    /// Exit code reported for that section.
    pub exit_code: Option<i32>,
    /// Timestamp, verbatim.
    pub date: Option<String>,
    /// Decoded `detailedMessage`.
    pub message: Option<String>,
    /// Decoded `exception`, usually a Java stack trace.
    pub exception: Option<String>,
}

/// Read and decode `<sum_home>/bin/result.json`.
///
/// The interesting fields are base64-encoded, which is why tailing the file is
/// unhelpful. Decoding them turns the last run into something a caller can act
/// on without opening a log.
pub fn read_result(sum_home: &Path) -> Result<Vec<ResultSection>> {
    let path = sum_home.join("bin").join("result.json");
    if !path.is_file() {
        return Err(Error::NotFound {
            what: "Update Manager result",
            path,
        });
    }
    let text = fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| Error::Malformed(format!("{}: {e}", path.display())))?;
    let Some(object) = value.as_object() else {
        return Err(Error::Malformed(format!(
            "{} is not a JSON object",
            path.display()
        )));
    };
    Ok(object
        .iter()
        .map(|(name, section)| ResultSection {
            name: name.clone(),
            exit_code: section
                .get("exitCode")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok()),
            date: section
                .get("date")
                .and_then(Value::as_str)
                .map(str::to_string),
            message: decode_field(section, "detailedMessage"),
            exception: decode_field(section, "exception"),
        })
        .collect())
}

fn decode_field(section: &Value, key: &str) -> Option<String> {
    let raw = section.get(key)?.as_str()?;
    if raw.is_empty() {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(raw)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        // A field that is not base64 is still worth surfacing verbatim.
        .or_else(|| Some(raw.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(action: Action) -> FixStep {
        FixStep {
            action,
            install_dir: "/opt/webmethods".into(),
            selected_fixes: Vec::new(),
            image_file: None,
            image_platform: None,
            empower_user: None,
            empower_password_encrypted: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn single_step_scripts_are_unprefixed() {
        let rendered = FixScript::single(step(Action::ViewInstalled)).render();
        assert!(rendered.contains("action=View installed fixes\n"));
        assert!(rendered.contains("installDir=/opt/webmethods\n"));
        assert!(!rendered.contains("batch=true"));
    }

    #[test]
    fn multi_step_scripts_switch_to_batch() {
        let script = FixScript {
            steps: vec![step(Action::ViewInstalled), step(Action::CreateInventory)],
        };
        let rendered = script.render();
        assert!(rendered.starts_with("batch=true\n"));
        assert!(rendered.contains("1.action=View installed fixes\n"));
        assert!(rendered.contains("2.action=Create inventory\n"));
    }

    #[test]
    fn create_image_without_fixes_is_reported() {
        let mut s = step(Action::CreateImage);
        s.image_file = Some("/images/fixes.zip".into());
        s.image_platform = Some("LNXAMD64".into());
        s.empower_user = Some("user@example.com".into());
        let problems = s.validate();
        assert!(problems.iter().any(|p| p.contains("launcher-only")));
    }

    #[test]
    fn batches_longer_than_nine_are_rejected() {
        let script = FixScript {
            steps: (0..10).map(|_| step(Action::ViewInstalled)).collect(),
        };
        assert!(script.validate().iter().any(|p| p.contains("single digit")));
    }

    #[test]
    fn view_installed_needs_no_credentials() {
        assert!(!Action::ViewInstalled.needs_credentials());
        assert!(Action::InstallFromEmpower.needs_credentials());
        assert!(step(Action::ViewInstalled).validate().is_empty());
    }

    #[test]
    fn an_install_step_without_fixes_is_a_problem() {
        // Measured on 12.0.0.0008: an empty selectedFixes makes Update
        // Manager perform only its self-update, exit -1 from the client, and
        // install nothing. It is not "all applicable".
        let mut s = step(Action::InstallFromEmpower);
        s.empower_user = Some("user@example.com".into());
        assert!(s.validate().iter().any(|p| p.contains("self-update")));
        s.selected_fixes = vec!["IBM webMethods Adapter 6.5 for MQ Fix 53".into()];
        assert!(s.validate().is_empty(), "{:?}", s.validate());

        let u = step(Action::Uninstall);
        assert!(u
            .validate()
            .iter()
            .any(|p| p.contains("uninstall anything")));
    }

    #[test]
    fn a_lock_file_nobody_holds_is_stale_and_the_installation_lock_is_seen() {
        let base = std::env::temp_dir().join(format!("wm-sum-locks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sum = base.join("sum");
        let home = base.join("wm");
        std::fs::create_dir_all(sum.join("bin")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(sum.join("bin").join(".lock"), b"").unwrap();
        std::fs::write(home.join("SumWorksOnThisDir.lock"), b"").unwrap();

        let locks = stale_locks(&sum, Some(&home));
        assert_eq!(locks.len(), 2, "{locks:?}");
        assert!(locks.iter().all(|l| !l.held), "{locks:?}");
        assert!(locks
            .iter()
            .any(|l| l.path.ends_with("SumWorksOnThisDir.lock")));
        // Without the installation, only Update Manager's own are reported.
        assert_eq!(stale_locks(&sum, None).len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn decodes_base64_result_fields() {
        let json = serde_json::json!({ "detailedMessage": "aGVsbG8=", "exception": "" });
        assert_eq!(
            decode_field(&json, "detailedMessage").as_deref(),
            Some("hello")
        );
        assert_eq!(decode_field(&json, "exception"), None);
    }
}
