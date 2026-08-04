//! An index must never change the answer, including when the literal's type is
//! not the column's type.
//!
//! The planner is pure, so `.price < 3` against a `float` column emits a
//! `RangeScan` carrying `Literal::Int(3)`. Plain-column B-tree keys are encoded
//! behind a type tag (`btree::encode_composite_value` leads with `type_id`), so
//! an Int-tagged probe addresses a different byte lane than the stored Float
//! keys: the same query answered 2 without an index and 0 with one, and
//! `filter .price < 3 delete` deleted nothing while reporting success. The
//! coercion that repairs this already existed in `plan_exec::lowering`, but was
//! only called from the conjunction lane, so `.price < 3 and .id > 0` was right
//! and `.price < 3` alone was wrong.
//!
//! ## Why this file exists rather than another case in an existing suite
//!
//! `proptest_frontend_equivalence::an_index_never_changes_the_answer` was
//! written to catch exactly this and structurally cannot: its schema declares
//! `str`, `int` and `datetime` columns and it generates `0i64..12` literals, so
//! no cell it can produce has a float on either side.
//! `access_path_equivalence` varies index presence across five catalog states
//! but every literal it uses matches its column's declared type.
//! `cross_type_matrix` covers cross-type literals but builds no index on any
//! tested column. The union of all three misses the defect.
//!
//! This runner therefore enumerates the cross product that reaches it:
//!
//!   * **column type**: `int`, `float`, `datetime`, plus a `json` path index;
//!   * **literal type and magnitude**: ints and floats, including the
//!     `2^53` precision boundary where `i64 as f64` stops being injective,
//!     `i64::MIN` / `i64::MAX`, a fractional float, a negative and a zero;
//!   * **operator**: all six comparisons, not only the ones that were reported
//!     broken (only 5 of 24 numeric cells were, and the unique-index half fails
//!     by over-returning rather than by returning nothing);
//!   * **orientation**: `column op literal` and `literal op column`;
//!   * **index kind**: none, non-unique B-tree, unique B-tree, JSON path index;
//!   * **statement kind**: `count`, row projection, `update` rowcount, `delete`
//!     rowcount. The two mutations are not decoration: they build their rid set
//!     through a third copy of the probe logic, and a wrong answer there
//!     destroys or fails to destroy data;
//!   * **physical path**: fast paths on, and forced off via
//!     [`Engine::set_force_generic_path`];
//!   * **frontend**: PowQL and SQL.
//!
//! Every cell is compared against the same cell run against the same rows with
//! no index at all, at the same path setting. That reference is the definition
//! of the invariant: an index is an access path, never a semantic.
//!
//! [`filter_and_projection_agree_on_every_cross_type_cell`] adds the other half
//! that a `count(... filter ...)` sweep cannot see. A filter over a compiled
//! predicate and the same expression projected as a boolean column are two
//! different evaluators, and `.f = 2` shipped answering true in one and false
//! in the other. Cells where they still disagree are listed explicitly in
//! `KNOWN_EVALUATOR_DIVERGENCES`, so a new one fails the build and fixing an
//! old one also fails the build until the list shrinks.

#![cfg(feature = "testing")]

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_DIR: AtomicU64 = AtomicU64::new(0);

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_xtype_{tag}_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_nanos(),
        UNIQUE_DIR.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

// ─── the matrix axes ────────────────────────────────────────────────────────

/// A column type under test, with the values the fixture stores in it. Every
/// value is distinct so the same rows can carry a unique index, and the last
/// row leaves the column missing so the NULL rule is exercised on every path
/// (a missing value must match no comparison, and it lives in the B-tree's
/// separate empty list rather than in the ordered key space).
struct ColumnKind {
    name: &'static str,
    declaration: &'static str,
    values: &'static [&'static str],
}

/// `-9223372036854775807` rather than `i64::MIN`: PowQL lexes a negative
/// literal as unary minus over a positive one, so `i64::MIN` has no spelling as
/// a stored value. It is still probed as a literal below, where a parse error
/// is itself an answer both sides must agree on.
const INT_VALUES: &[&str] = &[
    "-9223372036854775807",
    "-9007199254740993",
    "-3",
    "0",
    "1",
    "2",
    "3",
    "9007199254740992",
    "9007199254740993",
    "9223372036854775807",
];

const COLUMN_KINDS: &[ColumnKind] = &[
    ColumnKind {
        name: "int",
        declaration: "v: int",
        values: INT_VALUES,
    },
    ColumnKind {
        name: "float",
        declaration: "v: float",
        // `-0.0` is stored on purpose: the compiled float leaf compares with
        // IEEE `==`, under which it equals `0.0`, while the B-tree orders keys
        // with `total_cmp`, under which it sorts strictly below `0.0`. A zero
        // literal therefore cannot address the same rows through both, which is
        // why `lowering::float_key_is_faithful` refuses one.
        values: &[
            "-9007199254740992.0",
            "-3.75",
            "-0.5",
            "-0.0",
            "0.0",
            "1.0",
            "2.0",
            "2.5",
            "3.0",
            "9007199254740992.0",
        ],
    },
    // A datetime column is written as raw micros, so every literal that probes
    // it is an Int or a Float against a DateTime-tagged key. The Int half was
    // fixed in v0.20.0 and is pinned here against regression.
    ColumnKind {
        name: "datetime",
        declaration: "v: datetime",
        values: INT_VALUES,
    },
    // The refusal side of the rule. A numeric literal against a `str` or `bool`
    // column matches nothing and errors respectively, and the coercion refuses
    // to build an index key for either. That refusal is only correct while it
    // stays a refusal, so the pairs are enumerated rather than assumed: adding
    // a coercion arm for one of them would show up here as an index that starts
    // answering differently from the scan.
    ColumnKind {
        name: "str",
        declaration: "v: str",
        values: &[
            "\"-3\"", "\"0\"", "\"1\"", "\"2\"", "\"3\"", "\"2.5\"", "\"\"", "\"zzz\"",
        ],
    },
    ColumnKind {
        name: "bool",
        declaration: "v: bool",
        values: &["true", "false"],
    },
];

/// Literals probed against every column kind. The float column stores `2.0` and
/// `3.0` and the int column stores `2` and `3`, so both orientations of the
/// cross-type pair land on a boundary value rather than between two of them.
const LITERALS: &[&str] = &[
    "-9223372036854775808",
    "-9007199254740993",
    "-3",
    "0",
    "1",
    "2",
    "3",
    "9007199254740992",
    "9007199254740993",
    "9223372036854775807",
    "-3.75",
    "-0.5",
    "-0.0",
    "0.0",
    "1.0",
    "2.0",
    "2.5",
    "3.0",
    "9007199254740992.0",
    "9007199254740993.0",
];

/// The literals the mutation sweep uses. A mutation reaches the same lowering
/// decision as a read, so what it has to cover is every distinct OUTCOME of
/// that decision rather than every literal: an int and a float that the column
/// can take exactly, a fractional float, both zeroes, and the `2^53` boundary
/// on each side of it. Each cell rebuilds rows, so the full literal list would
/// cost minutes for coverage the read sweep already has.
const MUTATION_LITERALS: &[&str] = &[
    "-3",
    "0",
    "2",
    "3",
    "9007199254740992",
    "9007199254740993",
    "-0.0",
    "2.0",
    "2.5",
    "9007199254740992.0",
];

const OPERATORS: &[&str] = &["=", "!=", "<", "<=", ">", ">="];

/// The nesting depths a cross-type predicate has to survive, as templates over
/// the predicate `{p}`.
///
/// This axis is not decoration. A subquery is planned and executed by a
/// different code path from the statement that contains it, and eight of those
/// sites used to hand raw planner output straight to the executor. The result
/// was that the *same predicate* answered correctly at depth 0 and incorrectly
/// at depth 1, which is a worse failure than the original bug: before the fix
/// both depths were uniformly wrong, after it they disagreed.
///
/// Each template names the materialization site it reaches:
///
///   * `Top`: no subquery, the depth-0 reference.
///   * `In` / `NotIn`: uncorrelated `InSubquery`, materialized into an
///     `InList` before the outer filter runs.
///   * `Exists` / `NotExists`: uncorrelated `ExistsSubquery`, collapsed to a
///     `Bool` literal.
///   * `Scalar`: an aggregate over the subquery's rows, so the subquery's row
///     SET (not merely its emptiness) reaches the answer.
///   * `Correlated`: the subquery references an outer column, so it is
///     re-planned and re-executed once per outer row through the per-row
///     materialization sites rather than the uncorrelated ones.
///
/// A template that stopped being a subquery (a rewrite that inlined it, say)
/// would still pass a parity assertion while covering nothing, so
/// [`nesting_shapes_are_discriminating`] separately proves each one is
/// sensitive to the predicate it carries.
const NESTED_SHAPES: &[(&str, &str)] = &[
    ("Top", "T filter {p} {{ .id }}"),
    ("In", "T filter .id in (T filter {p} {{ .id }}) {{ .id }}"),
    (
        "NotIn",
        "T filter .id not in (T filter {p} {{ .id }}) {{ .id }}",
    ),
    ("Exists", "T filter exists (T filter {p}) {{ .id }}"),
    ("NotExists", "T filter not exists (T filter {p}) {{ .id }}"),
    ("Scalar", "count(T filter .id in (T filter {p} {{ .id }}))"),
    // The outer scan is `Probe` so the inner `.pid` reference resolves to no
    // column of `T` and the subquery is therefore correlated: it is re-planned
    // and re-executed per outer row, through the two per-row materialization
    // sites the uncorrelated shapes above never touch.
    (
        "Correlated",
        "Probe filter exists (T filter {p} and .id = .pid) {{ id: .pid }}",
    ),
];

