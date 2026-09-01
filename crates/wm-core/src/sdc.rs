//! Native client for the IBM webMethods Software Download Centre.
//!
//! This is the whole of what the Java installer does over the network, without
//! the Java installer. Three protocols are involved, and they do not overlap:
//!
//! 1. **A modern REST API** at `https://<host>/services/`. `POST auth` with the
//!    account and entitlement key returns an OAuth bearer token, valid an hour.
//!    Under that token, `sd-access-service/entitlements/suites` lists the
//!    releases the account may install and `sd-repository-service/v1/repositories/sandboxes`
//!    names their repositories. `sd-access-service/entitlements/products` returns
//!    the whole product tree for one release and platform — for 12.1 on Linux
//!    x64, 2 MB describing 394 products and 1380 downloadable artifacts, each
//!    with its size, md5 and sha256.
//!
//! 2. **"Protocol G"**, a handshake with the release's CGI. `POST <cgi>?G`
//!    authenticated with the account, body `locale=…&buildNo=…`, answers a
//!    single line `OK,a=<user>:<password>`. Those are short-lived credentials
//!    scoped to the download repository; the account's own credentials are not
//!    accepted there.
//!
//! 3. **Plain HTTP file access** to the repository, authenticated with the
//!    credentials from step 2. The path is derived from the product tree, not
//!    served by it: an artifact listed at
//!    `e2ei/11/<CODE>_<VERSION>/<GROUP>/<COMPONENT>/<COMPONENT>-<PLATFORM>-Any/<BM>`
//!    is fetched from `<host>/dataserve<release>/e2ei/11/<CODE>_<VERSION>/bms/<BM>.zip`.

use std::io::Read;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Serialize;
use sha2::Digest as _;

use crate::{Error, Result};

/// Default download-centre host.
pub const DEFAULT_HOST: &str = "sdc.webmethods.io";

/// Installer build number sent in the protocol-G handshake.
///
/// The server uses it to decide which client features to enable; the value from
/// the 12.1 installer is accepted for every release.
pub const DEFAULT_BUILD_NO: &str = "123";

/// How long a download-credential grant is reused before being renewed.
const GRANT_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// One release the account may install.
#[derive(Debug, Clone, Serialize)]
pub struct Release {
    /// Numeric id in the download centre.
    pub id: i64,
    /// Release number, e.g. `12.1`.
    pub release: String,
    /// Human name, e.g. `2026 May webMethods 12.1`.
    pub display_name: String,
    /// Release code, e.g. `2026_May`.
    pub code: String,
    /// CGI endpoints, e.g. `https://sdc.webmethods.io/cgi-bin/dataservewebM121.cgi`.
    pub urls: Vec<String>,
}

impl Release {
    /// The repository name embedded in the CGI URL, e.g. `dataservewebM121`.
    ///
    /// The installer derives it by stripping `cgi-bin/` and `.cgi`; it is the
    /// path segment the artifact repository is served under.
    pub fn repository(&self) -> Option<String> {
        let url = self.urls.first()?;
        let file = url.rsplit('/').next()?;
        Some(file.strip_suffix(".cgi").unwrap_or(file).to_string())
    }

    /// The sandbox name used by the entitlements API, e.g. `webM121`.
    pub fn sandbox(&self) -> Option<String> {
        Some(self.repository()?.strip_prefix("dataserve")?.to_string())
    }

    /// The CGI endpoint.
    pub fn cgi(&self) -> Option<&str> {
        self.urls.first().map(String::as_str)
    }
}

/// Short-lived credentials for the artifact repository.
#[derive(Clone)]
struct Grant {
    user: String,
    password: String,
    issued: Instant,
}

/// An authenticated session against the download centre.
pub struct Session {
    host: String,
    username: String,
    password: String,
    agent: ureq::Agent,
    access_token: String,
    grant: Option<Grant>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the token or the entitlement key.
        f.debug_struct("Session")
            .field("host", &self.host)
            .field("user", &self.username)
            .finish()
    }
}

