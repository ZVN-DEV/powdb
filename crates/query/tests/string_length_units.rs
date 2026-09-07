//! Q22: `length()` counts characters, the same unit `substring` and `like _`
//! already count.
//!
//! It returned `str::len()`, the UTF-8 byte count, so `length("Zoë")` was 4 and
//! `length("emoji")` was 4 for a single emoji, while `substring(s, 1, 3)`
//! returned all three characters of the same string and `like "___"` matched
//! it. Three string operations, two units, no error anywhere.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

const MULTI_BYTE: [&str; 4] = ["abc", "Zoë", "日本語", "😀😀😀"];

fn seeded() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, s: str }")
        .unwrap();
    for (id, s) in MULTI_BYTE.iter().enumerate() {
        engine
            .execute_powql(&format!("insert T {{ id := {id}, s := \"{s}\" }}"))
            .unwrap();
    }
    (dir, engine)
}

fn ints(engine: &mut Engine, query: &str) -> Vec<i64> {
    match engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
    {
        QueryResult::Scalar(Value::Int(n)) => vec![n],
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| match row[0] {
                Value::Int(n) => n,
                ref v => panic!("expected int, got {v:?}"),
            })
            .collect(),
        other => panic!("expected ints, got {other:?}"),
    }
}

#[test]
fn length_counts_characters_not_utf8_bytes() {
    let (_dir, mut engine) = seeded();
    assert_eq!(
        ints(&mut engine, "T { n: length(.s) } order .id"),
        vec![3, 3, 3, 3]
    );
}

#[test]
fn length_agrees_with_the_underscore_wildcard() {
    let (_dir, mut engine) = seeded();
    let by_length = ints(&mut engine, "count(T filter length(.s) = 3)");
    let by_pattern = ints(&mut engine, "count(T filter .s like \"___\")");
    assert_eq!(by_length, by_pattern);
    assert_eq!(by_length, vec![4]);
}

/// `substring` already counts characters, so taking two of them has to measure
/// two. Under the byte count it measured 2, 3 or 6 depending on the encoding.
#[test]
fn length_measures_what_substring_took() {
    let (_dir, mut engine) = seeded();
    assert_eq!(
        ints(
            &mut engine,
            "count(T filter length(substring(.s, 1, 2)) = 2)"
        ),
        vec![4]
    );
}

#[test]
fn length_of_an_ascii_string_is_unchanged() {
    let (_dir, mut engine) = seeded();
    assert_eq!(ints(&mut engine, "count(T filter length(.s) = 3)"), vec![4]);
    assert_eq!(ints(&mut engine, "count(T filter length(.s) = 6)"), vec![0]);
}
