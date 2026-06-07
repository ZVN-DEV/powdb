//! Persisted user/role store backed by `auth.json`.
//!
//! Passwords are never stored in plaintext — only argon2id PHC hashes.
//! On Unix the on-disk file is written with mode `0600`. This slice is
//! additive: nothing in the server/cli reads this store yet.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::AuthError;
use crate::hash::{hash_password, verify_password};
use crate::role::Role;

const AUTH_FILE: &str = "auth.json";

/// A single user record. The password is stored only as an argon2 PHC hash.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct User {
    /// Unique user name.
    pub name: String,
    /// Argon2id PHC hash string (never plaintext).
    pub password_hash: String,
    /// Name of the role assigned to this user (a builtin role name).
    pub role: String,
}

/// In-memory, serializable collection of users keyed by name.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UserStore {
    users: BTreeMap<String, User>,
}

impl UserStore {
    /// Create an empty store.
    pub fn new() -> Self {
        UserStore {
            users: BTreeMap::new(),
        }
    }

    /// Create a new user.
    ///
    /// Errors if `name` already exists or `role` is not a known builtin.
    /// The password is hashed with argon2id before storage.
    pub fn create_user(&mut self, name: &str, password: &str, role: &str) -> Result<(), AuthError> {
        if self.users.contains_key(name) {
            return Err(AuthError::UserExists(name.to_string()));
        }
        if Role::builtin(role).is_none() {
            return Err(AuthError::UnknownRole(role.to_string()));
        }
        // Hold the plaintext in a buffer that is zeroed on drop so the
        // password does not linger in freed memory longer than necessary.
        let secret = Zeroizing::new(password.to_string());
        let password_hash = hash_password(&secret)?;
        self.users.insert(
            name.to_string(),
            User {
                name: name.to_string(),
                password_hash,
                role: role.to_string(),
            },
        );
        Ok(())
    }

    /// Authenticate a user by name + candidate password.
    ///
    /// Returns `Some(&User)` only on a verified match. Unknown user or wrong
    /// password both return `None`.
    pub fn authenticate(&self, name: &str, candidate: &str) -> Option<&User> {
        let user = self.users.get(name)?;
        if verify_password(&user.password_hash, candidate) {
            Some(user)
        } else {
            None
        }
    }

    /// Reassign a user's role.
    ///
    /// Errors if the user is unknown or the role is not a known builtin.
    pub fn set_role(&mut self, name: &str, role: &str) -> Result<(), AuthError> {
        if Role::builtin(role).is_none() {
            return Err(AuthError::UnknownRole(role.to_string()));
        }
        let user = self
            .users
            .get_mut(name)
            .ok_or_else(|| AuthError::UnknownUser(name.to_string()))?;
        user.role = role.to_string();
        Ok(())
    }

    /// Remove a user. Errors if the user is unknown.
    pub fn delete_user(&mut self, name: &str) -> Result<(), AuthError> {
        self.users
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| AuthError::UnknownUser(name.to_string()))
    }

    /// List users as `(name, role)` pairs. Never exposes password hashes.
    pub fn list_users(&self) -> Vec<(String, String)> {
        self.users
            .values()
            .map(|u| (u.name.clone(), u.role.clone()))
            .collect()
    }

    /// Persist the store to `dir/auth.json` as pretty JSON.
    ///
    /// On Unix the file is written with mode `0600`.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let path = dir.join(AUTH_FILE);
        fs::write(&path, json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&path, perms)?;
        }
        Ok(())
    }

    /// Load the store from `dir/auth.json`.
    ///
    /// If the file does not exist, returns an empty store.
    pub fn load(dir: &Path) -> io::Result<Self> {
        let path = dir.join(AUTH_FILE);
        match fs::read_to_string(&path) {
            Ok(json) => serde_json::from_str(&json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(UserStore::new()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_authenticate() {
        let mut s = UserStore::new();
        s.create_user("alice", "pw", "admin").unwrap();
        assert!(s.authenticate("alice", "pw").is_some());
        assert!(s.authenticate("alice", "bad").is_none());
        assert!(s.authenticate("nobody", "pw").is_none());
    }

    #[test]
    fn duplicate_create_rejected() {
        let mut s = UserStore::new();
        s.create_user("a", "pw", "readonly").unwrap();
        assert!(matches!(
            s.create_user("a", "pw2", "readonly"),
            Err(AuthError::UserExists(_))
        ));
    }

    #[test]
    fn create_with_unknown_role_rejected() {
        let mut s = UserStore::new();
        assert!(matches!(
            s.create_user("a", "pw", "wizard"),
            Err(AuthError::UnknownRole(_))
        ));
    }
}
