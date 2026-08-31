//! Failure signatures the installer and Update Manager produce, and what to do.
//!
//! Both products report late and thinly: an hour of downloading ends in
//! `installer is exiting with code: 30`, or Update Manager exits 211 in silence.
//! Each entry below pairs a signature — an exit code, a log fragment, or both —
//! with the cause and the remedy, so a log can be turned into an action without
//! a second run.

use serde::Serialize;

/// Which tool produced the failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    /// The webMethods installer.
    Installer,
    /// Update Manager.
    UpdateManager,
    /// Either.
    Both,
}

/// One known failure.
#[derive(Debug, Clone, Serialize)]
pub struct Signature {
    /// Stable identifier.
    pub id: &'static str,
    /// Which tool this applies to.
    pub tool: Tool,
    /// Exit code, when the failure has a distinctive one.
    pub exit_code: Option<i32>,
    /// Case-insensitive fragments; any one matching is enough.
    pub patterns: &'static [&'static str],
    /// What is actually wrong.
    pub cause: &'static str,
    /// What to do about it.
    pub remedy: &'static str,
}

/// A matched signature, with why it matched.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnosis {
    /// The signature.
    pub signature: Signature,
    /// Which evidence matched: the exit code, a pattern, or both.
    pub matched_on: Vec<String>,
}

/// Every known signature.
pub const SIGNATURES: &[Signature] = &[
    Signature {
        id: "installer-missing-admin-password",
        tool: Tool::Installer,
        exit_code: Some(30),
        patterns: &["requires you to supply a default product administrator password"],
        cause: "The script has no adminPassword. Since 12.1 the installer refuses to \
                proceed without a default product administrator password.",
        remedy: "Add adminPassword=<password> to the script. Keep it out of the file with a \
                 $NAME$ placeholder and export the variable before the run. It must be at \
                 least 8 characters, mix letters, digits and specials, and avoid three \
                 identical consecutive characters — those rules come from the product \
                 password scripts, not the installer, so a weaker one fails later, not sooner.",
    },
    Signature {
        id: "installer-invalid-script",
        tool: Tool::Installer,
        exit_code: None,
        patterns: &[
            "specifies neither an installation image",
            "missing Username and/or Password and/or ServerURL",
        ],
        cause: "isScriptValid rejected the script: it has neither ImageFile nor the complete \
                Username + Password + ServerURL triple.",
        remedy: "For an online install set all three of ServerURL, Username and Password. For \
                 an install from an image put ImageFile in the script itself — passing \
                 -readImage on the command line does not satisfy the validator.",
    },
    Signature {
        id: "installer-incomplete-image",
        tool: Tool::Installer,
        exit_code: None,
        patterns: &["products they require do not exist in the image"],
        cause: "The image was built from a product list that was not closed over its \
                prerequisites. -writeImage embeds exactly what InstallProducts names.",
        remedy: "Resolve the prerequisite closure before building the image and rebuild. \
                 Note that License Agreement, Java Package and CustomInstall are required \
                 but declared by nothing, so a pure closure still misses them.",
    },
    Signature {
        id: "installer-missing-base-products",
        tool: Tool::Installer,
        exit_code: None,
        patterns: &["must exist in the installation image or the target directory"],
        cause: "The image lacks Infrastructure > License Agreement or Infrastructure > \
                Java Package.",
        remedy: "Add the license and sjp components to the selection and rebuild the image.",
    },
    Signature {
        id: "jvm-jit-crash",
        tool: Tool::Both,
        exit_code: None,
        patterns: &[
            "Fatal Crash in the JIT",
            "old api and new api did not match",
            "getSysPropBeforePropertiesInitialized",
        ],
        cause: "The bundled OpenJ9 aborts inside its JIT before any application code runs. \
                Its two CPU-detection APIs disagree, which happens when a hypervisor \
                presents an inconsistent CPUID — a processor sub-type predating features \
                the same javacore reports as present.",
        remedy: "export TR_DisableCPUDetectionTest=1 before the run; it disables only that \
                 consistency check. If it persists, try SAG_JAVA_OPTIONS=-Xshareclasses:none, \
                 then add -Xint. The real fix is on the hypervisor: expose the host CPU to \
                 the guest. Apply the same variable to the installed servers through \
                 custom_wrapper.conf, which fixes do not overwrite.",
    },
    Signature {
        id: "installer-empty-log",
        tool: Tool::Installer,
        exit_code: Some(255),
        patterns: &["Installation failed (255)"],
        cause: "-debug is deprecated and equivalent to -debugLvl <n> -debugErr, which sends \
                the diagnostics to stderr. A pipeline capturing only stdout therefore keeps \
                the failure line and none of the detail.",
        remedy: "Re-run with -debugLvl verbose -debugFile <path> -maxLogSize 20M and read \
                 that file, or redirect stderr as well.",
    },
    Signature {
        id: "sum-stale-lock",
        tool: Tool::UpdateManager,
        exit_code: Some(211),
        patterns: &["SumAlreadyRunning"],
        cause: "A previous Update Manager run left a lock file behind; the new run exits \
                without explanation.",
        remedy: "Delete <sum_home>/bin/.lock and <sum_home>/UpdateManager/SumAlreadyRunning.lock \
                 once no Update Manager process is running.",
    },
    Signature {
        id: "sum-plaintext-password",
        tool: Tool::UpdateManager,
        exit_code: None,
        patterns: &["password is not encrypted or in plain text"],
        cause: "A password in a script must be encrypted with the product's own utility; \
                Update Manager will not accept a plaintext value.",
        remedy: "Pass -empowerUser and -empowerPass on the command line instead of putting \
                 empowerPwd in the script, or encrypt the value first.",
    },
    Signature {
        id: "sum-launcher-only-image",
        tool: Tool::UpdateManager,
        exit_code: None,
        patterns: &["will create only launcher image"],
        cause: "The image action ran with no fixes selected, so the resulting image contains \
                the launcher and nothing else.",
        remedy: "Select the fixes explicitly, or the 'All fixes' entry, before creating \
                 the image.",
    },
    Signature {
        id: "sum-auth-failure",
        tool: Tool::UpdateManager,
        exit_code: Some(25),
        patterns: &["RetrieveTokenException", "SUMServiceException"],
        cause: "Update Manager could not obtain a token from IBM: wrong account, expired \
                entitlement key, or no route to the update service.",
        remedy: "Check the account and key, then the proxy settings in \
                 <sum_home>/UpdateManager/conf/proxy.cnf. bin/result.json holds the decoded \
                 stack trace for the last attempt.",
    },
];

