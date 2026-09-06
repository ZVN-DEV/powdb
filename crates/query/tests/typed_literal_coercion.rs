//! E1/Q14: a `uuid`, `datetime` or `bytes` column compared with a literal is
//! coerced to the column's type before execution, exactly as `insert` coerces
//! the same literal, and an uncoercible literal is a typed error instead of a
//! silently false predicate.
//!
//! Before this, `.u = "<its exact uuid>"` returned zero rows, `!=` returned
//! everything, `>` returned everything, update and delete by uuid key affected
//! zero rows, and the index was never probed. Every frontend was affected:
//! PowQL, SQL, `$1` parameters and prepared statements.

use powdb_query::ast::{Literal, ParamValue};
use powdb_query::executor::Engine;
use powdb_query::result::{QueryError, QueryResult};
use powdb_storage::types::Value;

const U1: &str = "550e8400-e29b-41d4-a716-446655440000";
const U2: &str = "00000000-0000-0000-0000-000000000002";
const U3: &str = "ffffffff-ffff-ffff-ffff-ffffffffffff";

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn scalar_int(result: QueryResult) -> i64 {
    match result {
        QueryResult::Scalar(Value::Int(n)) => n,
        QueryResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            ref v => panic!("expected int, got {v:?}"),
        },
        other => panic!("expected scalar, got {other:?}"),
    }
}

/// Three rows keyed by uuid, with a bytes and a datetime column, optionally
/// indexed on `u`.
fn fixture(indexed: bool) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { id: int, u: uuid, b: bytes, t: datetime }")
        .unwrap();
    if indexed {
        engine.execute_powql("alter T add index .u").unwrap();
        engine.execute_powql("alter T add index .b").unwrap();
        engine.execute_powql("alter T add index .t").unwrap();
    }
    for (id, u, b, t) in [
        (1, U1, r"\\x0a0b", 1_000),
        (2, U2, r"\\x0c0d", 2_000),
        (3, U3, r"\\x0e0f", 3_000),
    ] {
        engine
            .execute_powql(&format!(
                r#"insert T {{ id := {id}, u := "{u}", b := "{b}", t := {t} }}"#
            ))
            .unwrap();
    }
    (dir, engine)
}

