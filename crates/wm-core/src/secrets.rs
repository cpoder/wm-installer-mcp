//! Secrets at rest.
//!
//! Until now the only place to put an entitlement key was the environment: the
//! MCP client's configuration file in clear, or a shell export. That is worse
//! than it sounds, because the key is not a password — it is a bearer token.
//! Anyone holding it can download against the account's entitlement, and
//! nothing about it is scoped or short-lived. It also gets lost: a key parked
//! in a file under `/tmp` disappears with the tmpfs, and the operator is asked
//! for it again.
//!
//! So: one encrypted file, `credentials.enc`, under the config directory.
//!
//! # What the encryption is for
//!
//! Two modes, and it is worth being exact about what each buys, because "the
//! credentials are encrypted" is a sentence that gets believed further than it
//! deserves.
//!
//! * **Passphrase** — `WM_MCP_PASSPHRASE` is set. The key is derived with
//!   PBKDF2-HMAC-SHA256 over 600 000 iterations and exists only in the running
//!   process. Someone who takes the disk, the backup or the file cannot read
//!   the store. One secret in the environment now stands in for every secret in
//!   it, which is the actual win: the entitlement key, the database passwords
//!   and the administrator passwords all come off disk together.
//! * **Key file** — no passphrase. A 32-byte key is generated once into
//!   `key`, mode 0600, beside the store. This protects against *disclosure*:
//!   the store survives a `cat`, a screen share, a backup tarball someone
//!   forwards, a directory copied into a repository. It does **not** protect
//!   against anyone who can already read files as you — the key is right
//!   there. It is the default because the alternative on an unattended
//!   installation host is no store at all.
//!
//! Neither mode is a secrets manager. An installation host that needs one
//! should keep the passphrase in it and let this hold the rest.
//!
//! # What never happens
//!
//! A value is never returned by a read tool, never written into a generated
//! installer or Update Manager script, and never put in a job wrapper. The
//! existing `$NAME$` marker convention — the script names the variable and the
//! product substitutes it at read time — stays exactly as it is; this store
//! feeds the environment of the process that runs, and nothing else.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::config::{config_dir, write_private};
use crate::{Error, Result};

/// The encrypted store, relative to the config directory.
const STORE_FILE: &str = "credentials.enc";

/// The machine-local key, relative to the config directory.
const KEY_FILE: &str = "key";

/// Environment variable holding a passphrase, when one is used.
pub const PASSPHRASE_VAR: &str = "WM_MCP_PASSPHRASE";

/// The IBM download-centre account.
pub const EMPOWER_USER: &str = "empower.user";

/// The IBM entitlement key.
///
/// Not a password: a bearer token that downloads against the account's
/// entitlement until it is revoked. It is the reason this module exists.
pub const EMPOWER_KEY: &str = "empower.key";

/// The name of a secret belonging to one registered installation.
///
/// Names are a flat namespace, so the convention carries the structure:
/// `install.<name>.<what>` — `install.b2b.db.password`,
/// `install.b2b.admin_password`. Keeping it in one function means a tool that
/// stores a secret and a tool that reads it cannot disagree about the spelling.
pub fn for_install(install: &str, what: &str) -> String {
    format!("install.{install}.{what}")
}

/// PBKDF2 iterations for passphrase mode. Matches [`crate::password`], which
/// reproduces Integration Server's own work factor.
const ITERATIONS: u32 = 600_000;

/// AES-256 key length.
const KEY_LEN: usize = 32;

/// GCM nonce length.
const NONCE_LEN: usize = 12;

/// PBKDF2 salt length.
const SALT_LEN: usize = 16;

/// On-disk format revision, bound into the authenticated data.
const VERSION: u8 = 1;

/// How the store is sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protection {
    /// Key derived from `WM_MCP_PASSPHRASE`; nothing on disk opens the store.
    Passphrase,
    /// Key read from a 0600 file beside the store.
    KeyFile,
}

impl Protection {
    /// One line the operator can act on, stating the limit as well as the mode.
    pub fn explain(self) -> &'static str {
        match self {
            Self::Passphrase => {
                "sealed with the passphrase in $WM_MCP_PASSPHRASE; nothing on disk opens it"
            }
            Self::KeyFile => {
                "sealed with a 0600 key file beside it: safe to back up or copy, but readable \
                 by anything running as this user. Set WM_MCP_PASSPHRASE to take the key off \
                 disk entirely."
            }
        }
    }

    fn as_aad(self) -> &'static [u8] {
        match self {
            Self::Passphrase => b"wm-mcp/credentials/passphrase",
            Self::KeyFile => b"wm-mcp/credentials/keyfile",
        }
    }
}

