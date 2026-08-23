//! Handshake authentication and the RBAC boundary every statement crosses.

use super::*;

// ---- Named-database gate (P-10) ----

#[test]
fn db_name_unpinned_accepts_any_name() {
    for requested in ["", "default", "prod", "anything"] {
        assert!(
            check_db_name(None, requested).is_ok(),
            "rejected {requested}"
        );
    }
}

#[test]
fn db_name_pinned_accepts_match_empty_and_default_sentinel() {
    // The configured name, the empty name, and the client default sentinel
    // are all "no foreign database explicitly requested".
    assert!(check_db_name(Some("prod"), "prod").is_ok());
    assert!(check_db_name(Some("prod"), "").is_ok());
    assert!(check_db_name(Some("prod"), DEFAULT_DB_NAME).is_ok());
}

#[test]
fn db_name_pinned_rejects_foreign_with_clear_message() {
    let err = check_db_name(Some("prod"), "staging").unwrap_err();
    assert_eq!(err, "unknown database 'staging'; this server serves 'prod'");
}

// ---- Role enforcement (Fix: readonly role was not enforced) ----

fn parsed(q: &str) -> powdb_query::ast::Statement {
    parser::parse(q).unwrap()
}

#[test]
fn readonly_can_read_but_not_write() {
    let p = principal("readonly");
    // Reads pass.
    assert!(check_statement_permitted(p.as_ref(), &parsed("User")).is_ok());
    assert!(check_statement_permitted(p.as_ref(), &parsed("count(User)")).is_ok());
    assert!(check_statement_permitted(p.as_ref(), &parsed("explain User")).is_ok());
    // Writes, DDL, and transaction control are denied.
    for q in [
        r#"insert User { name := "x" }"#,
        "User filter .id = 1 update { age := 2 }",
        "User filter .id = 1 delete",
        "drop User",
        "alter User add column c: str",
        "type T { required id: int }",
        "begin",
        "commit",
        "rollback",
    ] {
        let err = check_statement_permitted(p.as_ref(), &parsed(q))
            .expect_err(&format!("must deny: {q}"));
        assert!(
            err.to_string().contains("permission denied"),
            "unexpected error for {q}: {err}"
        );
    }
}

#[test]
fn readwrite_and_admin_have_full_query_access() {
    for role in ["readwrite", "admin"] {
        let p = principal(role);
        assert!(check_statement_permitted(p.as_ref(), &parsed("User")).is_ok());
        assert!(
            check_statement_permitted(p.as_ref(), &parsed(r#"insert User { name := "x" }"#))
                .is_ok()
        );
        assert!(check_statement_permitted(p.as_ref(), &parsed("drop User")).is_ok());
    }
}

#[test]
fn unknown_role_fails_closed_for_writes() {
    let p = principal("mystery");
    assert!(check_statement_permitted(p.as_ref(), &parsed("User")).is_ok());
    assert!(
        check_statement_permitted(p.as_ref(), &parsed(r#"insert User { name := "x" }"#)).is_err()
    );
}

#[test]
fn no_principal_means_full_access() {
    // Shared-password / open mode: no per-user identity, no restriction.
    assert!(check_statement_permitted(None, &parsed("drop User")).is_ok());
    assert!(check_statement_permitted(None, &parsed(r#"insert User { name := "x" }"#)).is_ok());
}

fn store_with_alice() -> UserStore {
    let mut s = UserStore::new();
    s.create_user("alice", "pw", "readwrite").unwrap();
    s
}

// ---- Empty store: legacy shared-password fallback ----

#[test]
fn empty_store_no_password_is_open() {
    let s = UserStore::new();
    assert_eq!(
        authenticate_connect(&s, None, None, None),
        AuthOutcome::Authenticated { principal: None }
    );
    // Even a stray username/password is accepted (legacy open behavior).
    assert_eq!(
        authenticate_connect(&s, None, Some("x"), Some("y")),
        AuthOutcome::Authenticated { principal: None }
    );
}

#[test]
fn empty_store_correct_shared_password_succeeds() {
    let s = UserStore::new();
    assert_eq!(
        authenticate_connect(&s, Some("pw"), None, Some("pw")),
        AuthOutcome::Authenticated { principal: None }
    );
}

#[test]
fn empty_store_wrong_shared_password_rejected() {
    let s = UserStore::new();
    assert_eq!(
        authenticate_connect(&s, Some("pw"), None, Some("bad")),
        AuthOutcome::Rejected
    );
}

#[test]
fn empty_store_missing_password_rejected_when_expected() {
    let s = UserStore::new();
    assert_eq!(
        authenticate_connect(&s, Some("pw"), None, None),
        AuthOutcome::Rejected
    );
}

#[test]
fn empty_store_ignores_username_for_shared_password() {
    // A new client may send a username even against a shared-password
    // server; the username is ignored and the password still governs.
    let s = UserStore::new();
    assert_eq!(
        authenticate_connect(&s, Some("pw"), Some("whoever"), Some("pw")),
        AuthOutcome::Authenticated { principal: None }
    );
}

// ---- Populated store: multi-user auth ----

#[test]
fn user_auth_success_binds_principal() {
    let s = store_with_alice();
    assert_eq!(
        authenticate_connect(&s, None, Some("alice"), Some("pw")),
        AuthOutcome::Authenticated {
            principal: Some(Principal {
                name: "alice".into(),
                role: "readwrite".into(),
            })
        }
    );
}

#[test]
fn user_auth_wrong_password_rejected() {
    let s = store_with_alice();
    assert_eq!(
        authenticate_connect(&s, None, Some("alice"), Some("bad")),
        AuthOutcome::Rejected
    );
}

#[test]
fn user_auth_unknown_user_rejected() {
    let s = store_with_alice();
    assert_eq!(
        authenticate_connect(&s, None, Some("mallory"), Some("pw")),
        AuthOutcome::Rejected
    );
}

#[test]
fn user_auth_missing_username_rejected() {
    let s = store_with_alice();
    assert_eq!(
        authenticate_connect(&s, None, None, Some("pw")),
        AuthOutcome::Rejected
    );
}

#[test]
fn user_auth_missing_password_rejected() {
    let s = store_with_alice();
    assert_eq!(
        authenticate_connect(&s, Some("pw"), Some("alice"), None),
        AuthOutcome::Rejected
    );
}

#[test]
fn user_auth_ignores_shared_password_when_users_present() {
    // With users present, the shared password is irrelevant: supplying it as
    // the password without a valid user must NOT authenticate.
    let s = store_with_alice();
    assert_eq!(
        authenticate_connect(&s, Some("shared"), None, Some("shared")),
        AuthOutcome::Rejected
    );
}
