//! P0 regression: native-PowQL single-table aliased qualified column
//! references (`Mem as m filter m.agent_id = 4217`).
//!
//! Before the fix the executor only resolved `alias.field` inside joins; in a
//! single-table scan the qualifier was dropped and the field fell through to
//! missing-field `Empty` semantics — silently returning wrong rows, empty
//! projections, and zero-effect UPDATE/DELETE on a normal documented pattern.
//! This is the native-PowQL twin of the SQL-frontend P0 fixed in v0.18.1.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn engine() -> Engine {
    let dir = std::env::temp_dir().join(format!(
        "powdb_aliasrefs_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut e = Engine::new(&dir).unwrap();
    e.execute_powql("type Mem { required id: int, required agent_id: int, required score: int }")
        .unwrap();
    // ids 0..5 -> agent_id 4217, scores 0,10,20,30,40 (sum 100)
    // ids 5..8 -> agent_id 99,   scores 50,60,70       (sum 180)
    for i in 0..5i64 {
        e.execute_powql(&format!(
            "insert Mem {{ id := {i}, agent_id := 4217, score := {} }}",
            i * 10
        ))
        .unwrap();
    }
    for i in 5..8i64 {
        e.execute_powql(&format!(
            "insert Mem {{ id := {i}, agent_id := 99, score := {} }}",
            i * 10
        ))
        .unwrap();
    }
    e
}

fn ok(e: &mut Engine, q: &str) -> QueryResult {
    e.execute_powql(q)
        .unwrap_or_else(|err| panic!("query `{q}` failed: {err}"))
}

fn scalar_int(e: &mut Engine, q: &str) -> i64 {
    match ok(e, q) {
        QueryResult::Scalar(Value::Int(n)) => n,
        other => panic!("expected scalar int from `{q}`, got {other:?}"),
    }
}

fn int_col(e: &mut Engine, q: &str) -> Vec<i64> {
    match ok(e, q) {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match &r[0] {
                Value::Int(n) => *n,
                other => panic!("non-int cell in `{q}`: {other:?}"),
            })
            .collect(),
        other => panic!("expected rows from `{q}`, got {other:?}"),
    }
}

// --- Requirement #1: the alias resolves in every clause ---------------------

#[test]
fn aliased_filter_count_resolves() {
    let mut e = engine();
    // Bare form is the control; aliased form must agree.
    assert_eq!(scalar_int(&mut e, "count(Mem filter .agent_id = 4217)"), 5);
    assert_eq!(
        scalar_int(&mut e, "count(Mem as m filter m.agent_id = 4217)"),
        5
    );
}

#[test]
fn aliased_projection_returns_column_values_not_empty() {
    let mut e = engine();
    let mut vals = int_col(&mut e, "Mem as m { m.score }");
    vals.sort();
    assert_eq!(vals, vec![0, 10, 20, 30, 40, 50, 60, 70]);
}

#[test]
fn aliased_filter_projection_resolves() {
    let mut e = engine();
    let mut vals = int_col(&mut e, "Mem as m filter m.agent_id = 4217 { m.score }");
    vals.sort();
    assert_eq!(vals, vec![0, 10, 20, 30, 40]);
}

#[test]
fn aliased_order_resolves() {
    let mut e = engine();
    let ids = int_col(
        &mut e,
        "Mem as m filter m.agent_id = 4217 order m.score desc { m.id }",
    );
    assert_eq!(ids, vec![4, 3, 2, 1, 0]);
}

#[test]
fn aliased_group_resolves() {
    let mut e = engine();
    // group by agent_id, count per group. Two groups: 4217 -> 5, 99 -> 3.
    let mut counts = match ok(
        &mut e,
        "Mem as m group m.agent_id { m.agent_id, n: count(m.score) }",
    ) {
        QueryResult::Rows { rows, columns } => {
            let aid = columns.iter().position(|c| c.ends_with("agent_id")).unwrap();
            let n = columns.iter().position(|c| c == "n").unwrap();
            rows.into_iter()
                .map(|r| {
                    let a = match &r[aid] {
                        Value::Int(v) => *v,
                        o => panic!("{o:?}"),
                    };
                    let c = match &r[n] {
                        Value::Int(v) => *v,
                        o => panic!("{o:?}"),
                    };
                    (a, c)
                })
                .collect::<Vec<_>>()
        }
        other => panic!("{other:?}"),
    };
    counts.sort();
    assert_eq!(counts, vec![(99, 3), (4217, 5)]);
}

#[test]
fn aliased_aggregate_argument_resolves() {
    let mut e = engine();
    assert_eq!(
        scalar_int(&mut e, "sum(Mem as m filter m.agent_id = 4217 { m.score })"),
        100
    );
}