/// Substitute the predicate into a [`NESTED_SHAPES`] template.
fn nested_query(template: &str, predicate: &str) -> String {
    template
        .replace("{p}", predicate)
        .replace("{{", "{")
        .replace("}}", "}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Index {
    None,
    Btree,
    Unique,
}

impl Index {
    fn ddl(self) -> &'static [&'static str] {
        match self {
            Index::None => &[],
            Index::Btree => &["alter T add index .v"],
            Index::Unique => &["alter T add unique .v"],
        }
    }
}

/// The two indexed states, each compared against [`Index::None`].
const INDEXED: [Index; 2] = [Index::Btree, Index::Unique];

// ─── fixtures ───────────────────────────────────────────────────────────────

fn exec(engine: &mut Engine, statement: &str) {
    engine
        .execute_powql(statement)
        .unwrap_or_else(|err| panic!("fixture statement `{statement}` failed: {err}"));
}

/// The schema and rows shared by every state: `id` identifies a row, `v` is the
/// column under test, and `tag` gives `update` something to write that is not
/// the indexed column itself.
fn populate(engine: &mut Engine, kind: &ColumnKind) {
    exec(
        engine,
        &format!(
            "type T {{ required unique id: int, {}, tag: int }}",
            kind.declaration
        ),
    );
    for (row, value) in kind.values.iter().enumerate() {
        exec(
            engine,
            &format!("insert T {{ id := {}, v := {value}, tag := 0 }}", row + 1),
        );
    }
    // The missing-value row, last so its id is one past the populated ones.
    exec(
        engine,
        &format!("insert T {{ id := {}, tag := 0 }}", kind.values.len() + 1),
    );
    // Outer driver for the correlated nesting shape. It has to be a DIFFERENT
    // table from `T`, and its key column has to be named something `T` does not
    // have, or the reference inside the subquery resolves against `T` and the
    // subquery is uncorrelated after all -- which would quietly move that shape
    // onto the same materialization site the other shapes already cover.
    exec(engine, "type Probe { required unique pid: int }");
    for row in 0..=kind.values.len() {
        exec(engine, &format!("insert Probe {{ pid := {} }}", row + 1));
    }
}

fn build(kind: &ColumnKind, index: Index, force_generic: bool) -> Engine {
    let mut engine =
        Engine::new(&fresh_dir(kind.name)).expect("engine opens over a fresh temp dir");
    populate(&mut engine, kind);
    for statement in index.ddl() {
        exec(&mut engine, statement);
    }
    engine.set_force_generic_path(force_generic);
    engine
}

// ─── outcomes ───────────────────────────────────────────────────────────────

/// Whatever a cell produced, including the error. An error is part of the
/// contract: an index path that errors where a scan answers is exactly the kind
/// of divergence this runner exists to catch.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Ids(Vec<i64>),
    Scalar(String),
    Modified(u64),
    Other(String),
    Err(String),
}

/// Row ids a projection returned, sorted. PowDB promises no order for a query
/// that does not ask for one, and an index walk and a heap scan legitimately
/// produce different orders, so the comparison is over the set of rows.
fn ids_of(result: Result<QueryResult, powdb_query::result::QueryError>) -> Outcome {
    match result {
        Ok(QueryResult::Rows { columns, rows }) => {
            let Some(position) = columns.iter().position(|c| c == "id") else {
                return Outcome::Other(format!("projection has no id column: {columns:?}"));
            };
            let mut ids: Vec<i64> = Vec::with_capacity(rows.len());
            for row in rows {
                match row.get(position) {
                    Some(Value::Int(id)) => ids.push(*id),
                    other => return Outcome::Other(format!("non-int id: {other:?}")),
                }
            }
            ids.sort_unstable();
            Outcome::Ids(ids)
        }
        Ok(QueryResult::Scalar(value)) => Outcome::Scalar(format!("{value:?}")),
        Ok(QueryResult::Modified(n)) => Outcome::Modified(n),
        Ok(other) => Outcome::Other(format!("{other:?}")),
        Err(err) => Outcome::Err(err.to_string()),
    }
}

fn powql(engine: &mut Engine, query: &str) -> Outcome {
    ids_of(engine.execute_powql(query))
}

fn sql(engine: &mut Engine, query: &str) -> Outcome {
    ids_of(engine.execute_sql(query))
}

/// Every `(operator, literal, orientation)` predicate, with a label that names
/// the cell in a failure message.
fn predicates() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for operator in OPERATORS {
        for literal in LITERALS {
            out.push((
                format!(".v {operator} {literal}"),
                format!("column {operator} literal"),
            ));
            out.push((
                format!("{literal} {operator} .v"),
                format!("literal {operator} column"),
            ));
        }
    }
    out
}

// ─── the invariant ──────────────────────────────────────────────────────────

/// Cells where an index cannot be in step with both no-index answers, because
/// the two no-index answers are not in step with each other.
///
/// Empty. It held twelve `float` equality cells: a float column holding `2.0`
/// answered `.v = 2` true through the compiled float leaf, which widens the int
/// literal, and false through the generic evaluator, which used `Value`'s
/// strictly-typed equality. An index stores one key per row, so its probe
/// necessarily reproduced one of the two and not the other, and it reproduced
/// the compiled one. The entries were a consequence of that evaluator
/// disagreement rather than an independent index defect, and they went when the
/// comparison operators were taught to read one numeric order
/// (`eval::cross_type_numeric_cmp`).
///
/// `.v = 0` was never here: the float column stores both `0.0` and `-0.0`,
/// which the compiled leaf and the B-tree's total order disagree about, so a
/// zero literal gives up the index entirely and both answers come from the same
/// scan.
const KNOWN_REFERENCE_SPLITS: &[(&str, &str)] = &[];

