use super::*;

// ════════════════════════════════════════════════════════════════════════
// Parser and DDL regression coverage (reserved words, idempotency, intro)
// ════════════════════════════════════════════════════════════════════════

fn fresh_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_dogfood_{}_{}", std::process::id(), id));
    let _ = std::fs::remove_dir_all(&dir);
    Engine::new(&dir).unwrap()
}

fn rows_of(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

// ── P-6: reserved words usable as columns via backtick quoting ──────────

#[test]
fn test_reserved_word_column_roundtrips_end_to_end() {
    let mut engine = fresh_engine();
    // DDL with reserved-word columns quoted.
    engine
        .execute_powql("type Post { required `type`: str, `order`: int }")
        .unwrap();
    // Insert quoting the reserved-word field names.
    engine
        .execute_powql(r#"insert Post { `type` := "news", `order` := 3 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Post { `type` := "blog", `order` := 1 }"#)
        .unwrap();
    // Filter, project and order over the reserved-word columns.
    let result = engine
        .execute_powql("Post filter .`type` = \"news\" { .`type`, .`order` }")
        .unwrap();
    let rows = rows_of(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Str("news".into()));
    assert_eq!(rows[0][1], Value::Int(3));

    // Order by the reserved-word column (plain `.type` also works — dot refs
    // bypass keywords — but exercise the quoted form for round-trip).
    let ordered = rows_of(
        engine
            .execute_powql("Post order .`order` asc { .`type` }")
            .unwrap(),
    );
    assert_eq!(ordered[0][0], Value::Str("blog".into()));
    assert_eq!(ordered[1][0], Value::Str("news".into()));

    // Index DDL on a reserved-word column.
    engine
        .execute_powql("alter Post add index .`order`")
        .unwrap();
    // Index-backed lookup returns the right row.
    let looked = rows_of(
        engine
            .execute_powql("Post filter .`order` = 1 { .`type` }")
            .unwrap(),
    );
    assert_eq!(looked.len(), 1);
    assert_eq!(looked[0][0], Value::Str("blog".into()));
}

#[test]
fn test_reserved_word_ddl_without_quote_still_errors_clearly() {
    let mut engine = fresh_engine();
    let err = engine.execute_powql("type Post { type: str }").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("reserved word") && msg.contains("`type`"),
        "{msg}"
    );
}

// ── P-7: DDL idempotency ────────────────────────────────────────────────

#[test]
fn test_create_type_if_not_exists_is_noop() {
    let mut engine = fresh_engine();
    engine.execute_powql("type Post { id: int }").unwrap();
    // Re-declare with if-not-exists — must succeed as a no-op.
    engine
        .execute_powql("type Post if not exists { id: int, extra: str }")
        .unwrap();
    // The original schema is untouched (one column).
    let cols = rows_of(engine.execute_powql("describe Post").unwrap());
    assert_eq!(cols.len(), 1, "if-not-exists must not redefine the type");
}

#[test]
fn test_duplicate_create_type_names_the_type() {
    let mut engine = fresh_engine();
    engine.execute_powql("type Post { id: int }").unwrap();
    let err = engine.execute_powql("type Post { id: int }").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cannot create type 'Post'"),
        "expected a type-named error, got: {msg}"
    );
}

#[test]
fn test_drop_if_exists_is_noop_on_missing_type() {
    let mut engine = fresh_engine();
    // No such type — plain drop errors, `if exists` is a clean no-op.
    assert!(engine.execute_powql("drop Ghost").is_err());
    engine.execute_powql("drop if exists Ghost").unwrap();
}

#[test]
fn test_alter_drop_column_if_exists_is_noop() {
    let mut engine = fresh_engine();
    engine
        .execute_powql("type Post { id: int, tag: str }")
        .unwrap();
    engine
        .execute_powql("alter Post drop column if exists nonexistent")
        .unwrap();
    // Sanity: the real column is still droppable.
    engine.execute_powql("alter Post drop column tag").unwrap();
}