/// Match `text` and an optional exit code against the known signatures.
///
/// Returns every match, most specific first: signatures matched on both the exit
/// code and a pattern come before those matched on one alone.
pub fn diagnose(text: &str, exit_code: Option<i32>, tool: Option<Tool>) -> Vec<Diagnosis> {
    let haystack = text.to_lowercase();
    let mut found: Vec<Diagnosis> = SIGNATURES
        .iter()
        .filter(|s| match tool {
            Some(t) => s.tool == t || s.tool == Tool::Both || t == Tool::Both,
            None => true,
        })
        .filter_map(|s| {
            let mut matched = Vec::new();
            if let (Some(want), Some(got)) = (s.exit_code, exit_code) {
                if want == got {
                    matched.push(format!("exit code {got}"));
                }
            }
            for pattern in s.patterns {
                if haystack.contains(&pattern.to_lowercase()) {
                    matched.push(format!("log contains {pattern:?}"));
                }
            }
            (!matched.is_empty()).then(|| Diagnosis {
                signature: s.clone(),
                matched_on: matched,
            })
        })
        .collect();
    found.sort_by_key(|d| std::cmp::Reverse(d.matched_on.len()));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_missing_admin_password() {
        let found = diagnose(
            "installer is exiting with code: 30",
            Some(30),
            Some(Tool::Installer),
        );
        assert_eq!(found[0].signature.id, "installer-missing-admin-password");
    }

    #[test]
    fn a_jit_crash_is_reported_for_either_tool() {
        let log = "JIT: Fatal Crash in the JIT while compiling <unknown>";
        assert_eq!(
            diagnose(log, None, Some(Tool::Installer))[0].signature.id,
            "jvm-jit-crash"
        );
        assert_eq!(
            diagnose(log, None, Some(Tool::UpdateManager))[0]
                .signature
                .id,
            "jvm-jit-crash"
        );
    }

    #[test]
    fn ranks_a_double_match_first() {
        let log = "SumAlreadyRunning.lock present";
        let found = diagnose(log, Some(211), Some(Tool::UpdateManager));
        assert_eq!(found[0].signature.id, "sum-stale-lock");
        assert_eq!(found[0].matched_on.len(), 2);
    }

    #[test]
    fn an_unremarkable_log_matches_nothing() {
        assert!(diagnose("everything went fine", Some(0), None).is_empty());
    }

    #[test]
    fn filters_by_tool() {
        let log = "products they require do not exist in the image";
        assert!(diagnose(log, None, Some(Tool::UpdateManager)).is_empty());
        assert_eq!(diagnose(log, None, Some(Tool::Installer)).len(), 1);
    }
}
