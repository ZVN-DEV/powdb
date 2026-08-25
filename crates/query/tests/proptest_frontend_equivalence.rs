//! Property tests for the invariants this engine keeps breaking by hand.
//!
//! 1. The PowQL and SQL frontends lower to the same plan tree, so any query
//!    expressible in both must return the same answer. Two releases in a row
//!    shipped a case where they did not (`COUNT(col)` counted rows on one side
//!    and non-null values on the other), and both were found by a person
//!    noticing, not by a test.
//! 2. Whichever access path the engine picks (compiled predicate leaf, generic
//!    row decode, or an index probe) must agree with a plain model of the data.
//!    The v0.20.0 datetime bug was exactly a disagreement of this kind: the
//!    compiled and generic paths were consistent with each other but not with
//!    the index, and none of them was consistent with arithmetic.
//!
//! The oracle deliberately reimplements PowDB's documented two-valued NULL
//! rule (a comparison against a missing value is false, never "unknown") rather
//! than importing anything from the engine, so a semantic change has to be a
//! deliberate edit here and not a silently shared helper.
//!
//! ## Why nothing here funnels through a scalar
//!
//! This file used to compare answers through a `scalar_count` helper that
//! panicked on anything that was not `Scalar(Int)`. That made the count the
//! only thing under test: a query could return the right number of the wrong
//! rows, in the wrong order, under the wrong column names, and every property
//! still passed. Every property now compares full rows, and the helpers refuse
//! to lose information on the way.
//!
//! ## Physical arrangements
//!
//! Each property runs its query against four engines holding identical rows:
//! unindexed and indexed, each with the executor fast paths on and forced off
//! (`Engine::set_force_generic_path`, `testing` feature). All four must agree
//! before the answer is compared with the model, so a property failure names
//! either a model disagreement or a path disagreement, never a coincidence.

#![cfg(feature = "testing")]

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::pj1::parse_json_text;
use powdb_storage::types::Value;
use proptest::prelude::*;

/// One generated row.
///
/// `a` is stored in the `mirror` (int) column, the `ts` (datetime) column and
/// as `doc->k` (json), so every predicate is exercised against three physically
/// different encodings of the identical value: they must agree.
#[derive(Debug, Clone)]
struct GenRow {
    a: Option<i64>,
    label: Option<String>,
    f: Option<f64>,
    b: Option<bool>,
}

impl GenRow {
    /// The primary key, which is the row's 1-based position.
    fn id(index: usize) -> i64 {
        index as i64 + 1
    }

    /// Children this row owns in the child table. Rows with no `a` own none,
    /// odd values own one, even values own two, so the generated data covers
    /// empty, single and multi-child parents.
    fn child_weights(&self) -> Vec<i64> {
        match self.a {
            None => Vec::new(),
            Some(a) if a % 2 != 0 => vec![a],
            Some(a) => vec![a, a + 1],
        }
    }
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