#[test]
fn test_add_unique_if_not_exists_is_noop_when_indexed() {
    let mut engine = fresh_engine();
    engine
        .execute_powql("type Post { id: int, slug: str }")
        .unwrap();
    engine.execute_powql("alter Post add index .slug").unwrap();
    // Already indexed — plain `add unique` errors, `if not exists` no-ops.
    assert!(engine.execute_powql("alter Post add unique .slug").is_err());
    engine
        .execute_powql("alter Post add unique if not exists .slug")
        .unwrap();
}

// ── P-8: introspection ──────────────────────────────────────────────────

#[test]
fn test_schema_lists_types_with_column_counts() {
    let mut engine = fresh_engine();
    engine
        .execute_powql("type Post { id: int, body: str }")
        .unwrap();
    engine.execute_powql("type Tag { name: str }").unwrap();
    let rows = rows_of(engine.execute_powql("schema").unwrap());
    // One row per type; find Post and check its column count.
    let post = rows
        .iter()
        .find(|r| r[0] == Value::Str("Post".into()))
        .expect("Post listed");
    assert_eq!(post[1], Value::Int(2));
    let tag = rows
        .iter()
        .find(|r| r[0] == Value::Str("Tag".into()))
        .expect("Tag listed");
    assert_eq!(tag[1], Value::Int(1));
}

#[test]
fn test_describe_reports_columns_types_nullability_and_indexes() {
    let mut engine = fresh_engine();
    engine
        .execute_powql("type Post { required id: int, body: str, unique slug: str }")
        .unwrap();
    engine.execute_powql("alter Post add index .body").unwrap();
    let rows = rows_of(engine.execute_powql("describe Post").unwrap());
    assert_eq!(rows.len(), 3);

    // id: int, required → not nullable, no index.
    assert_eq!(rows[0][0], Value::Str("id".into()));
    assert_eq!(rows[0][1], Value::Str("int".into()));
    assert_eq!(rows[0][2], Value::Bool(false)); // nullable = !required
    assert_eq!(rows[0][3], Value::Str("".into()));

    // body: str, optional, non-unique index.
    assert_eq!(rows[1][0], Value::Str("body".into()));
    assert_eq!(rows[1][2], Value::Bool(true));
    assert_eq!(rows[1][3], Value::Str("index".into()));

    // slug: unique index.
    assert_eq!(rows[2][0], Value::Str("slug".into()));
    assert_eq!(rows[2][3], Value::Str("unique".into()));
}

#[test]
fn test_describe_unknown_type_errors() {
    let mut engine = fresh_engine();
    let err = engine.execute_powql("describe Ghost").unwrap_err();
    assert!(err.to_string().contains("Ghost"), "{err}");
}

#[test]
fn test_describe_reflects_live_schema_after_alter() {
    // Plan-cache safety: a cached `describe` plan must still reflect the
    // current schema after a DDL change, not a stale snapshot.
    let mut engine = fresh_engine();
    engine.execute_powql("type Post { id: int }").unwrap();
    assert_eq!(
        rows_of(engine.execute_powql("describe Post").unwrap()).len(),
        1
    );
    engine
        .execute_powql("alter Post add column body: str")
        .unwrap();
    // Same query text — would hit the plan cache — must now show 2 columns.
    assert_eq!(
        rows_of(engine.execute_powql("describe Post").unwrap()).len(),
        2
    );
}

#[test]
fn test_schema_readonly_path() {
    // Introspection must work over the read-only executor (server reader side).
    let mut engine = fresh_engine();
    engine.execute_powql("type Post { id: int }").unwrap();
    let rows = rows_of(engine.execute_powql_readonly("schema").unwrap());
    assert!(rows.iter().any(|r| r[0] == Value::Str("Post".into())));
    let desc = rows_of(engine.execute_powql_readonly("describe Post").unwrap());
    assert_eq!(desc.len(), 1);
}

