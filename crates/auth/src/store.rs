//! Persisted user/role store backed by `auth.json`.
//!
//! Passwords are never stored in plaintext — only argon2id PHC hashes.
//! On Unix the on-disk file is written with mode `0600`. `powdb-server` loads
//! this store at startup and authenticates every connection against it;
//! `powdb-cli` manages it via the `useradd`/`passwd`/`userdel` subcommands.

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
    /// password both return `None` — and both cost one argon2 verification,
    /// so response timing cannot enumerate usernames (an unknown name used
    /// to return in nanoseconds against ~100ms for a known one).
    pub fn authenticate(&self, name: &str, candidate: &str) -> Option<&User> {
        let Some(user) = self.users.get(name) else {
            verify_password(dummy_password_hash(), candidate);
            return None;
        };
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

    /// Replace a user's password with a new argon2id hash, preserving role.
    ///
    /// Errors if the user is unknown. The new plaintext is held in a buffer
    /// that is zeroed on drop.
    pub fn set_password(&mut self, name: &str, new_password: &str) -> Result<(), AuthError> {
        let secret = Zeroizing::new(new_password.to_string());
        let password_hash = hash_password(&secret)?;
        let user = self
            .users
            .get_mut(name)
            .ok_or_else(|| AuthError::UnknownUser(name.to_string()))?;
        user.password_hash = password_hash;
        Ok(())
    }

    /// Remove a user. Errors if the user is unknown.
    pub fn delete_user(&mut self, name: &str) -> Result<(), AuthError> {
        self.users
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| AuthError::UnknownUser(name.to_string()))
    }

    /// Number of users in the store.
    pub fn len(&self) -> usize {
        self.users.len()
    }

    /// Whether the store has no users. When empty, the server falls back to
    /// the legacy shared-password authentication path.
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
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

/// A real argon2id hash (of a value no caller can present: it is discarded
/// after hashing) that [`UserStore::authenticate`] verifies unknown-user
/// candidates against, so the unknown branch does the same work as the known
/// one. Computed once per process; the one-time cost is one extra hash.
fn dummy_password_hash() -> &'static str {
    use std::sync::OnceLock;
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        let throwaway = Zeroizing::new(format!(
            "powdb-timing-equalizer-{}-{:p}",
            std::process::id(),
            &DUMMY
        ));
        // A hashing failure here cannot be surfaced (this is the "user does
        // not exist" path); an empty PHC makes verify_password parse-fail
        // fast, which merely restores the old timing rather than breaking
        // authentication.
        hash_password(&throwaway).unwrap_or_default()
    })
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
    fn empty_and_len_track_users() {
        let mut s = UserStore::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        s.create_user("alice", "pw", "readwrite").unwrap();
        assert!(!s.is_empty());
        assert_eq!(s.len(), 1);
        s.create_user("bob", "pw", "readonly").unwrap();
        assert_eq!(s.len(), 2);
        s.delete_user("alice").unwrap();
        assert_eq!(s.len(), 1);
        assert!(!s.is_empty());
        s.delete_user("bob").unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn set_password_changes_credential() {
        let mut s = UserStore::new();
        s.create_user("alice", "old", "readwrite").unwrap();
        assert!(s.authenticate("alice", "old").is_some());
        s.set_password("alice", "new").unwrap();
        // Old password no longer works; new one does; role is preserved.
        assert!(s.authenticate("alice", "old").is_none());
        assert!(s.authenticate("alice", "new").is_some());
        assert_eq!(s.authenticate("alice", "new").unwrap().role, "readwrite");
    }

    #[test]
    fn set_password_unknown_user_rejected() {
        let mut s = UserStore::new();
        assert!(matches!(
            s.set_password("ghost", "pw"),
            Err(AuthError::UnknownUser(_))
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

    /// Authenticating an unknown user must cost the same argon2 work as a
    /// wrong password for a known user. Before the dummy-verify, the unknown
    /// branch returned in microseconds while the known branch ran ~tens of
    /// milliseconds of argon2id — a timing oracle that let a remote caller
    /// enumerate usernames. Ratio-based with a wide margin (4x against a
    /// ~1000x historical gap), so machine speed cannot flake it.
    #[test]
    fn unknown_user_costs_the_same_argon2_work_as_a_wrong_password() {
        let mut s = UserStore::new();
        s.create_user("alice", "correct horse", "admin").unwrap();

        let median = |f: &dyn Fn() -> ()| {
            let mut runs: Vec<std::time::Duration> = (0..3)
                .map(|_| {
                    let t = std::time::Instant::now();
                    f();
                    t.elapsed()
                })
                .collect();
            runs.sort();
            runs[1]
        };

        let wrong = median(&|| {
            assert!(s.authenticate("alice", "wrong password").is_none());
        });
        let unknown = median(&|| {
            assert!(s.authenticate("mallory", "wrong password").is_none());
        });

        assert!(
            unknown * 4 > wrong,
            "unknown-user auth ({unknown:?}) must not be cheaper than \
             wrong-password auth ({wrong:?}) — that timing gap enumerates usernames"
        );
    }
}
