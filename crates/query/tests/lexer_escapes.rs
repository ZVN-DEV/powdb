//! Q13: an unknown backslash escape in a string literal is a parse error, and
//! the escape set covers `\r`, `\0` and `\uXXXX`.
//!
//! The lexer dropped the backslash of anything it did not recognise, so
//! `"a\u0041b"` was stored as `au0041b`, `"\x41"` as `x41` and `"\0"` as `0`.
//! User data was silently rewritten on the way in, with nothing in the result
//! to show it had happened, and a bytes literal had to be written with a
//! doubled backslash for a reason no message ever gave.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, s: str }")
        .unwrap();
    (dir, engine)
}

/// The single stored string of a one-row table.
fn stored(engine: &mut Engine, literal: &str) -> String {
    engine
        .execute_powql(&format!("insert T {{ id := 1, s := \"{literal}\" }}"))
        .unwrap_or_else(|e| panic!("insert of \"{literal}\": {e}"));
    let result = engine.execute_powql("T { .s }").unwrap();
    match result {
        QueryResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Str(s) => s.clone(),
            other => panic!("expected str, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn a_unicode_escape_produces_its_character() {
    let (_dir, mut engine) = engine();
    assert_eq!(stored(&mut engine, r"a\u0041b"), "aAb");
}

#[test]
fn carriage_return_and_nul_have_escapes() {
    let (_dir, mut engine) = engine();
    assert_eq!(stored(&mut engine, r"a\rb\0c"), "a\rb\0c");
}

#[test]
fn the_documented_escapes_are_unchanged() {
    let (_dir, mut engine) = engine();
    assert_eq!(stored(&mut engine, r#"a\"b\\c\nd\te"#), "a\"b\\c\nd\te");
}

#[test]
fn an_unknown_escape_is_a_parse_error_naming_it() {
    let (_dir, mut engine) = engine();
    let message = engine
        .execute_powql(r#"insert T { id := 1, s := "\x41" }"#)
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("unknown escape") && message.contains("\\x"),
        "expected an error naming the escape, got {message}"
    );
}

#[test]
fn a_truncated_unicode_escape_is_a_parse_error() {
    let (_dir, mut engine) = engine();
    let message = engine
        .execute_powql(r#"insert T { id := 1, s := "\u00" }"#)
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("\\u"),
        "expected an error about the \\u escape, got {message}"
    );
}

#[test]
fn a_lone_surrogate_is_a_parse_error() {
    let (_dir, mut engine) = engine();
    let message = engine
        .execute_powql(r#"insert T { id := 1, s := "\ud800" }"#)
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("\\u"),
        "expected an error about the \\u escape, got {message}"
    );
}

/// A bytes literal is written with a doubled backslash, which is now the only
/// way to write one: the single-backslash form names an unknown escape.
#[test]
fn a_bytes_literal_still_takes_the_doubled_backslash_form() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type B { required unique id: int, b: bytes }")
        .unwrap();
    engine
        .execute_powql(r#"insert B { id := 1, b := "\\x0a0b" }"#)
        .unwrap();
    match engine.execute_powql("B { .b }").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Bytes(vec![0x0a, 0x0b]))
        }
        other => panic!("expected rows, got {other:?}"),
    }
}