// ── P-5: grouped aggregates over joins (v0.11 bug fixes) ────────────────
//
// Covers qualified group keys, qualified aggregate arguments, unqualified
// suffix resolution (unique / zero / ambiguous), the silent-null-becomes-error
// guard, HAVING over qualified refs, count_distinct over a fan-out join, and
// SQL GROUP BY parity, over both the hash-join and nested-loop join paths.

fn cols_rows(r: QueryResult) -> (Vec<String>, Vec<Vec<Value>>) {
    match r {
        QueryResult::Rows { columns, rows } => (columns, rows),
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// A one-to-many User→Order fixture with a `status` column on User.
///   active:   Alice (id 1) → 100, 200 ; Bob (id 2) → 50
///   inactive: Carol (id 3) → 300
fn group_join_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_gjoin_{}_{}", std::process::id(), id));
    let _ = std::fs::remove_dir_all(&dir);
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type User { required id: int, required name: str, required status: str }")
        .unwrap();
    engine
        .execute_powql(
            "type Order { required id: int, required user_id: int, required total: int }",
        )
        .unwrap();
    for (uid, name, status) in [
        (1, "Alice", "active"),
        (2, "Bob", "active"),
        (3, "Carol", "inactive"),
    ] {
        engine
            .execute_powql(&format!(
                r#"insert User {{ id := {uid}, name := "{name}", status := "{status}" }}"#
            ))
            .unwrap();
    }
    for (oid, uid, total) in [(10, 1, 100), (11, 1, 200), (12, 2, 50), (13, 3, 300)] {
        engine
            .execute_powql(&format!(
                "insert Order {{ id := {oid}, user_id := {uid}, total := {total} }}"
            ))
            .unwrap();
    }
    engine
}

/// Sort output rows by the first (status) column so assertions are
/// order-independent (grouping preserves insertion order, but the join
/// materialization order is an implementation detail we do not want to bind).
fn sorted_by_status(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));
    rows
}

#[test]
fn test_group_qualified_key_over_hash_join() {
    // Equi `on u.id = o.user_id` takes the hash-join path.
    let mut engine = group_join_engine();
    let (cols, rows) = cols_rows(
        engine
            .execute_powql(
                "User as u join Order as o on u.id = o.user_id \
                 group u.status { u.status, n: count(*) }",
            )
            .unwrap(),
    );
    assert_eq!(cols, vec!["u.status", "n"]);
    let rows = sorted_by_status(rows);
    // active: 3 order rows ; inactive: 1
    assert_eq!(rows[0], vec![Value::Str("active".into()), Value::Int(3)]);
    assert_eq!(rows[1], vec![Value::Str("inactive".into()), Value::Int(1)]);
}

#[test]
fn test_joined_group_order_limit_offset_apply_to_grouped_rows() {
    let mut engine = group_join_engine();
    let top_group = "User as u join Order as o on u.id = o.user_id \
                     group u.status { u.status, n: count(*) } \
                     order n desc limit 1";
    let (columns, rows) = cols_rows(engine.execute_powql(top_group).unwrap());
    assert_eq!(columns, vec!["u.status", "n"]);
    assert_eq!(
        rows,
        vec![vec![Value::Str("active".into()), Value::Int(3)]],
        "LIMIT must select from complete groups, not truncate joined input rows"
    );

    // Reuse the same canonical plan with a different limit literal. This
    // guards both operator placement and plan-cache literal substitution.
    let all_groups = "User as u join Order as o on u.id = o.user_id \
                      group u.status { u.status, n: count(*) } \
                      order n desc limit 2";
    let (_, rows) = cols_rows(engine.execute_powql(all_groups).unwrap());
    assert_eq!(
        rows,
        vec![
            vec![Value::Str("active".into()), Value::Int(3)],
            vec![Value::Str("inactive".into()), Value::Int(1)],
        ]
    );

    let second_group = "User as u join Order as o on u.id = o.user_id \
                        group u.status { u.status, n: count(*) } \
                        order n desc offset 1 limit 1";
    let (_, rows) = cols_rows(engine.execute_powql_readonly(second_group).unwrap());
    assert_eq!(
        rows,
        vec![vec![Value::Str("inactive".into()), Value::Int(1)]],
        "OFFSET and LIMIT must run after grouped-result ordering"
    );
}

