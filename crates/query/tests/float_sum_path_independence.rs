//! A float `sum` answers the same number whichever way the rows are reached.
//!
//! Left-to-right `f64` accumulation makes a total depend on the order the rows
//! arrive in, and the access path is what fixes that order: a sequential scan
//! visits insertion order, a range scan visits index order. On a column that
//! mixes values near 2^53 with small ones, every small value added while the
//! running total sits at 2^53 is rounded away, so the same rows summed two
//! ways gave two different answers and neither was the right one.
//!
//! The fixture below is built so the exact total is representable: 100 rows
//! of +/- 2^53 that cancel, and 100 small values that are multiples of 1/8.
//! Insertion order interleaves them (a running sum in that order keeps hitting
//! 2^53), index order puts the big rows first (they cancel to zero, then the
//! small values add exactly), so a naive accumulator cannot give both paths
//! the same answer.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

/// 2^53, the first magnitude at which `f64` stops representing every integer.
const BIG: f64 = 9_007_199_254_740_992.0;

/// The 100 small values are `k/8` for k in 1..=100, so their total is exact.
const EXACT_TOTAL: f64 = 631.25;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_float_sum_{name}_{}_{}",
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

fn scalar_float(result: QueryResult, query: &str) -> f64 {
    match result {
        QueryResult::Scalar(Value::Float(f)) => f,
        QueryResult::Rows { rows, .. } if rows.len() == 1 && rows[0].len() == 1 => {
            match rows[0][0] {
                Value::Float(f) => f,
                ref other => panic!("`{query}` answered {other:?}, wanted a float"),
            }
        }
        other => panic!("`{query}` answered {other:?}, wanted a float"),
    }
}

fn float_of(engine: &mut Engine, query: &str) -> f64 {
    let result = exec(engine, query);
    scalar_float(result, query)
}

fn readonly_float(engine: &Engine, query: &str) -> f64 {
    let result = engine
        .execute_powql_readonly(query)
        .unwrap_or_else(|e| panic!("failed to execute `{query}` read-only: {e}"));
    scalar_float(result, query)
}

/// ids 0..99 carry the +/- 2^53 rows, ids 100..199 carry the small ones, and
/// the two halves are inserted interleaved so heap order and id order differ.
fn load(engine: &mut Engine) {
    exec(engine, "type F { required id: int, v: float }");
    for i in 0..100i64 {
        let big = if i % 2 == 0 { BIG } else { -BIG };
        exec(
            engine,
            &format!("insert F {{ id := {i}, v := {big:?} }}", big = big),
        );
        let small = f64::from((i + 1) as i32) / 8.0;
        exec(
            engine,
            &format!("insert F {{ id := {}, v := {small:?} }}", 100 + i),
        );
    }
    exec(engine, "alter F add index .id");
}

#[test]
fn a_float_sum_is_the_same_number_on_every_access_path() {
    let dir = temp_dir("sum");
    std::fs::create_dir_all(&dir).unwrap();
    let mut engine = Engine::new(&dir).unwrap();
    load(&mut engine);

    let scan = float_of(&mut engine, "sum(F { .v })");
    let indexed = float_of(&mut engine, "sum(F filter .id >= 0 { .v })");
    let read_only = readonly_float(&engine, "sum(F filter .id >= 0 { .v })");
    let grouped = {
        let query = "F group .id >= 0 { total: sum(.v) }";
        match exec(&mut engine, query) {
            QueryResult::Rows { rows, columns } => {
                assert_eq!(rows.len(), 1, "one group expected, got {rows:?}");
                let idx = columns.iter().position(|c| c == "total").unwrap();
                match rows[0][idx] {
                    Value::Float(f) => f,
                    ref other => panic!("grouped sum answered {other:?}"),
                }
            }
            other => panic!("`{query}` answered {other:?}"),
        }
    };

    assert_eq!(
        scan.to_bits(),
        indexed.to_bits(),
        "the same rows summed through a scan ({scan:?}) and through an index \
         ({indexed:?}) must be the same number"
    );
    assert_eq!(
        scan.to_bits(),
        read_only.to_bits(),
        "the read-only handle answered {read_only:?}, the writable one {scan:?}"
    );
    assert_eq!(
        scan.to_bits(),
        grouped.to_bits(),
        "the grouped path answered {grouped:?}, the ungrouped one {scan:?}"
    );
    assert_eq!(
        scan, EXACT_TOTAL,
        "the exact total is {EXACT_TOTAL}, got {scan:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_float_avg_is_the_same_number_on_every_access_path() {
    let dir = temp_dir("avg");
    std::fs::create_dir_all(&dir).unwrap();
    let mut engine = Engine::new(&dir).unwrap();
    load(&mut engine);

    let scan = float_of(&mut engine, "avg(F { .v })");
    let indexed = float_of(&mut engine, "avg(F filter .id >= 0 { .v })");
    let read_only = readonly_float(&engine, "avg(F filter .id >= 0 { .v })");

    assert_eq!(
        scan.to_bits(),
        indexed.to_bits(),
        "avg through a scan ({scan:?}) and through an index ({indexed:?}) must \
         be the same number"
    );
    assert_eq!(
        scan.to_bits(),
        read_only.to_bits(),
        "the read-only handle answered {read_only:?}, the writable one {scan:?}"
    );
    assert_eq!(
        scan,
        EXACT_TOTAL / 200.0,
        "the exact average is {}, got {scan:?}",
        EXACT_TOTAL / 200.0
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// An infinite input still answers infinity: the compensation term must not
/// turn an overflowing total into a NaN.
#[test]
fn an_infinite_float_sum_stays_infinite() {
    let dir = temp_dir("infinite");
    std::fs::create_dir_all(&dir).unwrap();
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type I { required id: int, v: float }");
    for i in 0..3i64 {
        exec(
            &mut engine,
            &format!("insert I {{ id := {i}, v := 1.7e308 }}"),
        );
    }
    let total = float_of(&mut engine, "sum(I { .v })");
    assert!(
        total.is_infinite() && total.is_sign_positive(),
        "a total past f64's range is +inf, got {total:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
