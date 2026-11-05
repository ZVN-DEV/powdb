//! Indexed projection metadata must remain live without cloning the schema.
use powdb_query::cancel::{CancelReason, ExecCancel};
use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::page::OVERFLOW_CHAIN_END;
use powdb_storage::types::Value;
use std::sync::Arc;

fn fixture() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, k: int, name: str, body: str }")
        .unwrap();
    for id in 0..20 {
        engine
            .catalog_mut()
            .get_table_mut("T")
            .unwrap()
            .insert(&vec![
                Value::Int(id),
                Value::Int(if id < 3 { 7 } else { -id }),
                Value::Str("same".into()),
                Value::Str("b".repeat(12_000)),
            ])
            .unwrap();
    }
    engine.execute_powql("alter T add index .k").unwrap();
    (dir, engine)
}

fn rows(result: QueryResult) -> (Vec<String>, Vec<Vec<Value>>) {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("expected rows, got {result:?}")
    };
    (columns, rows)
}

#[test]
fn indexed_projection_preserves_names_duplicates_bags_and_computed_fallback() {
    let (_dir, mut engine) = fixture();
    let plan = engine
        .execute_powql("explain T filter .k = 7 { .name, again: .name }")
        .unwrap();
    let plan = format!("{plan:?}");
    assert!(plan.contains("IndexScan"), "{plan}");
    assert!(!plan.contains("SeqScan"), "{plan}");
    for (query, expected_columns, expected_row, count) in [
        (
            "T filter .id = 1 { x: .name, .k, .name }",
            vec!["x", "k", "name"],
            vec![
                Value::Str("same".into()),
                Value::Int(7),
                Value::Str("same".into()),
            ],
            1,
        ),
        (
            "T filter .k = 7 { .name, again: .name }",
            vec!["name", "again"],
            vec![Value::Str("same".into()), Value::Str("same".into())],
            3,
        ),
        (
            "T filter .id = 1 { n: length(.name), .k }",
            vec!["n", "k"],
            vec![Value::Int(4), Value::Int(7)],
            1,
        ),
        ("T filter .id = 99 { .body }", vec!["body"], vec![], 0),
        (
            "T filter .id = 1 { .body }",
            vec!["body"],
            vec![Value::Str("b".repeat(12_000))],
            1,
        ),
    ] {
        for generic in [false, true] {
            engine.set_force_generic_path(generic);
            for result in [
                engine.execute_powql(query).unwrap(),
                engine.execute_powql_readonly(query).unwrap(),
            ] {
                let (columns, actual) = rows(result);
                assert_eq!(columns, expected_columns, "{query}, generic={generic}");
                assert_eq!(actual, vec![expected_row.clone(); count], "{query}");
            }
        }
    }
}

#[test]
fn cached_indexed_projection_revalidates_live_schema_and_types() {
    let (_dir, mut engine) = fixture();
    let query = "T filter .id = 1 { .name, .k }";
    engine.execute_powql(query).unwrap();
    engine.execute_powql_readonly(query).unwrap();
    engine.execute_powql("alter T drop column name").unwrap();
    assert!(engine
        .execute_powql(query)
        .unwrap_err()
        .to_string()
        .contains("name"));
    assert!(engine
        .execute_powql_readonly(query)
        .unwrap_err()
        .to_string()
        .contains("name"));
    let (_, actual) = rows(engine.execute_powql("T filter .id = 1 { .k }").unwrap());
    assert_eq!(actual, vec![vec![Value::Int(7)]]);
    for result in [
        engine.execute_powql("T filter .id = true { .k }"),
        engine.execute_powql_readonly("T filter .id = true { .k }"),
    ] {
        assert!(result.is_err(), "wrong key type must still be rejected");
    }
}

#[test]
fn indexed_projection_still_checks_unselected_overflow_corruption() {
    let (_dir, mut engine) = fixture();
    let table = engine.catalog_mut().get_table_mut("T").unwrap();
    let rid = table.index_lookup_all("id", &Value::Int(1))[0];
    let raw = table.heap.get(rid).unwrap().unwrap();
    let stub = powdb_storage::row::raw_stub(table.schema(), table.row_layout(), &raw, 3)
        .expect("body must be spilled");
    table
        .heap
        .write_overflow_page(stub.first_page, OVERFLOW_CHAIN_END, b"corrupt", 0)
        .unwrap();
    for query in ["T filter .id = 1 { .body }", "T filter .id = 1 { .name }"] {
        for result in [
            engine.execute_powql(query),
            engine.execute_powql_readonly(query),
        ] {
            let error = result.expect_err("full-row read must validate even unselected spill");
            assert!(error.to_string().contains("overflow"), "{error}");
        }
    }
}

#[test]
fn cached_indexed_projection_honors_precancellation() {
    let (_dir, mut engine) = fixture();
    let query = "T filter .id = 1 { .name }";
    engine.execute_powql(query).unwrap();
    let cancel = Arc::new(ExecCancel::new());
    cancel.cancel(CancelReason::Disconnect);
    assert!(engine
        .execute_powql_with_cancel(query, cancel.clone())
        .is_err());
    assert!(engine
        .execute_powql_readonly_with_cancel(query, cancel)
        .is_err());
    assert_eq!(rows(engine.execute_powql(query).unwrap()).1.len(), 1);
}
