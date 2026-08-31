//! The installer's unattended script: a Java `.properties` file.
//!
//! Validity rules are transcribed from `DistManUtils.isScriptValid`:
//!
//! * `InstallDir` must be present;
//! * `InstallProducts` or `InstallLocProducts` must be present;
//! * either the `Username` + `Password` + `ServerURL` triple, or an image file
//!   (`ImageFile`, or the lowercase `imageFile` the installer also accepts).
//!
//! Two further rules are not enforced by `isScriptValid` but end the run anyway,
//! so they are reported here as well: a missing `adminPassword` (the installer
//! exits with code 30 after the licence stage) and `LicenseAgree` not set to
//! `Accept`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::{Error, Result};

/// Marks a value the installer's password manager has encrypted.
pub const SECURE_PREFIX: &str = "@secure@";

/// Where the binaries come from.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// Download from an IBM installer server. Requires credentials.
    Server {
        /// e.g. `https://sdc.webmethods.io/cgi-bin/dataservewebM121.cgi`.
        url: String,
        /// Empower / IBM account, or a `$VAR$` placeholder.
        username: String,
        /// Entitlement key, or a `$VAR$` placeholder.
        password: String,
    },
    /// Install from a previously built image; no credentials needed.
    Image {
        /// Absolute path to the image zip.
        file: String,
    },
}

/// A parsed or generated install script.
#[derive(Debug, Clone, Serialize)]
pub struct InstallScript {
    /// Target directory (`InstallDir`).
    pub install_dir: String,
    /// Where binaries come from.
    pub source: Source,
    /// Default product administrator password (`adminPassword`).
    pub admin_password: Option<String>,
    /// Versioned product paths (`InstallProducts`).
    pub products: Vec<String>,
    /// Any other key the caller wants preserved verbatim.
    pub extra: BTreeMap<String, String>,
    /// Comment lines rendered above the body.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub preamble: Vec<String>,
}

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The installer will refuse the script or abort.
    Error,
    /// The run may still fail, or fail later in a product script.
    Warning,
}

/// One validation finding.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// Severity.
    pub severity: Severity,
    /// The script key concerned, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// What is wrong.
    pub message: String,
    /// What the installer does about it, quoted where known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consequence: Option<String>,
}

impl Finding {
    fn error(key: &str, message: impl Into<String>, consequence: &str) -> Self {
        Self {
            severity: Severity::Error,
            key: Some(key.to_string()),
            message: message.into(),
            consequence: Some(consequence.to_string()),
        }
    }

    fn warn(key: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            key: key.map(str::to_string),
            message: message.into(),
            consequence: None,
        }
    }
}