    fn apply<T: PartialOrd>(self, left: T, right: T) -> bool {
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

/// Float literals restricted to values with an exact binary representation, so
/// a model comparison and an engine comparison can never disagree for a
/// rounding reason that has nothing to do with the property under test.
const FLOATS: &[f64] = &[-2.5, -0.5, 0.0, 0.5, 1.0, 2.5, 4.0];

/// Values are drawn from a deliberately narrow band so that generated
/// predicates land on interesting boundaries (empty result, every row, and the
/// partial cases) instead of almost always matching everything.
fn row_strategy() -> impl Strategy<Value = GenRow> {
    (
        prop::option::of(0i64..12),
        prop::option::of("[a-c]{1,3}"),
        prop::option::of(prop::sample::select(FLOATS)),
        prop::option::of(any::<bool>()),
    )
        .prop_map(|(a, label, f, b)| GenRow { a, label, f, b })
}

fn rows_strategy() -> impl Strategy<Value = Vec<GenRow>> {
    prop::collection::vec(row_strategy(), 1..12)
}

// ─── engines ────────────────────────────────────────────────────────────────

const SCHEMA: &str = "type T { required unique id: int, mirror: int, ts: datetime, \
                      f: float, b: bool, label: str, doc: json }";
const CHILD_SCHEMA: &str = "type C { required unique cid: int, ref_id: int, w: int }";

/// An engine plus the temp directory it lives in.
///
/// The previous version of this file derived its directory name from
/// `rows as *const _`, betting that two live `&[GenRow]` slices never share an
/// address. Nothing guarantees that: proptest reuses buffers between cases, and
/// a recycled allocation silently reopens a previous case's database, so a
/// property could pass against the wrong rows. A real temp directory removes
/// the bet.
struct Db {
    // Declared first so it drops first: `Engine::drop` checkpoints, and the
    // directory has to still exist when that runs.
    engine: Engine,
    _dir: tempfile::TempDir,
}

/// The four physical arrangements every query is run against.
struct Arrangements {
    unindexed: Db,
    unindexed_generic: Db,
    indexed: Db,
    indexed_generic: Db,
}

impl Arrangements {
    fn build(rows: &[GenRow]) -> Arrangements {
        Arrangements {
            unindexed: seeded(rows, false, false),
            unindexed_generic: seeded(rows, false, true),
            indexed: seeded(rows, true, false),
            indexed_generic: seeded(rows, true, true),
        }
    }

    fn each(&mut self) -> [(&'static str, &mut Db); 4] {
        [
            ("unindexed", &mut self.unindexed),
            ("unindexed+generic", &mut self.unindexed_generic),
            ("indexed", &mut self.indexed),
            ("indexed+generic", &mut self.indexed_generic),
        ]
    }
}

fn seeded(rows: &[GenRow], indexed: bool, force_generic: bool) -> Db {
    let dir = tempfile::tempdir().expect("temp dir for a property engine");
    let mut engine = Engine::new(dir.path()).expect("engine opens over a fresh temp dir");
    ddl(&mut engine, SCHEMA);
    ddl(&mut engine, CHILD_SCHEMA);
    ddl(&mut engine, "link C.parent -> T on ref_id = id");
    ddl(&mut engine, "link T.kids -> C on id = ref_id");

    let mut child_id = 1000i64;
    for (index, row) in rows.iter().enumerate() {
        let id = GenRow::id(index);
        let mut fields = vec![format!("id := {id}")];
        if let Some(a) = row.a {
            fields.push(format!("mirror := {a}"));
            fields.push(format!("ts := {a}"));
            fields.push(format!("doc := \"{{\\\"k\\\": {a}}}\""));
        } else {
            fields.push("doc := \"{\\\"k\\\": null}\"".to_string());
        }
        if let Some(label) = &row.label {
            fields.push(format!("label := \"{label}\""));
        }
        if let Some(f) = row.f {
            fields.push(format!("f := {f:?}"));
        }
        if let Some(b) = row.b {
            fields.push(format!("b := {b}"));
        }
        ddl(
            &mut engine,
            &format!("insert T {{ {} }}", fields.join(", ")),
        );

        for weight in row.child_weights() {
            child_id += 1;
            ddl(
                &mut engine,
                &format!("insert C {{ cid := {child_id}, ref_id := {id}, w := {weight} }}"),
            );
        }
    }

    if indexed {
        for statement in [
            "alter T add index .mirror",
            "alter T add index .ts",
            "alter T add index .label",
            "alter C add index .ref_id",
        ] {
            ddl(&mut engine, statement);
        }
    }
    engine.set_force_generic_path(force_generic);
    Db { engine, _dir: dir }
}

fn ddl(engine: &mut Engine, statement: &str) {
    engine
        .execute_powql(statement)
        .unwrap_or_else(|err| panic!("fixture statement `{statement}` failed: {err}"));
}

// ─── running and comparing ──────────────────────────────────────────────────

/// A query's answer, kept whole. `QueryResult` has no `PartialEq`, and the
/// point of this file is that nothing gets narrowed to a number on the way to
/// an assertion.
#[derive(Debug, Clone, PartialEq)]
enum Answer {
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    Scalar(Value),
    Modified(u64),
    Other(String),
    Error(String),
}

impl Answer {
    fn sorted(&self) -> Answer {
        match self {
            Answer::Rows { columns, rows } => {
                let mut rows = rows.clone();
                rows.sort();
                Answer::Rows {
                    columns: columns.clone(),
                    rows,
                }
            }
            other => other.clone(),
        }
    }

    /// The rows, or a test failure naming what came back instead. Used by
    /// properties that model row content directly.
    fn expect_rows(&self) -> Result<&[Vec<Value>], TestCaseError> {
        match self {
            Answer::Rows { rows, .. } => Ok(rows),
            other => Err(TestCaseError::fail(format!("expected rows, got {other:?}"))),
        }
    }
}

fn answer(engine: &mut Engine, query: &str) -> Answer {
    to_answer(engine.execute_powql(query))
}

fn answer_sql(engine: &mut Engine, query: &str) -> Answer {
    to_answer(engine.execute_sql(query))
}

fn to_answer(result: Result<QueryResult, powdb_query::result::QueryError>) -> Answer {
    match result {
        Ok(QueryResult::Rows { columns, rows }) => Answer::Rows { columns, rows },
        Ok(QueryResult::Scalar(value)) => Answer::Scalar(value),
        Ok(QueryResult::Modified(n)) => Answer::Modified(n),
        Ok(QueryResult::Created(name)) => Answer::Other(format!("created {name}")),
        Ok(QueryResult::Executed { message }) => Answer::Other(message),
        Err(err) => Answer::Error(err.to_string()),
    }
}

/// Run `query` against all four arrangements, require them to agree, and hand
/// back the single answer. `ordered` decides whether row order is part of the
/// comparison: an index probe and a heap scan legitimately visit rows in a
/// different order, so only a query that asked for an order is held to one.
fn agreed(
    arrangements: &mut Arrangements,
    query: &str,
    ordered: bool,
) -> Result<Answer, TestCaseError> {
    let mut baseline: Option<(&'static str, Answer)> = None;
    for (name, db) in arrangements.each() {
        let observed = answer(&mut db.engine, query);
        match &baseline {
            None => baseline = Some((name, observed)),
            Some((base_name, base)) => {
                let (left, right) = if ordered {
                    (base.clone(), observed.clone())
                } else {
                    (base.sorted(), observed.sorted())
                };
                if left != right {
                    return Err(TestCaseError::fail(format!(
                        "`{query}` answered differently under {name} than under {base_name}:\n\
                         {base_name}: {left:?}\n{name}: {right:?}"
                    )));
                }
            }
        }
    }
    let (_, base) = baseline.expect("four arrangements are always built");
    Ok(base)
}

/// PowQL and SQL must agree. Both sides are run across every arrangement first,
/// so a mismatch here is a frontend difference and not a path difference.
fn agreed_across_frontends(
    arrangements: &mut Arrangements,
    powql: &str,
    sql: &str,
    ordered: bool,
) -> Result<Answer, TestCaseError> {
    let from_powql = agreed(arrangements, powql, ordered)?;
    let mut sql_baseline: Option<Answer> = None;
    for (name, db) in arrangements.each() {
        let observed = answer_sql(&mut db.engine, sql);
        match &sql_baseline {
            None => sql_baseline = Some(observed),
            Some(base) => {
                let (left, right) = if ordered {
                    (base.clone(), observed)
                } else {
                    (base.sorted(), observed.sorted())
                };
                if left != right {
                    return Err(TestCaseError::fail(format!(
                        "`{sql}` answered differently under {name}: {left:?} vs {right:?}"
                    )));
                }
            }
        }
    }
    let from_sql = sql_baseline.expect("four arrangements are always built");
    let (left, right) = if ordered {
        (from_powql.clone(), from_sql)
    } else {
        (from_powql.sorted(), from_sql.sorted())
    };
    if left != right {
        return Err(TestCaseError::fail(format!(
            "PowQL `{powql}` and SQL `{sql}` disagree:\nPowQL: {left:?}\nSQL:   {right:?}"
        )));
    }
    Ok(from_powql)
}

/// The model's answer for an id-only projection.
fn expected_id_rows(rows: &[GenRow], keep: impl Fn(&GenRow) -> bool) -> Vec<Vec<Value>> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| keep(row))
        .map(|(index, _)| vec![Value::Int(GenRow::id(index))])
        .collect()
}

fn sorted_rows(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort();
    rows
}

// ─── properties ─────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// PowQL, SQL and the model must agree on a comparison against an int
    /// column, a datetime column and a json path all holding the same value.
    /// Full rows, not a count: a wrong-rows bug that happens to preserve the
    /// cardinality is exactly the shape the old `scalar_count` funnel hid.
    #[test]
    fn powql_sql_and_model_agree_on_a_single_comparison(
        rows in rows_strategy(),
        op in op_strategy(),
        literal in 0i64..12,
    ) {
        let mut arrangements = Arrangements::build(&rows);

        // Documented two-valued NULL rule: a comparison against a missing
        // value is false, so the row is excluded, including for `!=`.
        let expected = sorted_rows(expected_id_rows(&rows, |row| {
            row.a.is_some_and(|a| op.apply(a, literal))
        }));

        for column in ["mirror", "ts"] {
            let observed = agreed_across_frontends(
                &mut arrangements,
                &format!("T filter .{column} {} {literal} {{ .id }}", op.powql()),
                &format!("SELECT id FROM T WHERE {column} {} {literal}", op.sql()),
                false,
            )?;
            prop_assert_eq!(
                sorted_rows(observed.expect_rows()?.to_vec()),
                expected.clone(),
                "`{} {} {}` disagreed with the model",
                column, op.powql(), literal
            );
        }

        // The json encoding of the same number has no SQL spelling, so it is
        // compared against the model directly.
        let observed = agreed(
            &mut arrangements,
            &format!("T filter .doc->k {} {literal} {{ .id }}", op.powql()),
            false,
        )?;
        prop_assert_eq!(
            sorted_rows(observed.expect_rows()?.to_vec()),
            expected,
            "`doc->k {} {}` disagreed with the model",
            op.powql(), literal
        );
    }

    /// The same invariant across an `and` chain, which is the shape the
    /// compiled predicate path specifically handles.
    #[test]
    fn powql_sql_and_model_agree_on_a_conjunction(
        rows in rows_strategy(),
        left_op in op_strategy(),
        left_lit in 0i64..12,
        right_op in op_strategy(),
        right_lit in 0i64..12,
    ) {
        let mut arrangements = Arrangements::build(&rows);

        let expected = sorted_rows(expected_id_rows(&rows, |row| {
            row.a.is_some_and(|a| {
                left_op.apply(a, left_lit) && right_op.apply(a, right_lit)
            })
        }));

        let observed = agreed_across_frontends(
            &mut arrangements,
            &format!(
                "T filter .mirror {} {left_lit} and .mirror {} {right_lit} {{ .id }}",
                left_op.powql(), right_op.powql()
            ),
            &format!(
                "SELECT id FROM T WHERE mirror {} {left_lit} AND mirror {} {right_lit}",
                left_op.sql(), right_op.sql()
            ),
            false,
        )?;
        prop_assert_eq!(
            sorted_rows(observed.expect_rows()?.to_vec()),
            expected,
            "the conjunction disagreed with the model"
        );
    }

    /// The same invariant across an `or` chain. This case exists specifically
    /// to reach the GENERIC evaluator: the compiled predicate path handles
    /// `and` chains of simple leaves and bails on disjunction, so an `and`-only
    /// suite silently tests the compiled path twice and never checks that the
    /// two agree.
    #[test]
    fn powql_sql_and_model_agree_on_a_disjunction(
        rows in rows_strategy(),
        left_op in op_strategy(),
        left_lit in 0i64..12,
        right_op in op_strategy(),
        right_lit in 0i64..12,
    ) {
        let mut arrangements = Arrangements::build(&rows);

        let expected = sorted_rows(expected_id_rows(&rows, |row| {
            row.a.is_some_and(|a| {
                left_op.apply(a, left_lit) || right_op.apply(a, right_lit)
            })
        }));

        for column in ["mirror", "ts"] {
            let observed = agreed_across_frontends(
                &mut arrangements,
                &format!(
                    "T filter .{column} {} {left_lit} or .{column} {} {right_lit} {{ .id }}",
                    left_op.powql(), right_op.powql()
                ),
                &format!(
                    "SELECT id FROM T WHERE {column} {} {left_lit} OR {column} {} {right_lit}",
                    left_op.sql(), right_op.sql()
                ),
                false,
            )?;
            prop_assert_eq!(
                sorted_rows(observed.expect_rows()?.to_vec()),
                expected.clone(),
                "the disjunction on `{}` disagreed with the model",
                column
            );
        }
    }

    /// Float and bool columns: the generated schema reaches past
    /// `{str, int, datetime}`, because a coercion rule that only holds for the
    /// three original types is not a coercion rule.
    #[test]
    fn powql_sql_and_model_agree_on_float_and_bool(
        rows in rows_strategy(),
        op in op_strategy(),
        literal_index in 0usize..FLOATS.len(),
        flag in any::<bool>(),
    ) {
        let mut arrangements = Arrangements::build(&rows);
        let literal = FLOATS[literal_index];

        let expected = sorted_rows(expected_id_rows(&rows, |row| {
            row.f.is_some_and(|f| op.apply(f, literal))
        }));
        let observed = agreed_across_frontends(
            &mut arrangements,
            &format!("T filter .f {} {literal:?} {{ .id }}", op.powql()),
            &format!("SELECT id FROM T WHERE f {} {literal:?}", op.sql()),
            false,
        )?;
        prop_assert_eq!(
            sorted_rows(observed.expect_rows()?.to_vec()),
            expected,
            "`f {} {:?}` disagreed with the model",
            op.powql(), literal
        );

        let expected = sorted_rows(expected_id_rows(&rows, |row| {
            row.b.is_some_and(|b| b == flag)
        }));
        let observed = agreed_across_frontends(
            &mut arrangements,
            &format!("T filter .b = {flag} {{ .id }}"),
            &format!("SELECT id FROM T WHERE b = {flag}"),
            false,
        )?;
        prop_assert_eq!(
            sorted_rows(observed.expect_rows()?.to_vec()),
            expected,
            "`b = {}` disagreed with the model",
            flag
        );
    }

    /// String comparison, including the ordering operators that a byte-level
    /// compiled leaf and a decoded `Value::cmp` could plausibly disagree on.
    #[test]
    fn powql_sql_and_model_agree_on_strings(
        rows in rows_strategy(),
        op in op_strategy(),
        literal in "[a-c]{1,3}",
    ) {
        let mut arrangements = Arrangements::build(&rows);

        let expected = sorted_rows(expected_id_rows(&rows, |row| {
            row.label
                .as_deref()
                .is_some_and(|label| op.apply(label, literal.as_str()))
        }));
        let observed = agreed_across_frontends(
            &mut arrangements,
            &format!("T filter .label {} \"{literal}\" {{ .id }}", op.powql()),
            &format!("SELECT id FROM T WHERE label {} '{literal}'", op.sql()),
            false,
        )?;
        prop_assert_eq!(
            sorted_rows(observed.expect_rows()?.to_vec()),
            expected,
            "`label {} {}` disagreed with the model",
            op.powql(), literal
        );
    }

    /// Adding an index must not change any answer, and neither must switching
    /// off the fast paths. `agreed` already enforces both; this property makes
    /// the whole projected row the unit of comparison rather than the key, so a
    /// path that returns the right ids with a wrong payload column is caught.
    #[test]
    fn no_arrangement_changes_a_full_row(
        rows in rows_strategy(),
        op in op_strategy(),
        literal in 0i64..12,
    ) {
        let mut arrangements = Arrangements::build(&rows);
        let observed = agreed(
            &mut arrangements,
            &format!(
                "T filter .mirror {} {literal} {{ .id, .mirror, .ts, .f, .b, .label }}",
                op.powql()
            ),
            false,
        )?;

        let expected: Vec<Vec<Value>> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.a.is_some_and(|a| op.apply(a, literal)))
            .map(|(index, row)| {
                vec![
                    Value::Int(GenRow::id(index)),
                    row.a.map(Value::Int).unwrap_or(Value::Empty),
                    row.a.map(Value::DateTime).unwrap_or(Value::Empty),
                    row.f.map(Value::Float).unwrap_or(Value::Empty),
                    row.b.map(Value::Bool).unwrap_or(Value::Empty),
                    row.label
                        .clone()
                        .map(Value::Str)
                        .unwrap_or(Value::Empty),
                ]
            })
            .collect();
        prop_assert_eq!(
            sorted_rows(observed.expect_rows()?.to_vec()),
            sorted_rows(expected),
            "a full projected row disagreed with the model"
        );
    }

