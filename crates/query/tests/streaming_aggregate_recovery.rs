use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn fixture(rows: i64) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql(
            "type Comment { id: int, score: int, rating: float, created_at: datetime, body: str }",
        )
        .unwrap();
    for id in 0..rows {
        engine
            .catalog_mut()
            .get_table_mut("Comment")
            .unwrap()
            .insert(&vec![
                Value::Int(id),
                Value::Int(id % 100),
                Value::Float((id % 20) as f64 / 2.0),
                Value::DateTime(1_700_000_000_000_000 + id),
                Value::Str("x".repeat(256)),
            ])
            .unwrap();
    }
    (dir, engine)
}

const QUERY: &str = "Comment group 1 { cnt: count(.id), total: sum(.score), mean: avg(.rating), latest: max(.created_at) }";

fn result_rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows: {other:?}"),
    }
}

#[test]
fn constant_group_numeric_aggregates_preserve_values_on_both_surfaces() {
    let (_dir, mut engine) = fixture(100);
    let expected = vec![vec![
        Value::Int(100),
        Value::Int(4950),
        Value::Float(4.75),
        Value::DateTime(1_700_000_000_000_099),
    ]];
    assert_eq!(result_rows(engine.execute_powql(QUERY).unwrap()), expected);
    assert_eq!(
        result_rows(engine.execute_powql_readonly(QUERY).unwrap()),
        expected
    );
}

#[test]
fn empty_constant_group_has_no_output_row() {
    let (_dir, mut engine) = fixture(0);
    assert!(result_rows(engine.execute_powql(QUERY).unwrap()).is_empty());
    assert!(result_rows(engine.execute_powql_readonly(QUERY).unwrap()).is_empty());
}

#[test]
fn constant_group_nulls_and_having_match_the_generic_executor() {
    let (_dir, mut engine) = fixture(3);
    engine
        .execute_powql(
            "insert Comment { id := 3, score := null, rating := null, created_at := null }",
        )
        .unwrap();
    for query in [QUERY, "Comment group 1 { cnt: count(.score), total: sum(.score), mean: avg(.rating), oldest: min(.created_at) } having cnt > 0"] {
        engine.set_force_generic_path(true);
        engine.reset_forced_generic_sites();
        let generic = result_rows(engine.execute_powql(query).unwrap());
        assert!(engine.forced_generic_sites().contains(&"aggregate:stream-constant-group"));
        engine.set_force_generic_path(false);
        assert_eq!(result_rows(engine.execute_powql(query).unwrap()), generic);
        assert_eq!(result_rows(engine.execute_powql_readonly(query).unwrap()), generic);
    }
}

#[test]
fn all_null_constant_group_preserves_empty_aggregate_values() {
    let (_dir, mut engine) = fixture(0);
    engine.execute_powql("insert Comment { id := null, score := null, rating := null, created_at := null, body := null }").unwrap();
    let expected = vec![vec![
        Value::Int(0),
        Value::Empty,
        Value::Empty,
        Value::Empty,
    ]];
    assert_eq!(result_rows(engine.execute_powql(QUERY).unwrap()), expected);
    assert_eq!(
        result_rows(engine.execute_powql_readonly(QUERY).unwrap()),
        expected
    );
}

#[test]
fn spilled_bodies_use_generic_fallback_without_losing_aggregate_rows() {
    for body_bytes in [10_000, 70_000] {
        let (_dir, mut engine) = fixture(0);
        engine
            .catalog_mut()
            .get_table_mut("Comment")
            .unwrap()
            .insert(&vec![
                Value::Int(4),
                Value::Int(12),
                Value::Float(2.5),
                Value::DateTime(1_700_000_000_000_004),
                Value::Str("x".repeat(body_bytes)),
            ])
            .unwrap();
        assert!(engine.catalog_mut().table_has_overflow("Comment"));
        let expected = vec![vec![
            Value::Int(1),
            Value::Int(12),
            Value::Float(2.5),
            Value::DateTime(1_700_000_000_000_004),
        ]];
        engine.set_force_generic_path(true);
        engine.reset_forced_generic_sites();
        assert_eq!(result_rows(engine.execute_powql(QUERY).unwrap()), expected);
        assert!(
            !engine
                .forced_generic_sites()
                .contains(&"aggregate:stream-constant-group"),
            "overflow must decline before entering the streaming path"
        );
        engine.set_force_generic_path(false);
        assert_eq!(result_rows(engine.execute_powql(QUERY).unwrap()), expected);
        assert_eq!(
            result_rows(engine.execute_powql_readonly(QUERY).unwrap()),
            expected
        );
    }
}