#[test]
fn uuid_equality_matches_its_own_literal() {
    for indexed in [false, true] {
        let (_dir, mut engine) = fixture(indexed);
        let r = rows(
            engine
                .execute_powql(&format!(r#"T filter .u = "{U1}" {{ .id }}"#))
                .unwrap(),
        );
        assert_eq!(r.len(), 1, "indexed={indexed}");
        assert_eq!(r[0][0], Value::Int(1), "indexed={indexed}");
    }
}

#[test]
fn uuid_inequality_excludes_the_matching_row() {
    for indexed in [false, true] {
        let (_dir, mut engine) = fixture(indexed);
        let n = scalar_int(
            engine
                .execute_powql(&format!(r#"count(T filter .u != "{U1}")"#))
                .unwrap(),
        );
        assert_eq!(n, 2, "indexed={indexed}");
    }
}

#[test]
fn uuid_ordered_comparison_uses_uuid_order() {
    for indexed in [false, true] {
        let (_dir, mut engine) = fixture(indexed);
        // U2 (00000000-…) < U1 (550e…) < U3 (ffff…).
        let n = scalar_int(
            engine
                .execute_powql(&format!(r#"count(T filter .u > "{U1}")"#))
                .unwrap(),
        );
        assert_eq!(n, 1, "indexed={indexed}");
    }
}

#[test]
fn uuid_in_list_matches_by_string_literal() {
    for indexed in [false, true] {
        let (_dir, mut engine) = fixture(indexed);
        let n = scalar_int(
            engine
                .execute_powql(&format!(r#"count(T filter .u in ("{U1}", "{U3}"))"#))
                .unwrap(),
        );
        assert_eq!(n, 2, "indexed={indexed}");
    }
}

#[test]
fn uuid_between_matches_by_string_literal() {
    for indexed in [false, true] {
        let (_dir, mut engine) = fixture(indexed);
        let n = scalar_int(
            engine
                .execute_powql(&format!(r#"count(T filter .u between "{U2}" and "{U1}")"#))
                .unwrap(),
        );
        assert_eq!(n, 2, "indexed={indexed}");
    }
}

#[test]
fn bytes_equality_matches_its_own_literal() {
    for indexed in [false, true] {
        let (_dir, mut engine) = fixture(indexed);
        let r = rows(
            engine
                .execute_powql(r#"T filter .b = "\\x0a0b" { .id }"#)
                .unwrap(),
        );
        assert_eq!(r.len(), 1, "indexed={indexed}");
        assert_eq!(r[0][0], Value::Int(1), "indexed={indexed}");
    }
}

#[test]
fn datetime_equality_matches_an_integer_timestamp() {
    for indexed in [false, true] {
        let (_dir, mut engine) = fixture(indexed);
        let n = scalar_int(engine.execute_powql("count(T filter .t = 2000)").unwrap());
        assert_eq!(n, 1, "indexed={indexed}");
        let n = scalar_int(engine.execute_powql("count(T filter .t > 1000)").unwrap());
        assert_eq!(n, 2, "indexed={indexed}");
    }
}

#[test]
fn update_by_uuid_key_affects_the_matching_row() {
    let (_dir, mut engine) = fixture(true);
    engine
        .execute_powql(&format!(r#"T filter .u = "{U1}" update {{ id := 99 }}"#))
        .unwrap();
    let n = scalar_int(engine.execute_powql("count(T filter .id = 99)").unwrap());
    assert_eq!(n, 1);
}

#[test]
fn delete_by_uuid_key_removes_the_matching_row() {
    let (_dir, mut engine) = fixture(true);
    engine
        .execute_powql(&format!(r#"T filter .u = "{U1}" delete"#))
        .unwrap();
    assert_eq!(scalar_int(engine.execute_powql("count(T)").unwrap()), 2);
}

#[test]
fn sql_frontend_matches_a_uuid_string_literal() {
    let (_dir, mut engine) = fixture(true);
    let n = scalar_int(
        engine
            .execute_sql(&format!("select count(*) from T where u = '{U1}'"))
            .unwrap(),
    );
    assert_eq!(n, 1);
}

#[test]
fn bound_parameter_matches_a_uuid_column() {
    let (_dir, engine) = fixture(true);
    let r = rows(
        engine
            .execute_powql_readonly_with_params(
                "T filter .u = $1 { .id }",
                &[ParamValue::Str(U1.to_string())],
            )
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn prepared_statement_matches_a_uuid_literal() {
    let (_dir, mut engine) = fixture(true);
    let prep = engine
        .prepare(&format!(r#"T filter .u = "{U2}" {{ .id }}"#))
        .unwrap();
    let r = rows(
        engine
            .execute_prepared(&prep, &[Literal::String(U1.to_string())])
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn an_uncoercible_uuid_literal_is_a_typed_error() {
    let (_dir, mut engine) = fixture(false);
    let err = engine
        .execute_powql(r#"count(T filter .u = "not-a-uuid")"#)
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("'u'") && message.contains("uuid"),
        "expected a typed error naming the column and the format, got {message}"
    );
    assert!(matches!(err, QueryError::Execution(_)), "got {err:?}");
}

#[test]
fn an_uncoercible_bytes_literal_is_a_typed_error() {
    let (_dir, mut engine) = fixture(false);
    let message = engine
        .execute_powql(r#"count(T filter .b = "nothex")"#)
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("'b'") && message.contains("bytes"),
        "expected a typed error naming the column and the format, got {message}"
    );
}

#[test]
fn a_string_literal_against_a_datetime_column_is_a_typed_error() {
    let (_dir, mut engine) = fixture(false);
    let message = engine
        .execute_powql(r#"count(T filter .t = "2024-01-01")"#)
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("'t'") && message.contains("datetime"),
        "expected a typed error naming the column, got {message}"
    );
}

#[test]
fn a_numeric_literal_against_a_uuid_column_is_a_typed_error() {
    let (_dir, mut engine) = fixture(false);
    let message = engine
        .execute_powql("count(T filter .u = 5)")
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("'u'"),
        "expected a typed error naming the column, got {message}"
    );
}