/// The file as it sits on disk. Only `ciphertext` holds anything secret.
#[derive(Serialize, Deserialize)]
struct Sealed {
    version: u8,
    protection: Protection,
    /// PBKDF2 salt, base64. Absent in key-file mode, which needs no derivation.
    #[serde(skip_serializing_if = "Option::is_none")]
    salt: Option<String>,
    /// PBKDF2 iteration count, recorded so the cost can be raised later
    /// without making existing stores unreadable.
    #[serde(skip_serializing_if = "Option::is_none")]
    iterations: Option<u32>,
    nonce: String,
    ciphertext: String,
}

/// An opened credential store.
pub struct Store {
    entries: BTreeMap<String, String>,
    protection: Protection,
    path: PathBuf,
}

/// Written by hand rather than derived, and the difference matters: a derived
/// `Debug` prints every value, and the first thing that formats a store into an
/// error or a trace would put an entitlement key in a log. Not even the names
/// are shown — they say which systems this machine holds credentials for.
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .field("protection", &self.protection)
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl Store {
    /// Open the store, or start an empty one when none exists yet.
    ///
    /// Fails when a store exists but the key or passphrase does not open it,
    /// rather than quietly presenting an empty store and losing the contents on
    /// the next save.
    pub fn open() -> Result<Self> {
        let path = config_dir().join(STORE_FILE);
        let protection = if std::env::var_os(PASSPHRASE_VAR).is_some() {
            Protection::Passphrase
        } else {
            Protection::KeyFile
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    entries: BTreeMap::new(),
                    protection,
                    path,
                })
            }
            Err(e) => return Err(Error::io(&path, e)),
        };
        let sealed: Sealed = serde_json::from_str(&text).map_err(|e| {
            Error::Malformed(format!("{} is not a credential store: {e}", path.display()))
        })?;
        if sealed.version != VERSION {
            return Err(Error::Malformed(format!(
                "{} is version {}, this build understands version {VERSION}",
                path.display(),
                sealed.version
            )));
        }
        if sealed.protection != protection {
            return Err(Error::Malformed(match sealed.protection {
                Protection::Passphrase => format!(
                    "{} is sealed with a passphrase; set {PASSPHRASE_VAR} to open it",
                    path.display()
                ),
                Protection::KeyFile => format!(
                    "{} is sealed with its key file, but {PASSPHRASE_VAR} is set. Unset it, or \
                     move the store aside to start a passphrase-sealed one",
                    path.display()
                ),
            }));
        }

        let key = key_material(protection, sealed.salt.as_deref(), sealed.iterations)?;
        let nonce = decode(&sealed.nonce, "nonce")?;
        let ciphertext = decode(&sealed.ciphertext, "ciphertext")?;
        let nonce: [u8; NONCE_LEN] = nonce.as_slice().try_into().map_err(|_| {
            Error::Malformed(format!(
                "{} has a {}-byte nonce, expected {NONCE_LEN}",
                path.display(),
                nonce.len()
            ))
        })?;
        let plain = Aes256Gcm::new(&Key::<Aes256Gcm>::from(key))
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &ciphertext,
                    aad: protection.as_aad(),
                },
            )
            .map_err(|_| {
                Error::Malformed(format!(
                    "{} does not decrypt: {}",
                    path.display(),
                    match protection {
                        Protection::Passphrase =>
                            format!("wrong {PASSPHRASE_VAR}, or the file was altered"),
                        Protection::KeyFile => format!(
                            "the key file {} does not match it, or the file was altered",
                            config_dir().join(KEY_FILE).display()
                        ),
                    }
                ))
            })?;
        let entries = serde_json::from_slice(&plain)
            .map_err(|e| Error::Malformed(format!("the store decrypted but is not valid: {e}")))?;
        Ok(Self {
            entries,
            protection,
            path,
        })
    }

    /// How this store is sealed.
    pub fn protection(&self) -> Protection {
        self.protection
    }

    /// Where the store lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether anything has been stored yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look a secret up.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries.get(name).map(String::as_str)
    }

    /// Store a secret, replacing any previous value. Returns whether it replaced one.
    pub fn set(&mut self, name: &str, value: &str) -> bool {
        self.entries
            .insert(name.to_string(), value.to_string())
            .is_some()
    }

    /// Remove a secret. Returns whether there was one.
    pub fn remove(&mut self, name: &str) -> bool {
        self.entries.remove(name).is_some()
    }

    /// The names held, in order. Never the values.
    pub fn names(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// Seal and write the store.
    ///
    /// A fresh nonce every time: GCM reuses a (key, nonce) pair at its peril,
    /// and the whole map is re-encrypted on each save.
    pub fn save(&self) -> Result<()> {
        let (key, salt) = match self.protection {
            Protection::Passphrase => {
                let mut salt = [0u8; SALT_LEN];
                random(&mut salt)?;
                (
                    key_material(self.protection, Some(&encode(&salt)), Some(ITERATIONS))?,
                    Some(encode(&salt)),
                )
            }
            Protection::KeyFile => (key_material(self.protection, None, None)?, None),
        };
        let mut nonce = [0u8; NONCE_LEN];
        random(&mut nonce)?;
        let plain = serde_json::to_vec(&self.entries)
            .map_err(|e| Error::Malformed(format!("cannot serialise the store: {e}")))?;
        let ciphertext = Aes256Gcm::new(&Key::<Aes256Gcm>::from(key))
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &plain,
                    aad: self.protection.as_aad(),
                },
            )
            .map_err(|_| Error::Exec("cannot encrypt the credential store".into()))?;
        let sealed = Sealed {
            version: VERSION,
            protection: self.protection,
            salt,
            iterations: matches!(self.protection, Protection::Passphrase).then_some(ITERATIONS),
            nonce: encode(&nonce),
            ciphertext: encode(&ciphertext),
        };
        let text = serde_json::to_string_pretty(&sealed)
            .map_err(|e| Error::Malformed(format!("cannot serialise the store: {e}")))?;
        write_private(&self.path, text.as_bytes())
    }
}

