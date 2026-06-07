//! Permission and role model for PowDB RBAC.
//!
//! A small, fixed permission lattice plus three builtin roles
//! (`admin`, `readwrite`, `readonly`). This slice does not enforce
//! permissions anywhere — it only defines the data model.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// A single capability a role may grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Permission {
    /// Read rows.
    Read,
    /// Insert/update/delete rows.
    Write,
    /// Data-definition (create/alter/drop table or index).
    Ddl,
    /// Administrative operations (user/role management, etc.).
    Admin,
}

/// A named bundle of permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    /// Role name (e.g. `admin`).
    pub name: String,
    /// Permissions granted by this role.
    pub permissions: BTreeSet<Permission>,
}

impl Role {
    /// Whether this role grants permission `p`.
    pub fn allows(&self, p: Permission) -> bool {
        self.permissions.contains(&p)
    }

    /// Builtin `admin` role: all permissions.
    pub fn admin() -> Role {
        Role {
            name: "admin".to_string(),
            permissions: BTreeSet::from([
                Permission::Read,
                Permission::Write,
                Permission::Ddl,
                Permission::Admin,
            ]),
        }
    }

    /// Builtin `readwrite` role: read + write.
    pub fn readwrite() -> Role {
        Role {
            name: "readwrite".to_string(),
            permissions: BTreeSet::from([Permission::Read, Permission::Write]),
        }
    }

    /// Builtin `readonly` role: read only.
    pub fn readonly() -> Role {
        Role {
            name: "readonly".to_string(),
            permissions: BTreeSet::from([Permission::Read]),
        }
    }

    /// Resolve a builtin role by name, or `None` if it is not a builtin.
    pub fn builtin(name: &str) -> Option<Role> {
        match name {
            "admin" => Some(Role::admin()),
            "readwrite" => Some(Role::readwrite()),
            "readonly" => Some(Role::readonly()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_has_all_permissions() {
        let r = Role::admin();
        assert_eq!(
            r.permissions,
            BTreeSet::from([
                Permission::Read,
                Permission::Write,
                Permission::Ddl,
                Permission::Admin,
            ])
        );
    }

    #[test]
    fn admin_allows_everything() {
        let r = Role::admin();
        for p in [
            Permission::Read,
            Permission::Write,
            Permission::Ddl,
            Permission::Admin,
        ] {
            assert!(r.allows(p));
        }
    }

    #[test]
    fn readwrite_has_read_and_write_only() {
        let r = Role::readwrite();
        assert_eq!(
            r.permissions,
            BTreeSet::from([Permission::Read, Permission::Write])
        );
        assert!(r.allows(Permission::Read));
        assert!(r.allows(Permission::Write));
        assert!(!r.allows(Permission::Ddl));
        assert!(!r.allows(Permission::Admin));
    }

    #[test]
    fn readonly_has_read_only() {
        let r = Role::readonly();
        assert_eq!(r.permissions, BTreeSet::from([Permission::Read]));
        assert!(r.allows(Permission::Read));
        assert!(!r.allows(Permission::Write));
        assert!(!r.allows(Permission::Ddl));
        assert!(!r.allows(Permission::Admin));
    }

    #[test]
    fn builtin_resolves_known_roles() {
        assert_eq!(Role::builtin("admin").unwrap().name, "admin");
        assert_eq!(Role::builtin("readwrite").unwrap().name, "readwrite");
        assert_eq!(Role::builtin("readonly").unwrap().name, "readonly");
    }

    #[test]
    fn builtin_unknown_is_none() {
        assert!(Role::builtin("nope").is_none());
    }
}