#[test]
fn test_group_qualified_key_over_nested_loop_join() {
    // The extra `and o.total > 0` conjunct defeats the equi-key extractor, so
    // this exercises the nested-loop path (with identical logical results).
    let mut engine = group_join_engine();
    let (cols, rows) = cols_rows(
        engine
            .execute_powql(
                "User as u join Order as o on u.id = o.user_id and o.total > 0 \
                 group u.status { u.status, n: count(*) }",
            )
            .unwrap(),
    );
    assert_eq!(cols, vec!["u.status", "n"]);
    let rows = sorted_by_status(rows);
    assert_eq!(rows[0], vec![Value::Str("active".into()), Value::Int(3)]);
    assert_eq!(rows[1], vec![Value::Str("inactive".into()), Value::Int(1)]);
}

#[test]
fn test_group_qualified_agg_args_all_funcs() {
    // count(o.total), sum(o.total), avg(o.total), min(o.total), max(o.total):
    // the qualified inner used to silently evaluate to Empty (bug P-5.2).
    let mut engine = group_join_engine();
    let (cols, rows) = cols_rows(
        engine
            .execute_powql(
                "User as u join Order as o on u.id = o.user_id group u.status \
                 { u.status, c: count(o.total), s: sum(o.total), a: avg(o.total), \
                   lo: min(o.total), hi: max(o.total) }",
            )
            .unwrap(),
    );
    assert_eq!(cols, vec!["u.status", "c", "s", "a", "lo", "hi"]);
    let rows = sorted_by_status(rows);
    // active: totals 100,200,50 → count 3, sum 350, avg 116.666…, min 50, max 200
    assert_eq!(rows[0][0], Value::Str("active".into()));
    assert_eq!(rows[0][1], Value::Int(3));
    assert_eq!(rows[0][2], Value::Int(350));
    match rows[0][3] {
        Value::Float(v) => assert!((v - 350.0 / 3.0).abs() < 1e-9, "avg was {v}"),
        ref other => panic!("expected Float avg, got {other:?}"),
    }
    assert_eq!(rows[0][4], Value::Int(50));
    assert_eq!(rows[0][5], Value::Int(200));
    // inactive: single order 300
    assert_eq!(rows[1][0], Value::Str("inactive".into()));
    assert_eq!(rows[1][2], Value::Int(300));
}

#[test]
fn test_group_unqualified_key_suffix_resolves_over_join() {
    // `.status` is unqualified but unique across the join output columns
    // (only u.status ends with `.status`), so it resolves to u.status.
    let mut engine = group_join_engine();
    let (cols, rows) = cols_rows(
        engine
            .execute_powql(
                "User as u join Order as o on u.id = o.user_id \
                 group .status { .status, n: count(*) }",
            )
            .unwrap(),
    );
    // Unqualified key keeps its bare output name.
    assert_eq!(cols, vec!["status", "n"]);
    let rows = sorted_by_status(rows);
    assert_eq!(rows[0], vec![Value::Str("active".into()), Value::Int(3)]);
    assert_eq!(rows[1], vec![Value::Str("inactive".into()), Value::Int(1)]);
}

#[test]
fn test_group_unqualified_symmetric_agg_requires_explicit_raw() {
    // The catalog-free planner cannot prove an unqualified joined expression
    // belongs to one source, even when runtime columns would have one suffix
    // match. Explicit raw retains the existing suffix-resolution behavior.
    let mut engine = group_join_engine();
    let error = engine
        .execute_powql(
            "User as u join Order as o on u.id = o.user_id \
             group u.status { u.status, s: sum(.total) }",
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("use sum(raw ...)"), "{error}");
    let (_, rows) = cols_rows(
        engine
            .execute_powql(
                "User as u join Order as o on u.id = o.user_id \
                 group u.status { u.status, s: sum(raw .total) }",
            )
            .unwrap(),
    );
    let rows = sorted_by_status(rows);
    assert_eq!(rows[0][1], Value::Int(350)); // active
    assert_eq!(rows[1][1], Value::Int(300)); // inactive
}