#[test]
fn an_index_never_changes_a_cross_type_answer() {
    let mut checked = 0usize;
    let mut splits: Vec<(&'static str, String)> = Vec::new();

    for kind in COLUMN_KINDS {
        // Both no-index references are built up front. Where they agree, that
        // agreed answer is what every indexed state must produce. Where they
        // disagree the cell is a pinned evaluator defect, and the index is
        // required to reproduce the answer a shipped build gives, which is the
        // fast-path one.
        let mut reference_fast = build(kind, Index::None, false);
        let mut reference_generic = build(kind, Index::None, true);
        let mut indexed: Vec<(Index, bool, Engine)> = Vec::new();
        for index in INDEXED {
            for force_generic in [false, true] {
                indexed.push((index, force_generic, build(kind, index, force_generic)));
            }
        }

        for (predicate, orientation) in predicates() {
            for shape in [
                format!("count(T filter {predicate})"),
                format!("T filter {predicate} {{ .id }}"),
            ] {
                let fast = powql(&mut reference_fast, &shape);
                let generic = powql(&mut reference_generic, &shape);
                let split = fast != generic;
                for (index, force_generic, engine) in indexed.iter_mut() {
                    let actual = powql(engine, &shape);
                    let expected = if *force_generic { &generic } else { &fast };
                    checked += 1;
                    if &actual == expected {
                        continue;
                    }
                    if split && actual == fast {
                        if !splits
                            .iter()
                            .any(|(k, p)| *k == kind.name && p == &predicate)
                        {
                            splits.push((kind.name, predicate.clone()));
                        }
                        continue;
                    }
                    panic!(
                        "an index changed the answer.\n  column type: {}\n  index: \
                         {index:?}\n  orientation: {orientation}\n  fast paths: {}\n  \
                         query: {shape}\n  with no index: {expected:?}\n  with the index: \
                         {actual:?}",
                        kind.name,
                        if *force_generic { "forced off" } else { "on" },
                    );
                }
            }
        }
    }

    // A runner that silently stopped generating cells would pass forever.
    assert!(
        checked >= 5_000,
        "the matrix collapsed to {checked} comparisons"
    );

    splits.sort();
    let mut known: Vec<(&str, String)> = KNOWN_REFERENCE_SPLITS
        .iter()
        .map(|(kind, predicate)| (*kind, (*predicate).to_string()))
        .collect();
    known.sort();
    assert_eq!(
        splits, known,
        "the set of cells where the two no-index answers disagree, so an index has to pick \
         one of them, changed. A new entry is a new bug; a missing entry means one was \
         fixed and KNOWN_REFERENCE_SPLITS should shrink."
    );
}

/// The nesting axis: the same rule, at every depth a predicate can sit at.
///
/// Round one fixed the depth-0 probe and left the eight subquery
/// materialization sites planning statements and executing the raw planner
/// output. Against the same rows and the same index,
/// `count(H filter .price < 3)` then answered 2 while
/// `count(H filter .n in (H filter .price < 3 { .n }))` answered 0. This is
/// the sweep that would have caught it: a spot check at depth 0 cannot, and
/// neither can one nested query with one hand-picked literal.
///
/// The literal set is [`MUTATION_LITERALS`] rather than the full
/// [`LITERALS`]: a subquery cell costs a nested execution per outer row, and
/// what the depth axis has to cover is every distinct OUTCOME of the lowering
/// decision (int and float taken exactly, fractional, both zeroes, both sides
/// of `2^53`), which that set already spans. The full literal sweep runs at
/// depth 0 in [`an_index_never_changes_a_cross_type_answer`].
#[test]
fn an_index_never_changes_a_nested_cross_type_answer() {
    let mut checked = 0usize;
    let mut shapes_seen: Vec<&str> = Vec::new();
    let mut observed: Vec<String> = Vec::new();

    for kind in COLUMN_KINDS {
        let mut reference = build(kind, Index::None, false);
        let mut indexed: Vec<(Index, Engine)> = INDEXED
            .iter()
            .map(|index| (*index, build(kind, *index, false)))
            .collect();

        for (name, template) in NESTED_SHAPES {
            if !shapes_seen.contains(name) {
                shapes_seen.push(name);
            }
            for operator in OPERATORS {
                for literal in MUTATION_LITERALS {
                    let predicate = format!(".v {operator} {literal}");
                    let query = nested_query(template, &predicate);
                    let expected = powql(&mut reference, &query);
                    for (index, engine) in indexed.iter_mut() {
                        let actual = powql(engine, &query);
                        checked += 1;
                        if actual != expected {
                            observed.push(format!("{} {name} {index:?}: {predicate}", kind.name));
                        }
                    }
                }
            }
        }
    }

    let expected_shapes: Vec<&str> = NESTED_SHAPES.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        shapes_seen, expected_shapes,
        "the nesting axis stopped covering every shape"
    );
    assert!(
        checked >= 3_000,
        "the nesting matrix collapsed to {checked} comparisons"
    );

    observed.sort();
    observed.dedup();
    let mut known: Vec<String> = KNOWN_NESTED_INDEX_DIVERGENCES
        .iter()
        .map(|entry| (*entry).to_string())
        .collect();
    known.sort();
    assert_eq!(
        observed, known,
        "the set of nested cells where an index changes the answer changed. A new entry \
         is a new bug; a missing entry means one was fixed and \
         KNOWN_NESTED_INDEX_DIVERGENCES should shrink."
    );
}

/// Nested cells where an index changes the answer.
///
/// Empty. It held four `float Correlated Unique` equality cells, every one of
/// them an instance of the evaluator disagreement rather than an index-key
/// defect. A float column holding `2.0` answered `.v = 2` true through the
/// compiled float leaf, which widens the int literal, and false through
/// `Value`'s equality, which is strictly typed and has no Int/Float arm. Inside
/// a conjunction the choice between the two was made by which conjunct ended up
/// driving: with no index on `.v` the driver is the unique `.id` probe and the
/// `.v = 2` conjunct is re-checked as a residual by the generic evaluator
/// (false); with an index on `.v` that conjunct drives the probe and answers
/// through the coerced float key (true). The index was not changing the meaning
/// of `.v = 2`, it was changing which of two evaluators that already disagreed
/// got asked. Now they agree (`eval::cross_type_numeric_cmp`), so there is no
/// second answer to pick.
///
/// Only the `Unique` state ever appeared: with a plain B-tree on `.v` the
/// `.v = <int>` conjunct is a non-unique candidate and loses the driver choice
/// to the unique `.id` probe, so the residual was evaluated generically and
/// agreed with the no-index answer. That is the same reasoning read from the
/// other side, and it is why the index KIND was part of each entry rather than
/// collapsed away.
const KNOWN_NESTED_INDEX_DIVERGENCES: &[&str] = &[];

/// Every nesting template must be sensitive to the predicate it carries.
///
/// A parity assertion over a shape that answers the same thing no matter what
/// is inside it passes forever while covering nothing: that is exactly how the
/// proptest this file's header names failed to see the defect it was written
/// for. So each template is run over the whole operator and literal sweep with
/// no index at all, and is required to produce more than one distinct outcome.
/// A template that gets inlined away, always errors, or always returns every
/// row fails here rather than silently weakening
/// [`an_index_never_changes_a_nested_cross_type_answer`].
#[test]
fn nesting_shapes_are_discriminating() {
    for kind in COLUMN_KINDS {
        let mut engine = build(kind, Index::None, false);
        for (name, template) in NESTED_SHAPES {
            let mut outcomes: Vec<Outcome> = Vec::new();
            for operator in OPERATORS {
                for literal in MUTATION_LITERALS {
                    let query = nested_query(template, &format!(".v {operator} {literal}"));
                    let outcome = powql(&mut engine, &query);
                    if !outcomes.contains(&outcome) {
                        outcomes.push(outcome);
                    }
                }
            }
            assert!(
                outcomes.len() > 1,
                "nesting shape `{name}` on a {} column answered {:?} for every \
                 operator and literal, so a parity assertion over it proves nothing",
                kind.name,
                outcomes.first(),
            );
        }
    }
}

#[test]
fn an_index_never_changes_which_rows_a_mutation_touches() {
    // `update` and `delete` build their rid set through their own copy of the
    // probe logic, so a read-only sweep cannot see them. `filter .v < 3 delete`
    // reported 0 rows deleted and left every row in place.
    for kind in COLUMN_KINDS {
        let mut reference = build(kind, Index::None, false);
        let mut indexed: Vec<(Index, Engine)> = INDEXED
            .iter()
            .map(|index| (*index, build(kind, *index, false)))
            .collect();

        for operator in OPERATORS {
            for literal in MUTATION_LITERALS {
                let predicate = format!(".v {operator} {literal}");
                for shape in [
                    format!("T filter {predicate} update {{ tag := 9 }}"),
                    format!("T filter {predicate} delete"),
                ] {
                    let expected = mutation_cell(&mut reference, kind, &shape);
                    for (index, engine) in indexed.iter_mut() {
                        let actual = mutation_cell(engine, kind, &shape);
                        assert_eq!(
                            actual, expected,
                            "an index changed which rows a mutation touched.\n  column type: \
                             {}\n  index: {index:?}\n  query: {shape}\n  with no index: \
                             {expected:?}\n  with the index: {actual:?}",
                            kind.name,
                        );
                    }
                }
            }
        }
    }
}

/// Run one mutation and restore the fixture, so the whole matrix shares one
/// table per state instead of rebuilding an engine per cell.
///
/// An `update` is undone by resetting the tag it wrote, which leaves the
/// indexed column and every rid untouched. A `delete` has to rebuild the rows,
/// which also re-inserts them into whatever index the state carries, so each
/// delete cell runs against a freshly built index rather than a worn one.
fn mutation_cell(engine: &mut Engine, kind: &ColumnKind, shape: &str) -> Outcome {
    let outcome = powql(engine, shape);
    if shape.ends_with("delete") {
        exec(engine, "T delete");
        for (row, value) in kind.values.iter().enumerate() {
            exec(
                engine,
                &format!("insert T {{ id := {}, v := {value}, tag := 0 }}", row + 1),
            );
        }
        exec(
            engine,
            &format!("insert T {{ id := {}, tag := 0 }}", kind.values.len() + 1),
        );
    } else {
        exec(engine, "T filter .tag = 9 update { tag := 0 }");
    }
    outcome
}

#[test]
fn an_index_never_changes_a_cross_type_answer_through_the_sql_frontend() {
    // One shared engine and plan cache serve both frontends, but SQL lowers
    // through its own parser, so a coercion applied in one lowering arm and not
    // the other shows up here rather than in the PowQL sweep.
    for kind in COLUMN_KINDS {
        let mut reference = build(kind, Index::None, false);
        let mut indexed: Vec<(Index, Engine)> = INDEXED
            .iter()
            .map(|index| (*index, build(kind, *index, false)))
            .collect();

        for operator in ["=", "<>", "<", "<=", ">", ">="] {
            for literal in LITERALS {
                let query = format!("SELECT id FROM T WHERE v {operator} {literal}");
                let expected = sql(&mut reference, &query);
                for (index, engine) in indexed.iter_mut() {
                    let actual = sql(engine, &query);
                    assert_eq!(
                        actual, expected,
                        "an index changed the answer through the SQL frontend.\n  column \
                         type: {}\n  index: {index:?}\n  query: {query}\n  with no index: \
                         {expected:?}\n  with the index: {actual:?}",
                        kind.name,
                    );
                }
            }
        }
    }
}