impl Session {
    /// Authenticate against `host` with an IBM account and entitlement key.
    pub fn login(host: &str, username: &str, password: &str) -> Result<Self> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .build()
            .into();
        let body = serde_json::json!({ "username": username, "password": password });
        let response = agent
            .post(format!("https://{host}/services/auth"))
            .header("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|e| {
                Error::Exec(format!(
                    "authentication request to https://{host}/services/auth failed: {e}"
                ))
            })?;
        let payload: serde_json::Value = response.into_body().read_json().map_err(|e| {
            Error::Exec(format!(
                "authentication response from https://{host}/services/auth unreadable: {e}"
            ))
        })?;
        let access_token = payload
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                let reason = payload
                    .get("error_description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("no access_token in response");
                Error::Exec(format!("authentication rejected: {reason}"))
            })?
            .to_string();
        Ok(Self {
            host: host.to_string(),
            username: username.to_string(),
            password: password.to_string(),
            agent,
            access_token,
            grant: None,
        })
    }

    /// The releases this account is entitled to install.
    pub fn releases(&self) -> Result<Vec<Release>> {
        let url = format!(
            "https://{}/services/sd-access-service/entitlements/suites?installerVersion=12.1.0.0.{}",
            self.host, DEFAULT_BUILD_NO
        );
        let payload: serde_json::Value = self
            .agent
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .call()
            .map_err(|e| Error::Exec(format!("cannot list entitlements ({url}): {e}")))?
            .into_body()
            .read_json()
            .map_err(|e| {
                Error::Exec(format!("entitlements response from {url} unreadable: {e}"))
            })?;
        let data = payload
            .get("data")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| Error::Malformed("entitlements response has no data array".into()))?;
        Ok(data
            .iter()
            .map(|item| Release {
                id: item
                    .get("id")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or_default(),
                release: string_field(item, "release"),
                display_name: string_field(item, "displayName"),
                code: string_field(item, "code"),
                urls: item
                    .get("urls")
                    .and_then(serde_json::Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|u| u.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect())
    }

    /// Fetch the raw product tree for one sandbox and platform.
    ///
    /// The response is a flat `key=value` list in the same dialect as the
    /// `.prop` files an installation carries, and is a couple of megabytes.
    pub fn product_tree(&self, sandbox: &str, platform: &str) -> Result<String> {
        let url = format!(
            "https://{}/services/sd-access-service/entitlements/products?sandbox={sandbox}&platform={platform}",
            self.host
        );
        let mut body = self
            .agent
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("Accept", "application/octet-stream")
            .call()
            .map_err(|e| Error::Exec(format!("cannot fetch the product tree ({url}): {e}")))?
            .into_body()
            .into_reader();
        let mut text = String::new();
        body.read_to_string(&mut text)
            .map_err(|e| Error::Exec(format!("product tree from {url} unreadable: {e}")))?;
        if text.starts_with('{') {
            return Err(Error::Exec(format!(
                "the download centre refused sandbox {sandbox:?}: {}",
                text.chars().take(300).collect::<String>()
            )));
        }
        Ok(text)
    }

    /// The fix repository a sandbox publishes updates through, e.g. `prodRepo_WM`.
    pub fn fix_repository(&self, sandbox: &str) -> Result<Option<String>> {
        let url = format!(
            "https://{}/services/sd-repository-service/v1/repositories/sandboxes/{sandbox}",
            self.host
        );
        let payload: serde_json::Value = self
            .agent
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .call()
            .map_err(|e| Error::Exec(format!("cannot describe sandbox {sandbox}: {e}")))?
            .into_body()
            .read_json()
            .map_err(|e| {
                Error::Exec(format!(
                    "description of sandbox {sandbox} from {url} unreadable: {e}"
                ))
            })?;
        Ok(payload
            .get("data")
            .and_then(|d| d.get("fixRepository"))
            .and_then(|r| r.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string))
    }

    /// Ask which fixes apply to an installation, returning the raw p2 archive.
    ///
    /// The body describes the installation; the answer is `content.jar`. The
    /// `X-IBM-wMSUM-P2-SCHEMA` header selects the webMethods flavour of the
    /// schema and is not optional.
    pub fn fix_metadata(
        &self,
        fix_repository: &str,
        inventory: &serde_json::Value,
        show_all: bool,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "https://{}/services/sum-repository-service/repositories/{fix_repository}/fixes?showAll={show_all}",
            self.host
        );
        let mut reader = self
            .agent
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("Content-Type", "application/json")
            .header("X-IBM-wMSUM-P2-SCHEMA", "WM")
            .send_json(inventory)
            .map_err(|e| Error::Exec(format!("cannot list fixes ({url}): {e}")))?
            .into_body()
            .into_reader();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map_err(|e| {
            Error::Exec(format!(
                "fix metadata download from {url} was cut short: {e}"
            ))
        })?;
        Ok(bytes)
    }

    /// Obtain — or reuse — repository credentials via the protocol-G handshake.
    fn grant(&mut self, cgi: &str) -> Result<Grant> {
        if let Some(existing) = &self.grant {
            if existing.issued.elapsed() < GRANT_LIFETIME {
                return Ok(existing.clone());
            }
        }
        let basic = basic_auth(&self.username.to_lowercase(), &self.password);
        let text = self
            .agent
            .post(format!("{cgi}?G"))
            .header("Authorization", &basic)
            .header("Cookie", "SD_SERVER_ENVIRONMENT_VERSION=1")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(format!("locale=en_US&buildNo={DEFAULT_BUILD_NO}"))
            .map_err(|e| Error::Exec(format!("download handshake with {cgi} failed: {e}")))?
            .into_body()
            .read_to_string()
            .map_err(|e| {
                Error::Exec(format!(
                    "download handshake response from {cgi} unreadable: {e}"
                ))
            })?;
        let grant = parse_grant(&text).ok_or_else(|| {
            Error::Exec(format!(
                "download handshake did not return credentials: {}",
                text.lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect::<String>()
            ))
        })?;
        let grant = Grant {
            user: grant.0,
            password: grant.1,
            issued: Instant::now(),
        };
        self.grant = Some(grant.clone());
        Ok(grant)
    }

    /// Fetch the p2 artifact index of a fix repository.
    ///
    /// Served from `/updates/<repo>/artifacts.jar` and readable only with the
    /// protocol-G credentials — the bearer token that lists fixes is refused
    /// here, and the account's own credentials are refused too.
    pub fn fix_artifact_index(&mut self, cgi: &str, fix_repository: &str) -> Result<Vec<u8>> {
        self.updates_get(cgi, fix_repository, "artifacts.jar")
    }

    /// Download one fix artifact, e.g. `binary/wMFix.SPM_12.1.0.0001-0556`.
    pub fn fix_artifact(&mut self, cgi: &str, fix_repository: &str, path: &str) -> Result<Vec<u8>> {
        self.updates_get(cgi, fix_repository, path)
    }

    fn updates_get(&mut self, cgi: &str, fix_repository: &str, path: &str) -> Result<Vec<u8>> {
        let grant = self.grant(cgi)?;
        let url = format!("https://{}/updates/{fix_repository}/{path}", self.host);
        let mut reader = self
            .agent
            .get(&url)
            .header("Authorization", &basic_auth(&grant.user, &grant.password))
            .call()
            .map_err(|e| Error::Exec(format!("cannot fetch {path}: {e}")))?
            .into_body()
            .into_reader();
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|e| Error::Exec(format!("download of {path} was cut short: {e}")))?;
        Ok(bytes)
    }

    /// Download one artifact, returning its bytes.
    ///
    /// `repository` is the release's repository name (`dataservewebM121`) and
    /// `path` the repository-relative path from [`artifact_path`].
    pub fn download(&mut self, cgi: &str, repository: &str, path: &str) -> Result<Vec<u8>> {
        let grant = self.grant(cgi)?;
        let url = format!("https://{}/{repository}/{path}", self.host);
        let mut reader = self
            .agent
            .get(&url)
            .header("Authorization", &basic_auth(&grant.user, &grant.password))
            .header("Cookie", "SD_SERVER_ENVIRONMENT_VERSION=1")
            .call()
            .map_err(|e| Error::Exec(format!("cannot download {path}: {e}")))?
            .into_body()
            .into_reader();
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|e| Error::Exec(format!("download of {path} was cut short: {e}")))?;
        Ok(bytes)
    }
}

