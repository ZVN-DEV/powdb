//! Argon2id password hashing.
//!
//! Produces PHC-format hash strings (`$argon2id$...`) with a random salt
//! per password, and verifies candidates against them without ever
//! panicking or revealing which step of verification failed.

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;

use crate::error::AuthError;

/// Hash `password` with argon2id and a fresh random salt.
///
/// Returns the PHC string (e.g. `$argon2id$v=19$m=...,t=...,p=...$salt$hash`).
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Hash(e.to_string()))
}

/// Verify `candidate` against a stored PHC `phc` string.
///
/// Returns `false` on any parse or verification failure — never panics and
/// never distinguishes "bad hash format" from "wrong password".
pub fn verify_password(phc: &str, candidate: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(candidate.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hash_then_verify_roundtrips() {
        let h = hash_password("correct horse").unwrap();
        assert!(verify_password(&h, "correct horse"));
    }
    #[test]
    fn verify_rejects_wrong_password() {
        let h = hash_password("s3cret").unwrap();
        assert!(!verify_password(&h, "wrong"));
    }
    #[test]
    fn two_hashes_of_same_password_differ() {
        // distinct random salts => distinct PHC strings
        assert_ne!(
            hash_password("same").unwrap(),
            hash_password("same").unwrap()
        );
    }
    #[test]
    fn verify_rejects_garbage_hash() {
        assert!(!verify_password("not-a-phc-string", "x"));
    }
}