// ─── the lowering boundary ──────────────────────────────────────────────────

/// No plan may reach execution without going through the lowering pass, and
/// that has to be true by construction rather than by everyone remembering.
///
/// Round one fixed the probe coercion and left eight sites planning a statement
/// and executing the raw planner output, so a predicate that answered correctly
/// at the top level answered differently one level of nesting down. Adding a
/// lowering call at those eight sites would have fixed those eight sites; site
/// nine is the failure mode.
///
/// So the executor now reaches the planner from exactly two functions,
/// `Engine::plan_and_lower_cacheable` and `Engine::plan_text_and_lower`, both
/// of which return a `LoweredPlan`, and `LoweredPlan` has no other constructor.
/// Any new site has to call one of them to get a plan at all, and what it gets
/// back is already lowered.
///
/// This test is the part that a compiler cannot express: it reads the executor
/// sources and fails if a THIRD call to the planner appears anywhere under
/// `src/executor/`. A future ninth site therefore fails the build instead of
/// silently regressing.
///
/// `tests.rs` is excluded because the crate's own unit tests build plans
/// deliberately, including deliberately unlowered ones, in order to test the
/// lowering pass itself.
#[test]
fn no_execution_entry_point_can_receive_an_unlowered_plan() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("executor");

    let mut sites: Vec<String> = Vec::new();
    let mut files = 0usize;
    visit_rust_files(&root, &mut |path| {
        if path.file_name().and_then(|n| n.to_str()) == Some("tests.rs") {
            return;
        }
        files += 1;
        let text = std::fs::read_to_string(path).expect("executor source is readable");
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        for (number, line) in text.lines().enumerate() {
            // `plan_statement(` and `planner::plan(` are the planner's two
            // entry points. Matching the text is the point: the rule is about
            // where in the source the planner may be named at all.
            if line.contains("planner::plan_statement(") || line.contains("planner::plan(") {
                sites.push(format!("{relative}:{}: {}", number + 1, line.trim()));
            }
        }
    });

    assert!(
        files >= 8,
        "the source walk found only {files} executor files, so it is not \
         scanning what it thinks it is"
    );

    // Two calls, both in `mod.rs`: one per planner entry point. The count is
    // asserted rather than the source text so that reflowing those two lines
    // does not fail the build, while a THIRD call anywhere (including inside
    // `mod.rs`) still does.
    let mut files_with_calls: Vec<&str> = sites
        .iter()
        .map(|site| site.split(':').next().expect("site is file:line: text"))
        .collect();
    files_with_calls.sort_unstable();
    files_with_calls.dedup();

    assert_eq!(
        (files_with_calls.as_slice(), sites.len()),
        (["mod.rs"].as_slice(), 2),
        "a call to the planner appeared outside `Engine::plan_and_lower_cacheable` / \
         `Engine::plan_text_and_lower`, so a plan can reach execution unlowered again. \
         Route it through those instead; they hand back a `LoweredPlan`, which is what \
         the execution entry points take.\nfound:\n  {}",
        sites.join("\n  ")
    );

    // And those two calls are still inside the two sanctioned helpers rather
    // than somewhere else in the same file.
    let entry = std::fs::read_to_string(root.join("mod.rs")).expect("mod.rs is readable");
    for helper in [
        "fn plan_and_lower_cacheable(",
        "fn plan_text_and_lower(",
        "fn plan_and_lower(",
    ] {
        assert!(
            entry.contains(helper),
            "`{helper}` is gone, so the two planner calls this test allows are no \
             longer the ones it was written to allow"
        );
    }
}

/// The rule above governs the executor's own sources, and that is exactly why
/// it missed the hole an EMBEDDER could reach: nothing inside `src/executor/`
/// called the planner, and yet
/// `planner::plan(..)` -> `PlanNode` -> `Engine::execute_plan(..)` ran an
/// unlowered plan, because all three of those are public and `LoweredPlan` is
/// crate-private, so an embedder could not lower even deliberately.
///
/// So the surface is pinned as well as the internals: exactly one publicly
/// reachable function under `src/executor/` may name a `PlanNode`, and it has
/// to lower what it is handed. A second one fails here, which is the only
/// place it can fail, since the crate's own tests would keep passing.
#[test]
fn the_only_public_plan_entry_point_lowers_what_it_is_given() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("executor");

    // `pub fn` only: `pub(crate) fn` and `pub(in crate::executor) fn` do not
    // contain that substring and are not reachable from outside the crate, so
    // the recursion targets are correctly not counted here.
    let mut public: Vec<String> = Vec::new();
    visit_rust_files(&root, &mut |path| {
        if path.file_name().and_then(|n| n.to_str()) == Some("tests.rs") {
            return;
        }
        let text = std::fs::read_to_string(path).expect("executor source is readable");
        for (index, _) in text.match_indices("pub fn ") {
            let rest = &text[index..];
            let Some(brace) = rest.find('{') else {
                continue;
            };
            let signature = &rest[..brace];
            if !signature.contains("PlanNode") {
                continue;
            }
            let name: String = rest["pub fn ".len()..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            public.push(name);
        }
    });
    public.sort();
    public.dedup();

    assert_eq!(
        public,
        vec!["execute_plan".to_string()],
        "a publicly reachable executor function takes a `PlanNode`. Every one of \
         them runs whatever it is given, and planner output is not executable as \
         it stands, so either lower inside it or do not expose it."
    );

    let dispatch = std::fs::read_to_string(root.join("plan_exec").join("dispatch.rs"))
        .expect("dispatch.rs is readable");
    let body = dispatch
        .split_once("pub fn execute_plan(")
        .expect("execute_plan is still declared in dispatch.rs")
        .1;
    let body = &body[..body.find("\n    }").unwrap_or(body.len())];
    assert!(
        body.contains("self.lower("),
        "`Engine::execute_plan` stopped lowering its argument, so an embedder's \
         plan reaches execution raw again. Its body now reads:\n{body}"
    );
}

/// The behaviour the rule above exists to protect, asserted directly rather
/// than inferred from the shape of the source.
///
/// `.v < 3` against a float column is the original defect: the planner emits a
/// speculative `RangeScan` carrying `Literal::Int(3)`, and only lowering turns
/// that into something that addresses the float keys the column actually
/// stores. Through the public plan entry point this answered nothing at all
/// while the identical text through `execute_powql` answered the rows.
///
/// Both index states are swept because they fail in opposite directions: with
/// no index the unlowered `RangeScan` has no index to drive it, and with one it
/// probes the wrong type lane.
#[test]
fn the_public_plan_entry_point_answers_like_the_same_query_as_text() {
    for kind in COLUMN_KINDS {
        for index in [Index::None, Index::Btree, Index::Unique] {
            // Reads leave the fixture alone, so one engine per state serves
            // every cell. The two engines are separate so that a plan-cache
            // entry written by one spelling cannot answer the other.
            let mut text_engine = build(kind, index, false);
            let mut plan_engine = build(kind, index, false);

            for operator in OPERATORS {
                for literal in ["3", "3.0", "0"] {
                    let query = format!("T filter .v {operator} {literal} {{ .id }}");
                    let from_text = powql(&mut text_engine, &query);
                    let from_plan = match powdb_query::planner::plan(&query) {
                        Ok(plan) => ids_of(plan_engine.execute_plan(&plan)),
                        // The engine reports a planner failure as a parse
                        // error, so match that wrapping or a refusal would
                        // compare unequal for spelling reasons alone.
                        Err(err) => Outcome::Err(
                            powdb_query::result::QueryError::Parse(err.to_string()).to_string(),
                        ),
                    };

                    assert_eq!(
                        from_plan, from_text,
                        "the public plan entry point answered differently from the same \
                         query as text.\n  column type: {}\n  index: {index:?}\n  query: \
                         {query}\n  as text: {from_text:?}\n  via planner::plan + \
                         execute_plan: {from_plan:?}",
                        kind.name
                    );
                }
            }
        }
    }
}

fn visit_rust_files(dir: &std::path::Path, visit: &mut impl FnMut(&std::path::Path)) {
    let entries = std::fs::read_dir(dir).expect("executor source directory is readable");
    for entry in entries {
        let path = entry.expect("directory entry is readable").path();
        if path.is_dir() {
            visit_rust_files(&path, visit);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            visit(&path);
        }
    }
}