/// The entitlement credentials a spawned job needs in its environment.
///
/// Only what this process's own environment does not already carry: a job
/// inherits that anyway, and re-supplying it would let a stale stored value
/// shadow a deliberate export. The result is handed to
/// [`crate::runner::Environment::secret_env`], which puts it in the child's
/// environment and not in the wrapper script that stays on disk.
pub fn job_environment() -> Vec<(String, String)> {
    [
        ("WM_EMPOWER_USER", EMPOWER_USER),
        ("WM_EMPOWER_KEY", EMPOWER_KEY),
    ]
    .into_iter()
    .filter(|(var, _)| std::env::var_os(var).is_none())
    .filter_map(|(var, stored)| lookup(stored).map(|value| (var.to_string(), value)))
    .collect()
}

/// Read a secret without holding the store open. `None` when it is not set, or
/// when the store cannot be opened — a caller falling back to the environment
/// should not be stopped by a store that was never created.
pub fn lookup(name: &str) -> Option<String> {
    Store::open().ok()?.get(name).map(str::to_string)
}

/// The key that seals the store, derived or read according to `protection`.
fn key_material(
    protection: Protection,
    salt: Option<&str>,
    iterations: Option<u32>,
) -> Result<[u8; KEY_LEN]> {
    match protection {
        Protection::Passphrase => {
            let passphrase = std::env::var(PASSPHRASE_VAR)
                .map_err(|_| Error::Exec(format!("{PASSPHRASE_VAR} is not set")))?;
            let salt = salt.ok_or_else(|| {
                Error::Malformed("a passphrase-sealed store carries no salt".into())
            })?;
            let salt = decode(salt, "salt")?;
            let mut key = [0u8; KEY_LEN];
            pbkdf2::pbkdf2_hmac::<sha2::Sha256>(
                passphrase.as_bytes(),
                &salt,
                iterations.unwrap_or(ITERATIONS),
                &mut key,
            );
            Ok(key)
        }
        Protection::KeyFile => read_or_create_key(),
    }
}

