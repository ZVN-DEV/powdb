//! Safety-limit tests: verify that the query engine rejects pathological
//! inputs with clear error messages instead of consuming unbounded memory.

use powdb_query::lexer::lex;

// ---------------------------------------------------------------------------
// TASK-22: String literal length limit
// ---------------------------------------------------------------------------

#[test]
fn string_literal_exceeding_16mb_returns_lex_error() {
    // Build a string that exceeds the 16 MB limit (16 * 1024 * 1024 + 1 bytes
    // of content, plus the enclosing quotes).
    let content_len = 16 * 1024 * 1024 + 1;
    let mut query = String::with_capacity(content_len + 2);
    query.push('"');
    for _ in 0..content_len {
        query.push('x');
    }
    query.push('"');

    let err = lex(&query).expect_err("must reject oversized string literal");
    assert!(
        err.message.contains("exceeds maximum size"),
        "unexpected error message: {}",
        err.message
    );
    assert!(
        err.message.contains("16MB"),
        "error should mention the limit in MB: {}",
        err.message
    );
}

#[test]
fn string_literal_at_16mb_is_accepted() {
    // Exactly 16 MB should be fine (the limit is >16MB, not >=).
    let content_len = 16 * 1024 * 1024;
    let mut query = String::with_capacity(content_len + 2);
    query.push('"');
    for _ in 0..content_len {
        query.push('a');
    }
    query.push('"');

    let tokens = lex(&query).expect("16MB string should be accepted");
    assert_eq!(tokens.len(), 2); // StringLit + Eof
}

// ---------------------------------------------------------------------------
// TASK-23: Human-readable error messages
// ---------------------------------------------------------------------------

#[test]
fn parse_error_shows_human_readable_token_names() {
    // A query like `42` should produce an error mentioning "number" not "IntLit(42)".
    let err = powdb_query::parser::parse("42").expect_err("should fail to parse bare number");
    let msg = err.to_string();
    assert!(
        !msg.contains("IntLit"),
        "error should not contain raw Debug format 'IntLit': {msg}"
    );
    assert!(
        msg.contains("number"),
        "error should contain human-readable 'number': {msg}"
    );
}

#[test]
fn parse_error_for_string_in_type_position() {
    // `insert "hello" { }` — string where type name expected.
    let err = powdb_query::parser::parse(r#"insert "hello" { }"#)
        .expect_err("should fail to parse string as type name");
    let msg = err.to_string();
    assert!(
        !msg.contains("StringLit"),
        "error should not contain raw Debug format 'StringLit': {msg}"
    );
    assert!(
        msg.contains("string"),
        "error should contain human-readable 'string': {msg}"
    );
}

#[test]
fn display_name_covers_common_tokens() {
    use powdb_query::token::Token;

    assert_eq!(Token::IntLit(42).display_name(), "number 42");
    assert_eq!(Token::FloatLit(2.72).display_name(), "decimal number 2.72");
    assert_eq!(
        Token::StringLit("hi".into()).display_name(),
        "string \"hi\""
    );
    assert_eq!(
        Token::Ident("User".into()).display_name(),
        "identifier 'User'"
    );
    assert_eq!(Token::Filter.display_name(), "'filter'");
    assert_eq!(Token::Comma.display_name(), "','");
    assert_eq!(Token::Eof.display_name(), "end of input");
    assert_eq!(Token::LParen.display_name(), "'('");
    assert_eq!(Token::RParen.display_name(), "')'");
}

// ---------------------------------------------------------------------------
// v0.4.9: Remote-DoS regressions (product-review security findings)
// ---------------------------------------------------------------------------

use powdb_query::executor::Engine;

fn dos_temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_dos_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// `i64::MIN / -1` overflows and panics even in release builds; with
/// `panic = "abort"` that is a remotely-craftable server crash. The divisor is
/// derived from a column at runtime so the executor's eval path is exercised
/// (no constant folding can sidestep it). Must return cleanly, never panic.
#[test]
fn integer_division_overflow_does_not_crash() {
    let dir = dos_temp_dir("div_overflow");
    let mut engine = Engine::new(&dir).unwrap();
    engine.execute_powql("type T { required v: int }").unwrap();
    // -9223372036854775807 is i64::MIN + 1 (a valid literal). Saturating `- 1`
    // produces i64::MIN at runtime, and `i64::MIN / -1` hits the overflow case.
    engine
        .execute_powql("insert T { v := -9223372036854775807 }")
        .unwrap();
    let res = engine.execute_powql("T filter ((.v - 1) / -1) = 0 { .v }");
    assert!(
        res.is_ok(),
        "i64::MIN / -1 must not panic the engine: {res:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// An attacker-supplied huge `LIMIT` on a sort path used to pre-reserve a heap
/// of that capacity (~terabytes) before reading a single row, aborting the
/// process. The pre-allocation is now capped; the query must succeed and still
/// return the real rows.
#[test]
fn huge_limit_does_not_preallocate_and_crash() {
    let dir = dos_temp_dir("huge_limit");
    let mut engine = Engine::new(&dir).unwrap();
    engine.execute_powql("type T { required v: int }").unwrap();
    for i in 1..=3i64 {
        engine
            .execute_powql(&format!("insert T {{ v := {i} }}"))
            .unwrap();
    }
    let res = engine.execute_powql("T order .v desc limit 99999999999 { .v }");
    assert!(res.is_ok(), "huge LIMIT must not OOM/abort: {res:?}");
    if let Ok(powdb_query::result::QueryResult::Rows { rows, .. }) = res {
        assert_eq!(rows.len(), 3, "all rows should still be returned");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The data directory must be created owner-only (0700) so heap/WAL/index
/// files holding row data are not world/group readable under a default umask.
#[cfg(unix)]
#[test]
fn data_dir_created_with_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = dos_temp_dir("perms");
    let _engine = Engine::new(&dir).unwrap();
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "data dir must be 0700, got {mode:o}");
    let _ = std::fs::remove_dir_all(&dir);
}