/// Lowering has to be idempotent, or routing every plan through it is unsound.
///
/// A plan can reach the pass more than once by design: the plan cache stores
/// the pre-lowering tree and lowers on every hit, a prepared statement lowers
/// its template on every execution, and the expression-index fallback re-lowers
/// a tree that came out of the pass already. If the second pass rewrote
/// anything, those paths would answer differently from a first execution of the
/// same text.
///
/// The corpus deliberately includes the shapes the pass actually rewrites (a
/// cross-type bound that gets coerced, one that gets rejected back to a scan, a
/// conjunction whose driver is chosen, a hot equality that is demoted to a
/// scan) rather than only shapes it passes through untouched, because a pass
/// that never fires is trivially idempotent.
#[test]
fn lowering_is_idempotent() {
    for kind in COLUMN_KINDS {
        for index in [Index::None, Index::Btree, Index::Unique] {
            let engine = build(kind, index, false);
            for query in [
                "T filter .v < 3 { .id }",
                "T filter .v <= 0.0 { .id }",
                "T filter .v > 0.0 { .id }",
                "T filter .v >= 0.0 { .id }",
                "T filter .v = 2 { .id }",
                "T filter .v = 2 and .id > 0 { .id }",
                "T filter .v > 1 and .v < 3 and .tag = 0 { .id }",
                "T filter .tag = 0 { .id }",
                "count(T filter .v != 2.5)",
                "T filter .v < 2.5 update { tag := 9 }",
                "T filter .v >= 1 delete",
            ] {
                let once = engine
                    .lowered_plan_text(query, 1)
                    .unwrap_or_else(|err| panic!("`{query}` should plan: {err}"));
                let twice = engine
                    .lowered_plan_text(query, 2)
                    .expect("second pass plans");
                let thrice = engine
                    .lowered_plan_text(query, 3)
                    .expect("third pass plans");
                assert_eq!(
                    once, twice,
                    "lowering `{query}` twice changed the plan on a {} column with \
                     {index:?}.\n  after one pass:\n{once}\n  after two:\n{twice}",
                    kind.name
                );
                assert_eq!(twice, thrice, "lowering `{query}` is not stable");
            }
        }
    }
}

/// A prepared statement has to answer what the same query text answers.
///
/// `prepare` stores the RAW planner output as its template, exactly as the plan
/// cache does, because lowering depends on catalog state and a prepared
/// statement outlives the DDL that changes it. That makes every execution of a
/// prepared statement a place where an unlowered plan can reach the executor,
/// and it was one: the template was substituted and run directly, so a prepared
/// cross-type filter took a different access path from the identical text.
///
/// This is the ninth site, found while making the boundary true by construction
/// rather than by listing the sites. It is swept over the same axes as
/// everything else in this file rather than spot-checked, since a single
/// hand-picked literal is exactly what missed it.
#[test]
fn a_prepared_statement_answers_like_the_same_query_as_text() {
    let mut checked = 0usize;
    for kind in COLUMN_KINDS {
        for index in [Index::None, Index::Btree, Index::Unique] {
            let mut engine = build(kind, index, false);
            for operator in OPERATORS {
                for literal in MUTATION_LITERALS {
                    let text = format!("T filter .v {operator} {literal} {{ .id }}");
                    let from_text = powql(&mut engine, &text);
                    // The prepared spelling reaches the same predicate through
                    // a substituted literal slot.
                    let Ok(prepared) = engine.prepare(&text) else {
                        continue;
                    };
                    let literals = prepared_literals(literal);
                    if literals.len() != prepared.param_count {
                        continue;
                    }
                    let from_prepared = ids_of(engine.execute_prepared(&prepared, &literals));
                    checked += 1;
                    assert_eq!(
                        from_prepared, from_text,
                        "a prepared statement answered differently from the same query \
                         as text.\n  column type: {}\n  index: {index:?}\n  query: \
                         {text}\n  as text: {from_text:?}\n  prepared: {from_prepared:?}",
                        kind.name,
                    );
                }
            }
        }
    }
    assert!(
        checked >= 500,
        "the prepared sweep collapsed to {checked} comparisons"
    );
}

/// The literal slot a `T filter .v <op> <literal>` template carries, parsed
/// back out of its source spelling. Only the two numeric shapes occur in
/// [`MUTATION_LITERALS`].
fn prepared_literals(literal: &str) -> Vec<powdb_query::ast::Literal> {
    use powdb_query::ast::Literal;
    if let Ok(value) = literal.parse::<i64>() {
        return vec![Literal::Int(value)];
    }
    match literal.parse::<f64>() {
        Ok(value) => vec![Literal::Float(value)],
        Err(_) => Vec::new(),
    }
}

// ─── the index must survive an ordinary zero bound ──────────────────────────

/// `.v > 0.0` on an indexed float column has to keep using the index.
///
/// Zero is the one finite float literal whose key the B-tree's total order and
/// the compiled leaf's IEEE comparison can disagree about, because they rank
/// `-0.0` against `0.0` differently. The first fix for that rejected every zero
/// outright, for all six operators in both directions, which took the most
/// ordinary filter there is off an index it had always used correctly. Only the
/// four combinations that actually split the pair `{-0.0, +0.0}` have to give
/// the index up.
///
/// The assertion is over the PLAN, not the answer: a full scan returns the
/// right rows, so an answer-only check cannot tell the difference between using
/// the index and silently abandoning it. The answers are compared too, because
/// keeping the index is only worth anything if it is still correct, and this
/// fixture stores BOTH zeroes so the comparison is against the case that made
/// the guard necessary in the first place.
#[test]
fn a_zero_bound_gives_up_the_index_only_where_the_two_orders_split() {
    let float_kind = COLUMN_KINDS
        .iter()
        .find(|kind| kind.name == "float")
        .expect("the float column kind is part of the matrix");

    // (predicate, keeps the index): the full enumeration of zero probes, both
    // signs, both sides, both inclusivities, plus the equality probe.
    let cases: &[(&str, bool)] = &[
        (".v > 0.0", true),
        (".v >= 0.0", false),
        (".v > -0.0", false),
        (".v >= -0.0", true),
        (".v < 0.0", false),
        (".v <= 0.0", true),
        (".v < -0.0", true),
        (".v <= -0.0", false),
        (".v = 0.0", false),
        (".v = -0.0", false),
        // The int spelling widens to `+0.0` and must land on the same verdict
        // as the float spelling of the same bound.
        (".v > 0", true),
        (".v >= 0", false),
        (".v < 0", false),
        (".v <= 0", true),
        (".v = 0", false),
    ];

    for index in INDEXED {
        let mut engine = build(float_kind, index, false);
        let mut reference = build(float_kind, Index::None, false);
        for (predicate, keeps_index) in cases {
            let query = format!("T filter {predicate} {{ .id }}");
            let plan = engine
                .lowered_plan_text(&query, 1)
                .expect("the probe query plans");
            let uses_index = plan.contains("RangeScan") || plan.contains("IndexScan");
            assert_eq!(
                uses_index,
                *keeps_index,
                "`{predicate}` with a {index:?} index should {} the index.\n{plan}",
                if *keeps_index { "keep" } else { "give up" }
            );
            assert_eq!(
                powql(&mut engine, &query),
                powql(&mut reference, &query),
                "`{predicate}` with a {index:?} index answered differently from the scan"
            );
        }
    }
}

// ─── the read-only executor ─────────────────────────────────────────────────

/// The read-only dispatch is a second copy of the read path, and it is missing
/// a branch its mutable sibling has: `dispatch.rs`'s `RangeScan` arm walks a
/// NON-unique index's composite `(value, rid)` leaves natively, while the
/// `&self` copy in `executor/mod.rs` only has the unique-index branch and falls
/// through to a compiled sequential scan for a non-unique one.
///
/// **Decision: leave it.** The gap is a performance gap, not a correctness gap,
/// and closing it means writing a THIRD copy of the probe-and-recheck logic
/// whose divergence from the other two is precisely the class of defect this
/// lane exists to remove. What the gap needs is proof that the two paths agree
/// on answers, which is what this test is; if the branch is ever added for
/// performance, this test is what keeps it honest.
///
/// It also covers the read-only half of the lowering boundary: the read-only
/// entry points and the read-only subquery materialization sites are four of
/// the eight that used to execute raw planner output, and nothing else in this
/// file runs them.
#[test]
fn the_read_only_executor_answers_exactly_like_the_mutable_one() {
    let mut checked = 0usize;
    for kind in COLUMN_KINDS {
        for index in [Index::None, Index::Btree, Index::Unique] {
            let mut engine = build(kind, index, false);
            for operator in OPERATORS {
                for literal in MUTATION_LITERALS {
                    let predicate = format!(".v {operator} {literal}");
                    for (name, template) in NESTED_SHAPES {
                        let query = nested_query(template, &predicate);
                        let mutable = ids_of(engine.execute_powql(&query));
                        let read_only = ids_of(engine.execute_powql_readonly(&query));
                        checked += 1;
                        assert_eq!(
                            mutable, read_only,
                            "the read-only executor disagreed with the mutable one.\n  \
                             column type: {}\n  index: {index:?}\n  shape: {name}\n  \
                             query: {query}\n  mutable: {mutable:?}\n  read-only: \
                             {read_only:?}",
                            kind.name,
                        );
                    }
                }
            }
        }
    }
    assert!(
        checked >= 3_000,
        "the read-only sweep collapsed to {checked} comparisons"
    );
}

// ─── JSON path indexes ──────────────────────────────────────────────────────