    /// `count(T)` counts rows; `count(T { .col })` counts non-null values of
    /// that column. The two frontends must agree on both, which is the exact
    /// pair that diverged before v0.19. The other aggregates are modelled here
    /// too, since they share the single-column fast path.
    #[test]
    fn aggregates_agree_across_frontends_and_with_the_model(
        rows in rows_strategy(),
    ) {
        let mut arrangements = Arrangements::build(&rows);

        let total = rows.len() as i64;
        let present: Vec<i64> = rows.iter().filter_map(|row| row.a).collect();

        let observed = agreed_across_frontends(
            &mut arrangements, "count(T)", "SELECT COUNT(*) FROM T", false)?;
        prop_assert_eq!(observed, Answer::Scalar(Value::Int(total)));

        let observed = agreed_across_frontends(
            &mut arrangements, "count(T { .mirror })", "SELECT COUNT(mirror) FROM T", false)?;
        prop_assert_eq!(observed, Answer::Scalar(Value::Int(present.len() as i64)));

        let observed = agreed_across_frontends(
            &mut arrangements, "sum(T { .mirror })", "SELECT SUM(mirror) FROM T", false)?;
        let expected_sum = if present.is_empty() {
            Value::Empty
        } else {
            Value::Int(present.iter().sum())
        };
        prop_assert_eq!(observed, Answer::Scalar(expected_sum));

        let observed = agreed_across_frontends(
            &mut arrangements, "min(T { .mirror })", "SELECT MIN(mirror) FROM T", false)?;
        let expected_min = present.iter().copied().min().map(Value::Int).unwrap_or(Value::Empty);
        prop_assert_eq!(observed, Answer::Scalar(expected_min));

        let observed = agreed_across_frontends(
            &mut arrangements, "max(T { .mirror })", "SELECT MAX(mirror) FROM T", false)?;
        let expected_max = present.iter().copied().max().map(Value::Int).unwrap_or(Value::Empty);
        prop_assert_eq!(observed, Answer::Scalar(expected_max));

        let observed = agreed_across_frontends(
            &mut arrangements, "avg(T { .mirror })", "SELECT AVG(mirror) FROM T", false)?;
        let expected_avg = if present.is_empty() {
            Value::Empty
        } else {
            let sum: i64 = present.iter().sum();
            Value::Float(sum as f64 / present.len() as f64)
        };
        prop_assert_eq!(observed, Answer::Scalar(expected_avg));
    }

