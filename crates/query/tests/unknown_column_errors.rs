//! Silent-wrong-answer regressions: a reference to a column that does not
//! exist, a comparison between incompatible types, and a negative `limit` all
//! used to be accepted and answered with plausible-looking rows.
//!
//! - `User filter .agee > 25` returned `(empty set)`.
//! - `User { .agee }` returned a column of NULLs.
//! - `count(User filter .agee = null)` returned EVERY row, so
//!   `User filter .agee = null delete` would have emptied the table.
//! - `User filter .name > 25` on a `str` column returned ALL rows.
//! - `User limit -1` returned all rows.
//!
//! `group`, `order`, and `insert` already rejected unknown columns with
//! `column '<name>' not found`; these tests hold `filter`, projections, and
//! the comparison/limit paths to the same standard.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;

fn fixture() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type User { required name: str, age: int }")
        .unwrap();
    for (name, age) in [("alice", 30), ("bob", 20)] {
        engine
            .execute_powql(&format!(
                "insert User {{ name := \"{name}\", age := {age} }}"
            ))
            .unwrap();
    }
    // A row with a genuinely NULL `age`: real NULL semantics must be untouched.
    engine
        .execute_powql("insert User { name := \"carol\" }")
        .unwrap();
    (dir, engine)
}

fn assert_column_not_found(engine: &mut Engine, query: &str, column: &str) {
    let err = match engine.execute_powql(query) {
        Err(err) => err,
        Ok(ok) => panic!("`{query}` must be rejected, got {ok:?}"),
    };
    let message = err.to_string();
    assert!(
        message.contains(&format!("column '{column}' not found")),
        "`{query}`: expected \"column '{column}' not found\", got: {message}"
    );
}

#[test]
fn unknown_column_in_filter_errors() {
    let (_dir, mut engine) = fixture();
    assert_column_not_found(&mut engine, "User filter .agee > 25", "agee");
    assert_column_not_found(&mut engine, "User filter .agee = 25", "agee");
    assert_column_not_found(&mut engine, "User filter .agee = null", "agee");
    assert_column_not_found(&mut engine, "User filter .age > 25 and .agee > 25", "agee");
}

#[test]
fn unknown_column_in_projection_errors() {
    let (_dir, mut engine) = fixture();
    assert_column_not_found(&mut engine, "User { .agee }", "agee");
    assert_column_not_found(&mut engine, "User { .name, x: .agee }", "agee");
    assert_column_not_found(&mut engine, "User { .agee + 1 }", "agee");
}

/// The worst shape: an unknown column in a `delete` predicate silently matched
/// every row.
#[test]
fn unknown_column_in_mutation_predicate_errors() {
    let (_dir, mut engine) = fixture();
    assert_column_not_found(&mut engine, "User filter .agee = null delete", "agee");
    assert_column_not_found(
        &mut engine,
        "User filter .agee = null update { name := \"x\" }",
        "agee",
    );
    match engine.execute_powql("count(User)").unwrap() {
        QueryResult::Scalar(value) => {
            assert_eq!(
                value,
                powdb_storage::types::Value::Int(3),
                "no row may be lost"
            )
        }
        other => panic!("expected a scalar, got {other:?}"),
    }
}

#[test]
fn unknown_column_in_aggregate_errors() {
    let (_dir, mut engine) = fixture();
    assert_column_not_found(&mut engine, "count(User filter .agee = null)", "agee");
    assert_column_not_found(&mut engine, "sum(User { .agee })", "agee");
}

/// The pre-existing validators must keep their exact message.
#[test]
fn group_and_order_still_report_the_same_message() {
    let (_dir, mut engine) = fixture();
    assert_column_not_found(&mut engine, "User group .bogus { .bogus }", "bogus");
    assert_column_not_found(&mut engine, "User order .bogus", "bogus");
}

/// Known columns must keep working, including a genuinely NULL value: only
/// names absent from the schema are errors.
#[test]
fn known_columns_and_real_nulls_still_work() {
    let (_dir, mut engine) = fixture();
    match engine
        .execute_powql("count(User filter .age = null)")
        .unwrap()
    {
        QueryResult::Scalar(value) => assert_eq!(
            value,
            powdb_storage::types::Value::Int(1),
            "carol's NULL age must still match `= null`"
        ),
        other => panic!("expected a scalar, got {other:?}"),
    }
    match engine
        .execute_powql("User filter .age > 25 { .name }")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("expected rows, got {other:?}"),
    }
    engine
        .execute_powql("User { .name, .age }")
        .expect("projecting real columns must work");
}

