//! Ordered pages must bound physical work without changing query answers.
use powdb_query::ast::ParamValue;
use powdb_query::executor::{capture_query_work, Engine};
use powdb_query::result::{QueryError, QueryResult};
use powdb_storage::types::Value;

fn fixture() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().expect("temporary database");
    let mut engine = Engine::new(dir.path()).expect("open");
    engine
        .execute_powql("type T { required unique id: int, unique k: int, payload: str }")
        .expect("schema");
    for id in (0..4096).rev() {
        engine
            .catalog_mut()
            .get_table_mut("T")
            .expect("table")
            .insert(&vec![
                Value::Int(id),
                if id % 7 == 0 {
                    Value::Empty
                } else {
                    Value::Int(id)
                },
                Value::Str(format!("row_{id}")),
            ])
            .expect("fixture row");
    }
    engine.catalog_mut().checkpoint().expect("checkpoint");
    (dir, engine)
}

fn rows(result: QueryResult) -> (Vec<String>, Vec<Vec<Value>>) {
    match result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn parity(engine: &mut Engine, query: &str) {
    engine.set_force_generic_path(false);
    let fast = rows(engine.execute_powql(query).expect(query));
    let readonly = rows(engine.execute_powql_readonly(query).expect(query));
    engine.set_force_generic_path(true);
    let generic = rows(engine.execute_powql(query).expect(query));
    let generic_readonly = rows(engine.execute_powql_readonly(query).expect(query));
    engine.set_force_generic_path(false);
    assert_eq!(fast, generic, "mutable {query}");
    assert_eq!(readonly, generic_readonly, "readonly {query}");
    assert_eq!(fast, readonly, "admission surfaces {query}");
}

#[test]
fn ordered_index_pages_bound_entries_and_heap_fetches_on_both_surfaces() {
    let (_dir, mut engine) = fixture();
    let query = "T filter .id >= 100 order .id limit 17 { n: .id, .payload }";
    for readonly in [false, true] {
        let (result, work) = capture_query_work(|| {
            if readonly {
                engine.execute_powql_readonly(query)
            } else {
                engine.execute_powql(query)
            }
        });
        let (columns, actual) = rows(result.expect("bounded page"));
        assert_eq!(columns, ["n", "payload"]);
        assert_eq!(actual.len(), 17);
        assert_eq!(actual[0][0], Value::Int(100));
        assert_eq!(actual[16][0], Value::Int(116));
        assert_eq!(work.index_entries, 17);
        assert_eq!(work.heap_fetches, 17);
        assert_eq!(work.seq_rows, 0);
    }
    parity(&mut engine, query);
    assert!(engine
        .forced_generic_sites()
        .contains(&"bounded-index-page"));
}

#[test]
fn bounds_empty_results_nullable_columns_and_limit_edges_match_generic() {
    let (_dir, mut engine) = fixture();
    for query in [
        "T order .id limit 5",
        "T order .id limit 0 { .id }",
        "T order .id limit 9223372036854775807 { .id }",
        "T filter .id > 4090 order .id limit 10 { .id }",
        "T filter .id >= 4090 and .id < 4095 order .id limit 10 { .id }",
        "T filter .id > 4090 and .id <= 4095 order .id limit 10 { .id }",
        "T filter .id < 0 order .id limit 10 { .id }",
        "T filter .id > 9999 order .id limit 10 { .id }",
        "T filter .id >= 20 and .id <= 10 order .id limit 10 { .id }",
        "T filter .k <= 20 order .k limit 10 { .k, .id }",
        "T filter .k >= 20 order .k limit 10 { .k, .id }",
        "T filter .id >= 4090.5 order .id limit 10 { .id }",
    ] {
        parity(&mut engine, query);
    }
    let (result, work) = capture_query_work(|| engine.execute_powql("T order .id limit 0"));
    assert!(rows(result.expect("zero page")).1.is_empty());
    assert_eq!(work.heap_fetches, 0);
    assert_eq!(work.index_entries, 0);
}

#[test]
fn unsupported_shapes_and_errors_preserve_generic_behavior() {
    let (_dir, mut engine) = fixture();
    for query in [
        "T order .k limit 4096 { .id, .k }", // unfiltered nullable key: keep nulls
        "T order .id desc limit 4 { .id }",
        "T order .id limit 4 offset 20 { .id }",
        "T order .id limit 4 { v: .id + 1 }",
        "T filter .id > 0 and .payload = \"row_3\" order .id limit 4 { .id }",
        "T as t filter t.id > 0 order t.id limit 4 { t.id }",
        "T filter .id > 0 order .k limit 4 { .id }",
    ] {
        parity(&mut engine, query);
    }
    for query in [
        "T order .id limit -1 { .id }",
        "T order .id limit 1 + 1 { .id }",
        "T order .missing limit 0 { .id }",
        "T order .id limit 0 { .missing }",
    ] {
        engine.set_force_generic_path(false);
        let fast = engine.execute_powql(query).expect_err(query);
        engine.set_force_generic_path(true);
        let generic = engine.execute_powql(query).expect_err(query);
        engine.set_force_generic_path(false);
        assert_eq!(fast, generic, "{query}");
    }
}

#[test]
fn overflow_updates_deletes_and_restart_preserve_pages() {
    let (dir, mut engine) = fixture();
    let payload = "z".repeat(80_000);
    engine
        .execute_powql(&format!(
            "T filter .id = 101 update {{ payload := \"{payload}\" }}"
        ))
        .expect("overflow update");
    engine
        .execute_powql("T filter .id = 100 delete")
        .expect("delete");
    engine
        .execute_powql("T filter .id = 102 update { id := 9000 }")
        .expect("key update");
    let query = "T filter .id >= 100 order .id limit 3 { .id, .payload }";
    parity(&mut engine, query);
    let before = rows(engine.execute_powql(query).expect("page"));
    assert_eq!(before.1[0], vec![Value::Int(101), Value::Str(payload)]);
    drop(engine);
    let mut engine = Engine::new(dir.path()).expect("reopen");
    assert_eq!(
        rows(engine.execute_powql(query).expect("reopened page")),
        before
    );
    parity(&mut engine, query);
}

#[test]
fn small_pages_fit_small_budgets_and_large_pages_fail_cleanly() {
    let (_dir, mut engine) = fixture();
    engine.set_query_memory_limit(4096);
    for readonly in [false, true] {
        let small = "T filter .id > 0 order .id limit 3 { .id }";
        let large = "T filter .id > 0 order .id limit 4000 { .id }";
        if readonly {
            assert_eq!(
                rows(engine.execute_powql_readonly(small).expect("small page"))
                    .1
                    .len(),
                3
            );
            assert!(matches!(
                engine.execute_powql_readonly(large),
                Err(QueryError::MemoryLimitExceeded { .. })
            ));
        } else {
            assert_eq!(
                rows(engine.execute_powql(small).expect("small page"))
                    .1
                    .len(),
                3
            );
            assert!(matches!(
                engine.execute_powql(large),
                Err(QueryError::MemoryLimitExceeded { .. })
            ));
        }
    }
}

#[test]
fn integer_extremes_and_non_unique_or_missing_indexes() {
    let (_dir, mut engine) = fixture();
    for value in [i64::MIN, i64::MAX] {
        engine
            .catalog_mut()
            .get_table_mut("T")
            .expect("table")
            .insert(&vec![Value::Int(value), Value::Empty, Value::Empty])
            .expect("edge row");
    }
    let query = "T filter .id >= $1 order .id limit 2 { .id }";
    for value in [i64::MIN, i64::MAX] {
        let params = [ParamValue::Int(value)];
        let actual = rows(
            engine
                .execute_powql_with_params(query, &params)
                .expect("edge bound"),
        );
        assert_eq!(actual.1[0], vec![Value::Int(value)]);
        assert_eq!(
            rows(
                engine
                    .execute_powql_readonly_with_params(query, &params)
                    .expect("readonly edge")
            ),
            actual
        );
        engine.set_force_generic_path(true);
        assert_eq!(
            rows(
                engine
                    .execute_powql_with_params(query, &params)
                    .expect("generic edge")
            ),
            actual
        );
        engine.set_force_generic_path(false);
    }
    engine
        .execute_powql("type Plain { id: int }")
        .expect("plain schema");
    for id in [3, 1, 1, 2] {
        engine
            .execute_powql(&format!("insert Plain {{ id := {id} }}"))
            .expect("plain row");
    }
    for indexed in [false, true] {
        if indexed {
            engine
                .execute_powql("alter Plain add index .id")
                .expect("nonunique index");
        }
        let query = "Plain filter .id > 0 order .id limit 3 { .id }";
        let (actual, work) = capture_query_work(|| engine.execute_powql(query));
        assert_eq!(
            rows(actual.expect("fallback")).1,
            vec![
                vec![Value::Int(1)],
                vec![Value::Int(1)],
                vec![Value::Int(2)]
            ]
        );
        assert_eq!(
            work.index_entries, 0,
            "nonunique and unindexed paths must decline"
        );
        parity(&mut engine, query);
    }
}