#[test]
fn unaliased_table_name_qualifier_resolves() {
    let mut e = engine();
    // With no alias, the qualifier may name the table itself.
    assert_eq!(scalar_int(&mut e, "count(Mem filter Mem.agent_id = 4217)"), 5);
    let mut vals = int_col(&mut e, "Mem filter Mem.agent_id = 99 { Mem.score }");
    vals.sort();
    assert_eq!(vals, vec![50, 60, 70]);
}

// --- Requirement #2: unknown / hidden qualifier is a hard error -------------

#[test]
fn alias_hides_table_name() {
    // Per SQL convention the alias hides the base table name, so `Mem.score`
    // under `Mem as m` must error rather than silently returning Empty.
    let mut e = engine();
    assert!(e.execute_powql("Mem as m { Mem.score }").is_err());
    assert!(e
        .execute_powql("Mem as m filter Mem.agent_id = 4217 { m.id }")
        .is_err());
}

#[test]
fn unknown_qualifier_is_hard_error() {
    let mut e = engine();
    assert!(e.execute_powql("Mem as m { x.score }").is_err());
    assert!(e
        .execute_powql("Mem as m filter x.agent_id = 4217 { m.id }")
        .is_err());
    assert!(e.execute_powql("Mem filter x.agent_id = 4217").is_err());
}

// --- Regressions: bare refs and doc-store missing-field semantics unchanged --

#[test]
fn bare_field_refs_still_work() {
    let mut e = engine();
    assert_eq!(scalar_int(&mut e, "count(Mem filter .agent_id = 4217)"), 5);
    let mut vals = int_col(&mut e, "Mem filter .agent_id = 4217 { .score }");
    vals.sort();
    assert_eq!(vals, vec![0, 10, 20, 30, 40]);
}

#[test]
fn missing_optional_field_still_returns_empty_not_error() {
    // Doc-store semantics: a genuinely missing optional column projects Empty,
    // it does not error. The unknown-qualifier hard error must not leak here.
    let dir = std::env::temp_dir().join(format!(
        "powdb_aliasrefs_opt_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut e = Engine::new(&dir).unwrap();
    e.execute_powql("type Doc { required id: int, note: str }")
        .unwrap();
    e.execute_powql("insert Doc { id := 1 }").unwrap();
    // Bare missing field -> Empty (unchanged).
    match ok(&mut e, "Doc { .note }") {
        QueryResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Empty]]),
        other => panic!("{other:?}"),
    }
    // Known qualifier + missing value -> still Empty, not an error.
    match ok(&mut e, "Doc as d { d.note }") {
        QueryResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Empty]]),
        other => panic!("{other:?}"),
    }
}

// --- Requirement #1 for mutations: UPDATE / DELETE -------------------------

#[test]
fn aliased_update_resolves() {
    let mut e = engine();
    // Update the three agent_id=99 rows via the alias.
    match ok(&mut e, "Mem as m filter m.agent_id = 99 update { score := 1 }") {
        QueryResult::Modified(n) => assert_eq!(n, 3),
        other => panic!("{other:?}"),
    }
    assert_eq!(scalar_int(&mut e, "count(Mem filter .score = 1)"), 3);
}

#[test]
fn aliased_delete_resolves() {
    let mut e = engine();
    match ok(&mut e, "Mem as m filter m.agent_id = 4217 delete") {
        QueryResult::Modified(n) => assert_eq!(n, 5),
        other => panic!("{other:?}"),
    }
    assert_eq!(scalar_int(&mut e, "count(Mem)"), 3);
}

#[test]
fn table_name_qualified_update_resolves() {
    let mut e = engine();
    match ok(&mut e, "Mem filter Mem.agent_id = 99 update { score := 7 }") {
        QueryResult::Modified(n) => assert_eq!(n, 3),
        other => panic!("{other:?}"),
    }
    assert_eq!(scalar_int(&mut e, "count(Mem filter .score = 7)"), 3);
}

#[test]
fn unknown_qualifier_in_update_is_hard_error() {
    let mut e = engine();
    assert!(e
        .execute_powql("Mem as m filter x.agent_id = 99 update { score := 1 }")
        .is_err());
}

// --- Requirement #3: joins are unaffected ----------------------------------

#[test]
fn joins_still_resolve_qualified_refs() {
    let mut e = engine();
    e.execute_powql("type Ord { required oid: int, required mem_id: int }")
        .unwrap();
    e.execute_powql("insert Ord { oid := 100, mem_id := 0 }").unwrap();
    e.execute_powql("insert Ord { oid := 101, mem_id := 5 }").unwrap();
    let rows = match ok(
        &mut e,
        "Mem as m inner join Ord as o on m.id = o.mem_id { m.id, o.oid }",
    ) {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    };
    let mut pairs: Vec<(i64, i64)> = rows
        .into_iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Int(a), Value::Int(b)) => (*a, *b),
            o => panic!("{o:?}"),
        })
        .collect();
    pairs.sort();
    assert_eq!(pairs, vec![(0, 100), (5, 101)]);
}