/// Cells where a JSON path index disagrees with the sequential scan over the
/// same documents.
///
/// Empty. It held `= 3`. A JSON path index stores the canonical PJ1 scalar,
/// which normalizes `3.0` to an integer, while the scan compared the value the
/// path yields, so `count(P filter .doc->v = 3)` answered 0 on the scan and 1
/// through the index. Here the index was the right one and the scan was wrong,
/// the opposite direction from the plain-column defect.
///
/// A JSON path has no declared type, so the "coerce the literal to the column's
/// type" rule has no input and this could not be repaired the way the
/// plain-column cells were. It was closed from the comparison side instead:
/// `compiled::json_scalar_eq` now compares a numeric node against a numeric
/// literal numerically, exactly as `json_scalar_cmp` and the generic evaluator
/// already did for `<` and `>`, so an int node and an equal float literal are
/// equal on every path including the index probe.
const KNOWN_JSON_PATH_DIVERGENCES: &[&str] = &[];

#[test]
fn a_json_path_index_changes_only_the_pinned_cross_type_cells() {
    let mut observed: Vec<String> = Vec::new();
    let mut reference = build_json(false);
    let mut indexed = build_json(true);

    for operator in OPERATORS {
        for literal in ["1", "3", "1.5", "3.0", "4.5"] {
            let query = format!("count(P filter .doc->v {operator} {literal})");
            let expected = powql(&mut reference, &query);
            let actual = powql(&mut indexed, &query);
            if actual != expected {
                observed.push(format!(
                    "{operator} {literal}: none={expected:?} index={actual:?}"
                ));
            }
        }
    }

    observed.sort();
    let mut known: Vec<String> = KNOWN_JSON_PATH_DIVERGENCES
        .iter()
        .map(|entry| (*entry).to_string())
        .collect();
    known.sort();
    assert_eq!(
        observed, known,
        "the set of cells where a JSON path index disagrees with the scan changed. A new \
         entry is a new bug; a missing entry means one was fixed and \
         KNOWN_JSON_PATH_DIVERGENCES should shrink."
    );
}

fn build_json(indexed: bool) -> Engine {
    let mut engine = Engine::new(&fresh_dir("json")).expect("engine opens over a fresh temp dir");
    exec(&mut engine, "type P { required unique id: int, doc: json }");
    for (row, value) in ["1.5", "2.5", "3.0", "4.5"].iter().enumerate() {
        exec(
            &mut engine,
            &format!(
                "insert P {{ id := {}, doc := \"{{\\\"v\\\": {value}}}\" }}",
                row + 1
            ),
        );
    }
    if indexed {
        exec(&mut engine, "alter P add index (.doc->v)");
    }
    engine
}

// ─── EXPLAIN link cardinality ───────────────────────────────────────────────

/// EXPLAIN must read a link's cardinality from the catalog, not from the
/// syntax it was written in.
///
/// Cardinality is not a property of the query text. `Catalog::derive_link_kind`
/// answers it from whether the target key carries a unique index, and
/// `alter <Target> add unique .<key>` flips the answer between one statement
/// and the next with the link untouched. EXPLAIN used to print "to-many link"
/// for every block traversal and "scalar to-one path" for every scalar one,
/// which meant it stated a cardinality it had never checked and could state the
/// opposite of what executing the very same plan would then do.
///
/// The test is built so that the label CAN differ: the same query text is
/// explained against two schemas that differ only in the uniqueness of the
/// target key, and the two labels are required to be different from each other
/// as well as individually correct. A probe whose expected string is identical
/// in every case it runs cannot fail, so asserting one string against one
/// schema would have been no coverage at all.
///
/// Each label is also cross-checked against what execution does with the same
/// path, which is the property that actually matters: EXPLAIN must not
/// contradict the run it describes.
#[test]
fn explain_reads_link_cardinality_from_the_catalog() {
    let scalar = "Order as o { uname: o.user.name }";
    let block = "User as u { .name, orders: u.orders { .oid } }";

    // Neither target key is unique: `Order.user` is a to-many link, so the
    // scalar spelling is refused at execution and the block spelling runs.
    let mut loose = link_fixture("linkcard_loose", false);
    assert_eq!(link_cardinality(&mut loose, "user"), "to-many");
    assert_eq!(link_cardinality(&mut loose, "orders"), "to-many");
    let scalar_loose = explain_text(&mut loose, scalar);
    let block_loose = explain_text(&mut loose, block);
    assert!(
        scalar_loose.contains("link uname: scalar to-many path o.user.name"),
        "a scalar path through a non-unique target key is a to-many path, got:\n{scalar_loose}"
    );
    assert!(
        block_loose.contains("nested orders: to-many link u.orders"),
        "a block over a non-unique target key is to-many, got:\n{block_loose}"
    );
    assert!(
        loose.execute_powql(scalar).is_err(),
        "execution refuses a scalar path through a to-many link, so EXPLAIN calling \
         it to-one would contradict the run"
    );
    assert!(
        loose.execute_powql(block).is_ok(),
        "a block traversal of a to-many link executes"
    );

    // The same query text against a schema that differs only in the uniqueness
    // of the two target keys. Every label has to move, and move the other way.
    let mut unique = link_fixture("linkcard_unique", true);
    assert_eq!(link_cardinality(&mut unique, "user"), "to-one");
    assert_eq!(link_cardinality(&mut unique, "orders"), "to-one");
    let scalar_unique = explain_text(&mut unique, scalar);
    let block_unique = explain_text(&mut unique, block);
    assert!(
        scalar_unique.contains("link uname: scalar to-one path o.user.name"),
        "a unique target key makes the same path to-one, got:\n{scalar_unique}"
    );
    assert!(
        block_unique.contains("nested orders: to-one link u.orders"),
        "a unique target key makes the same block to-one, got:\n{block_unique}"
    );
    assert!(
        unique.execute_powql(scalar).is_ok(),
        "the scalar path executes once its target key is unique"
    );
    assert!(
        unique.execute_powql(block).is_err(),
        "execution refuses a block traversal of a to-one link, so EXPLAIN calling it \
         to-many would contradict the run"
    );

    assert_ne!(
        scalar_loose, scalar_unique,
        "the scalar link label did not move when the catalog did, so it is still \
         being read off the syntax"
    );
    assert_ne!(
        block_loose, block_unique,
        "the block link label did not move when the catalog did, so it is still \
         being read off the syntax"
    );

    // The same engine, after the DDL rather than before it. The links are never
    // re-declared; only the catalog moves. This is the case a cardinality read
    // from a stored byte at declare time would get wrong, and it is explained
    // once on a fresh engine so no plan-cache entry from an earlier spelling of
    // the same text can be in play.
    let mut altered = link_fixture("linkcard_altered", false);
    exec(&mut altered, "alter User add unique .id");
    exec(&mut altered, "alter Order add unique .user_id");
    assert_eq!(
        link_cardinality(&mut altered, "user"),
        "to-one",
        "the catalog did not register the new unique index, so nothing below is \
         about EXPLAIN"
    );
    let scalar_altered = explain_text(&mut altered, scalar);
    let block_altered = explain_text(&mut altered, block);
    assert!(
        scalar_altered.contains("link uname: scalar to-one path o.user.name"),
        "DDL after the link declaration must move the label, got:\n{scalar_altered}"
    );
    assert!(
        block_altered.contains("nested orders: to-one link u.orders"),
        "DDL after the link declaration must move the label, got:\n{block_altered}"
    );

    // A hop that names no declared link is neither to-one nor to-many, and
    // saying either would be a guess. Execution reports it as unknown; EXPLAIN
    // has to agree.
    let unknown = "Order as o { c: o.user.company.name }";
    let unknown_plan = explain_text(&mut unique, unknown);
    assert!(
        unknown_plan.contains("link c: scalar unresolved path o.user.company.name"),
        "an undeclared hop is unresolved, not to-one, got:\n{unknown_plan}"
    );
    assert!(
        unique.execute_powql(unknown).is_err(),
        "an undeclared hop fails at execution"
    );
}

/// Two linked types whose target keys are unique or not, per `unique`.
///
/// Each state gets its own engine so that a plan cached under one catalog state
/// can never be read back under another: what is under test is the label, not
/// the cache, and mixing the two makes a failure impossible to attribute.
fn link_fixture(tag: &str, unique: bool) -> Engine {
    let key = if unique {
        "required unique"
    } else {
        "required"
    };
    let mut engine = Engine::new(&fresh_dir(tag)).expect("engine opens over a fresh temp dir");
    for statement in [
        format!("type User {{ {key} id: int, required name: str }}"),
        if unique {
            "type Order { required oid: int, unique user_id: int }".to_string()
        } else {
            "type Order { required oid: int, user_id: int }".to_string()
        },
        "link Order.user -> User on user_id = id".to_string(),
        "link User.orders -> Order on id = user_id".to_string(),
        r#"insert User { id := 1, name := "alice" }"#.to_string(),
        "insert Order { oid := 1, user_id := 1 }".to_string(),
    ] {
        exec(&mut engine, &statement);
    }
    engine
}

