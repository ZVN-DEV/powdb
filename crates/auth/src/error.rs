/// Structured error type for the auth crate.
///
/// Mirrors the `thiserror`-based style used by `powdb-storage`. Kept
/// intentionally small — this slice is a library + data model only.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Password hashing failed (argon2 internal error).
    #[error("password hashing failed: {0}")]
    Hash(String),

    /// A user with the given name already exists.
    #[error("user already exists: {0}")]
    UserExists(String),

    /// No user with the given name is known.
    #[error("unknown user: {0}")]
    UnknownUser(String),

    /// The referenced role is not a known builtin.
    #[error("unknown role: {0}")]
    UnknownRole(String),
}

/// Convenience alias used throughout the auth crate.
pub type Result<T> = std::result::Result<T, AuthError>;
