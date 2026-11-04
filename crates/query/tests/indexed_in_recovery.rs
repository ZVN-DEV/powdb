//! Indexed membership must preserve scan answers and row multiplicity.
use powdb_query::ast::ParamValue;
use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn fixture(indexed: bool) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, k: int, f: float, label: str }")
        .unwrap();
    for id in 0..100 {
        engine
            .catalog_mut()
            .get_table_mut("T")
            .unwrap()
            .insert(&vec![
                Value::Int(id),
                if id % 11 == 0 {
                    Value::Empty
                } else {
                    Value::Int(id % 13)
                },
                Value::Float((id % 13) as f64),
                Value::Str(format!("key_{}", id % 13)),
            ])
            .unwrap();
    }
    if indexed {
        for col in ["k", "f", "label"] {
            engine
                .execute_powql(&format!("alter T add index .{col}"))
                .unwrap();
        }
    }
    engine.catalog_mut().checkpoint().unwrap();
    (dir, engine)
}

fn ids(result: QueryResult) -> Vec<i64> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows")
    };
    let mut ids: Vec<_> = rows
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(v) => v,
            _ => panic!("expected id"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn indexed_membership_preserves_duplicates_coercions_nulls_and_conjunctions() {
    let (_a, mut indexed) = fixture(true);
    let (_b, mut scan) = fixture(false);
    for predicate in [
        ".k in (1, 3, 3, 1.0)",
        ".k in (null, 1, 3)",
        ".k in (null)",
        ".k in (1, 1.5)",
        ".k not in (1, 3)",
        ".k in (1, 3) and .id > 20",
        ".f in (1, 3.0, 1.0)",
        ".label in (\"key_1\", \"key_3\", \"key_1\")",
    ] {
        let query = format!("T filter {predicate} order .id asc {{ .id }}");
        let expected = ids(scan.execute_powql(&query).unwrap());
        assert_eq!(
            ids(indexed.execute_powql(&query).unwrap()),
            expected,
            "{query}"
        );
        assert_eq!(
            ids(indexed.execute_powql_readonly(&query).unwrap()),
            expected,
            "readonly {query}"
        );
    }
}

#[test]
fn membership_with_unindexable_types_preserves_scan_outcome() {
    let (_a, mut indexed) = fixture(true);
    let (_b, mut scan) = fixture(false);
    for query in [
        "T filter .k in () { .id }",
        "T filter .k in (1, \"wrong_type\") { .id }",
        "T filter .k in (.id, 3) { .id }",
    ] {
        match (indexed.execute_powql(query), scan.execute_powql(query)) {
            (Ok(actual), Ok(expected)) => assert_eq!(ids(actual), ids(expected), "{query}"),
            (Err(actual), Err(expected)) => {
                assert_eq!(actual.to_string(), expected.to_string(), "{query}")
            }
            (actual, expected) => {
                panic!("different outcomes for {query}: {actual:?} versus {expected:?}")
            }
        }
    }
}

#[test]
fn parameterized_membership_matches_scan_on_both_surfaces() {
    let (_a, mut indexed) = fixture(true);
    let (_b, mut scan) = fixture(false);
    let query = "T filter .k in ($1, $2, $3) and .id > $4 order .id { .id }";
    for key in [1, 3, 7] {
        let params = [
            ParamValue::Int(key),
            ParamValue::Int(key + 1),
            ParamValue::Int(key),
            ParamValue::Int(20),
        ];
        let expected = ids(scan.execute_powql_with_params(query, &params).unwrap());
        assert_eq!(
            ids(indexed.execute_powql_with_params(query, &params).unwrap()),
            expected
        );
        assert_eq!(
            ids(indexed
                .execute_powql_readonly_with_params(query, &params)
                .unwrap()),
            expected
        );
    }
}

#[test]
fn membership_rechecks_index_state_after_update_and_rollback() {
    let (_dir, mut engine) = fixture(true);
    let query = "T filter .k in (1, 3, 1) order .id { .id }";
    let before = ids(engine.execute_powql(query).unwrap());
    engine.execute_powql("begin").unwrap();
    engine
        .execute_powql("T filter .id = 2 update { k := 1 }")
        .unwrap();
    assert!(ids(engine.execute_powql(query).unwrap()).contains(&2));
    engine.execute_powql("rollback").unwrap();
    assert_eq!(ids(engine.execute_powql(query).unwrap()), before);
}

#[test]
fn selective_membership_explain_uses_index_probes() {
    let (_dir, mut engine) = fixture(true);
    let result = engine
        .execute_powql("explain T filter .k in (1, 3, 1) { .id }")
        .unwrap();
    let text = format!("{result:?}");
    assert!(
        text.contains("IndexScan"),
        "membership should use existing index: {text}"
    );
    assert!(
        !text.contains("SeqScan"),
        "selective membership must not scan: {text}"
    );
}

#[test]
fn broad_and_large_membership_keep_bounded_scan_fallback() {
    let (_a, mut indexed) = fixture(true);
    let (_b, mut scan) = fixture(false);
    for count in [13, 129] {
        let list = (0..count)
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let query = format!("T filter .k in ({list}) order .id {{ .id }}");
        assert_eq!(
            ids(indexed.execute_powql(&query).unwrap()),
            ids(scan.execute_powql(&query).unwrap())
        );
        let plan = indexed.execute_powql(&format!("explain {query}")).unwrap();
        assert!(format!("{plan:?}").contains("SeqScan"));
    }
}

#[test]
fn membership_projection_respects_small_budget_eligibility() {
    fn wide_fixture(indexed: bool) -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql("type Wide { id: int, body: str }")
            .unwrap();
        for id in 0..2_000 {
            engine
                .catalog_mut()
                .get_table_mut("Wide")
                .unwrap()
                .insert(&vec![Value::Int(id), Value::Str("x".repeat(1024))])
                .unwrap();
        }
        if indexed {
            engine.execute_powql("alter Wide add unique .id").unwrap();
        }
        engine.catalog_mut().checkpoint().unwrap();
        engine.set_query_memory_limit(32 * 1024);
        (dir, engine)
    }
    let (_a, mut indexed) = wide_fixture(true);
    let (_b, mut scan) = wide_fixture(false);
    let keys = (0..128)
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let query = format!("Wide filter .id in ({keys}) {{ .id }}");
    let plan = indexed.execute_powql(&format!("explain {query}")).unwrap();
    assert!(format!("{plan:?}").contains("SeqScan"));
    let expected = ids(scan.execute_powql(&query).unwrap());
    assert_eq!(expected.len(), 128);
    assert_eq!(ids(indexed.execute_powql(&query).unwrap()), expected);
    assert_eq!(
        ids(indexed.execute_powql_readonly(&query).unwrap()),
        expected
    );
}

#[test]
fn overflow_membership_keeps_scan_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type OverflowMembership { id: int, body: str }")
        .unwrap();
    for id in 0..40 {
        engine
            .catalog_mut()
            .get_table_mut("OverflowMembership")
            .unwrap()
            .insert(&vec![Value::Int(id), Value::Str("x".repeat(12_000))])
            .unwrap();
    }
    engine
        .execute_powql("alter OverflowMembership add unique .id")
        .unwrap();
    let query = "OverflowMembership filter .id in (1, 3, 1) { .id }";
    let plan = engine.execute_powql(&format!("explain {query}")).unwrap();
    assert!(format!("{plan:?}").contains("SeqScan"));
    assert_eq!(ids(engine.execute_powql(query).unwrap()), vec![1, 3]);
    assert_eq!(
        ids(engine.execute_powql_readonly(query).unwrap()),
        vec![1, 3]
    );
}