/// The cardinality `schema links` reports for a link, by link name.
fn link_cardinality(engine: &mut Engine, name: &str) -> String {
    match engine.execute_powql("schema links") {
        Ok(QueryResult::Rows { columns, rows }) => {
            let name_at = columns
                .iter()
                .position(|c| c == "name")
                .expect("`schema links` has a name column");
            let card_at = columns
                .iter()
                .position(|c| c == "cardinality")
                .expect("`schema links` has a cardinality column");
            for row in rows {
                if row.get(name_at) == Some(&Value::Str(name.to_string())) {
                    return match row.get(card_at) {
                        Some(Value::Str(card)) => card.clone(),
                        other => panic!("cardinality should be a string, got {other:?}"),
                    };
                }
            }
            panic!("`schema links` does not list `{name}`")
        }
        other => panic!("`schema links` should return rows, got {other:?}"),
    }
}

/// The plain text of an `explain` result, one plan line per row.
fn explain_text(engine: &mut Engine, query: &str) -> String {
    match engine.execute_powql(&format!("explain {query}")) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .into_iter()
            .flatten()
            .map(|value| match value {
                Value::Str(line) => line,
                other => panic!("explain cell should be a string, got {other:?}"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("explain should return rows, got {other:?}"),
    }
}

// ─── filter versus projection ───────────────────────────────────────────────

/// Cells where filtering on a comparison and projecting the same comparison as
/// a boolean disagree. Every entry is a live defect: the answer would depend on
/// which evaluator the plan shape happened to reach.
///
/// Empty. It held 28 `float` equality cells. A float column holding `2.0`
/// answered `.v = 2` true under the compiled float leaf (which widens the int
/// literal) and false under the generic evaluator, because `Value`'s equality
/// is strictly typed and has no Int/Float arm while its ordering does: the four
/// relational operators promoted and the two equality operators did not. A
/// filter runs the compiled leaf and the same expression projected as a boolean
/// column runs the generic evaluator, so one query could report both answers at
/// once.
///
/// The repair is in the comparison OPERATORS rather than in `Value`
/// (`eval::cross_type_numeric_cmp`). `Value::PartialEq` stays strictly typed
/// because it has to agree with `Value::Hash`, which is what GROUP BY, DISTINCT
/// and the hash joins key on; the operators read `Value`'s numeric order
/// instead, which is what the compiled leaves already read.
const KNOWN_EVALUATOR_DIVERGENCES: &[&str] = &[];

#[test]
fn filter_and_projection_agree_on_every_cross_type_cell() {
    let mut observed: Vec<String> = Vec::new();
    for kind in COLUMN_KINDS {
        for force_generic in [false, true] {
            let mut engine = build(kind, Index::None, force_generic);
            for (predicate, _) in predicates() {
                let filtered = powql(&mut engine, &format!("T filter {predicate} {{ .id }}"));
                let projected = projection_true_ids(
                    &mut engine,
                    &format!("T {{ id: .id, matched: {predicate} }}"),
                );
                if filtered != projected {
                    observed.push(format!(
                        "{}: {predicate} ({})",
                        kind.name,
                        if force_generic {
                            "fast paths forced off"
                        } else {
                            "fast paths on"
                        }
                    ));
                }
            }
        }
    }

    observed.sort();
    let mut known: Vec<String> = KNOWN_EVALUATOR_DIVERGENCES
        .iter()
        .map(|entry| (*entry).to_string())
        .collect();
    known.sort();
    assert_eq!(
        observed, known,
        "the set of cells where a filter and the same expression projected as a boolean \
         disagree changed. A new entry is a new bug; a missing entry means one was fixed \
         and KNOWN_EVALUATOR_DIVERGENCES should shrink."
    );
}

/// Ids of the rows whose projected boolean column is true.
fn projection_true_ids(engine: &mut Engine, query: &str) -> Outcome {
    match engine.execute_powql(query) {
        Ok(QueryResult::Rows { columns, rows }) => {
            let (Some(id_at), Some(matched_at)) = (
                columns.iter().position(|c| c == "id"),
                columns.iter().position(|c| c == "matched"),
            ) else {
                return Outcome::Other(format!("projection shape: {columns:?}"));
            };
            let mut ids = Vec::new();
            for row in rows {
                if row.get(matched_at) == Some(&Value::Bool(true)) {
                    match row.get(id_at) {
                        Some(Value::Int(id)) => ids.push(*id),
                        other => return Outcome::Other(format!("non-int id: {other:?}")),
                    }
                }
            }
            ids.sort_unstable();
            Outcome::Ids(ids)
        }
        Ok(other) => Outcome::Other(format!("{other:?}")),
        Err(err) => Outcome::Err(err.to_string()),
    }
}

// ─── prepared statements ────────────────────────────────────────────────────

/// A prepared MUTATION must answer like the same mutation as text.
///
/// The prepared path is a third copy of the probe logic, built from RAW planner
/// output at prepare time and dispatched before the substituted plan is lowered
/// at all: `Engine::try_build_update_pk_fast` accepted any indexed key column
/// and then probed it with `BTree::lookup_int`, which binary-searches assuming
/// every key is a `Value::Int`. Against a float, datetime, str or bool column,
/// or against a non-unique index, that probe addressed a key lane the rows are
/// not in, so the mutation reported `Modified(0)` and wrote nothing while the
/// same statement as text reported `Modified(1)` and wrote the row. A prepared
/// statement is the shape every driver and ORM emits, so this was the default
/// way to reach it.
#[test]
fn prepared_update_matches_text() {
    let mut divergences: Vec<String> = Vec::new();
    for kind in COLUMN_KINDS {
        for index in [Index::None, Index::Btree, Index::Unique] {
            for literal in MUTATION_LITERALS {
                let text = format!("T filter .v = {literal} update {{ tag := 9 }}");

                let mut a = build(kind, index, false);
                let from_text = powql(&mut a, &text);

                let mut b = build(kind, index, false);
                let Ok(prepared) = b.prepare(&text) else {
                    continue;
                };
                let mut literals = prepared_literals(literal);
                if literals.is_empty() {
                    continue;
                }
                literals.push(powdb_query::ast::Literal::Int(9));
                if literals.len() != prepared.param_count {
                    continue;
                }
                let from_prepared = ids_of(b.execute_prepared(&prepared, &literals));
                if from_prepared != from_text {
                    divergences.push(format!(
                        "{} {index:?}: `{text}` text={from_text:?} prepared={from_prepared:?}",
                        kind.name
                    ));
                }
            }
        }
    }
    assert!(
        divergences.is_empty(),
        "prepared update diverged from text:\n  {}",
        divergences.join("\n  ")
    );
}

/// A prepared handle must survive DDL applied under it.
///
/// The prepared fast path caches a table slot, a row layout, byte offsets and
/// an index decision taken at PREPARE time, and `alter` can invalidate any of
/// them. The dangerous direction is the one that makes the cached decision
/// wrong rather than merely stale: adding a unique index to the key column
/// makes the fast path newly ELIGIBLE, and adding a column moves every byte
/// offset the cached patch writes to. Either way the only acceptable outcomes
/// are the answer the same statement gives as text, or a typed error. A
/// silently different rowcount, or a patch applied at the old offsets, is
/// data corruption from a statement the caller has every reason to believe is
/// the same one they prepared.
#[test]
fn a_prepared_update_survives_ddl_applied_after_prepare() {
    let mut compared = 0usize;
    // (index state at prepare time, DDL applied afterwards).
    let transitions: &[(Index, &str)] = &[
        // Makes the fast path eligible where it was not.
        (Index::None, "alter T add unique .v"),
        (Index::Btree, "alter T add column extra: int"),
        // Moves the row layout out from under a fast path that IS live.
        (Index::Unique, "alter T add column extra: int"),
    ];

    for kind in COLUMN_KINDS {
        for (before, ddl) in transitions {
            for literal in MUTATION_LITERALS {
                let text = format!("T filter .v = {literal} update {{ tag := 9 }}");

                // The reference: the same rows, the same DDL, the statement run
                // as text rather than through a handle prepared before it.
                let mut reference = build(kind, *before, false);
                if reference.execute_powql(ddl).is_err() {
                    continue;
                }
                let from_text = powql(&mut reference, &text);

                let mut engine = build(kind, *before, false);
                let Ok(prepared) = engine.prepare(&text) else {
                    continue;
                };
                exec(&mut engine, ddl);
                let mut literals = prepared_literals(literal);
                if literals.is_empty() {
                    continue;
                }
                literals.push(powdb_query::ast::Literal::Int(9));
                if literals.len() != prepared.param_count {
                    continue;
                }
                let from_prepared = ids_of(engine.execute_prepared(&prepared, &literals));
                compared += 1;

                assert_eq!(
                    from_prepared, from_text,
                    "a handle prepared before `{ddl}` answered differently from the same \
                     statement as text after it.\n  column type: {}\n  index at prepare: \
                     {before:?}\n  statement: {text}\n  as text: {from_text:?}\n  through \
                     the handle: {from_prepared:?}",
                    kind.name
                );
            }
        }
    }

    // Every cell above can `continue`, so a change that made `prepare` or the
    // literal shapes stop lining up would leave this test green while comparing
    // nothing at all.
    assert!(
        compared >= 60,
        "the prepare-then-DDL sweep collapsed to {compared} comparisons"
    );
}

/// Same sweep with the prepared engine forced onto the generic path, which
/// disables `prepared-update-pk`. Running both is what separates the two
/// causes: a cell that fails here as well is the evaluator disagreeing with
/// itself, and a cell that fails only above is the fast path.
#[test]
fn prepared_update_matches_text_generic() {
    let mut divergences: Vec<String> = Vec::new();
    for kind in COLUMN_KINDS {
        for index in [Index::None, Index::Btree, Index::Unique] {
            for literal in MUTATION_LITERALS {
                let text = format!("T filter .v = {literal} update {{ tag := 9 }}");
                let mut a = build(kind, index, false);
                let from_text = powql(&mut a, &text);
                let mut b = build(kind, index, true);
                let Ok(prepared) = b.prepare(&text) else {
                    continue;
                };
                let mut literals = prepared_literals(literal);
                if literals.is_empty() {
                    continue;
                }
                literals.push(powdb_query::ast::Literal::Int(9));
                if literals.len() != prepared.param_count {
                    continue;
                }
                let from_prepared = ids_of(b.execute_prepared(&prepared, &literals));
                if from_prepared != from_text {
                    divergences.push(format!(
                        "{} {index:?}: `{text}` text={from_text:?} prepared={from_prepared:?}",
                        kind.name
                    ));
                }
            }
        }
    }
    assert!(
        divergences.is_empty(),
        "prepared update diverged from text (generic):\n  {}",
        divergences.join("\n  ")
    );
}

// ─── the paths a cross-type comparison reaches in a shipped build ───────────

/// One row too wide to inline takes the whole table off the compiled predicate,
/// and that must not change any answer.
///
/// `Table::has_overflow_rows` is a per-TABLE flag: a single row over
/// `MAX_ROW_DATA_SIZE` (~4070 bytes) makes every scan of that table fall back
/// to the decoded generic evaluator, because the raw byte path rehydrates to v1
/// and mis-reads a spilled value. That is a correct thing to do about overflow
/// and a catastrophic one if the two evaluators disagree: `count(O filter .v =
/// 1)` over a float column answered 0 unindexed and 1 indexed, purely because
/// an unrelated row in the same table was long. The delete case is the same
/// answer applied destructively, so the rows that survive depend on whether an
/// index exists.
///
/// This is the reachability half of the cross-type story. `cross_type_matrix`
/// compares the compiled and generic evaluators directly, by forcing the switch
/// that a shipped build has no way to reach. This reaches the generic evaluator
/// the way production does.
#[test]
fn an_overflow_row_does_not_change_a_cross_type_answer() {
    /// A `float` column, a wide `pad` column, and enough padding on one row to
    /// push it out of line.
    fn build_overflow(indexed: bool) -> Engine {
        let mut engine =
            Engine::new(&fresh_dir("overflow")).expect("engine opens over a fresh temp dir");
        exec(
            &mut engine,
            "type O { required unique id: int, v: float, pad: str }",
        );
        for (id, value) in [(1i64, "1.0"), (2, "2.0"), (3, "3.0")] {
            exec(
                &mut engine,
                &format!("insert O {{ id := {id}, v := {value}, pad := \"\" }}"),
            );
        }
        // The one wide row. Its own `v` is a value no probe below matches, so
        // it can only affect an answer through the path it forces.
        exec(
            &mut engine,
            &format!(
                "insert O {{ id := 4, v := 99.0, pad := \"{}\" }}",
                "x".repeat(5_000)
            ),
        );
        if indexed {
            exec(&mut engine, "alter O add index .v");
        }
        engine
    }

    let mut reference = build_overflow(false);
    let mut indexed = build_overflow(true);

    // The fixture is only meaningful while the wide row actually forces the
    // fallback; a change to the inline limit would otherwise quietly turn this
    // into a test of the compiled path against itself.
    assert!(
        matches!(
            powql(&mut reference, "count(O)"),
            Outcome::Scalar(ref text) if text == "Int(4)"
        ),
        "the overflow fixture lost a row, so the wide value was not stored"
    );

    for (probe, expected) in [
        // The cross-type cell: an int literal against a float column.
        ("count(O filter .v = 1)", Outcome::Scalar("Int(1)".into())),
        ("count(O filter .v != 1)", Outcome::Scalar("Int(3)".into())),
        ("count(O filter .v < 3)", Outcome::Scalar("Int(2)".into())),
        // And the same predicate as a conjunct, which re-checks the residual
        // through a different evaluator entry point again.
        (
            "count(O filter .v = 1 and .id > 0)",
            Outcome::Scalar("Int(1)".into()),
        ),
    ] {
        let from_scan = powql(&mut reference, probe);
        let from_index = powql(&mut indexed, probe);
        assert_eq!(
            from_scan, expected,
            "`{probe}` over a table holding one overflow row answered {from_scan:?}"
        );
        assert_eq!(
            from_index, expected,
            "`{probe}` answered {from_index:?} with an index on the column"
        );
    }

    // The row-returning form, so the assertion is about WHICH rows and not only
    // how many.
    for probe in [
        "O filter .v = 1 { .id }",
        "O filter .v = 1 and .id > 0 { .id }",
    ] {
        assert_eq!(
            powql(&mut reference, probe),
            Outcome::Ids(vec![1]),
            "`{probe}` returned the wrong rows on the scan"
        );
        assert_eq!(
            powql(&mut indexed, probe),
            Outcome::Ids(vec![1]),
            "`{probe}` returned the wrong rows through the index"
        );
    }

    // And destructively: a delete under a cross-type filter must destroy the
    // same rows whether or not an index exists.
    let mut delete_scan = build_overflow(false);
    let mut delete_index = build_overflow(true);
    assert_eq!(
        powql(&mut delete_scan, "O filter .v = 1 delete"),
        Outcome::Modified(1),
        "the delete destroyed the wrong number of rows on the scan"
    );
    assert_eq!(
        powql(&mut delete_index, "O filter .v = 1 delete"),
        Outcome::Modified(1),
        "the delete destroyed the wrong number of rows through the index"
    );
    let survivors_scan = powql(&mut delete_scan, "O { .id }");
    let survivors_index = powql(&mut delete_index, "O { .id }");
    assert_eq!(
        survivors_scan,
        Outcome::Ids(vec![2, 3, 4]),
        "the wrong rows survived the delete on the scan"
    );
    assert_eq!(
        survivors_index, survivors_scan,
        "an index changed which rows a delete destroyed"
    );
}

/// A cross-type comparison must mean the same thing alone and as a conjunct.
///
/// A conjunction is planned with one conjunct driving the access path and the
/// rest re-checked as a residual, and the residual re-check over a non-SeqScan
/// driver does not compile: it evaluates through the generic path. So
/// `count(T filter .v = 1)` answered 1 and `count(T filter .v = 1 and .id > 0)`
/// answered 0 over the same rows, which is not a physical-path difference a
/// user can see or avoid; it reads as the engine disagreeing with itself about
/// what `and` means.
#[test]
fn a_cross_type_comparison_means_the_same_thing_as_a_conjunct() {
    for kind in COLUMN_KINDS {
        for index in [Index::None, Index::Btree, Index::Unique] {
            for force_generic in [false, true] {
                let mut engine = build(kind, index, force_generic);
                for predicate in [".v = 1", ".v = 1.0", ".v != 1", ".v < 3", ".v >= 2"] {
                    // `.id > 0` is true of every fixture row, so the conjunct
                    // narrows nothing and can only change the answer by
                    // changing how the other conjunct is evaluated. It is also
                    // indexed (`id` is `required unique`), so it drives the
                    // access path and pushes `{predicate}` into the residual.
                    let alone = powql(&mut engine, &format!("T filter {predicate} {{ .id }}"));
                    let conjunct = powql(
                        &mut engine,
                        &format!("T filter {predicate} and .id > 0 {{ .id }}"),
                    );
                    let reversed = powql(
                        &mut engine,
                        &format!("T filter .id > 0 and {predicate} {{ .id }}"),
                    );
                    assert_eq!(
                        conjunct,
                        alone,
                        "`{predicate}` answered differently as a conjunct.\n  column type: \
                         {}\n  index: {index:?}\n  fast paths: {}\n  alone: {alone:?}\n  \
                         with `and .id > 0`: {conjunct:?}",
                        kind.name,
                        if force_generic { "forced off" } else { "on" }
                    );
                    assert_eq!(
                        reversed,
                        alone,
                        "`{predicate}` answered differently as the SECOND conjunct.\n  \
                         column type: {}\n  index: {index:?}\n  fast paths: {}",
                        kind.name,
                        if force_generic { "forced off" } else { "on" }
                    );
                }
            }
        }
    }
}
