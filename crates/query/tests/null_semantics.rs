//! NULL / missing-value comparison semantics (oracle 2026-07-21 finding C).
//!
//! Decided semantics: a missing (`Empty`) value NEVER matches an ordered or
//! equality/inequality comparison in a filter predicate. A row whose compared
//! field is `Empty` is EXCLUDED from `.f < x`, `.f <= x`, `.f > x`, `.f >= x`,
//! `.f = x`, and `.f != x` for any present value `x`. Presence is tested only
//! with `exists` / `not exists`.
//!
//! The bug these tests pin: the COMPILED predicate leaves
//! (`crates/query/src/executor/compiled.rs`) already null-guard and exclude
//! `Empty`, but the GENERIC evaluator
//! (`crates/query/src/executor/eval.rs` `eval_binop`) compared via `Value`'s
//! total order (where `Empty` sorts first), so the same logical filter
//! returned different rows depending on whether it compiled (AND-only over
//! int/float/str-eq atoms) or fell back (any OR/NOT, str/bool ordered compare,
//! nested residuals). All paths must now agree on exclusion.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::pj1::parse_json_text;
use powdb_storage::types::Value;

/// Canonical PJ1 value for a JSON text literal, for nested-block comparison.
fn json(text: &str) -> Value {
    Value::Json(parse_json_text(text).unwrap().into())
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_null_sem_{name}_{}_{}",
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

fn scalar(engine: &mut Engine, query: &str) -> i64 {
    match exec(engine, query) {
        QueryResult::Scalar(Value::Int(n)) => n,
        QueryResult::Rows { rows, .. } if rows.len() == 1 && rows[0].len() == 1 => {
            match &rows[0][0] {
                Value::Int(n) => *n,
                other => panic!("non-int scalar {other:?}"),
            }
        }
        other => panic!("expected scalar, got {other:?}"),
    }
}

/// PowQL row ids for a `{ .id }` projection, sorted.
fn ids(result: QueryResult) -> Vec<i64> {
    let QueryResult::Rows { rows, columns } = result else {
        panic!("expected rows, got scalar");
    };
    let id_col = columns
        .iter()
        .position(|c| c == "id")
        .expect("id column present");
    let mut out: Vec<i64> = rows
        .into_iter()
        .map(|row| match &row[id_col] {
            Value::Int(id) => *id,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

fn sql_ids(engine: &mut Engine, sql: &str) -> Vec<i64> {
    let result = engine
        .execute_sql(sql)
        .unwrap_or_else(|e| panic!("failed to execute `{sql}`: {e}"));
    ids(result)
}

/// One present row (id 1: s="a", v=5) and one all-missing row (id 2:
/// s and v `Empty`).
fn fresh(name: &str) -> (std::path::PathBuf, Engine) {
    let dir = temp_dir(name);
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type T { required id: int, s: str, v: int }");
    exec(&mut engine, r#"insert T { id := 1, s := "a", v := 5 }"#);
    exec(&mut engine, "insert T { id := 2 }");
    (dir, engine)
}

// ---------------------------------------------------------------------------
// Each of the six comparison operators excludes the Empty row.
// The row with the present value (id 1) is included or not on its own merits;
// what matters is that id 2 (Empty `v`) NEVER appears.
// ---------------------------------------------------------------------------

#[test]
fn ordered_and_equality_comparisons_exclude_missing_values() {
    let (dir, mut engine) = fresh("six_ops");

    // `.v < 0`: neither row qualifies (5 is not < 0; Empty excluded).
    assert_eq!(
        ids(exec(&mut engine, "T filter .v < 0 { .id }")),
        Vec::<i64>::new()
    );
    // `.v <= 0`: same.
    assert_eq!(
        ids(exec(&mut engine, "T filter .v <= 0 { .id }")),
        Vec::<i64>::new()
    );
    // `.v > 100`: 5 is not > 100; Empty excluded.
    assert_eq!(
        ids(exec(&mut engine, "T filter .v > 100 { .id }")),
        Vec::<i64>::new()
    );
    // `.v >= 100`: same.
    assert_eq!(
        ids(exec(&mut engine, "T filter .v >= 100 { .id }")),
        Vec::<i64>::new()
    );
    // `.v != 5`: id 1 has v = 5 (excluded by !=), id 2 is Empty (must be
    // excluded, NOT matched as "not equal to 5").
    assert_eq!(
        ids(exec(&mut engine, "T filter .v != 5 { .id }")),
        Vec::<i64>::new()
    );
    // `.v = 5`: only id 1; Empty never equals a present literal.
    assert_eq!(ids(exec(&mut engine, "T filter .v = 5 { .id }")), vec![1]);

    // And the positive controls: id 1 shows up when its present value matches.
    assert_eq!(ids(exec(&mut engine, "T filter .v > 0 { .id }")), vec![1]);
    assert_eq!(ids(exec(&mut engine, "T filter .v != 999 { .id }")), vec![1]);

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Compiled vs generic agreement. `.v < 0` compiles (single int leaf); wrapping
// the same atom in an OR forces the generic `eval_binop` path. The two must
// return the same rows for the Empty case.
// ---------------------------------------------------------------------------

#[test]
fn compiled_and_generic_paths_agree_on_missing() {
    let (dir, mut engine) = fresh("compiled_vs_generic");

    // Int ordered leaf: compiled vs OR-forced-generic.
    let compiled = scalar(&mut engine, "count(T filter .v < 0)");
    let generic_or = scalar(&mut engine, "count(T filter .v < 0 or .id < 0)");
    assert_eq!(
        compiled, generic_or,
        "`.v < 0` and `.v < 0 or .id < 0` must agree on the Empty row"
    );
    assert_eq!(compiled, 0);

    // Str inequality has no compiled ordered leaf, so it is always generic.
    // It must exclude the Empty `s` row exactly like the int leaf.
    let str_lt = scalar(&mut engine, r#"count(T filter .s < "a")"#);
    assert_eq!(str_lt, 0, "`.s < \"a\"` must exclude the Empty-s row");

    // Compiled StrEq `!=` vs generic (OR-forced): both exclude Empty.
    let compiled_neq = scalar(&mut engine, r#"count(T filter .s != "a")"#);
    let generic_neq = scalar(&mut engine, r#"count(T filter .s != "a" or .id < 0)"#);
    assert_eq!(
        compiled_neq, generic_neq,
        "compiled and generic `.s != \"a\"` must agree on the Empty-s row"
    );
    assert_eq!(compiled_neq, 0);

    // The exact generic residual from the oracle repro: still excludes id 2.
    assert_eq!(
        ids(exec(
            &mut engine,
            r#"T filter .s != "a" and (.id > 0 or .v = 99) { .id }"#
        )),
        Vec::<i64>::new(),
        "generic AND/OR residual must not resurrect the Empty row"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// A matching sibling of an OR must still include a row even though the other
// side is a missing-value comparison. (Empty comparison is false, not a hard
// veto of the whole disjunction.)
// ---------------------------------------------------------------------------

#[test]
fn or_with_missing_side_still_matches_on_present_side() {
    let (dir, mut engine) = fresh("or_sibling");
    // id 2 has Empty v, but `.id = 2` is a present, true comparison, so the
    // disjunction includes it.
    assert_eq!(
        ids(exec(&mut engine, "T filter .v > 100 or .id = 2 { .id }")),
        vec![2],
        "a true present-side comparison must still include the row"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// PowQL and SQL frontends lower to the same predicate and must agree.
// ---------------------------------------------------------------------------

#[test]
fn powql_and_sql_frontends_agree_on_missing() {
    let (dir, mut engine) = fresh("powql_vs_sql");

    let cases = [
        ("T filter .v < 0 { .id }", "SELECT id FROM T WHERE v < 0"),
        ("T filter .v != 5 { .id }", "SELECT id FROM T WHERE v != 5"),
        (
            "T filter .v < 0 or .id < 0 { .id }",
            "SELECT id FROM T WHERE v < 0 OR id < 0",
        ),
        (r#"T filter .s < "a" { .id }"#, "SELECT id FROM T WHERE s < 'a'"),
    ];
    for (powql, sql) in cases {
        let p = ids(exec(&mut engine, powql));
        let s = sql_ids(&mut engine, sql);
        assert_eq!(p, s, "PowQL `{powql}` and SQL `{sql}` must agree");
    }

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Nested-projection residual filters use the generic `eval_predicate` path.
// A child whose compared field is Empty must be excluded from the nested block.
// ---------------------------------------------------------------------------

#[test]
fn nested_residual_comparison_excludes_missing_children() {
    let dir = temp_dir("nested_residual");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type P { required pid: int }");
    exec(
        &mut engine,
        "type C { required cid: int, required parent: int, v: int }",
    );
    exec(&mut engine, "insert P { pid := 1 }");
    // Two children of parent 1: one with v = -3 (matches `.v < 0`), one with
    // Empty v (must NOT match `.v < 0`).
    exec(&mut engine, "insert C { cid := 10, parent := 1, v := -3 }");
    exec(&mut engine, "insert C { cid := 11, parent := 1 }");

    let result = exec(
        &mut engine,
        "P as p { p.pid, kids: C as c filter c.parent = p.pid and c.v < 0 { c.cid } }",
    );
    let QueryResult::Rows { rows, columns } = result else {
        panic!("expected rows");
    };
    let kids_col = columns
        .iter()
        .position(|c| c == "kids")
        .expect("kids column");
    // Only cid 10 (v = -3) qualifies; the Empty-v child (cid 11) is excluded
    // because `c.v < 0` must not match a missing value.
    assert_eq!(
        rows[0][kids_col],
        json(r#"[{"cid":10}]"#),
        "nested residual `c.v < 0` must exclude the Empty-v child"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Empty vs Empty inside a filter: excluded (a comparison is false if either
// operand is Empty). Presence is tested with exists / not exists.
// ---------------------------------------------------------------------------

#[test]
fn empty_equals_empty_in_filter_is_excluded_but_exists_still_works() {
    let dir = temp_dir("empty_vs_empty");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type T { required id: int, a: int, b: int }");
    // id 1: both present and equal. id 2: both Empty. id 3: a present, b Empty.
    exec(&mut engine, "insert T { id := 1, a := 7, b := 7 }");
    exec(&mut engine, "insert T { id := 2 }");
    exec(&mut engine, "insert T { id := 3, a := 7 }");

    // `.a = .b` matches only the present-and-equal row; both-Empty is excluded.
    assert_eq!(
        ids(exec(&mut engine, "T filter .a = .b { .id }")),
        vec![1],
        "Empty = Empty must not match in a filter"
    );
    // `.a != .b` excludes the Empty operands too (id 3 has b Empty).
    assert_eq!(
        ids(exec(&mut engine, "T filter .a != .b { .id }")),
        Vec::<i64>::new(),
        "a comparison with an Empty operand is excluded, not `not equal`"
    );

    // Presence is what `exists` / `not exists` are for.
    assert_eq!(
        ids(exec(&mut engine, "T filter exists .b { .id }")),
        vec![1],
        "exists selects rows where b is present"
    );
    assert_eq!(
        ids(exec(&mut engine, "T filter not exists .b { .id }")),
        vec![2, 3],
        "not exists selects rows where b is missing"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// JOIN ON equality is unchanged: two rows both missing the key still match on
// direct Value equality (Empty = Empty), because join key matching is not a
// filter comparison. See crates/query/src/join.rs.
// ---------------------------------------------------------------------------

#[test]
fn join_on_equality_with_missing_keys_unchanged() {
    let dir = temp_dir("join_empty_keys");
    let mut engine = Engine::new(&dir).unwrap();
    exec(&mut engine, "type L { required lid: int, k: int }");
    exec(&mut engine, "type R { required rid: int, k: int }");
    // Present matching key 5, plus one row on each side with a missing key.
    exec(&mut engine, "insert L { lid := 1, k := 5 }");
    exec(&mut engine, "insert L { lid := 2 }");
    exec(&mut engine, "insert R { rid := 1, k := 5 }");
    exec(&mut engine, "insert R { rid := 2 }");

    // Present key 5 joins (lid 1 x rid 1). Empty key joins Empty key
    // (lid 2 x rid 2), because join equality is direct Value equality.
    let n = scalar(&mut engine, "count(L as l join R as r on l.k = r.k)");
    assert_eq!(
        n, 2,
        "join ON equality must still match Empty = Empty (present pair + missing pair)"
    );
    std::fs::remove_dir_all(&dir).ok();
}
