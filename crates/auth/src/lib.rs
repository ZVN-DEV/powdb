//! `powdb-auth` — argon2id password hashing and a persisted user/role store.
//!
//! Provides PowDB's RBAC primitives: the [`role`] permission lattice, the
//! [`hash`] argon2id password hashing, and the persisted [`store::UserStore`].
//! These are live in production: `powdb-server` loads the [`store::UserStore`]
//! at startup and enforces the [`role`] lattice on every query
//! (`crates/server/src/handler/`), and `powdb-cli` manages users via the
//! `useradd`/`passwd`/`userdel` subcommands.

pub mod error;
pub mod hash;
pub mod role;
pub mod store;

pub use error::AuthError;
pub use hash::{hash_password, verify_password};
pub use role::{Permission, Role};
pub use store::{User, UserStore};