#[test]
fn dirty_materialized_view_preserves_refresh_and_readonly_escalation() {
    let (_dir, mut engine) = fixture(2);
    engine
        .execute_powql("materialize V as Comment { .id, .score }")
        .unwrap();
    engine
        .execute_powql("insert Comment { id := 2, score := 20 }")
        .unwrap();
    let query = "V group 1 { cnt: count(.id), total: sum(.score) }";
    assert!(engine.execute_powql_readonly(query).is_err());
    let expected = vec![vec![Value::Int(3), Value::Int(21)]];
    assert_eq!(result_rows(engine.execute_powql(query).unwrap()), expected);
    assert_eq!(
        result_rows(engine.execute_powql_readonly(query).unwrap()),
        expected
    );
}

#[test]
fn nonfinite_float_accumulation_matches_generic() {
    let (_dir, mut engine) = fixture(0);
    for (id, value) in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN]
        .into_iter()
        .enumerate()
    {
        engine
            .catalog_mut()
            .get_table_mut("Comment")
            .unwrap()
            .insert(&vec![
                Value::Int(id as i64),
                Value::Int(1),
                Value::Float(value),
                Value::Empty,
                Value::Empty,
            ])
            .unwrap();
    }
    let query = "Comment group 1 { total: sum(.rating), mean: avg(.rating), lo: min(.rating), hi: max(.rating) }";
    engine.set_force_generic_path(true);
    let expected = format!("{:?}", result_rows(engine.execute_powql(query).unwrap()));
    engine.set_force_generic_path(false);
    assert_eq!(
        format!("{:?}", result_rows(engine.execute_powql(query).unwrap())),
        expected
    );
    assert_eq!(
        format!(
            "{:?}",
            result_rows(engine.execute_powql_readonly(query).unwrap())
        ),
        expected
    );
}

#[test]
fn insufficient_state_budget_refuses_before_visiting_input() {
    let (_dir, mut engine) = fixture(10);
    engine.set_query_memory_limit(1);
    let (result, work) = powdb_query::executor::capture_query_work(|| engine.execute_powql(QUERY));
    assert!(result.is_err());
    assert_eq!(work.seq_rows, 0);
}

#[test]
fn ineligible_inputs_and_variable_arguments_keep_generic_results() {
    let (_dir, mut engine) = fixture(4);
    for query in [
        "Comment filter .id >= 2 group 1 { cnt: count(.id), total: sum(.score) }",
        "Comment group 1 { first_body: min(.body), cnt: count(.id) }",
        "Comment group .score { .score, cnt: count(.id) }",
    ] {
        engine.set_force_generic_path(true);
        let expected = result_rows(engine.execute_powql(query).unwrap());
        engine.set_force_generic_path(false);
        assert_eq!(result_rows(engine.execute_powql(query).unwrap()), expected);
        assert_eq!(
            result_rows(engine.execute_powql_readonly(query).unwrap()),
            expected
        );
    }
}

#[test]
fn integer_overflow_error_matches_generic_aggregate() {
    let (_dir, mut engine) = fixture(0);
    engine
        .execute_powql("insert Comment { id := 1, score := 9223372036854775807 }")
        .unwrap();
    engine
        .execute_powql("insert Comment { id := 2, score := 1 }")
        .unwrap();
    let query = "Comment group 1 { total: sum(.score), mean: avg(.score) }";
    engine.set_force_generic_path(true);
    let generic = engine.execute_powql(query).unwrap_err().to_string();
    engine.set_force_generic_path(false);
    assert_eq!(
        engine.execute_powql(query).unwrap_err().to_string(),
        generic
    );
    assert_eq!(
        engine
            .execute_powql_readonly(query)
            .unwrap_err()
            .to_string(),
        generic
    );
}

#[test]
fn numeric_multiaggregate_does_not_materialize_unused_body() {
    let (_dir, mut engine) = fixture(10_000);
    engine.set_query_memory_limit(64 * 1024);
    let expected = vec![vec![
        Value::Int(10_000),
        Value::Int(495_000),
        Value::Float(4.75),
        Value::DateTime(1_700_000_000_009_999),
    ]];
    assert_eq!(result_rows(engine.execute_powql(QUERY).unwrap()), expected);
    assert_eq!(
        result_rows(engine.execute_powql_readonly(QUERY).unwrap()),
        expected
    );
}
