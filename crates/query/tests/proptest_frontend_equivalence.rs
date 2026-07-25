//! Property tests for the two invariants this engine keeps breaking by hand.
//!
//! 1. The PowQL and SQL frontends lower to the same plan tree, so any query
//!    expressible in both must return the same answer. Two releases in a row
//!    shipped a case where they did not (`COUNT(col)` counted rows on one side
//!    and non-null values on the other), and both were found by a person
//!    noticing, not by a test.
//! 2. Whichever access path the engine picks (compiled predicate leaf, generic
//!    row decode, or an index probe) must agree with a plain model of the data.
//!    Today's datetime bug was exactly a disagreement of this kind: the
//!    compiled and generic paths were consistent with each other but not with
//!    the index, and none of them was consistent with arithmetic.
//!
//! The oracle deliberately reimplements PowDB's documented two-valued NULL
//! rule (a comparison against a missing value is false, never "unknown") rather
//! than importing anything from the engine, so a semantic change has to be a
//! deliberate edit here and not a silently shared helper.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use proptest::prelude::*;

/// One generated row. `a` doubles as the `mirror` int column and the `ts`
/// datetime column so that every predicate is exercised against both an Int
/// column and a DateTime column holding the identical value: they must agree.
#[derive(Debug, Clone)]
struct GenRow {
    a: Option<i64>,
    label: String,
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
    Neq,
}

impl Op {
    fn powql(self) -> &'static str {
        match self {
            Op::Lt => "<",
            Op::Lte => "<=",
            Op::Gt => ">",
            Op::Gte => ">=",
            Op::Eq => "=",
            Op::Neq => "!=",
        }
    }

    /// SQL spells inequality `<>`; everything else is identical.
    fn sql(self) -> &'static str {
        match self {
            Op::Neq => "<>",
            other => other.powql(),
        }
    }

    fn apply(self, left: i64, right: i64) -> bool {
        match self {
            Op::Lt => left < right,
            Op::Lte => left <= right,
            Op::Gt => left > right,
            Op::Gte => left >= right,
            Op::Eq => left == right,
            Op::Neq => left != right,
        }
    }
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::Lt),
        Just(Op::Lte),
        Just(Op::Gt),
        Just(Op::Gte),
        Just(Op::Eq),
        Just(Op::Neq),
    ]
}

/// Values are drawn from a deliberately narrow band so that generated
/// predicates land on interesting boundaries (empty result, every row, and the
/// partial cases) instead of almost always matching everything.
fn row_strategy() -> impl Strategy<Value = GenRow> {
    (prop::option::of(0i64..12), "[a-c]{1,3}").prop_map(|(a, label)| GenRow { a, label })
}

fn scalar_count(result: &QueryResult) -> i64 {
    match result {
        QueryResult::Scalar(value) => match value {
            powdb_storage::types::Value::Int(n) => *n,
            other => panic!("expected an Int scalar, got {other:?}"),
        },
        other => panic!("expected a scalar, got {other:?}"),
    }
}