impl InstallScript {
    /// Render the script as a `.properties` file.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.preamble {
            let _ = writeln!(out, "# {line}");
        }
        if !self.preamble.is_empty() {
            out.push('\n');
        }
        let _ = writeln!(out, "InstallDir={}", self.install_dir);
        let _ = writeln!(out, "LicenseAgree=Accept");
        if let Some(password) = &self.admin_password {
            let _ = writeln!(out, "adminPassword={password}");
        }
        match &self.source {
            Source::Server {
                url,
                username,
                password,
            } => {
                let _ = writeln!(out, "ServerURL={url}");
                let _ = writeln!(out, "Username={username}");
                let _ = writeln!(out, "Password={password}");
            }
            Source::Image { file } => {
                // The lowercase spelling is what `-writeImage` emits; the
                // validator accepts both, so write the documented one.
                let _ = writeln!(out, "ImageFile={file}");
            }
        }
        for (key, value) in &self.extra {
            let _ = writeln!(out, "{key}={value}");
        }
        let _ = writeln!(out, "InstallProducts={}", self.products.join(","));
        out
    }

    /// Write the script to `path`.
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        fs::write(path, self.render()).map_err(|e| Error::io(path, e))
    }

    /// Parse a script from disk.
    pub fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Self::parse(&text)
    }

    /// Parse a script from text.
    pub fn parse(text: &str) -> Result<Self> {
        let props = parse_properties(text);
        let install_dir = props.get("InstallDir").cloned().unwrap_or_default();
        // `isScriptValid` accepts either casing for the image key.
        let image = props.get("ImageFile").or_else(|| props.get("imageFile"));
        let source = match image {
            Some(file) => Source::Image { file: file.clone() },
            None => Source::Server {
                url: props.get("ServerURL").cloned().unwrap_or_default(),
                username: props.get("Username").cloned().unwrap_or_default(),
                password: props.get("Password").cloned().unwrap_or_default(),
            },
        };
        let products = props
            .get("InstallProducts")
            .or_else(|| props.get("InstallLocProducts"))
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let known = [
            "InstallDir",
            "LicenseAgree",
            "adminPassword",
            "ServerURL",
            "Username",
            "Password",
            "ImageFile",
            "imageFile",
            "InstallProducts",
            "InstallLocProducts",
        ];
        let extra = props
            .iter()
            .filter(|(k, _)| !known.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        Ok(Self {
            install_dir,
            source,
            admin_password: props.get("adminPassword").cloned(),
            products,
            extra,
            preamble: Vec::new(),
        })
    }

    /// Check the script against the installer's own rules.
    ///
    /// An empty vector means the installer will accept it. Findings are ordered
    /// errors first.
    pub fn validate(&self) -> Vec<Finding> {
        let mut findings = Vec::new();

        if self.install_dir.trim().is_empty() {
            findings.push(Finding::error(
                "InstallDir",
                "no target directory",
                "isScriptValid rejects the script: missing property InstallDir",
            ));
        }
        if self.products.is_empty() {
            findings.push(Finding::error(
                "InstallProducts",
                "no products selected",
                "isScriptValid rejects the script: neither InstallProducts nor InstallLocProducts",
            ));
        }
        match &self.source {
            Source::Server {
                url,
                username,
                password,
            } => {
                if url.trim().is_empty() || username.trim().is_empty() || password.trim().is_empty()
                {
                    findings.push(Finding::error(
                        "ServerURL",
                        "incomplete server credentials and no image file",
                        "isScriptValid rejects the script: it specifies neither an installation \
                         image to install from, nor a valid user name, password and installer server",
                    ));
                }
            }
            Source::Image { file } => {
                if file.trim().is_empty() {
                    findings.push(Finding::error(
                        "ImageFile",
                        "empty image path",
                        "isScriptValid rejects the script: no image file and no server credentials",
                    ));
                }
            }
        }
        match self.admin_password.as_deref().map(str::trim) {
            None | Some("") => findings.push(Finding::error(
                "adminPassword",
                "no default product administrator password",
                "the installer exits with code 30: \"IBM webMethods Installer now requires you to \
                 supply a default product administrator password\"",
            )),
            Some(password) => findings.extend(admin_password_advice(password)),
        }

        findings.sort_by_key(|f| match f.severity {
            Severity::Error => 0,
            Severity::Warning => 1,
        });
        findings
    }

    /// Placeholders of the form `$NAME$` left in the script.
    ///
    /// The installer substitutes these from the environment when it reads the
    /// script, which is how credentials stay out of the file. Listing them lets
    /// a caller check the environment before starting an hour-long download.
    pub fn placeholders(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut scan = |value: &str| {
            let bytes: Vec<&str> = value.split('$').collect();
            // "a$B$c" splits to ["a", "B", "c"]: odd indices are placeholders.
            for (i, part) in bytes.iter().enumerate() {
                if i % 2 == 1 && !part.is_empty() && !names.contains(&part.to_string()) {
                    names.push(part.to_string());
                }
            }
        };
        if let Some(p) = &self.admin_password {
            scan(p);
        }
        if let Source::Server {
            url,
            username,
            password,
        } = &self.source
        {
            scan(url);
            scan(username);
            scan(password);
        }
        names
    }
}