/// Where an artifact lives in the repository, given its path in the product tree.
///
/// The installer derives this rather than being told: the first three segments
/// of the tree path identify the release, and the artifact sits under `bms/`
/// beside it. Everything between — the group, the component, the platform
/// variant — is metadata, not location.
///
/// ```
/// # use wm_core::sdc::artifact_path;
/// let tree = "e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/\
///             TNServer-LNXAMD64-Any/BM_TNSServerConfiguration-ALL-Any";
/// assert_eq!(
///     artifact_path(tree).unwrap(),
///     "e2ei/11/TN_12.1.0.0.139/bms/BM_TNSServerConfiguration-ALL-Any.zip"
/// );
/// ```
pub fn artifact_path(tree_path: &str) -> Option<String> {
    let segments: Vec<&str> = tree_path.split('/').collect();
    if segments.len() < 4 {
        return None;
    }
    let release = &segments[..3];
    let artifact = segments.last()?;
    Some(format!("{}/bms/{artifact}.zip", release.join("/")))
}

/// Where a resource jar lives, given its name and the release prefix.
pub fn jar_path(release_prefix: &str, jar_name: &str) -> String {
    let name = jar_name.strip_suffix(".jar").unwrap_or(jar_name);
    format!("{release_prefix}/jars/{name}.jar")
}