/// A fresh engine seeded with `rows`, plus a table whose `mirror` (int) and
/// `ts` (datetime) columns always hold the same number.
fn seeded_engine(rows: &[GenRow], tag: &str) -> Engine {
    let dir = std::env::temp_dir().join(format!(
        "powdb_prop_{tag}_{}_{:p}",
        std::process::id(),
        rows as *const _
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type T { required label: str, mirror: int, ts: datetime }")
        .unwrap();
    for row in rows {
        let statement = match row.a {
            Some(a) => format!(
                "insert T {{ label := \"{}\", mirror := {a}, ts := {a} }}",
                row.label
            ),
            None => format!("insert T {{ label := \"{}\" }}", row.label),
        };
        engine.execute_powql(&statement).unwrap();
    }
    engine
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// PowQL and SQL must agree with each other and with a plain model, for
    /// both an Int column and a DateTime column holding the same values.
    #[test]
    fn powql_sql_and_model_agree_on_a_single_comparison(
        rows in prop::collection::vec(row_strategy(), 1..12),
        op in op_strategy(),
        literal in 0i64..12,
    ) {
        let mut engine = seeded_engine(&rows, "single");

        // Documented two-valued NULL rule: a comparison against a missing
        // value is false, so the row is excluded, including for `!=`.
        let expected = rows
            .iter()
            .filter(|row| row.a.is_some_and(|a| op.apply(a, literal)))
            .count() as i64;

        for column in ["mirror", "ts"] {
            let powql = engine
                .execute_powql(&format!("count(T filter .{column} {} {literal})", op.powql()))
                .unwrap();
            prop_assert_eq!(
                scalar_count(&powql),
                expected,
                "PowQL disagreed with the model on `{} {} {}`",
                column, op.powql(), literal
            );

            let sql = engine
                .execute_sql(&format!(
                    "SELECT COUNT(*) FROM T WHERE {column} {} {literal}",
                    op.sql()
                ))
                .unwrap();
            prop_assert_eq!(
                scalar_count(&sql),
                expected,
                "SQL disagreed with the model on `{} {} {}`",
                column, op.sql(), literal
            );
        }
    }

    /// The same invariant across an `and` chain, which is the shape the
    /// compiled predicate path specifically handles (and the shape that falls
    /// back to a generic decode when any leaf fails to compile).
    #[test]
    fn powql_sql_and_model_agree_on_a_conjunction(
        rows in prop::collection::vec(row_strategy(), 1..12),
        left_op in op_strategy(),
        left_lit in 0i64..12,
        right_op in op_strategy(),
        right_lit in 0i64..12,
    ) {
        let mut engine = seeded_engine(&rows, "conj");

        let expected = rows
            .iter()
            .filter(|row| {
                row.a.is_some_and(|a| {
                    left_op.apply(a, left_lit) && right_op.apply(a, right_lit)
                })
            })
            .count() as i64;

        let powql = engine
            .execute_powql(&format!(
                "count(T filter .mirror {} {left_lit} and .mirror {} {right_lit})",
                left_op.powql(),
                right_op.powql()
            ))
            .unwrap();
        prop_assert_eq!(scalar_count(&powql), expected, "PowQL conjunction disagreed with the model");

        let sql = engine
            .execute_sql(&format!(
                "SELECT COUNT(*) FROM T WHERE mirror {} {left_lit} AND mirror {} {right_lit}",
                left_op.sql(),
                right_op.sql()
            ))
            .unwrap();
        prop_assert_eq!(scalar_count(&sql), expected, "SQL conjunction disagreed with the model");
    }

    /// Adding an index must not change any answer. This is the property that
    /// the datetime bug violated: the index path and the scan path disagreed,
    /// so the result depended on physical schema rather than on the data.
    #[test]
    fn an_index_never_changes_the_answer(
        rows in prop::collection::vec(row_strategy(), 1..12),
        op in op_strategy(),
        literal in 0i64..12,
    ) {
        let mut engine = seeded_engine(&rows, "index");
        let queries: Vec<String> = ["mirror", "ts"]
            .iter()
            .map(|column| format!("count(T filter .{column} {} {literal})", op.powql()))
            .collect();

        let before: Vec<i64> = queries
            .iter()
            .map(|query| scalar_count(&engine.execute_powql(query).unwrap()))
            .collect();

        engine.execute_powql("alter T add index .mirror").unwrap();
        engine.execute_powql("alter T add index .ts").unwrap();

        for (query, unindexed) in queries.iter().zip(&before) {
            let indexed = scalar_count(&engine.execute_powql(query).unwrap());
            prop_assert_eq!(
                indexed,
                *unindexed,
                "`{}` answered differently once an index existed",
                query
            );
        }
    }

    /// The same invariant across an `or` chain. This case exists specifically
    /// to reach the GENERIC evaluator: the compiled predicate path handles
    /// `and` chains of simple leaves and bails on disjunction, so an `and`-only
    /// suite silently tests the compiled path twice and never checks that the
    /// two agree. Verified by mutation: reverting the generic datetime
    /// comparison fix leaves the `and` cases passing and fails these.
    #[test]
    fn powql_sql_and_model_agree_on_a_disjunction(
        rows in prop::collection::vec(row_strategy(), 1..12),
        left_op in op_strategy(),
        left_lit in 0i64..12,
        right_op in op_strategy(),
        right_lit in 0i64..12,
    ) {
        let mut engine = seeded_engine(&rows, "disj");

        let expected = rows
            .iter()
            .filter(|row| {
                row.a.is_some_and(|a| {
                    left_op.apply(a, left_lit) || right_op.apply(a, right_lit)
                })
            })
            .count() as i64;

        for column in ["mirror", "ts"] {
            let powql = engine
                .execute_powql(&format!(
                    "count(T filter .{column} {} {left_lit} or .{column} {} {right_lit})",
                    left_op.powql(),
                    right_op.powql()
                ))
                .unwrap();
            prop_assert_eq!(
                scalar_count(&powql),
                expected,
                "PowQL disjunction on `{}` disagreed with the model",
                column
            );

            let sql = engine
                .execute_sql(&format!(
                    "SELECT COUNT(*) FROM T WHERE {column} {} {left_lit} OR {column} {} {right_lit}",
                    left_op.sql(),
                    right_op.sql()
                ))
                .unwrap();
            prop_assert_eq!(
                scalar_count(&sql),
                expected,
                "SQL disjunction on `{}` disagreed with the model",
                column
            );
        }
    }

    /// `count(T)` counts rows; `count(T { .col })` counts non-null values of
    /// that column. The two frontends must agree on both, which is the exact
    /// pair that diverged before this release.
    #[test]
    fn count_rows_and_count_column_agree_across_frontends(
        rows in prop::collection::vec(row_strategy(), 1..12),
    ) {
        let mut engine = seeded_engine(&rows, "count");

        let total = rows.len() as i64;
        let present = rows.iter().filter(|row| row.a.is_some()).count() as i64;

        prop_assert_eq!(scalar_count(&engine.execute_powql("count(T)").unwrap()), total);
        prop_assert_eq!(
            scalar_count(&engine.execute_sql("SELECT COUNT(*) FROM T").unwrap()),
            total
        );
        prop_assert_eq!(
            scalar_count(&engine.execute_powql("count(T { .mirror })").unwrap()),
            present
        );
        prop_assert_eq!(
            scalar_count(&engine.execute_sql("SELECT COUNT(mirror) FROM T").unwrap()),
            present
        );
    }
}