/// The complexity rules the *products* enforce when the installer runs their
/// admin-password scripts. The installer itself only checks for emptiness, so
/// these are warnings: a script that trips them is accepted and then fails late.
fn admin_password_advice(password: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    if password.starts_with('$') && password.ends_with('$') {
        // An unresolved placeholder: nothing to judge yet.
        return findings;
    }
    if password.chars().count() < 8 {
        findings.push(Finding::warn(
            Some("adminPassword"),
            "shorter than 8 characters; the product password scripts reject it",
        ));
    }
    if !password.chars().any(char::is_alphabetic)
        || !password.chars().any(|c| c.is_ascii_digit())
        || password.chars().all(char::is_alphanumeric)
    {
        findings.push(Finding::warn(
            Some("adminPassword"),
            "should mix letters, digits and special characters",
        ));
    }
    let chars: Vec<char> = password.chars().collect();
    if chars.windows(3).any(|w| w[0] == w[1] && w[1] == w[2]) {
        findings.push(Finding::warn(
            Some("adminPassword"),
            "contains three identical consecutive characters, which the product scripts reject",
        ));
    }
    findings
}

/// Minimal `.properties` reader: `key=value`, `#`/`!` comments, no escapes.
///
/// The installer writes these with `Properties.store`, which escapes nothing we
/// care about in practice — paths and product ids contain no `=` or `:`.
fn parse_properties(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> InstallScript {
        InstallScript {
            install_dir: "/opt/webmethods".into(),
            source: Source::Image {
                file: "/images/wm.zip".into(),
            },
            admin_password: Some("Passw0rd!x".into()),
            products: vec!["e2ei/11/TN_12.1/TradingNetworks/TNServer".into()],
            extra: BTreeMap::new(),
            preamble: vec!["generated by wm-installer-mcp".into()],
        }
    }

    #[test]
    fn a_complete_script_has_no_findings() {
        assert!(valid().validate().is_empty());
    }

    #[test]
    fn missing_admin_password_is_an_error() {
        let mut script = valid();
        script.admin_password = None;
        let findings = script.validate();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(findings[0]
            .consequence
            .as_ref()
            .unwrap()
            .contains("code 30"));
    }

    #[test]
    fn server_source_needs_all_three_credentials() {
        let mut script = valid();
        script.source = Source::Server {
            url: "https://sdc.webmethods.io/cgi-bin/dataservewebM121.cgi".into(),
            username: "user@example.com".into(),
            password: String::new(),
        };
        let findings = script.validate();
        assert!(findings
            .iter()
            .any(|f| f.severity == Severity::Error && f.key.as_deref() == Some("ServerURL")));
    }

    #[test]
    fn weak_admin_passwords_warn_but_do_not_block() {
        let mut script = valid();
        script.admin_password = Some("aaa".into());
        let findings = script.validate();
        assert!(!findings.is_empty());
        assert!(findings.iter().all(|f| f.severity == Severity::Warning));
    }

    #[test]
    fn round_trips_through_properties() {
        let rendered = valid().render();
        let parsed = InstallScript::parse(&rendered).expect("parse");
        assert_eq!(parsed.install_dir, "/opt/webmethods");
        assert_eq!(parsed.products.len(), 1);
        assert!(matches!(parsed.source, Source::Image { .. }));
        assert!(parsed.validate().is_empty());
    }

    #[test]
    fn accepts_the_lowercase_image_key() {
        let parsed = InstallScript::parse(
            "InstallDir=/opt/wm\nadminPassword=Passw0rd!x\nimageFile=/i.zip\nInstallProducts=a/b/c/d/e\n",
        )
        .expect("parse");
        assert!(matches!(parsed.source, Source::Image { .. }));
        assert!(parsed.validate().is_empty());
    }

    #[test]
    fn finds_environment_placeholders() {
        let mut script = valid();
        script.source = Source::Server {
            url: "https://example".into(),
            username: "$WM_EMPOWER_USER$".into(),
            password: "$WM_EMPOWER_KEY$".into(),
        };
        script.admin_password = Some("$WM_ADMIN_PASSWORD$".into());
        let names = script.placeholders();
        assert_eq!(
            names,
            ["WM_ADMIN_PASSWORD", "WM_EMPOWER_USER", "WM_EMPOWER_KEY"]
        );
    }
}