#[test]
fn test_group_unqualified_key_zero_match_errors() {
    let mut engine = group_join_engine();
    let err = engine
        .execute_powql(
            "User as u join Order as o on u.id = o.user_id \
             group .nonexistent { .nonexistent, n: count(*) }",
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("nonexistent") && msg.contains("not found"),
        "expected column-not-found, got: {msg}"
    );
}

#[test]
fn test_group_unqualified_key_ambiguous_errors() {
    // Both u.id and o.id end with `.id`, so a bare `.id` key is ambiguous.
    let mut engine = group_join_engine();
    let err = engine
        .execute_powql(
            "User as u join Order as o on u.id = o.user_id \
             group .id { .id, n: count(*) }",
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("ambiguous") && msg.contains("u.id") && msg.contains("o.id"),
        "expected ambiguity naming candidates, got: {msg}"
    );
}

#[test]
fn test_group_unqualified_agg_arg_ambiguous_errors() {
    let mut engine = group_join_engine();
    let err = engine
        .execute_powql(
            "User as u join Order as o on u.id = o.user_id \
             group u.status { u.status, s: sum(.id) }",
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("ambiguous"),
        "expected ambiguity for agg arg, got: {msg}"
    );
}

#[test]
fn test_group_single_table_unqualified_unchanged() {
    // Regression: single-table grouping still resolves by exact match.
    let mut engine = group_join_engine();
    let (cols, rows) = cols_rows(
        engine
            .execute_powql("User group .status { .status, n: count(*) }")
            .unwrap(),
    );
    assert_eq!(cols, vec!["status", "n"]);
    let rows = sorted_by_status(rows);
    // 2 active users, 1 inactive.
    assert_eq!(rows[0], vec![Value::Str("active".into()), Value::Int(2)]);
    assert_eq!(rows[1], vec![Value::Str("inactive".into()), Value::Int(1)]);
}

#[test]
fn test_group_having_with_qualified_key_ref() {
    // HAVING references the qualified key output column `u.status`.
    let mut engine = group_join_engine();
    let (_, rows) = cols_rows(
        engine
            .execute_powql(
                r#"User as u join Order as o on u.id = o.user_id group u.status having u.status = "active" { u.status, n: count(*) }"#,
            )
            .unwrap(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], vec![Value::Str("active".into()), Value::Int(3)]);
}

#[test]
fn test_group_having_with_qualified_agg_ref() {
    // HAVING over a qualified aggregate arg: only the active group has >1 order.
    let mut engine = group_join_engine();
    let (_, rows) = cols_rows(
        engine
            .execute_powql(
                "User as u join Order as o on u.id = o.user_id \
                 group u.status having count(o.total) > 1 { u.status, n: count(o.total) }",
            )
            .unwrap(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], vec![Value::Str("active".into()), Value::Int(3)]);
}

#[test]
fn test_group_count_distinct_over_fan_out_join() {
    // count_distinct(u.id) is the sanctioned fan-out-safe count: the active
    // group has 3 order rows but only 2 distinct users.
    let mut engine = group_join_engine();
    let (_, rows) = cols_rows(
        engine
            .execute_powql(
                "User as u join Order as o on u.id = o.user_id \
                 group u.status { u.status, users: count(distinct u.id), orders: count(*) }",
            )
            .unwrap(),
    );
    let rows = sorted_by_status(rows);
    // active: 2 distinct users across 3 order rows
    assert_eq!(rows[0][1], Value::Int(2));
    assert_eq!(rows[0][2], Value::Int(3));
    // inactive: 1 distinct user, 1 order
    assert_eq!(rows[1][1], Value::Int(1));
    assert_eq!(rows[1][2], Value::Int(1));
}

