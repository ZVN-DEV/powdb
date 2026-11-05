#![cfg(feature = "testing")]

use powdb_query::ast::ParamValue;
use powdb_query::cancel::{CancelReason, ExecCancel};
use powdb_query::executor::Engine;
use powdb_storage::types::Value;
use std::sync::Arc;

fn fixture(body_bytes: usize) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, k: int, body: str }")
        .unwrap();
    for id in 0..200 {
        let k = match id {
            198 => Value::Int(i64::MIN),
            199 => Value::Int(i64::MAX),
            n if n % 11 == 0 => Value::Empty,
            n => Value::Int(n % 17 - 8),
        };
        engine
            .catalog_mut()
            .get_table_mut("T")
            .unwrap()
            .insert(&vec![Value::Int(id), k, Value::Str("x".repeat(body_bytes))])
            .unwrap();
    }
    engine.catalog_mut().checkpoint().unwrap();
    (dir, engine)
}

fn parity(engine: &mut Engine, query: &str) {
    let mut expected = None;
    for generic in [true, false] {
        engine.set_force_generic_path(generic);
        for readonly in [false, true] {
            let result = if readonly {
                engine.execute_powql_readonly(query)
            } else {
                engine.execute_powql(query)
            };
            let result = result.unwrap_or_else(|error| panic!("{query}: {error}"));
            let actual = format!("{result:?}");
            if let Some(ref expected) = expected {
                assert_eq!(
                    &actual, expected,
                    "{query}, generic={generic}, readonly={readonly}"
                );
            } else {
                expected = Some(actual);
            }
        }
    }
    engine.set_force_generic_path(false);
}

#[test]
fn membership_queries_match_generic_on_both_surfaces() {
    let (_dir, mut engine) = fixture(128);
    for predicate in [
        ".k in (1, 3, 3, null, -1)",
        ".k not in (1, 3, null)",
        "not (.k in (1, 3))",
        ".k in ()",
        ".k not in ()",
        ".k in (null)",
        ".k in (1.0, 2.5, 3)",
        ".k in (.id, 1)",
        ".k in (1, 3) and .id > 20",
        ".k in (-9223372036854775808, 9223372036854775807)",
    ] {
        parity(&mut engine, &format!("count(T filter {predicate})"));
        parity(
            &mut engine,
            &format!("T filter {predicate} order .id limit 10 {{ picked: .id }}"),
        );
        parity(&mut engine, &format!("sum(T filter {predicate} {{ .id }})"));
    }
    parity(
        &mut engine,
        "T as t filter t.k in (1, 3) order t.id { t.id }",
    );
    let query = "count(T filter .k in ($1, $2, $3))";
    for key in [-3, 1, 5] {
        let params = [
            ParamValue::Int(key),
            ParamValue::Null,
            ParamValue::Int(key + 2),
        ];
        engine.set_force_generic_path(true);
        let expected = format!(
            "{:?}",
            engine.execute_powql_with_params(query, &params).unwrap()
        );
        engine.set_force_generic_path(false);
        assert_eq!(
            format!(
                "{:?}",
                engine.execute_powql_with_params(query, &params).unwrap()
            ),
            expected
        );
        assert_eq!(
            format!(
                "{:?}",
                engine
                    .execute_powql_readonly_with_params(query, &params)
                    .unwrap()
            ),
            expected
        );
    }
}

#[test]
fn overflow_rows_and_live_schema_keep_existing_outcomes() {
    for body in [10_000, 70_000] {
        let (_dir, mut engine) = fixture(body);
        for query in [
            "count(T filter .k in (1, 3))",
            "T filter .k not in (1, 3) order .id limit 3 { .id, .body }",
        ] {
            parity(&mut engine, query);
        }
    }
    let (_dir, mut engine) = fixture(16);
    let query = "count(T filter .k in (1, 3))";
    parity(&mut engine, query);
    engine.execute_powql("alter T drop column k").unwrap();
    assert!(engine.execute_powql(query).is_err());
    assert!(engine.execute_powql_readonly(query).is_err());
    engine.execute_powql("alter T add column k: str").unwrap();
    parity(&mut engine, query);
}

#[test]
fn membership_mutations_and_rollback_match_generic() {
    let (_a, mut actual) = fixture(128);
    let (_b, mut generic) = fixture(128);
    generic.set_force_generic_path(true);
    for query in [
        "begin",
        "T filter .k in (1, 3, null) update { k := .k + 10 }",
        "rollback",
        "T filter .k not in (1, 3, null) delete",
    ] {
        assert_eq!(
            format!("{:?}", actual.execute_powql(query).unwrap()),
            format!("{:?}", generic.execute_powql(query).unwrap()),
            "{query}"
        );
    }
    assert_eq!(
        format!(
            "{:?}",
            actual.execute_powql("T order .id { .id, .k }").unwrap()
        ),
        format!(
            "{:?}",
            generic.execute_powql("T order .id { .id, .k }").unwrap()
        )
    );
}

#[test]
fn membership_cancellation_does_not_poison_reuse() {
    let (_dir, mut engine) = fixture(128);
    let query = "count(T filter .k in (1, 3))";
    let token = Arc::new(ExecCancel::new());
    token.cancel(CancelReason::Disconnect);
    assert!(engine
        .execute_powql_with_cancel(query, token.clone())
        .is_err());
    assert!(engine
        .execute_powql_readonly_with_cancel(query, token)
        .is_err());
    parity(&mut engine, query);
}

#[test]
fn sql_membership_remains_explicitly_unsupported() {
    let (_dir, mut engine) = fixture(16);
    for query in [
        "SELECT COUNT(*) FROM T WHERE k IN (1, 3, 3, NULL)",
        "SELECT COUNT(*) FROM T WHERE k NOT IN (1, 3, NULL)",
        "SELECT id FROM T WHERE NOT (k IN (1, 3)) ORDER BY id LIMIT 10",
    ] {
        for generic in [false, true] {
            engine.set_force_generic_path(generic);
            for result in [
                engine.execute_sql(query),
                engine.execute_sql_readonly(query),
            ] {
                let error = result.unwrap_err().to_string();
                assert!(
                    error.contains("SQL IN lists/subqueries are not supported"),
                    "{error}"
                );
            }
        }
    }
}
