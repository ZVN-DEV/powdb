//! Pins the two behaviors `docs/SQL.md` documents but nothing else guarded.
//!
//! `docs/SQL.md` is the page the repo-root README points ORM authors at, and it
//! claimed NULL comparison "matches the PowQL frontend exactly, since both
//! lower to the same predicates". That has been false since 0.23.0, which
//! deliberately made SQL's `= NULL` match nothing while PowQL kept `= null`
//! meaning `is null`. The engine is right in both languages; the sentence was
//! wrong. Neither that divergence nor 0.23.0's other ORM-facing change (double
//! quotes delimit an identifier) appeared in the SQL reference at all.
//!
//! Every assertion here is an example printed in `docs/SQL.md`, so the doc
//! cannot rot without a red test.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

/// `Author { id, name, age }` where bob has no age.
fn authors(dir: &std::path::Path) -> Engine {
    let mut engine = Engine::new(dir).unwrap();
    engine
        .execute_powql("type Author { required id: int, name: str, age: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert Author { id := 1, name := "alice", age := 30 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Author { id := 2, name := "bob" }"#)
        .unwrap();
    engine
}

#[track_caller]
fn names_sql(engine: &mut Engine, sql: &str) -> Vec<String> {
    names(
        engine
            .execute_sql(sql)
            .unwrap_or_else(|e| panic!("SQL `{sql}` must work, got: {e}")),
        sql,
    )
}

#[track_caller]
fn names_powql(engine: &mut Engine, powql: &str) -> Vec<String> {
    names(
        engine
            .execute_powql(powql)
            .unwrap_or_else(|e| panic!("PowQL `{powql}` must work, got: {e}")),
        powql,
    )
}

#[track_caller]
fn names(result: QueryResult, query: &str) -> Vec<String> {
    match result {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| match &row[0] {
                Value::Str(s) => s.clone(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("`{query}`: expected rows, got {other:?}"),
    }
}

/// The divergence `docs/SQL.md` used to deny. SQL follows SQL, PowQL follows
/// PowQL, and the two return different rows for the same source text.
#[test]
fn eq_null_matches_nothing_in_sql_and_means_is_null_in_powql() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = authors(dir.path());

    assert!(names_sql(&mut engine, "SELECT name FROM Author WHERE age = NULL").is_empty());
    assert!(names_sql(&mut engine, "SELECT name FROM Author WHERE age <> NULL").is_empty());
    assert!(names_sql(&mut engine, "SELECT name FROM Author WHERE age != NULL").is_empty());

    // The SQL way to ask the question the PowQL spelling answers.
    assert_eq!(
        names_sql(&mut engine, "SELECT name FROM Author WHERE age IS NULL"),
        vec!["bob"]
    );
    assert_eq!(
        names_powql(&mut engine, "Author filter .age = null { .name }"),
        vec!["bob"]
    );
    assert_eq!(
        names_powql(&mut engine, "Author filter .age != null { .name }"),
        vec!["alice"]
    );
}

/// The two-valued corner: SQL's `= NULL` is a constant-false predicate, so
/// negating it is a constant-true one. Standard three-valued SQL would return
/// no rows here. This is the already-documented 2VL divergence applied to the
/// new lowering, not a second one.
#[test]
fn not_of_eq_null_returns_every_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = authors(dir.path());

    assert_eq!(
        names_sql(
            &mut engine,
            "SELECT name FROM Author WHERE NOT (age = NULL)"
        ),
        vec!["alice", "bob"]
    );
}

/// Everything else in a filter still lowers to the same predicate in both
/// languages, which is what makes the `= NULL` case a *single* exception
/// rather than a general drift.
#[test]
fn every_other_comparison_agrees_across_the_two_frontends() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = authors(dir.path());

    for (sql, powql) in [
        (
            "SELECT name FROM Author WHERE age > 20",
            "Author filter .age > 20 { .name }",
        ),
        (
            "SELECT name FROM Author WHERE age != 30",
            "Author filter .age != 30 { .name }",
        ),
        (
            "SELECT name FROM Author WHERE age IS NOT NULL",
            "Author filter .age != null { .name }",
        ),
        (
            "SELECT name FROM Author WHERE name LIKE 'a%'",
            r#"Author filter .name like "a%" { .name }"#,
        ),
    ] {
        assert_eq!(
            names_sql(&mut engine, sql),
            names_powql(&mut engine, powql),
            "`{sql}` and `{powql}` must agree"
        );
    }
}

