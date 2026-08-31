//! The Integration Server administrator password file.
//!
//! Creating an instance writes `config/installerKeyFile`, holding the hash the
//! server checks the `Administrator` password against on first start, and
//! `config/changeFlagFile`, saying whether that password must be changed at
//! first login. The shipped Ant script produces them by running
//! `com.wm.security.UpdateInstanceKey`; this reproduces the format so an
//! instance can be provisioned without a JVM.
//!
//! The scheme, from `com.wm.security.PasswordUtil.getPasswordHash_v2`:
//!
//! * the hashed string is `"Administrator" + password + "SAG"` — the user name
//!   is a prefix and `SAG` a fixed suffix, so the same password hashes
//!   differently for a different account;
//! * 16 random bytes of salt;
//! * PBKDF2-HMAC-SHA256, 600 000 iterations, a 256-bit key;
//! * the file contains `{PBKDF2-HmacSHA256_2}` followed by
//!   base64(key ‖ salt).
//!
//! Note the argument order in the original: `new PBEKeySpec(password, salt,
//! workFactor, iterations)` reads as though `iterations` were the iteration
//! count, but Java's signature is `(password, salt, iterationCount, keyLength)`.
//! The iteration count is therefore `workFactor` (600 000) and `iterations`
//! (256) is the key length *in bits*. Reading it the other way round produces a
//! hash the server rejects.

use base64::Engine as _;

use crate::{Error, Result};

/// Marker the server uses to recognise this hash format.
pub const PREFIX: &str = "{PBKDF2-HmacSHA256_2}";

/// Fixed suffix appended to every password before hashing.
const SUFFIX: &str = "SAG";

/// PBKDF2 iteration count.
const ITERATIONS: u32 = 600_000;

/// Derived key length in bytes (256 bits).
const KEY_LEN: usize = 32;

/// Salt length in bytes.
const SALT_LEN: usize = 16;

/// The account an installer-seeded password belongs to.
pub const DEFAULT_USER: &str = "Administrator";

/// Hash `password` for `user`, generating a fresh salt.
pub fn hash(user: &str, password: &str) -> Result<String> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt)
        .map_err(|e| Error::Exec(format!("cannot obtain salt from the system: {e}")))?;
    Ok(hash_with_salt(user, password, &salt))
}

/// Hash with a caller-supplied salt. Exposed so the format can be tested.
pub fn hash_with_salt(user: &str, password: &str, salt: &[u8]) -> String {
    let material = format!("{user}{password}{SUFFIX}");
    let mut key = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(material.as_bytes(), salt, ITERATIONS, &mut key);

    let mut blob = Vec::with_capacity(KEY_LEN + salt.len());
    blob.extend_from_slice(&key);
    blob.extend_from_slice(salt);
    format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(&blob)
    )
}

/// Check a password against a hash the server would accept.
///
/// Used to prove the implementation matches the product's, and to let a caller
/// confirm a password before writing it into an instance.
pub fn verify(user: &str, password: &str, hashed: &str) -> Result<bool> {
    let Some(encoded) = hashed.trim().strip_prefix(PREFIX) else {
        return Err(Error::Malformed(format!(
            "not a {PREFIX} hash: {}",
            hashed.chars().take(32).collect::<String>()
        )));
    };
    let blob = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| Error::Malformed(format!("hash is not base64: {e}")))?;
    if blob.len() != KEY_LEN + SALT_LEN {
        return Err(Error::Malformed(format!(
            "hash is {} bytes, expected {}",
            blob.len(),
            KEY_LEN + SALT_LEN
        )));
    }
    let salt = &blob[KEY_LEN..];
    let candidate = hash_with_salt(user, password, salt);
    // Compare the whole encoded string in constant time.
    Ok(constant_time_eq(
        candidate.as_bytes(),
        hashed.trim().as_bytes(),
    ))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether a password satisfies the rules the product's own scripts enforce.
///
/// The installer itself only rejects an empty value, so a weak password is
/// accepted and then fails later inside a product script. Checking here turns
/// that into an answer before anything is written.
pub fn complaints(password: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if password.trim().is_empty() {
        problems.push("empty".to_string());
        return problems;
    }
    if password.chars().count() < 8 {
        problems.push("shorter than 8 characters".to_string());
    }
    if !password.chars().any(char::is_alphabetic) {
        problems.push("no letter".to_string());
    }
    if !password.chars().any(|c| c.is_ascii_digit()) {
        problems.push("no digit".to_string());
    }
    if password.chars().all(char::is_alphanumeric) {
        problems.push("no special character".to_string());
    }
    let chars: Vec<char> = password.chars().collect();
    if chars.windows(3).any(|w| w[0] == w[1] && w[1] == w[2]) {
        problems.push("three identical consecutive characters".to_string());
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_has_the_shape_the_server_expects() {
        let hashed = hash_with_salt(DEFAULT_USER, "Passw0rd!x", &[7u8; SALT_LEN]);
        assert!(hashed.starts_with(PREFIX));
        let blob = base64::engine::general_purpose::STANDARD
            .decode(hashed.strip_prefix(PREFIX).unwrap())
            .expect("base64");
        assert_eq!(blob.len(), KEY_LEN + SALT_LEN);
        // The salt is carried after the key, so it can be recovered.
        assert_eq!(&blob[KEY_LEN..], &[7u8; SALT_LEN]);
    }

    #[test]
    fn verifies_its_own_hashes() {
        let hashed = hash(DEFAULT_USER, "Passw0rd!x").expect("hash");
        assert!(verify(DEFAULT_USER, "Passw0rd!x", &hashed).expect("verify"));
        assert!(!verify(DEFAULT_USER, "Passw0rd!y", &hashed).expect("verify"));
        // The user name is part of the material, not decoration.
        assert!(!verify("Someone", "Passw0rd!x", &hashed).expect("verify"));
    }

    #[test]
    fn a_fresh_salt_is_used_each_time() {
        let first = hash(DEFAULT_USER, "Passw0rd!x").expect("hash");
        let second = hash(DEFAULT_USER, "Passw0rd!x").expect("hash");
        assert_ne!(first, second, "two hashes of one password must differ");
    }

    #[test]
    fn rejects_a_hash_of_the_wrong_shape() {
        assert!(verify(DEFAULT_USER, "x", "{sha-256}abc").is_err());
        assert!(verify(DEFAULT_USER, "x", &format!("{PREFIX}not-base64!!")).is_err());
        assert!(verify(DEFAULT_USER, "x", &format!("{PREFIX}YWJj")).is_err());
    }

    #[test]
    fn reports_what_the_product_scripts_would_reject() {
        assert!(complaints("Passw0rd!x").is_empty());
        assert!(complaints("").contains(&"empty".to_string()));
        assert!(complaints("aaa1!")
            .iter()
            .any(|c| c.contains("consecutive")));
        assert!(complaints("Password1")
            .iter()
            .any(|c| c.contains("special")));
        assert!(complaints("Pw1!")
            .iter()
            .any(|c| c.contains("8 characters")));
    }
}
