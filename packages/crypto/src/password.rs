//! Password hashes: argon2id with its default cost, as PHC text
//! (`$argon2id$v=19$…`), each with a new random salt.

use std::sync::LazyLock;

use argon2::{
    Argon2, PasswordHash, PasswordHasher as _, PasswordVerifier as _, password_hash::SaltString,
};

use crate::{Error, ErrorKind, random};

/// A hash of no one's password: what a login checks when no account has
/// the name given, so a wrong name costs the same time as a wrong
/// password.
pub static DECOY: LazyLock<String> =
    LazyLock::new(|| hash("not anyone's password").unwrap_or_default());

/// The argon2id hash of `password`, with a new random salt, as PHC text.
///
/// # Errors
///
/// [`ErrorKind::Random`] when the OS gives no random bytes;
/// [`ErrorKind::Hashing`] when argon2 refuses.
pub fn hash(password: &str) -> Result<String, Error> {
    let refused = |_| Error::new(ErrorKind::Hashing, "argon2 refused the password");
    let salt = SaltString::encode_b64(&random::<16>()?).map_err(refused)?;
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(refused)?;
    Ok(hash.to_string())
}

/// Whether `password` is the one `hash` (PHC text) was made from; false
/// for text that is not a hash.
#[must_use]
pub fn verify(password: &str, hash: &str) -> bool {
    let Ok(hash) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &hash)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::{DECOY, hash, verify};

    #[test]
    fn a_hash_verifies_its_password_only() {
        let hashed = hash("correct horse").unwrap();
        assert!(hashed.starts_with("$argon2id$"), "{hashed}");
        assert!(verify("correct horse", &hashed));
        assert!(!verify("wrong horse", &hashed));
        assert!(!verify("correct horse", "not a hash"));
        assert!(!verify("correct horse", &DECOY));
    }
}
