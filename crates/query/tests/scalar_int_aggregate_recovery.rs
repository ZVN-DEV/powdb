#![cfg(feature = "testing")]

use powdb_query::cancel::{CancelReason, ExecCancel};
use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::sync::Arc;

fn fixture(values: &[Option<i64>], body_bytes: usize) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { n: int, body: str }")
        .unwrap();
    for value in values {
        engine
            .catalog_mut()
            .get_table_mut("T")
            .unwrap()
            .insert(&vec![
                value.map(Value::Int).unwrap_or(Value::Empty),
                Value::Str("x".repeat(body_bytes)),
            ])
            .unwrap();
    }
    (dir, engine)
}

fn assert_parity(engine: &mut Engine, query: &str, expected: Value) {
    for generic in [false, true] {
        engine.set_force_generic_path(generic);
        engine.reset_forced_generic_sites();
        for result in [
            engine.execute_powql(query),
            engine.execute_powql_readonly(query),
        ] {
            match result.unwrap() {
                QueryResult::Scalar(value) => {
                    assert_eq!(value, expected, "{query}, generic={generic}")
                }
                other => panic!("{query}: {other:?}"),
            }
        }
        if generic {
            assert!(engine.forced_generic_sites().contains(&"agg-single-col"));
        }
    }
    engine.set_force_generic_path(false);
}

#[test]
fn empty_null_and_real_sentinel_values_match_generic() {
    for values in [
        vec![],
        vec![None, None],
        vec![Some(i64::MAX)],
        vec![Some(i64::MIN)],
    ] {
        let (_dir, mut engine) = fixture(&values, 16);
        for op in ["sum", "avg", "min", "max"] {
            let expected = match values.first().copied().flatten() {
                None => Value::Empty,
                Some(n) if op == "avg" => Value::Float(n as f64),
                Some(n) => Value::Int(n),
            };
            assert_parity(&mut engine, &format!("{op}(T {{ .n }})"), expected);
        }
    }
}

#[test]
fn mixed_nulls_and_filtered_inputs_match_generic() {
    let (_dir, mut engine) = fixture(&[None, Some(-4), Some(0), None, Some(8)], 16);
    for (filter, sum, avg, min, max) in [
        (
            "",
            Value::Int(4),
            Value::Float(4.0 / 3.0),
            Value::Int(-4),
            Value::Int(8),
        ),
        (
            " filter .n > -10",
            Value::Int(4),
            Value::Float(4.0 / 3.0),
            Value::Int(-4),
            Value::Int(8),
        ),
        (
            " filter .n > 0",
            Value::Int(8),
            Value::Float(8.0),
            Value::Int(8),
            Value::Int(8),
        ),
        (
            " filter .n > 100",
            Value::Empty,
            Value::Empty,
            Value::Empty,
            Value::Empty,
        ),
    ] {
        for (op, expected) in [("sum", sum), ("avg", avg), ("min", min), ("max", max)] {
            assert_parity(&mut engine, &format!("{op}(T{filter} {{ .n }})"), expected);
        }
    }
}

#[test]
fn sum_checks_final_total_not_intermediate_i64_total() {
    for values in [
        vec![Some(i64::MAX), Some(1), Some(-1)],
        vec![Some(i64::MIN), Some(-1), Some(1)],
    ] {
        let (_dir, mut engine) = fixture(&values, 16);
        assert_parity(&mut engine, "sum(T { .n })", Value::Int(values[0].unwrap()));
    }
    for values in [
        vec![Some(i64::MAX), Some(1)],
        vec![Some(i64::MIN), Some(-1)],
    ] {
        let (_dir, mut engine) = fixture(&values, 16);
        for generic in [false, true] {
            engine.set_force_generic_path(generic);
            for error in [
                engine.execute_powql("sum(T { .n })").unwrap_err(),
                engine.execute_powql_readonly("sum(T { .n })").unwrap_err(),
            ] {
                assert!(error.to_string().contains("overflow"), "{error}");
            }
        }
    }
}

#[test]
fn spilled_unused_body_preserves_all_numeric_results() {
    let (_dir, mut engine) = fixture(&[None, Some(-4), Some(8)], 70_000);
    assert!(engine.catalog_mut().table_has_overflow("T"));
    for (op, value) in [
        ("sum", Value::Int(4)),
        ("avg", Value::Float(2.0)),
        ("min", Value::Int(-4)),
        ("max", Value::Int(8)),
    ] {
        assert_parity(&mut engine, &format!("{op}(T {{ .n }})"), value);
    }
}

#[test]
fn cancelled_scalar_aggregates_leave_engine_usable() {
    let (_dir, mut engine) = fixture(&[Some(1); 100], 16);
    for op in ["sum", "avg", "min", "max"] {
        let token = Arc::new(ExecCancel::new());
        token.cancel(CancelReason::Disconnect);
        let query = format!("{op}(T {{ .n }})");
        assert!(engine
            .execute_powql_with_cancel(&query, token.clone())
            .is_err());
        assert!(engine
            .execute_powql_readonly_with_cancel(&query, token)
            .is_err());
    }
    assert_parity(&mut engine, "sum(T { .n })", Value::Int(100));
}

#[test]
fn dirty_materialized_view_refreshes_before_numeric_aggregates() {
    let (_dir, mut engine) = fixture(&[Some(1)], 16);
    engine.execute_powql("materialize V as T").unwrap();
    engine.execute_powql("insert T { n := 9 }").unwrap();
    assert!(engine.execute_powql_readonly("sum(V { .n })").is_err());
    assert_parity(&mut engine, "sum(V { .n })", Value::Int(10));
}
