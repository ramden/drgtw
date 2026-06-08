use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("password hashing failed: {0}")]
    Hash(argon2::password_hash::Error),
}

impl From<argon2::password_hash::Error> for AuthError {
    fn from(e: argon2::password_hash::Error) -> Self {
        Self::Hash(e)
    }
}