    /// Order is a contract: `order .mirror` sorts ascending with nulls last,
    /// and `desc` reverses only the non-null values. This is the one place row
    /// order is compared exactly across every arrangement.
    #[test]
    fn order_matches_the_model_including_nulls_last(
        rows in rows_strategy(),
        descending in any::<bool>(),
    ) {
        let mut arrangements = Arrangements::build(&rows);
        let direction = if descending { " desc" } else { "" };
        // `.id` breaks ties so the expected order is total, and the property
        // tests ordering rather than the engine's tie-breaking freedom.
        let observed = agreed(
            &mut arrangements,
            &format!("T order .mirror{direction}, .id {{ .id, .mirror }}"),
            true,
        )?;

        let mut expected: Vec<(Option<i64>, i64)> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| (row.a, GenRow::id(index)))
            .collect();
        expected.sort_by(|left, right| {
            use std::cmp::Ordering;
            let key = match (left.0, right.0) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) if descending => b.cmp(&a),
                (Some(a), Some(b)) => a.cmp(&b),
            };
            key.then(left.1.cmp(&right.1))
        });
        let expected: Vec<Vec<Value>> = expected
            .into_iter()
            .map(|(a, id)| vec![Value::Int(id), a.map(Value::Int).unwrap_or(Value::Empty)])
            .collect();
        prop_assert_eq!(observed.expect_rows()?.to_vec(), expected, "order disagreed with the model");
    }

    /// `limit` and `offset` must slice the ordered result, not reorder or
    /// resample it. The top-N heap fast path only fires with a limit, so this
    /// is the property that compares it against the full sort.
    #[test]
    fn limit_and_offset_slice_the_ordered_result(
        rows in rows_strategy(),
        limit in 0usize..14,
        offset in 0usize..6,
        descending in any::<bool>(),
    ) {
        let mut arrangements = Arrangements::build(&rows);
        let direction = if descending { " desc" } else { "" };
        let full = agreed(
            &mut arrangements,
            &format!("T order .mirror{direction}, .id {{ .id }}"),
            true,
        )?;
        let full = full.expect_rows()?.to_vec();

        let limited = agreed(
            &mut arrangements,
            &format!("T order .mirror{direction}, .id limit {limit} {{ .id }}"),
            true,
        )?;
        prop_assert_eq!(
            limited.expect_rows()?.to_vec(),
            full.iter().take(limit).cloned().collect::<Vec<_>>(),
            "`limit {}` is not the prefix of the ordered result",
            limit
        );

        let sliced = agreed(
            &mut arrangements,
            &format!("T order .mirror{direction}, .id offset {offset} limit {limit} {{ .id }}"),
            true,
        )?;
        prop_assert_eq!(
            sliced.expect_rows()?.to_vec(),
            full.iter().skip(offset).take(limit).cloned().collect::<Vec<_>>(),
            "`offset {} limit {}` is not the matching slice",
            offset, limit
        );
    }

    /// `limit` without `order` still has a contract: on any one engine it is a
    /// prefix of the same query without the limit. This is the shape that
    /// reaches the streaming projection fast path (the ordered cases above
    /// reach the top-N heap instead), so without it an off-by-one in that loop
    /// is invisible to the whole suite.
    ///
    /// The prefix relation holds per engine, not across engines: an index scan
    /// and a heap scan legitimately visit rows in different orders when the
    /// query never asked for one, so comparing their prefixes would only
    /// measure that.
    #[test]
    fn limit_without_order_is_a_prefix_of_the_unlimited_result(
        rows in rows_strategy(),
        limit in 0usize..14,
        literal in 0i64..12,
    ) {
        let mut arrangements = Arrangements::build(&rows);
        let pairs = [
            ("T { .id, .mirror }".to_string(), format!("T limit {limit} {{ .id, .mirror }}")),
            (
                format!("T filter .mirror >= {literal} {{ .id }}"),
                format!("T filter .mirror >= {literal} limit {limit} {{ .id }}"),
            ),
        ];
        for (name, db) in arrangements.each() {
            for (unlimited, limited) in &pairs {
                let full = answer(&mut db.engine, unlimited);
                let full = full.expect_rows()?.to_vec();
                let sliced = answer(&mut db.engine, limited);
                prop_assert_eq!(
                    sliced.expect_rows()?.to_vec(),
                    full.iter().take(limit).cloned().collect::<Vec<_>>(),
                    "`{}` is not a prefix of `{}` under {}",
                    limited, unlimited, name
                );
            }
        }
    }

    /// Grouped aggregation and `having`. The model groups the same rows in
    /// plain Rust, so a grouping change has to be a deliberate edit here.
    #[test]
    fn group_and_having_match_the_model(
        rows in rows_strategy(),
        threshold in 0i64..4,
    ) {
        use std::collections::BTreeMap;

        let mut arrangements = Arrangements::build(&rows);

        // Sum is `None` until a non-null value contributes, so the model can
        // say Empty (SQL NULL) for an all-null group, as the engine does.
        let mut groups: BTreeMap<Option<i64>, (i64, Option<i64>)> = BTreeMap::new();
        for row in &rows {
            let entry = groups.entry(row.a).or_insert((0, None));
            entry.0 += 1;
            if let Some(v) = row.a {
                *entry.1.get_or_insert(0) += v;
            }
        }

        let observed = agreed(
            &mut arrangements,
            "T group .mirror { .mirror, c: count(*), s: sum(.mirror) }",
            false,
        )?;
        let expected: Vec<Vec<Value>> = groups
            .iter()
            .map(|(key, (count, sum))| {
                vec![
                    key.map(Value::Int).unwrap_or(Value::Empty),
                    Value::Int(*count),
                    // A group whose every `.mirror` is null sums zero
                    // non-null values, which is Empty (SQL NULL) as of
                    // v0.27 — the same rule as min/max/avg. Pinned by
                    // `every_empty_set_aggregate_is_null` below.
                    sum.map(Value::Int).unwrap_or(Value::Empty),
                ]
            })
            .collect();
        prop_assert_eq!(
            sorted_rows(observed.expect_rows()?.to_vec()),
            sorted_rows(expected),
            "grouping disagreed with the model"
        );

        let observed = agreed(
            &mut arrangements,
            &format!("T group .mirror having count(*) > {threshold} {{ .mirror, c: count(*) }}"),
            false,
        )?;
        let expected: Vec<Vec<Value>> = groups
            .iter()
            .filter(|(_, (count, _))| *count > threshold)
            .map(|(key, (count, _))| {
                vec![key.map(Value::Int).unwrap_or(Value::Empty), Value::Int(*count)]
            })
            .collect();
        prop_assert_eq!(
            sorted_rows(observed.expect_rows()?.to_vec()),
            sorted_rows(expected),
            "`having count(*) > {}` disagreed with the model",
            threshold
        );
    }

    /// An inner join over the parent/child fixture, modelled as a plain
    /// cross-filter. Rows whose key is missing must not join to anything.
    #[test]
    fn inner_join_matches_the_model(rows in rows_strategy()) {
        let mut arrangements = Arrangements::build(&rows);
        let observed = agreed(
            &mut arrangements,
            "T as t inner join C as c on t.id = c.ref_id { t.id, c.w }",
            false,
        )?;

        let expected: Vec<Vec<Value>> = rows
            .iter()
            .enumerate()
            .flat_map(|(index, row)| {
                let id = GenRow::id(index);
                row.child_weights()
                    .into_iter()
                    .map(move |w| vec![Value::Int(id), Value::Int(w)])
            })
            .collect();
        prop_assert_eq!(
            sorted_rows(observed.expect_rows()?.to_vec()),
            sorted_rows(expected),
            "the join disagreed with the model"
        );
    }

    /// A nested projection emits exactly one row per parent with the children
    /// assembled into a json array: no row explosion, no null sentinel row, and
    /// `[]` for a childless parent.
    #[test]
    fn nested_projection_matches_the_model(rows in rows_strategy()) {
        let mut arrangements = Arrangements::build(&rows);
        let observed = agreed(
            &mut arrangements,
            "T as t order t.id { t.id, kids: C as c filter c.ref_id = t.id order c.w { c.w } }",
            true,
        )?;

        let expected: Vec<Vec<Value>> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let children = row
                    .child_weights()
                    .into_iter()
                    .map(|w| format!("{{\"w\":{w}}}"))
                    .collect::<Vec<_>>()
                    .join(",");
                vec![
                    Value::Int(GenRow::id(index)),
                    Value::Json(
                        parse_json_text(&format!("[{children}]"))
                            .expect("the model builds valid json")
                            .into(),
                    ),
                ]
            })
            .collect();
        prop_assert_eq!(
            observed.expect_rows()?.to_vec(),
            expected,
            "the nested projection disagreed with the model"
        );
    }

    /// An entity-link traversal must answer exactly what the equivalent join
    /// answers, for every arrangement. `c.parent.mirror` is a scalar hop
    /// through a unique target key.
    #[test]
    fn entity_link_traversal_matches_the_model(rows in rows_strategy()) {
        let mut arrangements = Arrangements::build(&rows);
        let observed = agreed(
            &mut arrangements,
            "C as c order c.cid { c.cid, c.w, c.parent.mirror }",
            true,
        )?;

        let mut expected: Vec<Vec<Value>> = Vec::new();
        let mut child_id = 1000i64;
        for row in rows.iter() {
            for w in row.child_weights() {
                child_id += 1;
                expected.push(vec![
                    Value::Int(child_id),
                    Value::Int(w),
                    row.a.map(Value::Int).unwrap_or(Value::Empty),
                ]);
            }
        }
        prop_assert_eq!(
            observed.expect_rows()?.to_vec(),
            expected,
            "the link traversal disagreed with the model"
        );
    }

    /// Mutations. A filtered UPDATE must touch every matching row and no
    /// other, and an UPDATE that fails partway must not leave a partial write:
    /// both the reported count and the resulting table are compared across
    /// arrangements and against the model.
    #[test]
    fn filtered_update_matches_the_model(
        rows in rows_strategy(),
        op in op_strategy(),
        literal in 0i64..12,
    ) {
        let mut arrangements = Arrangements::build(&rows);
        let mutation = format!(
            "T filter .mirror {} {literal} update {{ label := \"touched\" }}",
            op.powql()
        );
        let matched = rows
            .iter()
            .filter(|row| row.a.is_some_and(|a| op.apply(a, literal)))
            .count() as u64;

        let effect = agreed(&mut arrangements, &mutation, false)?;
        prop_assert_eq!(effect, Answer::Modified(matched), "wrong number of rows updated");

        let after = agreed(&mut arrangements, "T order .id { .id, .label }", true)?;
        let expected: Vec<Vec<Value>> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let label = if row.a.is_some_and(|a| op.apply(a, literal)) {
                    Value::Str("touched".into())
                } else {
                    row.label
                        .clone()
                        .map(Value::Str)
                        .unwrap_or(Value::Empty)
                };
                vec![Value::Int(GenRow::id(index)), label]
            })
            .collect();
        prop_assert_eq!(
            after.expect_rows()?.to_vec(),
            expected,
            "the table after the update disagreed with the model"
        );
    }

    /// The delete counterpart. A filtered DELETE must remove exactly the
    /// matching rows and leave the rest byte-identical.
    #[test]
    fn filtered_delete_matches_the_model(
        rows in rows_strategy(),
        op in op_strategy(),
        literal in 0i64..12,
    ) {
        let mut arrangements = Arrangements::build(&rows);
        let mutation = format!("T filter .mirror {} {literal} delete", op.powql());
        let matched = rows
            .iter()
            .filter(|row| row.a.is_some_and(|a| op.apply(a, literal)))
            .count() as u64;

        let effect = agreed(&mut arrangements, &mutation, false)?;
        prop_assert_eq!(effect, Answer::Modified(matched), "wrong number of rows deleted");

        let after = agreed(&mut arrangements, "T order .id { .id, .mirror }", true)?;
        let expected: Vec<Vec<Value>> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| !row.a.is_some_and(|a| op.apply(a, literal)))
            .map(|(index, row)| {
                vec![
                    Value::Int(GenRow::id(index)),
                    row.a.map(Value::Int).unwrap_or(Value::Empty),
                ]
            })
            .collect();
        prop_assert_eq!(
            after.expect_rows()?.to_vec(),
            expected,
            "the table after the delete disagreed with the model"
        );
    }
}