/// `docs/SQL.md` tells a reader who reached for `CAST(x AS INT)` to write
/// `cast(x, 'int')` instead, and enumerates the type strings. All of them have
/// to be real, or the doc trades one dead end for another.
#[test]
fn every_documented_cast_type_string_is_accepted_in_sql() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = authors(dir.path());

    for ty in ["int", "float", "str", "bool", "datetime", "uuid", "bytes"] {
        let sql = format!("SELECT cast(id, '{ty}') AS c FROM Author WHERE id = 1");
        assert!(
            engine.execute_sql(&sql).is_ok(),
            "`{sql}` must parse: docs/SQL.md lists `{ty}` as a valid cast type"
        );
    }
    // And an invented type is still rejected, so the list is a real list.
    assert!(engine
        .execute_sql("SELECT cast(id, 'int8') FROM Author")
        .is_err());
}

/// The two-valued `NOT` claim, including the `NOT (x = NULL)` corner that
/// `docs/SQL.md` now warns about explicitly.
#[test]
fn not_is_two_valued_so_it_admits_null_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = authors(dir.path());

    // bob has no age, and `age > 30` is false for him, so `NOT` admits him.
    // Standard three-valued SQL would exclude him.
    assert_eq!(
        names_sql(
            &mut engine,
            "SELECT name FROM Author WHERE NOT (age > 30) ORDER BY id"
        ),
        vec!["alice", "bob"]
    );
    // The documented guard.
    assert_eq!(
        names_sql(
            &mut engine,
            "SELECT name FROM Author WHERE age IS NOT NULL AND NOT (age > 30)"
        ),
        vec!["alice"]
    );
}

/// Double quotes delimit an identifier, single quotes a string. These are the
/// worked examples in `docs/SQL.md`.
#[test]
fn double_quotes_are_identifiers_and_single_quotes_are_strings() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = authors(dir.path());

    assert_eq!(
        names_sql(&mut engine, r#"SELECT "name" FROM "Author" ORDER BY "id""#),
        vec!["alice", "bob"]
    );
    assert_eq!(
        names_sql(&mut engine, "SELECT name FROM Author WHERE name = 'alice'"),
        vec!["alice"]
    );
    // A quoted identifier on the right of a comparison is the *column*, so this
    // compares the column to itself and matches every row.
    assert_eq!(
        names_sql(
            &mut engine,
            r#"SELECT name FROM Author WHERE name = "name" ORDER BY id"#
        ),
        vec!["alice", "bob"]
    );
}

/// A quoted identifier is never a keyword, which is the whole point of
/// delimited identifiers and the only way to reach a column named after one of
/// PowQL's reserved words.
#[test]
fn a_quoted_identifier_is_never_a_keyword() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required id: int, `limit`: int, `order`: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert T { id := 1, `limit` := 5, `order` := "x" }"#)
        .unwrap();

    match engine
        .execute_sql(r#"SELECT "limit", "order" FROM T"#)
        .unwrap()
    {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["limit", "order"]);
            assert_eq!(rows, vec![vec![Value::Int(5), Value::Str("x".into())]]);
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// A quoted identifier reaches a table alias and a qualified reference too,
/// so `docs/SQL.md` can say "anywhere an identifier is legal" without a
/// carve-out.
#[test]
fn quoted_identifiers_work_in_alias_and_qualified_positions() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = authors(dir.path());

    assert_eq!(
        names_sql(
            &mut engine,
            r#"SELECT a."name" FROM Author AS a WHERE a."age" > 20"#
        ),
        vec!["alice"]
    );
    assert_eq!(
        names_sql(
            &mut engine,
            r#"SELECT "a"."name" FROM Author AS "a" WHERE "a"."age" > 20"#
        ),
        vec!["alice"]
    );
}
