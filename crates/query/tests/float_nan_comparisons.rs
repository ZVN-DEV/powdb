//! Q15: a comparison against NaN follows IEEE, while sort and key order keep
//! the total order.
//!
//! `Value`'s equality is `total_cmp`, which has to be a total order: it is what
//! `Eq`/`Hash` are built on, so `order`, `group`, `distinct` and the B-tree all
//! depend on NaN sitting in one definite place. The filter path inherited it and
//! answered `.f = $nan` with the NaN row, `.f < $nan` with every number, and
//! `.f != $nan` by hiding the row that is not equal to anything. Comparison and
//! ordering are different questions, so only the comparison rule changes here.

use powdb_query::ast::ParamValue;
use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

const NAN: [ParamValue; 1] = [ParamValue::Float(f64::NAN)];

/// Rows 1 and 2 hold numbers, row 3 holds a NaN.
fn seeded(indexed: bool) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, f: float }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 1, f := 1.5 }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 2, f := 0.0 }")
        .unwrap();
    engine
        .execute_powql_with_params("insert T { id := 3, f := $1 }", &NAN)
        .unwrap();
    if indexed {
        engine.execute_powql("alter T add index .f").unwrap();
    }
    (dir, engine)
}

fn ids(result: Result<QueryResult, powdb_query::result::QueryError>, query: &str) -> Vec<i64> {
    match result.unwrap_or_else(|e| panic!("{query}: {e}")) {
        QueryResult::Rows { rows, .. } => {
            let mut out: Vec<i64> = rows
                .iter()
                .map(|row| match row[0] {
                    Value::Int(n) => n,
                    ref other => panic!("expected an int id, got {other:?}"),
                })
                .collect();
            out.sort_unstable();
            out
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The same query on every access path: compiled scan, generic scan, index,
/// and the read-only engine.
fn ids_everywhere(query: &str) -> Vec<i64> {
    let (_dir, mut compiled) = seeded(false);
    let answer = ids(compiled.execute_powql_with_params(query, &NAN), query);

    let (_dir, mut generic) = seeded(false);
    generic.set_force_generic_path(true);
    assert_eq!(
        ids(generic.execute_powql_with_params(query, &NAN), query),
        answer,
        "`{query}` differs with fast paths off"
    );

    let (_dir, mut indexed) = seeded(true);
    assert_eq!(
        ids(indexed.execute_powql_with_params(query, &NAN), query),
        answer,
        "`{query}` differs on the indexed path"
    );

    let (_dir, readonly) = seeded(false);
    assert_eq!(
        ids(
            readonly.execute_powql_readonly_with_params(query, &NAN),
            query
        ),
        answer,
        "`{query}` differs on the read-only path"
    );
    answer
}

#[test]
fn equality_against_nan_matches_nothing() {
    assert_eq!(
        ids_everywhere("T filter .f = $1 { .id }"),
        Vec::<i64>::new()
    );
}

#[test]
fn inequality_against_nan_matches_everything() {
    assert_eq!(ids_everywhere("T filter .f != $1 { .id }"), [1, 2, 3]);
}

#[test]
fn ordered_comparisons_against_nan_match_nothing() {
    for query in [
        "T filter .f < $1 { .id }",
        "T filter .f > $1 { .id }",
        "T filter .f <= $1 { .id }",
        "T filter .f >= $1 { .id }",
    ] {
        assert_eq!(ids_everywhere(query), Vec::<i64>::new(), "{query}");
    }
}

#[test]
fn a_stored_nan_answers_no_comparison_but_is_unequal_to_everything() {
    let (_dir, mut engine) = seeded(false);
    for query in [
        "T filter .f > 0.0 { .id }",
        "T filter .f < 0.0 { .id }",
        "T filter .f = 0.0 { .id }",
    ] {
        assert!(
            !ids(engine.execute_powql(query), query).contains(&3),
            "the NaN row answered `{query}`"
        );
    }
    assert_eq!(
        ids(engine.execute_powql("T filter .f != 0.0 { .id }"), "!="),
        [1, 3]
    );
}

#[test]
fn sort_group_and_distinct_keep_the_total_order() {
    let (_dir, mut engine) = seeded(false);
    assert_eq!(
        match engine.execute_powql("T order .f { .id }").unwrap() {
            QueryResult::Rows { rows, .. } => rows
                .iter()
                .map(|row| match row[0] {
                    Value::Int(n) => n,
                    ref other => panic!("{other:?}"),
                })
                .collect::<Vec<i64>>(),
            other => panic!("{other:?}"),
        },
        [2, 1, 3],
        "NaN keeps its one definite place in the order"
    );
    engine
        .execute_powql_with_params("insert T { id := 4, f := $1 }", &NAN)
        .unwrap();
    match engine
        .execute_powql("T group .f { c: count(.id) } order .c")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(
            rows.last().map(|row| row[0].clone()),
            Some(Value::Int(2)),
            "the two NaN rows group together"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        match engine.execute_powql("T { .f } distinct").unwrap() {
            QueryResult::Rows { rows, .. } => rows.len(),
            other => panic!("{other:?}"),
        },
        3,
        "distinct still collapses the two NaN values"
    );
}

#[test]
fn a_join_on_a_nan_key_still_matches() {
    let (_dir, mut engine) = seeded(false);
    engine
        .execute_powql("type U { required unique id: int, g: float }")
        .unwrap();
    engine
        .execute_powql_with_params("insert U { id := 9, g := $1 }", &NAN)
        .unwrap();
    assert_eq!(
        ids(
            engine.execute_powql("T as t join U as u on t.f = u.g { t.id }"),
            "join"
        ),
        [3],
        "join keys are matched by identity, like the missing value"
    );
}

#[test]
fn a_json_path_compares_the_same_way() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type D { required unique id: int, j: json }")
        .unwrap();
    engine
        .execute_powql("insert D { id := 1, j := \"{ \\\"v\\\": 1 }\" }")
        .unwrap();
    for query in [
        "D filter .j->v = $1 { .id }",
        "D filter .j->v < $1 { .id }",
        "D filter .j->v > $1 { .id }",
    ] {
        assert_eq!(
            ids(engine.execute_powql_with_params(query, &NAN), query),
            Vec::<i64>::new(),
            "{query}"
        );
    }
    assert_eq!(
        ids(
            engine.execute_powql_with_params("D filter .j->v != $1 { .id }", &NAN),
            "!="
        ),
        [1]
    );
}