/// PowDB's empty-set aggregates are pinned here so a change to them shows up
/// as a failure rather than as a surprise in someone's report.
///
/// As of v0.27 all four — `sum`, `min`, `max`, `avg` — return null over zero
/// non-null values, exactly as SQL specifies. (`sum` answered `0` before,
/// which also disagreed with PowDB's own `avg`.) Both frontends and both
/// physical paths must keep agreeing.
#[test]
fn every_empty_set_aggregate_is_null() {
    let rows = vec![GenRow {
        a: None,
        label: None,
        f: None,
        b: None,
    }];
    let mut arrangements = Arrangements::build(&rows);

    for (powql, sql, expected) in [
        (
            "sum(T { .mirror })",
            "SELECT SUM(mirror) FROM T",
            Value::Empty,
        ),
        (
            "min(T { .mirror })",
            "SELECT MIN(mirror) FROM T",
            Value::Empty,
        ),
        (
            "max(T { .mirror })",
            "SELECT MAX(mirror) FROM T",
            Value::Empty,
        ),
        (
            "avg(T { .mirror })",
            "SELECT AVG(mirror) FROM T",
            Value::Empty,
        ),
    ] {
        for (name, db) in arrangements.each() {
            assert_eq!(
                answer(&mut db.engine, powql),
                Answer::Scalar(expected.clone()),
                "`{powql}` under {name}"
            );
            assert_eq!(
                answer_sql(&mut db.engine, sql),
                Answer::Scalar(expected.clone()),
                "`{sql}` under {name}"
            );
        }
    }

    // The grouped path must make the same choice as the ungrouped one, or a
    // query's answer would depend on whether it carried a `group` clause.
    let grouped = answer(
        &mut arrangements.unindexed.engine,
        "T group .mirror { .mirror, s: sum(.mirror), m: min(.mirror) }",
    );
    assert_eq!(
        grouped,
        Answer::Rows {
            columns: vec!["mirror".into(), "s".into(), "m".into()],
            rows: vec![vec![Value::Empty, Value::Empty, Value::Empty]],
        },
        "grouped aggregates over an all-null group disagree with the ungrouped ones"
    );
}
