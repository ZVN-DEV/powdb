//! `sum` over zero contributing rows is Empty (SQL NULL), matching `avg`.
//!
//! Until v0.27 an empty `sum` returned `Int(0)` on the generic and compiled
//! int paths and `Float(0.0)` on the compiled float path: three answers for
//! one question, every one diverging from SQL's NULL, recorded in the oracle
//! ledger as `sum-of-no-numeric-rows-is-zero-not-null`. "No rows" and "a
//! total of zero" are different facts; only `Empty` distinguishes them.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_sum_empty_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn engine(name: &str) -> Engine {
    let mut engine = Engine::new(&temp_dir(name)).unwrap();
    engine
        .execute_powql("type Account { balance: int, rate: float, note: str }")
        .unwrap();
    engine
}

fn scalar(result: QueryResult) -> Value {
    match result {
        QueryResult::Scalar(value) => value,
        other => panic!("expected scalar, got {other:?}"),
    }
}

#[test]
fn sum_over_an_empty_table_is_empty_not_zero() {
    let mut engine = engine("table_int");
    // Whole-table stored-column sum: the compiled int fast path.
    let value = scalar(engine.execute_powql("sum(Account { .balance })").unwrap());
    assert_eq!(value, Value::Empty, "int sum over no rows");
}

#[test]
fn sum_with_a_filter_matching_nothing_is_empty() {
    let mut engine = engine("filtered_int");
    engine
        .execute_powql("insert Account { balance := 7, rate := 1.5, note := \"x\" }")
        .unwrap();
    // Compiled predicate + compiled int agg loop, zero surviving rows.
    let value = scalar(
        engine
            .execute_powql("sum(Account filter .balance > 100 { .balance })")
            .unwrap(),
    );
    assert_eq!(value, Value::Empty, "filtered-to-nothing int sum");
}

#[test]
fn float_sum_over_no_rows_is_empty_on_every_path() {
    let mut engine = engine("float");
    // Compiled float fast path (whole-table) — returned Float(0.0) before.
    let value = scalar(engine.execute_powql("sum(Account { .rate })").unwrap());
    assert_eq!(value, Value::Empty, "float sum over no rows");
}

#[test]
fn sum_over_only_null_values_is_empty() {
    let mut engine = engine("all_null");
    // Omitted columns store Empty (PowQL's null).
    engine
        .execute_powql("insert Account { note := \"x\" }")
        .unwrap();
    let value = scalar(engine.execute_powql("sum(Account { .balance })").unwrap());
    assert_eq!(value, Value::Empty, "sum where every input is Empty");
}

#[test]
fn empty_sum_agrees_with_empty_avg() {
    let mut engine = engine("parity");
    let sum = scalar(engine.execute_powql("sum(Account { .balance })").unwrap());
    let avg = scalar(engine.execute_powql("avg(Account { .balance })").unwrap());
    assert_eq!(sum, avg, "sum and avg must give one answer for no rows");
}

#[test]
fn grouped_expression_sum_over_only_nulls_is_empty() {
    let mut engine = engine("grouped_expr");
    // The group exists (one row), but its only balance is Empty, and
    // Empty + 1 stays Empty — so the group's sum folds zero numeric
    // values through the generic NumericFold path (no compiled loop).
    engine
        .execute_powql("insert Account { note := \"x\" }")
        .unwrap();
    let result = engine
        .execute_powql("Account group .note { total: sum(.balance + 1) }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0][0],
                Value::Empty,
                "generic-path sum over no numeric input"
            );
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn sum_over_no_rows_without_the_compiled_layout_is_empty() {
    let mut engine = Engine::new(&temp_dir("no_fast_layout")).unwrap();
    // A variable-length column ahead of the int denies the compiled
    // fixed-offset aggregate loop, forcing the generic path.
    engine
        .execute_powql("type Ledger { tag: str, amount: int }")
        .unwrap();
    let value = scalar(engine.execute_powql("sum(Ledger { .amount })").unwrap());
    assert_eq!(value, Value::Empty, "generic whole-table sum over no rows");
}

#[test]
fn sql_sum_over_an_empty_table_is_null() {
    let mut engine = engine("sql");
    let result = engine
        .execute_sql("SELECT SUM(balance) FROM Account")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Empty, "SQL SUM over no rows is NULL");
        }
        QueryResult::Scalar(value) => assert_eq!(value, Value::Empty),
        other => panic!("unexpected result: {other:?}"),
    }
}
