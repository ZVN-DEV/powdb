//! A materialized view's backing table gets a static schema, but a PowQL
//! projection is typed per ROW: `.tags ?? 0` is json in a row where `tags`
//! is set and int in a row where it is not. The original defect derived the
//! backing column types from the FIRST row only, so a later row of another
//! type reached the storage encoder with a value whose class contradicts the
//! column:
//!
//!   * variable column (str/json/bytes) fed a fixed value: `unreachable!` in
//!     `encode_row_into_with_layout`: an abort under the shipped
//!     panic=abort release profile, reachable from one query. Found by
//!     `fuzz_execute` in CI.
//!   * fixed 8-byte column fed the other fixed 8-byte type: the bytes are
//!     written as one type and decoded as the other, so `7` reads back as
//!     3.5e-323, a silent wrong answer, which is worse.
//!
//! The contract pinned here: `materialize` derives each backing column's
//! type from ALL rows (null never constrains it), and a projection that
//! genuinely mixes types in one column is a clean typed error, never a
//! panic and never bit-reinterpreted garbage. A `refresh` whose fresh rows
//! no longer fit the backing schema is equally a clean error, before any
//! old data is destroyed.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_matview_types_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("failed to execute `{query}`: {e}"))
}

fn rows_of(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The CI-found shape: json in the first row, int in a later one. The value
/// classes differ (variable vs fixed), which is the arm that panicked.
#[test]
fn a_projection_mixing_json_and_int_is_a_clean_error() {
    let dir = temp_dir("json_int");
    let mut engine = Engine::new(&dir).unwrap();
    exec(
        &mut engine,
        "type T { required unique id: int, tags: json }",
    );
    exec(&mut engine, "insert T { id := 1, tags := \"[1,2]\" }");
    exec(&mut engine, "insert T { id := 2 }");

    let err = engine
        .execute_powql("materialize V as T { x: .tags ?? 0 }")
        .expect_err("mixed json/int projection must be a typed error, not a panic");
    let msg = err.to_string();
    assert!(
        msg.contains("mix") && msg.contains('x'),
        "error should name the mixed column: {msg}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Float first, int later: both are fixed 8-byte columns, so nothing
/// panics: the int's bits were decoded as a float, a silent wrong answer.
#[test]
fn a_projection_mixing_float_and_int_is_a_clean_error_not_reinterpreted_bits() {
    let dir = temp_dir("float_int");
    let mut engine = Engine::new(&dir).unwrap();
    exec(
        &mut engine,
        "type T { required unique id: int, score: float }",
    );
    exec(&mut engine, "insert T { id := 1, score := 1.5 }");
    exec(&mut engine, "insert T { id := 2 }");

    engine
        .execute_powql("materialize V as T { x: .score ?? 7 }")
        .expect_err("mixed float/int projection must be a typed error, not stored garbage");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same pair in the other order: int first, float later.
#[test]
fn a_projection_mixing_int_and_float_is_a_clean_error_in_either_order() {
    let dir = temp_dir("int_float");
    let mut engine = Engine::new(&dir).unwrap();
    exec(
        &mut engine,
        "type T { required unique id: int, score: float }",
    );
    exec(&mut engine, "insert T { id := 1 }");
    exec(&mut engine, "insert T { id := 2, score := 1.5 }");

    engine
        .execute_powql("materialize V as T { x: .score ?? 7 }")
        .expect_err("mixed int/float projection must be a typed error, not stored garbage");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A null in the first row must not decide the column type: the type comes
/// from the rows that HAVE a value.
#[test]
fn a_leading_null_does_not_type_the_backing_column() {
    let dir = temp_dir("leading_null");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type T { required unique id: int, name: str }");
    exec(&mut engine, "insert T { id := 1 }");
    exec(&mut engine, "insert T { id := 2, name := \"bob\" }");

    exec(&mut engine, "materialize V as T { x: .name }");
    let mut rows = rows_of(exec(&mut engine, "V"));
    rows.sort_by_key(|r| matches!(r[0], Value::Empty));
    assert_eq!(rows.len(), 2);
    assert!(matches!(&rows[0][0], Value::Str(s) if s == "bob"));
    assert!(matches!(rows[1][0], Value::Empty));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A view whose result set is empty (or all-null in a column) still needs a
/// storable schema, and must accept a later refresh of the same type.
#[test]
fn an_all_null_column_still_materializes_and_reads_back() {
    let dir = temp_dir("all_null");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type T { required unique id: int, name: str }");
    exec(&mut engine, "insert T { id := 1 }");

    exec(&mut engine, "materialize V as T { x: .name }");
    let rows = rows_of(exec(&mut engine, "V"));
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Empty));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A refresh whose fresh rows no longer fit the backing schema fails as a
/// typed error BEFORE the old contents are destroyed, and so does a read
/// through the dirty view, rather than serving reinterpreted bytes.
#[test]
fn a_refresh_that_changes_a_column_type_is_a_clean_error() {
    let dir = temp_dir("refresh_flip");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type T { required unique id: int, a: int }");
    exec(&mut engine, "insert T { id := 1, a := 10 }");
    exec(&mut engine, "materialize V as T { x: .a ?? \"none\" }");
    assert_eq!(rows_of(exec(&mut engine, "V")), vec![vec![Value::Int(10)]]);

    // A new row with `a` unset makes the projection yield a string, which
    // the int backing column cannot store.
    exec(&mut engine, "insert T { id := 2 }");
    engine
        .execute_powql("refresh V")
        .expect_err("refresh into a mismatched backing schema must be a typed error");
    engine
        .execute_powql("V")
        .expect_err("a dirty view that cannot refresh must error, not serve garbage");
    let _ = std::fs::remove_dir_all(&dir);
}
