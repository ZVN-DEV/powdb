//! Product-review regression matrix: cross-feature correctness checks that use
//! the public query surfaces instead of unit internals.
//!
//! These tests intentionally compose features that already have focused tests
//! elsewhere. The value here is boundary coverage: cache hits after catalog or
//! data mutation, indexed vs scan path parity, frontend semantic boundaries,
//! and link-shaped results after late child writes.

use powdb_query::ast::ParamValue;
use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::pj1::parse_json_text;
use powdb_storage::types::Value;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(dir.path()).unwrap();
    (dir, engine)
}

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|error| panic!("PowQL `{query}` failed: {error}"))
}

fn sql(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_sql(query)
        .unwrap_or_else(|error| panic!("SQL `{query}` failed: {error}"))
}

fn rows(result: QueryResult) -> (Vec<String>, Vec<Vec<Value>>) {
    match result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn scalar(result: QueryResult) -> Value {
    match result {
        QueryResult::Scalar(value) => value,
        other => panic!("expected scalar, got {other:?}"),
    }
}

fn ids(result: QueryResult) -> Vec<i64> {
    let (_columns, rows) = rows(result);
    let mut ids: Vec<i64> = rows
        .into_iter()
        .map(|row| match row.first() {
            Some(Value::Int(id)) => *id,
            other => panic!("expected first projected value to be int id, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

fn json(text: &str) -> Value {
    Value::Json(parse_json_text(text).unwrap().into())
}

fn insert_doc(engine: &mut Engine, id: i64, data: &str) {
    let escaped = data.replace('\\', "\\\\").replace('"', "\\\"");
    exec(
        engine,
        &format!(r#"insert Doc {{ id := {id}, data := "{escaped}" }}"#),
    );
}

#[test]
fn json_path_index_cache_survives_update_delete_and_schema_change() {
    let (_dir, mut engine) = engine();
    exec(
        &mut engine,
        "type Doc { required unique id: int, data: json }",
    );
    insert_doc(&mut engine, 1, r#"{"tenant":"a","score":10}"#);
    insert_doc(&mut engine, 2, r#"{"tenant":"b","score":20}"#);
    insert_doc(&mut engine, 3, r#"{"tenant":"a","score":30}"#);

    let tenant_a = r#"Doc filter .data->tenant = "a" order .id asc { .id }"#;
    assert_eq!(ids(exec(&mut engine, tenant_a)), vec![1, 3]);
    assert_eq!(
        ids(exec(&mut engine, tenant_a)),
        vec![1, 3],
        "the second execution must be a cache hit with identical literals"
    );

    exec(&mut engine, "alter Doc add index (.data->tenant)");
    assert_eq!(
        ids(exec(&mut engine, tenant_a)),
        vec![1, 3],
        "adding an expression index must not change a cached query answer"
    );

    exec(
        &mut engine,
        r#"Doc filter .id = 2 update { data := "{\"tenant\":\"a\",\"score\":25}" }"#,
    );
    assert_eq!(
        ids(exec(&mut engine, tenant_a)),
        vec![1, 2, 3],
        "JSON-path index maintenance must make updated rows visible"
    );
    assert_eq!(
        ids(engine.execute_powql_readonly(tenant_a).unwrap()),
        vec![1, 2, 3],
        "read-only mirror must agree with the mutable executor on the indexed path"
    );

    exec(&mut engine, "Doc filter .id = 1 delete");
    assert_eq!(
        ids(exec(&mut engine, tenant_a)),
        vec![2, 3],
        "JSON-path index maintenance must remove deleted rows"
    );

    exec(&mut engine, "alter Doc drop index (.data->tenant)");
    assert_eq!(
        ids(exec(&mut engine, tenant_a)),
        vec![2, 3],
        "dropping the expression index must fall back to scan semantics"
    );
}

#[test]
fn link_nested_block_cache_reexecution_sees_late_children_and_preserves_order() {
    let (_dir, mut engine) = engine();
    for query in [
        "type User { required unique id: int, required name: str }",
        "type Order { required id: int, user_id: int, total: float }",
        "link User.orders -> Order on id = user_id",
        r#"insert User { id := 1, name := "alice" }"#,
        r#"insert User { id := 2, name := "bob" }"#,
        "insert Order { id := 1, user_id := 1, total := 7.0 }",
        "insert Order { id := 2, user_id := 1, total := 11.0 }",
        "insert Order { id := 3, user_id := 2, total := 20.0 }",
    ] {
        exec(&mut engine, query);
    }

    let query = "User as u order u.id asc { u.name, orders: u.orders filter total >= 7.0 order total desc limit 2 { id, total } }";
    let (_columns, first) = rows(exec(&mut engine, query));
    assert_eq!(
        first,
        vec![
            vec![
                Value::Str("alice".into()),
                json(r#"[{"id":2,"total":11.0},{"id":1,"total":7.0}]"#),
            ],
            vec![Value::Str("bob".into()), json(r#"[{"id":3,"total":20.0}]"#),],
        ]
    );

    exec(
        &mut engine,
        "insert Order { id := 4, user_id := 1, total := 30.0 }",
    );
    let (_columns, second) = rows(exec(&mut engine, query));
    assert_eq!(
        second[0],
        vec![
            Value::Str("alice".into()),
            json(r#"[{"id":4,"total":30.0},{"id":2,"total":11.0}]"#),
        ],
        "a cached to-many link block must see late children and re-run per-parent order/limit"
    );
    assert_eq!(
        second[1],
        vec![Value::Str("bob".into()), json(r#"[{"id":3,"total":20.0}]"#),]
    );
}

#[test]
fn symmetric_and_raw_aggregates_keep_their_contract_after_value_collision() {
    let (_dir, mut engine) = engine();
    for query in [
        "type Account { required unique id: int, dept: str, balance: int }",
        "type Entry { required id: int, account_id: int }",
        r#"insert Account { id := 1, dept := "ops", balance := 10 }"#,
        r#"insert Account { id := 2, dept := "ops", balance := 30 }"#,
        "insert Entry { id := 1, account_id := 1 }",
        "insert Entry { id := 2, account_id := 1 }",
        "insert Entry { id := 3, account_id := 2 }",
    ] {
        exec(&mut engine, query);
    }

    let base = "Account as a inner join Entry as e on a.id = e.account_id group a.dept";
    let symmetric = format!("{base} {{ total: sum(a.balance), n: count(a.balance) }}");
    let raw = format!("{base} {{ total: sum(raw a.balance), n: count(raw a.balance) }}");
    assert_eq!(
        rows(exec(&mut engine, &symmetric)).1,
        vec![vec![Value::Int(40), Value::Int(2)]],
        "default aggregate mode deduplicates by source RID"
    );
    assert_eq!(
        rows(exec(&mut engine, &raw)).1,
        vec![vec![Value::Int(50), Value::Int(3)]],
        "raw aggregate mode keeps join fanout"
    );

    exec(
        &mut engine,
        "Account filter .id = 2 update { balance := 10 }",
    );
    assert_eq!(
        rows(exec(&mut engine, &symmetric)).1,
        vec![vec![Value::Int(20), Value::Int(2)]],
        "symmetric aggregation deduplicates rows, not equal values"
    );
    assert_eq!(
        rows(exec(&mut engine, &raw)).1,
        vec![vec![Value::Int(30), Value::Int(3)]],
        "raw aggregation continues to expose fanout after mutation"
    );
}

#[test]
fn powql_exists_outer_reference_is_supported_where_sql_exists_is_explicitly_rejected() {
    let (_dir, mut engine) = engine();
    seed_exists_scope_fixture(&mut engine);

    assert_eq!(
        ids(exec(
            &mut engine,
            "User filter exists (Order filter .user_uid = .uid and .total > 10) { .uid }",
        )),
        vec![1],
        "unambiguous legacy PowQL correlated EXISTS still uses the outer field"
    );

    assert_eq!(
        ids(exec(
            &mut engine,
            "User as u filter exists (Order as o filter o.user_uid = u.uid and o.total > 10) { u.uid }",
        )),
        vec![1],
        "explicit outer aliases are the supported spelling for new correlated EXISTS"
    );

    let sql_err = engine
        .execute_sql(
            "SELECT uid FROM User WHERE EXISTS (SELECT id FROM Order WHERE user_uid = User.uid)",
        )
        .expect_err("SQL EXISTS is not currently supported");
    assert!(
        sql_err
            .to_string()
            .contains("SQL EXISTS subqueries are not supported yet"),
        "SQL frontend should state the support boundary, got: {sql_err}"
    );
}

fn seed_exists_scope_fixture(engine: &mut Engine) {
    for query in [
        "type User { required unique uid: int, name: str }",
        "type Order { required id: int, user_uid: int, total: int }",
        r#"insert User { uid := 1, name := "alice" }"#,
        r#"insert User { uid := 2, name := "bob" }"#,
        "insert Order { id := 1, user_uid := 1, total := 50 }",
        "insert Order { id := 2, user_uid := 1, total := 5 }",
        "insert Order { id := 3, user_uid := 2, total := 9 }",
    ] {
        exec(engine, query);
    }
}

#[test]
fn powql_exists_ambiguous_bare_reference_errors_and_explicit_alias_is_cache_safe() {
    let (_dir, mut engine) = engine();
    for query in [
        "type User { required unique id: int, name: str }",
        "type Order { required id: int, user_id: int, total: int }",
        r#"insert User { id := 1, name := "alice" }"#,
        r#"insert User { id := 2, name := "bob" }"#,
        "insert Order { id := 1, user_id := 1, total := 50 }",
        "insert Order { id := 2, user_id := 1, total := 5 }",
        "insert Order { id := 3, user_id := 2, total := 9 }",
    ] {
        exec(&mut engine, query);
    }

    let ambiguous =
        "User as u filter exists (Order as o filter o.user_id = .id and o.total > 10) { u.id }";
    let err = engine
        .execute_powql(ambiguous)
        .expect_err("ambiguous bare inner/outer field should be rejected");
    assert!(
        err.to_string().contains("ambiguous bare correlated field")
            && err.to_string().contains("outer alias"),
        "ambiguous correlated reference should get alias guidance, got: {err}"
    );

    let explicit =
        "User as u filter exists (Order as o filter o.user_id = u.id and o.total > 10) { u.id }";
    assert_eq!(
        ids(exec(&mut engine, explicit)),
        vec![1],
        "explicit outer alias should bind User.id, not Order.id"
    );
    assert_eq!(
        ids(exec(&mut engine, explicit)),
        vec![1],
        "plan-cache hits must preserve explicit outer alias binding"
    );

    exec(&mut engine, "alter Order add index .user_id");
    assert_eq!(
        ids(exec(&mut engine, explicit)),
        vec![1],
        "adding an index must not change correlated EXISTS scope binding"
    );
    exec(
        &mut engine,
        "insert Order { id := 4, user_id := 2, total := 20 }",
    );
    assert_eq!(
        ids(exec(&mut engine, explicit)),
        vec![1, 2],
        "cached correlated EXISTS must re-execute after child-table mutation"
    );

    let readonly = engine
        .execute_powql_readonly(explicit)
        .expect("readonly correlated EXISTS");
    assert_eq!(
        ids(readonly),
        vec![1, 2],
        "readonly path must share the same explicit outer alias semantics"
    );

    engine.set_force_generic_path(true);
    assert_eq!(
        ids(exec(&mut engine, explicit)),
        vec![1, 2],
        "forced-generic path must agree with optimized correlation execution"
    );
    engine.set_force_generic_path(false);

    let prep = engine.prepare(explicit).expect("prepare concrete EXISTS");
    assert_eq!(
        ids(engine
            .execute_prepared(&prep, &[])
            .expect("execute prepared concrete EXISTS")),
        vec![1, 2],
        "prepared execution must keep the explicit outer alias scoped"
    );

    let parameterized =
        "User as u filter exists (Order as o filter o.user_id = u.id and o.total > $1) { u.id }";
    assert_eq!(
        ids(engine
            .execute_powql_with_params(parameterized, &[ParamValue::Int(10)])
            .expect("execute parameterized threshold 10")),
        vec![1, 2],
        "parameter rebinding must keep the outer alias scoped while replacing inner literals"
    );
    assert_eq!(
        ids(engine
            .execute_powql_with_params(parameterized, &[ParamValue::Int(30)])
            .expect("execute parameterized threshold 30")),
        vec![1],
        "parameter rebinding must not capture the wrong scope on the second execution"
    );
}

#[test]
fn powql_exists_scope_errors_do_not_depend_on_outer_rows() {
    let (_dir, mut engine) = engine();
    for query in [
        "type User { required unique id: int, name: str }",
        "type Order { required id: int, user_id: int, total: int }",
        "insert Order { id := 1, user_id := 1, total := 50 }",
    ] {
        exec(&mut engine, query);
    }

    let ambiguous = engine
        .execute_powql("User as u filter exists (Order as o filter .id = 1) { u.id }")
        .expect_err("bare inner/outer id must be ambiguous even when User is empty");
    assert!(
        ambiguous
            .to_string()
            .contains("ambiguous bare correlated field"),
        "empty-outer ambiguity should be validated before per-row execution, got: {ambiguous}"
    );

    let unknown = engine
        .execute_powql("User as u filter exists (Order as o filter o.user_id = u.missing) { u.id }")
        .expect_err("unknown explicit outer field must error even when User is empty");
    assert!(
        unknown.to_string().contains("unknown outer field `missing`"),
        "empty-outer unknown alias field should be validated before per-row execution, got: {unknown}"
    );
}

#[test]
fn sql_in_subquery_with_shared_column_names_keeps_existing_support_boundary() {
    let (_dir, mut engine) = engine();
    for query in [
        "type T { required unique id: int }",
        "type U { required unique id: int }",
        "insert T { id := 1 }",
        "insert U { id := 1 }",
    ] {
        exec(&mut engine, query);
    }

    let err = engine
        .execute_sql("SELECT id FROM T WHERE id IN (SELECT id FROM U WHERE id > 0)")
        .expect_err("SQL IN subqueries are outside the current SQL subset");
    assert!(
        err.to_string().contains("SQL IN"),
        "SQL IN support boundary should remain explicit, got: {err}"
    );
}

#[test]
fn powql_exists_alias_shadowing_keeps_the_inner_alias_local() {
    let (_dir, mut engine) = engine();
    for query in [
        "type User { required unique id: int, name: str }",
        "type Order { required id: int, user_id: int, total: int }",
        r#"insert User { id := 1, name := "alice" }"#,
        r#"insert User { id := 2, name := "bob" }"#,
        "insert Order { id := 1, user_id := 1, total := 50 }",
        "insert Order { id := 2, user_id := 1, total := 5 }",
        "insert Order { id := 3, user_id := 2, total := 9 }",
    ] {
        exec(&mut engine, query);
    }

    assert_eq!(
        ids(exec(
            &mut engine,
            "User as u filter exists (Order as u filter u.user_id = 1 and u.total > 10) { u.id }",
        )),
        vec![1, 2],
        "an inner alias named like the outer alias must shadow it for qualified inner fields"
    );
}

#[test]
fn powql_exists_inner_only_bare_fields_stay_uncorrelated() {
    let (_dir, mut engine) = engine();
    for query in [
        "type User { required unique id: int, name: str }",
        "type Probe { required probe_id: int, flag: int }",
        r#"insert User { id := 1, name := "alice" }"#,
        r#"insert User { id := 2, name := "bob" }"#,
        "insert Probe { probe_id := 1, flag := 0 }",
    ] {
        exec(&mut engine, query);
    }

    let query = "User filter exists (Probe filter .probe_id = 999 and .flag = 1) { .id }";
    assert_eq!(
        ids(exec(&mut engine, query)),
        Vec::<i64>::new(),
        "an inner-only bare `.probe_id` in an uncorrelated filtered EXISTS must not \
         be reclassified as an ambiguous per-row outer reference"
    );
    assert_eq!(
        ids(exec(&mut engine, query)),
        Vec::<i64>::new(),
        "the same uncorrelated EXISTS shape must stay cache-safe on re-execution"
    );
}

#[test]
fn powql_exists_two_nested_scopes_bind_to_the_nearest_explicit_alias() {
    let (_dir, mut engine) = engine();
    for query in [
        "type User { required unique id: int, name: str }",
        "type Order { required unique id: int, user_id: int }",
        "type Line { required id: int, order_id: int, sku: str }",
        r#"insert User { id := 1, name := "alice" }"#,
        r#"insert User { id := 2, name := "bob" }"#,
        "insert Order { id := 10, user_id := 1 }",
        "insert Order { id := 20, user_id := 2 }",
        r#"insert Line { id := 1, order_id := 10, sku := "keep" }"#,
        r#"insert Line { id := 2, order_id := 20, sku := "skip" }"#,
    ] {
        exec(&mut engine, query);
    }

    let query = r#"User as u filter exists (
        Order as o filter o.user_id = u.id
            and exists (Line as l filter l.order_id = o.id and l.sku = "keep")
    ) { u.id }"#;
    assert_eq!(
        ids(exec(&mut engine, query)),
        vec![1],
        "the inner Line subquery must bind `o.id` to the nearest Order scope \
         while the outer Order subquery binds `u.id` to User"
    );
}

#[test]
fn sql_and_powql_null_counts_stay_aligned_across_filter_cache_hits() {
    let (_dir, mut engine) = engine();
    for query in [
        "type Event { required id: int, category: str, score: int }",
        r#"insert Event { id := 1, category := "a", score := 10 }"#,
        r#"insert Event { id := 2, category := "a" }"#,
        r#"insert Event { id := 3, category := "b", score := 30 }"#,
    ] {
        exec(&mut engine, query);
    }

    assert_eq!(
        scalar(exec(
            &mut engine,
            r#"count(Event filter .category = "a" { .score })"#
        )),
        Value::Int(1)
    );
    assert_eq!(
        scalar(sql(
            &mut engine,
            "SELECT COUNT(score) FROM Event WHERE category = 'a'",
        )),
        Value::Int(1)
    );

    exec(
        &mut engine,
        r#"Event filter .id = 2 update { score := 20 }"#,
    );
    assert_eq!(
        scalar(exec(
            &mut engine,
            r#"count(Event filter .category = "a" { .score })"#
        )),
        Value::Int(2),
        "PowQL count(col) shape must see a nullable-column mutation through the cached plan"
    );
    assert_eq!(
        scalar(sql(
            &mut engine,
            "SELECT COUNT(score) FROM Event WHERE category = 'a'",
        )),
        Value::Int(2),
        "SQL COUNT(col) must match the same post-mutation non-null count"
    );
}
