use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand_core::OsRng;

use crate::error::AuthError;

/// Hash `password` with argon2id and a random salt. Returns a PHC string.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default().hash_password(password.as_bytes(), &salt)?;
    Ok(hash.to_string())
}

/// Verify `password` against a PHC string. Returns `false` on any error.
pub fn verify_password(password: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let phc = hash_password("hunter2").unwrap();
        assert!(verify_password("hunter2", &phc));
    }

    #[test]
    fn wrong_password_is_false() {
        let phc = hash_password("correct").unwrap();
        assert!(!verify_password("wrong", &phc));
    }

    #[test]
    fn corrupted_phc_is_false() {
        assert!(!verify_password("any", "$argon2id$v=CORRUPT$garbage"));
        assert!(!verify_password("any", ""));
        assert!(!verify_password("any", "not-a-phc-string"));
    }
}