/// Read the machine-local key, creating it on first use.
fn read_or_create_key() -> Result<[u8; KEY_LEN]> {
    let path = config_dir().join(KEY_FILE);
    match std::fs::read(&path) {
        Ok(bytes) if bytes.len() == KEY_LEN => {
            let mut key = [0u8; KEY_LEN];
            key.copy_from_slice(&bytes);
            Ok(key)
        }
        Ok(bytes) => Err(Error::Malformed(format!(
            "{} is {} bytes, expected {KEY_LEN}. Delete it only if you are willing to lose the \
             store it opens",
            path.display(),
            bytes.len()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut key = [0u8; KEY_LEN];
            random(&mut key)?;
            write_private(&path, &key)?;
            Ok(key)
        }
        Err(e) => Err(Error::io(&path, e)),
    }
}

fn random(buf: &mut [u8]) -> Result<()> {
    getrandom::fill(buf)
        .map_err(|e| Error::Exec(format!("cannot obtain random bytes from the system: {e}")))
}

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode(text: &str, what: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|e| Error::Malformed(format!("the {what} is not base64: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `body` against a config directory of its own.
    ///
    /// `WM_CONFIG_DIR` is process-wide, so these tests must not run
    /// concurrently with one another; they are serialised through one lock.
    fn in_a_temp_config<T>(body: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let dir = std::env::temp_dir().join(format!(
            "wm-secrets-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: the lock above is the only writer of these variables in this
        // test binary, and no test here spawns threads that read them.
        unsafe {
            std::env::set_var("WM_CONFIG_DIR", &dir);
            std::env::remove_var(PASSPHRASE_VAR);
        }
        let out = body();
        unsafe {
            std::env::remove_var("WM_CONFIG_DIR");
            std::env::remove_var(PASSPHRASE_VAR);
        }
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn a_secret_survives_a_round_trip() {
        in_a_temp_config(|| {
            let mut store = Store::open().unwrap();
            assert!(store.is_empty());
            store.set("empower.key", "ey.a-bearer-token");
            store.save().unwrap();

            let reopened = Store::open().unwrap();
            assert_eq!(reopened.get("empower.key"), Some("ey.a-bearer-token"));
            assert_eq!(reopened.names(), vec!["empower.key"]);
        });
    }

    #[test]
    fn the_value_is_not_on_disk_in_clear() {
        in_a_temp_config(|| {
            let mut store = Store::open().unwrap();
            store.set("empower.key", "a-bearer-token-in-clear");
            store.save().unwrap();
            let raw = std::fs::read_to_string(store.path()).unwrap();
            assert!(!raw.contains("a-bearer-token-in-clear"), "{raw}");
            // The names are structure, not secrets, but they are inside the
            // ciphertext too: the file says nothing about what it holds.
            assert!(!raw.contains("empower.key"), "{raw}");
        });
    }

    #[test]
    fn a_tampered_store_is_refused_rather_than_read() {
        in_a_temp_config(|| {
            let mut store = Store::open().unwrap();
            store.set("empower.key", "value");
            store.save().unwrap();

            let mut sealed: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(store.path()).unwrap()).unwrap();
            let mut bytes = decode(sealed["ciphertext"].as_str().unwrap(), "ciphertext").unwrap();
            bytes[0] ^= 0xff;
            sealed["ciphertext"] = serde_json::Value::String(encode(&bytes));
            std::fs::write(store.path(), sealed.to_string()).unwrap();

            let err = Store::open().unwrap_err();
            assert!(err.to_string().contains("does not decrypt"), "{err}");
        });
    }

    #[test]
    fn a_passphrase_store_is_not_opened_by_the_key_file() {
        in_a_temp_config(|| {
            unsafe { std::env::set_var(PASSPHRASE_VAR, "correct horse") };
            let mut store = Store::open().unwrap();
            assert_eq!(store.protection(), Protection::Passphrase);
            store.set("empower.key", "value");
            store.save().unwrap();

            unsafe { std::env::set_var(PASSPHRASE_VAR, "wrong horse") };
            let err = Store::open().unwrap_err();
            assert!(err.to_string().contains("does not decrypt"), "{err}");

            // Dropping the passphrase must not silently fall back to a key
            // file and present an empty store: saving that would lose the lot.
            unsafe { std::env::remove_var(PASSPHRASE_VAR) };
            let err = Store::open().unwrap_err();
            assert!(err.to_string().contains(PASSPHRASE_VAR), "{err}");
        });
    }

    #[cfg(unix)]
    #[test]
    fn the_store_and_its_key_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        in_a_temp_config(|| {
            let mut store = Store::open().unwrap();
            store.set("empower.key", "value");
            store.save().unwrap();
            for path in [store.path().to_path_buf(), config_dir().join(KEY_FILE)] {
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{} is {mode:o}", path.display());
            }
            let mode = std::fs::metadata(config_dir())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "the config directory is {mode:o}");
        });
    }
}
