//! `powdb-auth` — argon2id password hashing and a persisted user/role store.
//!
//! Slice 1 of PowDB's RBAC epic. This crate is **additive**: it is a tested
//! library + data model and is not yet wired into the server or CLI, so it
//! does not change any runtime behavior.

pub mod error;
pub mod hash;
pub mod role;
pub mod store;

pub use error::AuthError;
pub use hash::{hash_password, verify_password};
pub use role::{Permission, Role};
pub use store::{User, UserStore};