/// The SQL frontend lowers to the same plans, so it inherits the same checks.
#[test]
fn sql_frontend_reports_unknown_columns() {
    let (_dir, mut engine) = fixture();
    for query in [
        "SELECT agee FROM User",
        "SELECT name FROM User WHERE agee > 25",
        "DELETE FROM User WHERE agee IS NULL",
    ] {
        let err = match engine.execute_sql(query) {
            Err(err) => err,
            Ok(ok) => panic!("`{query}` must be rejected, got {ok:?}"),
        };
        assert!(
            err.to_string().contains("column 'agee' not found"),
            "`{query}`: got {err}"
        );
    }
}

/// `{ alias: expr } order alias` plans the sort BELOW the projection, so the
/// alias does not exist yet at sort time. That used to evaluate to NULL for
/// every row and silently return unsorted output; it is now a clean error.
/// Sorting by an alias produced by grouping (where the sort sits above the
/// projection) is unaffected.
#[test]
fn ordering_by_a_not_yet_computed_alias_errors() {
    let (_dir, mut engine) = fixture();
    assert_column_not_found(
        &mut engine,
        "User { .name, doubled: .age * 2 } order doubled",
        "doubled",
    );
    engine
        .execute_powql("User group .age { .age, n: count(*) } order n desc")
        .expect("ordering by a grouped alias must still work");
}

// ---------------------------------------------------------------------------
// Type-mismatched comparisons
// ---------------------------------------------------------------------------

fn assert_type_mismatch(engine: &mut Engine, query: &str, column: &str) {
    let err = match engine.execute_powql(query) {
        Err(err) => err,
        Ok(ok) => panic!("`{query}` must be rejected, got {ok:?}"),
    };
    let message = err.to_string();
    assert!(
        message.contains("type mismatch") && message.contains(column),
        "`{query}`: expected a type-mismatch error naming '{column}', got: {message}"
    );
}

/// `User filter .name > 25` on a `str` column used to return ALL rows.
#[test]
fn type_mismatched_comparison_errors() {
    let (_dir, mut engine) = fixture();
    assert_type_mismatch(&mut engine, "User filter .name > 25", "name");
    assert_type_mismatch(&mut engine, "User filter .name = 25", "name");
    assert_type_mismatch(&mut engine, r#"User filter .age > "abc""#, "age");
    assert_type_mismatch(&mut engine, r#"User filter .age = "30""#, "age");
}

#[test]
fn compatible_comparisons_still_work() {
    let (_dir, mut engine) = fixture();
    for query in [
        r#"User filter .name = "alice""#,
        r#"User filter .name > "a""#,
        "User filter .age > 25",
        "User filter .age > 25.5",
        "User filter .age = null",
        "User filter .name is not null",
        "User filter .age > .age",
    ] {
        engine
            .execute_powql(query)
            .unwrap_or_else(|err| panic!("`{query}` must still be accepted, got: {err}"));
    }
}

// ---------------------------------------------------------------------------
// Negative limit / offset
// ---------------------------------------------------------------------------

#[test]
fn negative_limit_errors() {
    let (_dir, mut engine) = fixture();
    for query in ["User limit -1", "User order .age limit -5"] {
        let err = match engine.execute_powql(query) {
            Err(err) => err,
            Ok(ok) => panic!("`{query}` must be rejected, got {ok:?}"),
        };
        assert!(
            err.to_string().contains("limit"),
            "`{query}`: expected a limit error, got: {err}"
        );
    }
}

#[test]
fn zero_and_positive_limits_still_work() {
    let (_dir, mut engine) = fixture();
    match engine.execute_powql("User limit 0 { .name }").unwrap() {
        QueryResult::Rows { rows, .. } => assert!(rows.is_empty()),
        other => panic!("expected rows, got {other:?}"),
    }
    match engine.execute_powql("User limit 2 { .name }").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 2),
        other => panic!("expected rows, got {other:?}"),
    }
}
