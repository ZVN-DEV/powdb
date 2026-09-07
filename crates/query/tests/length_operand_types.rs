//! `length()` answers for every operand that has a length, and refuses the ones
//! that do not, instead of silently returning null.
//!
//! `length(.b)` on a `bytes` column produced no value and no error on every
//! row. A `bytes` value has exactly one length -- the number of bytes -- so it
//! now answers with that, the way `length()` on a `str` answers with the number
//! of characters. A value with no length at all (an int, a bool, a datetime, a
//! uuid, a json document) is a typed refusal naming the column and its type,
//! the way a mistyped `like` operand already is.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql(
            "type T { required unique id: int, b: bytes, s: str, n: int, \
             f: float, t: bool, u: uuid, d: datetime, j: json }",
        )
        .unwrap();
    // `\xdeadbeef` is four bytes; "é" is one character in two UTF-8 bytes, so
    // the two units cannot be confused by accident.
    engine
        .execute_powql(
            r#"insert T { id := 1, b := "\\xdeadbeef", s := "abc", n := 5, f := 1.5,
               t := true, u := "550e8400-e29b-41d4-a716-446655440000",
               d := 1704067200000000, j := "{\"a\": 1}" }"#,
        )
        .unwrap();
    engine
        .execute_powql(
            r#"insert T { id := 2, b := "\\xc3a9", s := "é", n := 6, f := 2.5,
               t := false, u := "550e8400-e29b-41d4-a716-446655440001",
               d := 1704153600000000, j := "{\"a\": 2}" }"#,
        )
        .unwrap();
    (dir, engine)
}

fn ints(result: QueryResult, query: &str) -> Vec<i64> {
    match result {
        QueryResult::Scalar(Value::Int(n)) => vec![n],
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| match row[0] {
                Value::Int(n) => n,
                ref other => panic!("`{query}`: expected an int, got {other:?}"),
            })
            .collect(),
        other => panic!("`{query}`: expected ints, got {other:?}"),
    }
}

fn powql_ints(engine: &mut Engine, query: &str) -> Vec<i64> {
    let result = engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"));
    ints(result, query)
}

fn sql_ints(engine: &mut Engine, query: &str) -> Vec<i64> {
    let result = engine
        .execute_sql(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"));
    ints(result, query)
}

fn powql_err(engine: &mut Engine, query: &str) -> String {
    engine
        .execute_powql(query)
        .map(|ok| panic!("`{query}` should have been refused, got {ok:?}"))
        .unwrap_err()
        .to_string()
}

fn sql_err(engine: &mut Engine, query: &str) -> String {
    engine
        .execute_sql(query)
        .map(|ok| panic!("`{query}` should have been refused, got {ok:?}"))
        .unwrap_err()
        .to_string()
}

#[test]
fn length_of_a_bytes_column_is_the_byte_count() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        powql_ints(&mut engine, "T { n: length(.b) } order .id"),
        vec![4, 2]
    );
}

/// The str column holds the same two bytes the second bytes column does, and
/// they must NOT measure the same: a `str` is counted in characters, a `bytes`
/// in bytes.
#[test]
fn bytes_counts_bytes_where_str_counts_characters() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        powql_ints(&mut engine, "T filter .id = 2 { n: length(.s) }"),
        vec![1]
    );
    assert_eq!(
        powql_ints(&mut engine, "T filter .id = 2 { n: length(.b) }"),
        vec![2]
    );
}

#[test]
fn a_bytes_length_is_usable_in_a_filter() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        powql_ints(&mut engine, "count(T filter length(.b) = 4)"),
        vec![1]
    );
    assert_eq!(
        powql_ints(&mut engine, "count(T filter length(.b) > 1)"),
        vec![2]
    );
}

#[test]
fn the_sql_frontend_measures_bytes_the_same_way() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        sql_ints(&mut engine, "select length(b) from T order by id"),
        vec![4, 2]
    );
    assert_eq!(
        sql_ints(&mut engine, "select length(s) from T where id = 2"),
        vec![1]
    );
}

#[test]
fn length_of_a_column_with_no_length_is_refused() {
    let (_dir, mut engine) = engine();
    for (query, column, type_name) in [
        ("T { len: length(.n) }", "n", "int"),
        ("T { len: length(.f) }", "f", "float"),
        ("T { len: length(.t) }", "t", "bool"),
        ("T { len: length(.u) }", "u", "uuid"),
        ("T { len: length(.d) }", "d", "datetime"),
        ("T { len: length(.j) }", "j", "json"),
    ] {
        let message = powql_err(&mut engine, query);
        assert!(
            message.contains("length")
                && message.contains(&format!("'{column}'"))
                && message.contains(type_name),
            "`{query}` was refused with {message}"
        );
    }
}

#[test]
fn a_refused_length_reads_the_same_in_a_filter() {
    let (_dir, mut engine) = engine();
    let message = powql_err(&mut engine, "T filter length(.n) = 1 { .id }");
    assert!(
        message.contains("length") && message.contains("'n'") && message.contains("int"),
        "a filter must refuse what a projection refuses: {message}"
    );
}

#[test]
fn the_sql_frontend_refuses_the_same_column() {
    let (_dir, mut engine) = engine();
    let message = sql_err(&mut engine, "select length(n) from T");
    assert!(
        message.contains("length") && message.contains("'n'") && message.contains("int"),
        "SQL was refused with {message}"
    );
}

#[test]
fn a_literal_with_no_length_is_refused() {
    let (_dir, mut engine) = engine();
    for query in ["T { len: length(5) }", "T { len: length(true) }"] {
        let message = powql_err(&mut engine, query);
        assert!(
            message.contains("length"),
            "`{query}` was refused with {message}"
        );
    }
}

/// Everything `length()` already answered keeps answering, including the
/// character counting `substring` and `like _` agree with.
#[test]
fn a_str_length_is_unchanged() {
    let (_dir, mut engine) = engine();
    assert_eq!(
        powql_ints(&mut engine, "T { n: length(.s) } order .id"),
        vec![3, 1]
    );
    assert_eq!(
        powql_ints(
            &mut engine,
            "count(T filter length(substring(.s, 1, 2)) = 2)"
        ),
        vec![1]
    );
}
