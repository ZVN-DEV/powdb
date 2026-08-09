//! Double quotes delimit an *identifier* in SQL, not a string.
//!
//! The SQL lexer treated `'` and `"` identically and emitted a string token for
//! both, so `SELECT "name" FROM Author` returned the literal text `name` once
//! per row instead of the column, under a header of `?`, and `FROM "Author"`
//! failed outright with `expected table name`. Both are silent-or-broken on
//! input that every ORM emits as a matter of course (Prisma, Django,
//! SQLAlchemy and ActiveRecord all quote identifiers), so ported SQL was
//! wrong in one direction and rejected in the other.
//!
//! The rules pinned here:
//!   * `"x"` is the identifier x, everywhere an identifier is legal;
//!   * `'x'` is still the string x;
//!   * a quoted identifier is never a keyword, which is the entire reason
//!     delimited identifiers exist: `"limit"` is a column named limit;
//!   * the two are not interchangeable: comparing a column against `"literal"`
//!     is a column-to-column comparison, not a string match.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_DIR: AtomicU64 = AtomicU64::new(0);

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_sqlquote_{tag}_{}_{}",
        std::process::id(),
        UNIQUE_DIR.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn rows_sql(engine: &mut Engine, query: &str) -> Vec<String> {
    let result = engine
        .execute_sql(query)
        .unwrap_or_else(|e| panic!("failed to execute `{query}`: {e}"));
    match result {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| match value {
                        Value::Str(s) => s.clone(),
                        Value::Int(n) => n.to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("`{query}`: expected rows, got {other:?}"),
    }
}

fn authors(dir: &std::path::Path) -> Engine {
    let mut engine = Engine::new(dir).unwrap();
    engine
        .execute_powql("type Author { id: int, name: str }")
        .unwrap();
    for (id, name) in [(1, "alice"), (2, "bob")] {
        engine
            .execute_powql(&format!(
                "insert Author {{ id := {id}, name := \"{name}\" }}"
            ))
            .unwrap();
    }
    engine
}

#[test]
fn a_quoted_column_is_the_column_not_a_string() {
    let dir = fresh_dir("col");
    let mut engine = authors(&dir);

    // Before the fix this returned ["name", "name"], the literal text, once
    // per row, which is a silent wrong answer rather than an error.
    assert_eq!(
        rows_sql(&mut engine, r#"SELECT "name" FROM Author"#),
        vec!["alice", "bob"]
    );
    assert_eq!(
        rows_sql(&mut engine, "SELECT name FROM Author"),
        rows_sql(&mut engine, r#"SELECT "name" FROM Author"#),
    );
}

#[test]
fn a_quoted_table_is_the_table() {
    let dir = fresh_dir("table");
    let mut engine = authors(&dir);

    // Before the fix this was `expected table name, got <string>`.
    assert_eq!(
        rows_sql(&mut engine, r#"SELECT name FROM "Author""#),
        vec!["alice", "bob"]
    );
    assert_eq!(
        rows_sql(&mut engine, r#"SELECT "name" FROM "Author" WHERE "id" = 1"#),
        vec!["alice"]
    );
}

#[test]
fn single_quotes_are_still_string_literals() {
    let dir = fresh_dir("str");
    let mut engine = authors(&dir);

    assert_eq!(
        rows_sql(&mut engine, "SELECT name FROM Author WHERE name = 'alice'"),
        vec!["alice"]
    );
    // And the two spellings are not interchangeable: comparing the column
    // against a quoted *identifier* compares it to itself, matching every row.
    assert_eq!(
        rows_sql(
            &mut engine,
            r#"SELECT name FROM Author WHERE name = "name""#
        ),
        vec!["alice", "bob"]
    );
}

#[test]
fn a_quoted_identifier_is_never_a_keyword() {
    let dir = fresh_dir("reserved");
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type T { id: int, `limit`: int, `order`: str }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 1, `limit` := 5, `order` := \"x\" }")
        .unwrap();

    // This is the whole reason delimited identifiers exist, and it was
    // previously impossible to express: PowQL reserves ~93 lowercase words, and
    // with `"limit"` lexing as a string there was no escape hatch at all.
    assert_eq!(
        rows_sql(&mut engine, r#"SELECT "limit", "order" FROM T"#),
        vec!["5|x"]
    );
}

#[test]
fn a_quoted_identifier_that_cannot_be_expressed_is_rejected_loudly() {
    let dir = fresh_dir("reject");
    let mut engine = authors(&dir);

    // PowQL quotes identifiers with backticks and has no escape for one, so an
    // identifier containing a backtick must be refused rather than silently
    // mangled into a different name.
    let err = engine
        .execute_sql(r#"SELECT "na`me" FROM Author"#)
        .expect_err("a backtick in a quoted identifier must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("backtick"),
        "error should explain the backtick restriction, got: {msg}"
    );

    let err = engine
        .execute_sql(r#"SELECT "" FROM Author"#)
        .expect_err("an empty quoted identifier must be rejected");
    assert!(
        err.to_string().contains("empty quoted identifier"),
        "got: {err}"
    );
}