/// Parse `OK,a=<user>:<password>` out of a protocol-G response.
fn parse_grant(text: &str) -> Option<(String, String)> {
    for field in text.lines().next()?.split(',') {
        if let Some(pair) = field.trim().strip_prefix("a=") {
            let (user, password) = pair.split_once(':')?;
            if !user.is_empty() && !password.is_empty() {
                return Some((user.to_string(), password.to_string()));
            }
        }
    }
    None
}

fn basic_auth(user: &str, password: &str) -> String {
    let raw = format!("{user}:{password}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

fn string_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Hex-encoded sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hex-encoded md5 of `bytes`.
pub fn md5_hex(bytes: &[u8]) -> String {
    let digest = md5::Md5::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_artifact_path_from_the_tree_path() {
        let tree = "e2ei/11/TN_12.1.0.0.139/TradingNetworks/TNServer/\
                    TNServer-LNXAMD64-Any/BM_TNSServerConfiguration-ALL-Any";
        assert_eq!(
            artifact_path(tree).as_deref(),
            Some("e2ei/11/TN_12.1.0.0.139/bms/BM_TNSServerConfiguration-ALL-Any.zip")
        );
        assert!(artifact_path("too/short").is_none());
    }

    #[test]
    fn derives_the_repository_and_sandbox_from_a_release() {
        let release = Release {
            id: 20,
            release: "12.1".into(),
            display_name: "2026 May webMethods 12.1".into(),
            code: "2026_May".into(),
            urls: vec!["https://sdc.webmethods.io/cgi-bin/dataservewebM121.cgi".into()],
        };
        assert_eq!(release.repository().as_deref(), Some("dataservewebM121"));
        assert_eq!(release.sandbox().as_deref(), Some("webM121"));
    }

    #[test]
    fn parses_the_handshake_grant() {
        assert_eq!(
            parse_grant("OK,a=u26_242_46358:s3cr3t99\nsecond line"),
            Some(("u26_242_46358".into(), "s3cr3t99".into()))
        );
        // An error line carries no grant.
        assert_eq!(parse_grant("ERROR: 1"), None);
        assert_eq!(parse_grant("OK,a=onlyuser"), None);
    }

    #[test]
    fn hashes_match_the_reference_implementations() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn jar_paths_tolerate_the_extension_either_way() {
        assert_eq!(
            jar_path("e2ei/11/TN_12.1.0.0.139", "TNSInstallMessages-ALL-Any"),
            "e2ei/11/TN_12.1.0.0.139/jars/TNSInstallMessages-ALL-Any.jar"
        );
        assert_eq!(
            jar_path("e2ei/11/TN_12.1.0.0.139", "TNSInstallMessages-ALL-Any.jar"),
            "e2ei/11/TN_12.1.0.0.139/jars/TNSInstallMessages-ALL-Any.jar"
        );
    }
}