#[test]
fn test_group_silent_null_now_correct_and_readonly_parity() {
    // The exact bug shape count(o.total): must produce a real count, not Empty,
    // on BOTH the mutable and read-only executors.
    let mut engine = group_join_engine();
    let q = "User as u join Order as o on u.id = o.user_id group u.status \
             { u.status, c: count(o.total) }";
    let (_, rows) = cols_rows(engine.execute_powql(q).unwrap());
    let rows = sorted_by_status(rows);
    assert_eq!(rows[0][1], Value::Int(3));
    assert_ne!(rows[0][1], Value::Empty);

    let (_, ro_rows) = cols_rows(engine.execute_powql_readonly(q).unwrap());
    let ro_rows = sorted_by_status(ro_rows);
    assert_eq!(ro_rows[0][1], Value::Int(3));
}

#[test]
fn test_aggregate_in_unsupported_position_errors_not_empty() {
    // An aggregate in a projection with no GROUP BY has no computed column to
    // reference; it must be a typed error, never a silent Empty cell.
    let mut engine = group_join_engine();
    let err = engine.execute_powql("User { c: count(.id) }").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("aggregate"),
        "expected an aggregate-position error, got: {msg}"
    );
}

#[test]
fn test_group_fanout_avg_matches_docs_example() {
    // Guards the docs/POWQL.md fan-out example: avg(a.balance) over the joined
    // rows is 15.0 (fan-out weighted), while count(distinct a.id) is the
    // fan-out-safe count of 3 accounts. Keeps the documented number honest.
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_fanout_{}_{}", std::process::id(), id));
    let _ = std::fs::remove_dir_all(&dir);
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql(
            "type Account { required id: int, required tier: str, required balance: float }",
        )
        .unwrap();
    engine
        .execute_powql("type Ord { required id: int, required account_id: int }")
        .unwrap();
    for (aid, bal) in [(1, 10.0), (2, 10.0), (3, 40.0)] {
        engine
            .execute_powql(&format!(
                r#"insert Account {{ id := {aid}, tier := "gold", balance := {bal} }}"#
            ))
            .unwrap();
    }
    // A(1) has 4 orders, B(2) has 1, C(3) has 1 => 6 joined rows.
    for (oid, aid) in [(10, 1), (11, 1), (12, 1), (13, 1), (14, 2), (15, 3)] {
        engine
            .execute_powql(&format!(
                "insert Ord {{ id := {oid}, account_id := {aid} }}"
            ))
            .unwrap();
    }
    let (_, rows) = cols_rows(
        engine
            .execute_powql(
                "Account as a join Ord as o on a.id = o.account_id \
                 group a.tier { a.tier, avg_bal: avg(a.balance), \
                 raw_avg: avg(raw a.balance), accounts: count(distinct a.id) }",
            )
            .unwrap(),
    );
    assert_eq!(rows.len(), 1);
    match rows[0][1] {
        Value::Float(v) => assert!(
            (v - 20.0).abs() < 1e-9,
            "symmetric source-row avg was {v}, expected 20.0"
        ),
        ref other => panic!("expected Float avg, got {other:?}"),
    }
    assert_eq!(rows[0][2], Value::Float(15.0), "raw avg keeps fan-out");
    assert_eq!(rows[0][3], Value::Int(3), "count(distinct) is fan-out-safe");
}

#[test]
fn test_group_sql_qualified_group_by_parity() {
    // SQL GROUP BY u.status lowers to PowQL `group u.status` and inherits the
    // qualified-key and qualified-arg fixes.
    let mut engine = group_join_engine();
    let (_, rows) = cols_rows(
        engine
            .execute_sql(
                "SELECT u.status, COUNT(*), SUM(o.total) \
                 FROM User u JOIN Order o ON u.id = o.user_id GROUP BY u.status",
            )
            .unwrap(),
    );
    let rows = sorted_by_status(rows);
    // active: 3 rows, sum 350 ; inactive: 1 row, sum 300
    assert_eq!(rows[0][1], Value::Int(3));
    assert_eq!(rows[0][2], Value::Int(350));
    assert_eq!(rows[1][1], Value::Int(1));
    assert_eq!(rows[1][2], Value::Int(300));
}
