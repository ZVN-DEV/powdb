//! Integration tests for the persisted user/role store.

use powdb_auth::{AuthError, UserStore};

#[test]
fn create_authenticate_roundtrip() {
    let mut s = UserStore::new();
    s.create_user("alice", "hunter2", "admin").unwrap();
    assert!(s.authenticate("alice", "hunter2").is_some());
    assert!(s.authenticate("alice", "wrong").is_none());
    assert!(s.authenticate("ghost", "hunter2").is_none());
}

#[test]
fn duplicate_create_rejected() {
    let mut s = UserStore::new();
    s.create_user("bob", "pw", "readwrite").unwrap();
    assert!(matches!(
        s.create_user("bob", "pw2", "readwrite"),
        Err(AuthError::UserExists(_))
    ));
}

#[test]
fn create_with_unknown_role_rejected() {
    let mut s = UserStore::new();
    assert!(matches!(
        s.create_user("carol", "pw", "superuser"),
        Err(AuthError::UnknownRole(_))
    ));
}

#[test]
fn set_role_changes_role() {
    let mut s = UserStore::new();
    s.create_user("dave", "pw", "readonly").unwrap();
    s.set_role("dave", "admin").unwrap();
    let users = s.list_users();
    assert!(users.contains(&("dave".to_string(), "admin".to_string())));

    assert!(matches!(
        s.set_role("dave", "nope"),
        Err(AuthError::UnknownRole(_))
    ));
    assert!(matches!(
        s.set_role("nobody", "admin"),
        Err(AuthError::UnknownUser(_))
    ));
}

#[test]
fn delete_user_removes() {
    let mut s = UserStore::new();
    s.create_user("erin", "pw", "readonly").unwrap();
    s.delete_user("erin").unwrap();
    assert!(s.list_users().is_empty());
    assert!(matches!(
        s.delete_user("erin"),
        Err(AuthError::UnknownUser(_))
    ));
}

#[test]
fn list_users_returns_name_role_and_no_hashes() {
    let mut s = UserStore::new();
    s.create_user("frank", "secretpw", "readwrite").unwrap();
    let users = s.list_users();
    assert_eq!(users, vec![("frank".to_string(), "readwrite".to_string())]);
    // list_users yields only (name, role) tuples — there is no field that
    // could carry a hash, and certainly not the plaintext password.
    for (name, role) in &users {
        assert!(!name.contains("secretpw"));
        assert!(!role.contains("secretpw"));
        assert!(!name.contains("$argon2"));
        assert!(!role.contains("$argon2"));
    }
}

#[test]
fn persistence_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = UserStore::new();
        s.create_user("gina", "topsecret", "admin").unwrap();
        s.create_user("hank", "pw2", "readonly").unwrap();
        s.save(dir.path()).unwrap();
    }
    let reloaded = UserStore::load(dir.path()).unwrap();
    assert!(reloaded.authenticate("gina", "topsecret").is_some());
    assert!(reloaded.authenticate("hank", "pw2").is_some());
    assert!(reloaded.authenticate("gina", "wrong").is_none());
}

#[test]
fn auth_json_contains_hash_but_not_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = UserStore::new();
    s.create_user("iris", "PLAINTEXT_SECRET_123", "admin")
        .unwrap();
    s.save(dir.path()).unwrap();

    let bytes = std::fs::read(dir.path().join("auth.json")).unwrap();
    let contents = String::from_utf8(bytes).unwrap();

    // The plaintext password must NOT appear anywhere on disk.
    assert!(
        !contents.contains("PLAINTEXT_SECRET_123"),
        "plaintext password leaked into auth.json"
    );
    // The argon2id PHC hash MUST be present.
    assert!(
        contents.contains("$argon2id$"),
        "expected argon2id PHC hash in auth.json"
    );
}

#[cfg(unix)]
#[test]
fn auth_json_is_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut s = UserStore::new();
    s.create_user("jane", "pw", "admin").unwrap();
    s.save(dir.path()).unwrap();
    let meta = std::fs::metadata(dir.path().join("auth.json")).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
}

#[test]
fn load_missing_file_returns_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let s = UserStore::load(dir.path()).unwrap();
    assert!(s.list_users().is_empty());
}
