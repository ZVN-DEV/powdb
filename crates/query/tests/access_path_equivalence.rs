//! Access-path equivalence runner.
//!
//! The planner is pure: it never looks at the catalog. One query text therefore
//! runs through different physical code depending on what indexes happen to
//! exist and on which executor fast path matches its plan shape. Those paths
//! are supposed to be indistinguishable from outside. Twice they were not, and
//! both times the disagreement shipped because every test ran the query exactly
//! one way.
//!
//! This runner executes each query across the cross product of
//!
//!   * **catalog state**: no index, plain btree, unique index, expression
//!     index, materialized view;
//!   * **physical path**: fast paths on, and fast paths forced off via the
//!     `testing`-only [`Engine::set_force_generic_path`].
//!
//! and compares column names, full row values, and (where the query defines an
//! order) row order.
//!
//! ## What is compared with order, and why not everything
//!
//! Within one catalog state the two toggle settings run the *same* plan tree.
//! Any row-order difference between them is a bug in a fast path, so those two
//! are compared as exact ordered lists, always.
//!
//! Across catalog states the plan tree itself differs (an index probe walks the
//! btree, a scan walks the heap), and PowDB does not promise an order for a
//! query that does not ask for one. Those comparisons are therefore multiset
//! comparisons unless the query carries an `order` clause, in which case the
//! order is part of the contract and is compared exactly.
//!
//! ## EXPLAIN
//!
//! A silent fallback would make this whole runner vacuous: if a "btree" fixture
//! quietly scanned, the two sides would agree for the boring reason. Every read
//! case therefore also asserts that EXPLAIN names the physical path the catalog
//! state should have produced.

#![cfg(feature = "testing")]

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_DIR: AtomicU64 = AtomicU64::new(0);

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_pathequiv_{tag}_{}_{}_{}",
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

// ─── catalog states ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    NoIndex,
    Btree,
    Unique,
    ExprIndex,
    MatView,
}

const ALL_ACCESS: [Access; 5] = [
    Access::NoIndex,
    Access::Btree,
    Access::Unique,
    Access::ExprIndex,
    Access::MatView,
];

impl Access {
    /// The relation a query runs against. The materialized-view state answers
    /// the same questions through a physically separate copy of the rows.
    fn table(self) -> &'static str {
        match self {
            Access::MatView => "V",
            _ => "T",
        }
    }

    /// DDL applied after the rows are loaded.
    fn ddl(self) -> &'static [&'static str] {
        match self {
            Access::NoIndex => &[],
            Access::Btree => &[
                "alter T add index .n",
                "alter T add index .s",
                "alter T add index .ts",
            ],
            Access::Unique => &["alter T add unique .u"],
            Access::ExprIndex => &["alter T add index (.doc->k)"],
            // The view is materialized from the same rows and carries no index
            // of its own, so every query over it must take a plain scan.
            Access::MatView => &["materialize V as T"],
        }
    }

    /// Whether this catalog state can drive a scan from `column`.
    fn indexes(self, column: &str) -> bool {
        // `id` is declared `required unique`, so every base-table state carries
        // a unique index on it; the view copy carries none.
        match self {
            Access::Btree => matches!(column, "id" | "n" | "s"),
            Access::Unique => matches!(column, "id" | "u"),
            Access::NoIndex | Access::ExprIndex => column == "id",
            Access::MatView => false,
        }
    }

    fn indexes_doc_k(self) -> bool {
        self == Access::ExprIndex
    }
}

// ─── fixture ────────────────────────────────────────────────────────────────

/// One fixture row. `n` and `ts` always hold the same number so an int column
/// and a datetime column are compared against identical values: the v0.20.0
/// datetime bug was exactly those two disagreeing.
struct Seed {
    id: i64,
    n: Option<i64>,
    u: Option<i64>,
    s: Option<&'static str>,
    f: Option<f64>,
    b: Option<bool>,
}

/// Deliberately awkward data: duplicate keys, NULLs in every nullable column,
/// negatives, a zero, and an empty string.
const SEEDS: &[Seed] = &[
    Seed {
        id: 1,
        n: Some(3),
        u: Some(100),
        s: Some("alpha"),
        f: Some(1.5),
        b: Some(true),
    },
    Seed {
        id: 2,
        n: Some(7),
        u: Some(101),
        s: Some("beta"),
        f: Some(-0.5),
        b: Some(false),
    },
    Seed {
        id: 3,
        n: Some(3),
        u: None,
        s: Some("alpha"),
        f: Some(1.5),
        b: Some(true),
    },
    Seed {
        id: 4,
        n: None,
        u: Some(103),
        s: None,
        f: None,
        b: None,
    },
    Seed {
        id: 5,
        n: Some(9),
        u: Some(104),
        s: Some("gamma"),
        f: Some(2.25),
        b: Some(false),
    },
    Seed {
        id: 6,
        n: Some(7),
        u: Some(105),
        s: Some("beta"),
        f: Some(0.0),
        b: Some(true),
    },
    Seed {
        id: 7,
        n: Some(-2),
        u: None,
        s: Some(""),
        f: Some(-3.75),
        b: Some(false),
    },
    Seed {
        id: 8,
        n: Some(0),
        u: Some(107),
        s: Some("delta"),
        f: Some(4.0),
        b: Some(true),
    },
    Seed {
        id: 9,
        n: Some(9),
        u: Some(108),
        s: Some("gamma"),
        f: Some(2.25),
        b: None,
    },
    Seed {
        id: 10,
        n: Some(3),
        u: Some(109),
        s: Some("alpha"),
        f: Some(1.5),
        b: Some(false),
    },
    Seed {
        id: 11,
        n: None,
        u: Some(110),
        s: None,
        f: None,
        b: Some(true),
    },
    Seed {
        id: 12,
        n: Some(12),
        u: Some(111),
        s: Some("omega"),
        f: Some(8.125),
        b: Some(false),
    },
];

const SCHEMA: &str = "type T { required unique id: int, n: int, u: int, s: str, \
                      f: float, b: bool, ts: datetime, doc: json }";

fn insert_statement(seed: &Seed) -> String {
    let mut fields = vec![format!("id := {}", seed.id)];
    if let Some(n) = seed.n {
        fields.push(format!("n := {n}"));
        fields.push(format!("ts := {n}"));
    }
    if let Some(u) = seed.u {
        fields.push(format!("u := {u}"));
    }
    if let Some(s) = seed.s {
        fields.push(format!("s := \"{s}\""));
    }
    if let Some(f) = seed.f {
        fields.push(format!("f := {f:?}"));
    }
    if let Some(b) = seed.b {
        fields.push(format!("b := {b}"));
    }
    // The json document mirrors `n` so the expression-index state has something
    // to key on, and carries a null for the rows where `n` is missing.
    let k = match seed.n {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    };
    fields.push(format!("doc := \"{{\\\"k\\\": {k}}}\""));
    format!("insert T {{ {} }}", fields.join(", "))
}

fn build(access: Access, force_generic: bool) -> Engine {
    let mut engine =
        Engine::new(&fresh_dir("fixture")).expect("engine opens over a fresh temp dir");
    exec(&mut engine, SCHEMA);
    for seed in SEEDS {
        exec(&mut engine, &insert_statement(seed));
    }
    for statement in access.ddl() {
        exec(&mut engine, statement);
    }
    engine.set_force_generic_path(force_generic);
    engine
}

fn exec(engine: &mut Engine, query: &str) {
    engine
        .execute_powql(query)
        .unwrap_or_else(|err| panic!("fixture statement `{query}` failed: {err}"));
}

// ─── outcomes ───────────────────────────────────────────────────────────────

/// A comparable snapshot of whatever a query produced, including the error.
/// `QueryResult` has no `PartialEq`, and an error is as much a part of the
/// contract as a row is: a fast path that errors where the generic path
/// answers is exactly the kind of divergence this runner exists to catch.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    Scalar(Value),
    Modified(u64),
    Other(String),
    Error(String),
}

impl Outcome {
    /// The same outcome with rows put in a canonical order, for comparing two
    /// runs whose plan trees legitimately visit rows in a different order.
    fn order_insensitive(&self) -> Outcome {
        match self {
            Outcome::Rows { columns, rows } => {
                let mut rows = rows.clone();
                rows.sort();
                Outcome::Rows {
                    columns: columns.clone(),
                    rows,
                }
            }
            other => other.clone(),
        }
    }
}

fn run(engine: &mut Engine, query: &str) -> Outcome {
    match engine.execute_powql(query) {
        Ok(QueryResult::Rows { columns, rows }) => Outcome::Rows { columns, rows },
        Ok(QueryResult::Scalar(value)) => Outcome::Scalar(value),
        Ok(QueryResult::Modified(n)) => Outcome::Modified(n),
        Ok(QueryResult::Created(name)) => Outcome::Other(format!("created {name}")),
        Ok(QueryResult::Executed { message }) => Outcome::Other(message),
        Err(err) => Outcome::Error(err.to_string()),
    }
}

fn explain(engine: &mut Engine, query: &str) -> String {
    match engine.execute_powql(&format!("explain {query}")) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|row| match row.first() {
                Some(Value::Str(line)) => line.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("explain `{query}` produced {other:?}"),
    }
}

// ─── read cases ─────────────────────────────────────────────────────────────

/// What the catalog state should make the planner+executor do at the leaf.
#[derive(Clone, Copy, Debug)]
enum Driver {
    /// `.col = <literal>`: an index on `col` turns it into an IndexScan.
    Eq(&'static str),
    /// `.col <op> <literal>` with an inequality: an index turns it into a
    /// RangeScan.
    Range(&'static str),
    /// `.doc->k = <literal>`: an expression index turns it into ExprIndexScan.
    JsonEq,
    /// A predicate that must never reach an index however the catalog is
    /// arranged. Today's only member: an integer literal against a `datetime`
    /// column. Index keys carry a type tag, so an int literal cannot probe a
    /// datetime index and the lowering pass deliberately rewrites the scan
    /// (`lower_unindexed_scans`, and see the datetime section of docs/POWQL.md).
    /// Asserting it here means a change to that rule surfaces as a test
    /// failure rather than as a silent access-path shift.
    AlwaysScan,
    /// Shapes with no single index-eligible driver (compound predicates,
    /// aggregates over the whole table, joins). Only the relation is asserted.
    Unconstrained,
}

struct Case {
    powql: &'static str,
    driver: Driver,
    /// True when the query carries an `order` clause, making row order part of
    /// its contract rather than an artifact of the physical path.
    ordered: bool,
}

const READ_CASES: &[Case] = &[
    // ── single-column drivers, per type ──
    Case {
        powql: "{t} filter .n = 3 { .id, .n }",
        driver: Driver::Eq("n"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .n != 3 { .id, .n }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "{t} filter .n > 3 { .id, .n }",
        driver: Driver::Range("n"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .n >= 3 { .id, .n }",
        driver: Driver::Range("n"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .n < 3 { .id, .n }",
        driver: Driver::Range("n"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .n <= 3 { .id, .n }",
        driver: Driver::Range("n"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .ts = 3 { .id, .ts }",
        driver: Driver::AlwaysScan,
        ordered: false,
    },
    Case {
        powql: "{t} filter .ts > 3 { .id, .ts }",
        driver: Driver::AlwaysScan,
        ordered: false,
    },
    Case {
        powql: "{t} filter .ts <= 7 { .id, .ts }",
        driver: Driver::AlwaysScan,
        ordered: false,
    },
    Case {
        powql: "{t} filter .s = \"alpha\" { .id, .s }",
        driver: Driver::Eq("s"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .s > \"alpha\" { .id, .s }",
        driver: Driver::Range("s"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .s = \"\" { .id, .s }",
        driver: Driver::Eq("s"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .u = 107 { .id, .u }",
        driver: Driver::Eq("u"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .u >= 105 { .id, .u }",
        driver: Driver::Range("u"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .id = 8 { .id, .n }",
        driver: Driver::Eq("id"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .f > 1.5 { .id, .f }",
        driver: Driver::Range("f"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .f = 1.5 { .id, .f }",
        driver: Driver::Eq("f"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .b = true { .id, .b }",
        driver: Driver::Eq("b"),
        ordered: false,
    },
    Case {
        powql: "{t} filter .doc->k = 9 { .id }",
        driver: Driver::JsonEq,
        ordered: false,
    },
    // ── compound predicates: `and` compiles, `or` does not ──
    Case {
        powql: "{t} filter .n = 3 and .s = \"alpha\" { .id }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "{t} filter .n = 3 or .n = 9 { .id, .n }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "{t} filter .n > 0 and .s != \"beta\" { .id, .n, .s }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "{t} filter not (.n = 3) { .id, .n }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "{t} filter .n in (3, 9) { .id, .n }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    // ── whole-row and computed projections ──
    Case {
        powql: "{t} { .id, .n, .s, .f, .b, .ts }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "{t} { .id, doubled: .n + .n }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "{t} filter .n = 3 { .id, su: upper(.s) }",
        driver: Driver::Eq("n"),
        ordered: false,
    },
    // ── order / limit / offset ──
    Case {
        powql: "{t} order .n { .id, .n }",
        driver: Driver::Unconstrained,
        ordered: true,
    },
    Case {
        powql: "{t} order .n desc { .id, .n }",
        driver: Driver::Unconstrained,
        ordered: true,
    },
    Case {
        powql: "{t} order .n, .id { .id, .n }",
        driver: Driver::Unconstrained,
        ordered: true,
    },
    Case {
        powql: "{t} order .id limit 4 { .id, .n }",
        driver: Driver::Unconstrained,
        ordered: true,
    },
    Case {
        powql: "{t} order .id offset 3 limit 4 { .id, .n }",
        driver: Driver::Unconstrained,
        ordered: true,
    },
    Case {
        powql: "{t} order .s, .id { .id, .s }",
        driver: Driver::Unconstrained,
        ordered: true,
    },
    Case {
        powql: "{t} filter .n > 0 order .n desc, .id limit 3 { .id, .n }",
        driver: Driver::Unconstrained,
        ordered: true,
    },
    Case {
        powql: "{t} order .id limit 0 { .id }",
        driver: Driver::Unconstrained,
        ordered: true,
    },
    Case {
        powql: "{t} distinct { .n }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    // ── aggregates ──
    Case {
        powql: "count({t})",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "count({t} { .n })",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "count({t} filter .n = 3)",
        driver: Driver::Eq("n"),
        ordered: false,
    },
    Case {
        powql: "count({t} filter .n > 3)",
        driver: Driver::Range("n"),
        ordered: false,
    },
    Case {
        powql: "sum({t} { .n })",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "avg({t} { .n })",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "min({t} { .n })",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "max({t} { .n })",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "sum({t} filter .n > 0 { .n })",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "sum({t} { .f })",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "min({t} { .s })",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    // ── group / having ──
    Case {
        powql: "{t} group .n { .n, c: count(*) }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "{t} group .n having count(*) > 1 { .n, c: count(*) }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "{t} group .s { .s, total: sum(.n) }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
    Case {
        powql: "{t} filter .n > 0 group .b { .b, c: count(*), m: max(.n) }",
        driver: Driver::Unconstrained,
        ordered: false,
    },
];

/// Substring EXPLAIN must contain for this catalog state, or `None` when only
/// the relation can be pinned down.
fn expected_leaf(access: Access, driver: Driver) -> Option<String> {
    let table = access.table();
    match driver {
        Driver::Eq(column) if access.indexes(column) => {
            Some(format!("IndexScan table={table} column={column}"))
        }
        Driver::Range(column) if access.indexes(column) => {
            Some(format!("RangeScan table={table} column={column}"))
        }
        Driver::JsonEq if access.indexes_doc_k() => Some(format!("ExprIndexScan table={table}")),
        Driver::Eq(_) | Driver::Range(_) | Driver::JsonEq | Driver::AlwaysScan => {
            Some(format!("SeqScan table={table}"))
        }
        Driver::Unconstrained => None,
    }
}

#[test]
fn every_access_path_and_the_generic_evaluator_agree_on_reads() {
    let mut engines: Vec<(Access, bool, Engine)> = Vec::new();
    for access in ALL_ACCESS {
        for force_generic in [false, true] {
            engines.push((access, force_generic, build(access, force_generic)));
        }
    }

    for case in READ_CASES {
        let mut observed: Vec<(Access, bool, Outcome)> = Vec::new();
        for (access, force_generic, engine) in engines.iter_mut() {
            let query = case.powql.replace("{t}", access.table());

            // A fast path that silently declined would make the comparison
            // vacuous, so pin the physical leaf before trusting the answer.
            if let Some(expected) = expected_leaf(*access, case.driver) {
                let plan = explain(engine, &query);
                assert!(
                    plan.contains(&expected),
                    "EXPLAIN for `{query}` under {access:?} should name `{expected}`, got:\n{plan}"
                );
            } else {
                let plan = explain(engine, &query);
                assert!(
                    plan.contains(&format!("table={}", access.table())),
                    "EXPLAIN for `{query}` under {access:?} never mentions the relation:\n{plan}"
                );
            }

            observed.push((*access, *force_generic, run(engine, &query)));
        }

        // 1. Within one catalog state the plan tree is identical, so the two
        //    toggle settings must agree down to row order.
        for access in ALL_ACCESS {
            let fast = observed
                .iter()
                .find(|(a, force, _)| *a == access && !force)
                .expect("fast-path run recorded for every access state");
            let generic = observed
                .iter()
                .find(|(a, force, _)| *a == access && *force)
                .expect("forced-generic run recorded for every access state");
            assert_eq!(
                fast.2, generic.2,
                "`{}` under {access:?}: the fast paths and the generic evaluator disagree",
                case.powql
            );
        }

        // 2. Across catalog states the answer must not depend on which indexes
        //    exist. Order is only part of the contract when the query asked
        //    for one.
        let (base_access, _, base) = &observed[0];
        for (access, force_generic, outcome) in observed.iter().skip(1) {
            let (left, right) = if case.ordered {
                (base.clone(), outcome.clone())
            } else {
                (base.order_insensitive(), outcome.order_insensitive())
            };
            assert_eq!(
                left, right,
                "`{}` answers differently under {access:?} (force_generic={force_generic}) \
                 than under {base_access:?}",
                case.powql
            );
        }
    }
}

// ─── mutation cases ─────────────────────────────────────────────────────────

/// A mutation and the read-back that proves what it actually wrote. The
/// read-back is ordered by the primary key so its row order is a contract.
struct MutationCase {
    mutation: &'static str,
    readback: &'static str,
}

const MUTATION_CASES: &[MutationCase] = &[
    MutationCase {
        mutation: "{t} filter .n = 3 update { s := \"patched\" }",
        readback: "{t} order .id { .id, .n, .s }",
    },
    MutationCase {
        mutation: "{t} filter .n > 3 update { n := 0 }",
        readback: "{t} order .id { .id, .n, .ts }",
    },
    MutationCase {
        mutation: "{t} filter .id = 8 update { n := 42, s := \"eight\" }",
        readback: "{t} order .id { .id, .n, .s }",
    },
    MutationCase {
        mutation: "{t} filter .s = \"alpha\" update { f := 9.5 }",
        readback: "{t} order .id { .id, .s, .f }",
    },
    MutationCase {
        // Every row matches, including the ones whose `n` is NULL: this is the
        // shape where a partially applied multi-row update would show up.
        mutation: "{t} filter .id > 0 update { s := \"all\" }",
        readback: "{t} order .id { .id, .s }",
    },
    MutationCase {
        mutation: "{t} filter .n = 3 delete",
        readback: "{t} order .id { .id, .n, .s }",
    },
    MutationCase {
        mutation: "{t} filter .n > 3 delete",
        readback: "{t} order .id { .id, .n }",
    },
    MutationCase {
        mutation: "{t} filter .s = \"alpha\" delete",
        readback: "{t} order .id { .id, .s }",
    },
    MutationCase {
        mutation: "{t} filter .n = 999 delete",
        readback: "{t} order .id { .id, .n }",
    },
    MutationCase {
        mutation: "{t} filter .n = 3 update { n := .n + 100 }",
        readback: "{t} order .id { .id, .n }",
    },
    MutationCase {
        // A conjunction: under a btree state the discovery scan lowers to
        // `Filter(<index scan>)` and takes the index-and-recheck rid collection,
        // which is a physically different way of deciding the same row set.
        mutation: "{t} filter .n = 3 and .s = \"alpha\" update { f := 9.5 }",
        readback: "{t} order .id { .id, .n, .s, .f }",
    },
    MutationCase {
        mutation: "{t} filter .n = 3 and .s = \"alpha\" delete",
        readback: "{t} order .id { .id, .n, .s }",
    },
];

#[test]
fn every_access_path_and_the_generic_evaluator_agree_on_mutations() {
    // A materialized view is not writable, so mutations run against the base
    // table states only.
    let writable = [
        Access::NoIndex,
        Access::Btree,
        Access::Unique,
        Access::ExprIndex,
    ];

    for case in MUTATION_CASES {
        let mut observed: Vec<(Access, bool, Outcome, Outcome)> = Vec::new();
        for access in writable {
            for force_generic in [false, true] {
                // Mutations are destructive, so each run needs its own engine.
                let mut engine = build(access, force_generic);
                let mutation = case.mutation.replace("{t}", access.table());
                let readback = case.readback.replace("{t}", access.table());
                let effect = run(&mut engine, &mutation);
                let after = run(&mut engine, &readback);
                observed.push((access, force_generic, effect, after));
            }
        }

        for access in writable {
            let fast = observed
                .iter()
                .find(|(a, force, _, _)| *a == access && !force)
                .expect("fast-path run recorded for every writable access state");
            let generic = observed
                .iter()
                .find(|(a, force, _, _)| *a == access && *force)
                .expect("forced-generic run recorded for every writable access state");
            assert_eq!(
                fast.2, generic.2,
                "`{}` under {access:?}: fast and generic report different row counts",
                case.mutation
            );
            assert_eq!(
                fast.3, generic.3,
                "`{}` under {access:?}: fast and generic left the table in different states",
                case.mutation
            );
        }

        let (base_access, _, base_effect, base_after) = &observed[0];
        for (access, force_generic, effect, after) in observed.iter().skip(1) {
            assert_eq!(
                base_effect, effect,
                "`{}` reports a different row count under {access:?} \
                 (force_generic={force_generic}) than under {base_access:?}",
                case.mutation
            );
            assert_eq!(
                base_after, after,
                "`{}` leaves a different table under {access:?} \
                 (force_generic={force_generic}) than under {base_access:?}",
                case.mutation
            );
        }
    }
}

// ─── the switch itself ──────────────────────────────────────────────────────

/// One shape that is supposed to have a fast path, and the catalog state that
/// makes it fire.
struct DivertedShape {
    access: Access,
    query: &'static str,
    /// The check site this shape exists to cover, as reported by
    /// `Engine::forced_generic_sites`. Naming it is the whole point: a query
    /// passes several check sites, so "something declined" stays true even
    /// after the one guard under test is deleted. Asserting the specific name
    /// makes every guard individually mutation-testable.
    site: &'static str,
    /// Prose for the failure message.
    what: &'static str,
}

/// Every shape below is served by a distinct fast path in the executor. If one
/// of them stops declining, either that fast path lost its check site or the
/// planner stopped producing the shape; both make the equivalence runner blind
/// to that path, which is worse than not having it.
const DIVERTED_SHAPES: &[DivertedShape] = &[
    DivertedShape {
        access: Access::NoIndex,
        query: "T filter .n = 3 { .id, .n }",
        site: "project-filter-limit",
        what: "Project(Filter(SeqScan)) projection fast path",
    },
    DivertedShape {
        access: Access::NoIndex,
        query: "T { .id, .n }",
        site: "project-filter-limit",
        what: "Project(SeqScan) projection fast path",
    },
    DivertedShape {
        access: Access::NoIndex,
        query: "T order .n desc limit 3 { .id, .n }",
        site: "project-filter-sort-limit",
        what: "top-N heap fast path",
    },
    DivertedShape {
        access: Access::NoIndex,
        query: "count(T)",
        site: "count-fast-block",
        what: "raw row-count fast path",
    },
    DivertedShape {
        access: Access::NoIndex,
        query: "count(T filter .n = 3)",
        site: "count-fast-block",
        what: "compiled count-over-filter fast path",
    },
    DivertedShape {
        access: Access::NoIndex,
        query: "sum(T { .n })",
        site: "agg-single-col",
        what: "single-column aggregate fast path",
    },
    DivertedShape {
        access: Access::NoIndex,
        query: "T filter .n = 3",
        site: "filter-seqscan-raw",
        what: "fused Filter+SeqScan scan",
    },
    DivertedShape {
        access: Access::Btree,
        query: "T filter .n = 3 { .id }",
        site: "project-over-index-scan",
        what: "Project over IndexScan fast path",
    },
    DivertedShape {
        access: Access::Btree,
        query: "T filter .n = 3 and .s = \"alpha\"",
        site: "filter-index-residual",
        what: "index residual fast path",
    },
];

/// Mutations get their own list because each one needs a throwaway engine.
const DIVERTED_MUTATIONS: &[DivertedShape] = &[
    DivertedShape {
        access: Access::NoIndex,
        query: "T filter .n = 3 update { s := \"x\" }",
        site: "fused-scan-update",
        what: "fused scan+update",
    },
    DivertedShape {
        access: Access::NoIndex,
        query: "T filter .n = 3 delete",
        site: "delete-fused",
        what: "fused scan+delete",
    },
    DivertedShape {
        access: Access::NoIndex,
        query: "T filter .id = 8 update { s := \"x\" }",
        site: "update-byte-patch",
        what: "byte-patch update over collected rids",
    },
    DivertedShape {
        // The hole this labelling found. `Filter(IndexScan)` as the discovery
        // scan of a mutation took the index-and-recheck path whether the switch
        // was on or off, so the runner was comparing that path with itself.
        access: Access::Btree,
        query: "T filter .n = 3 and .s = \"alpha\" update { f := 9.5 }",
        site: "mutation-index-residual",
        what: "index residual rid collection for mutations",
    },
    DivertedShape {
        access: Access::Btree,
        query: "T filter .n = 3 and .s = \"alpha\" delete",
        site: "mutation-index-residual",
        what: "index residual rid collection for deletes",
    },
];

/// The runner is only as good as the switch it depends on. A flag that
/// silently stopped diverting would turn every comparison above into the same
/// code compared with itself, and the suite would still be green. Count the
/// declines instead of trusting them.
#[test]
fn forcing_the_generic_path_diverts_every_shape_it_claims_to() {
    for shape in DIVERTED_SHAPES {
        let mut engine = build(shape.access, true);
        engine.reset_forced_generic_sites();
        let forced = run(&mut engine, shape.query);
        let declined = engine.forced_generic_sites();
        assert!(
            declined.contains(&shape.site),
            "`{}` under {:?} never declined at `{}`: the {} has no \
             `generic_path_forced(\"{}\")` check, so the runner never compares \
             it against anything. Sites that did decline: {declined:?}",
            shape.query,
            shape.access,
            shape.site,
            shape.what,
            shape.site
        );

        // Same query, flag off: nothing may decline, or the record is
        // measuring something other than the switch.
        let mut unforced = build(shape.access, false);
        unforced.reset_forced_generic_sites();
        let fast = run(&mut unforced, shape.query);
        assert!(
            unforced.forced_generic_sites().is_empty(),
            "`{}` declined a fast path with the switch off: {:?}",
            shape.query,
            unforced.forced_generic_sites()
        );

        assert_eq!(
            fast.order_insensitive(),
            forced.order_insensitive(),
            "`{}` changed answer when the {} was switched off",
            shape.query,
            shape.what
        );
    }
}

#[test]
fn forcing_the_generic_path_diverts_every_mutation_shape_it_claims_to() {
    for shape in DIVERTED_MUTATIONS {
        let mut engine = build(shape.access, true);
        engine.reset_forced_generic_sites();
        let forced = run(&mut engine, shape.query);
        let declined = engine.forced_generic_sites();
        assert!(
            declined.contains(&shape.site),
            "`{}` under {:?} never declined at `{}`: the {} has no \
             `generic_path_forced(\"{}\")` check. Sites that did decline: {declined:?}",
            shape.query,
            shape.access,
            shape.site,
            shape.what,
            shape.site
        );
        let after_forced = run(&mut engine, "T order .id { .id, .n, .s, .f }");

        let mut unforced = build(shape.access, false);
        unforced.reset_forced_generic_sites();
        let fast = run(&mut unforced, shape.query);
        assert!(
            unforced.forced_generic_sites().is_empty(),
            "`{}` declined a fast path with the switch off: {:?}",
            shape.query,
            unforced.forced_generic_sites()
        );
        let after_fast = run(&mut unforced, "T order .id { .id, .n, .s, .f }");

        assert_eq!(
            fast, forced,
            "`{}` reported a different row count",
            shape.query
        );
        assert_eq!(
            after_fast, after_forced,
            "`{}` left a different table behind",
            shape.query
        );
    }
}

/// Flipping the flag back must restore the fast paths on the same engine, so
/// the switch is not a one-way door that quietly poisons an engine for the
/// rest of a test.
#[test]
fn the_switch_is_reversible() {
    let mut engine = build(Access::Btree, true);
    let forced = run(&mut engine, "count(T filter .n = 3)");
    engine.set_force_generic_path(false);
    engine.reset_forced_generic_sites();
    let restored = run(&mut engine, "count(T filter .n = 3)");
    assert_eq!(
        forced, restored,
        "toggling the flag back changed the answer"
    );
    assert!(
        engine.forced_generic_sites().is_empty(),
        "fast paths stayed suppressed after the flag was turned off: {:?}",
        engine.forced_generic_sites()
    );
}

/// Every label passed to `generic_path_forced` must be distinct. Two sites
/// sharing a name would let one of them lose its guard while a `contains`
/// assertion above still passed on the other's decline, which is the failure
/// mode this whole mechanism exists to remove.
///
/// The set is read out of the executor source rather than hand-listed, so a new
/// check site cannot be added with a copy-pasted label.
#[test]
fn every_check_site_label_is_unique() {
    let labels = declared_check_site_labels();
    assert!(
        labels.len() >= 30,
        "only {} check-site labels found; the scan is not seeing the executor sources",
        labels.len()
    );
    let mut sorted = labels.clone();
    sorted.sort();
    let mut deduped = sorted.clone();
    deduped.dedup();
    assert_eq!(
        sorted, deduped,
        "two `generic_path_forced` check sites share a label, so one of them can \
         lose its guard without any test noticing"
    );
}

/// Every `generic_path_forced` / `compile_predicate_unless_forced` label written
/// in the executor sources, read out of the source rather than hand-listed so a
/// newly added check site cannot escape the accounting below.
fn declared_check_site_labels() -> Vec<String> {
    fn labels_in(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        for marker in ["generic_path_forced(\"", "compile_predicate_unless_forced("] {
            let mut rest = source;
            while let Some(at) = rest.find(marker) {
                rest = &rest[at + marker.len()..];
                // `generic_path_forced` takes its label inline; the compile
                // helper takes it as the first argument on the next line.
                let quoted = match rest.find('"') {
                    Some(start) if rest[..start].chars().all(char::is_whitespace) => {
                        &rest[start + 1..]
                    }
                    _ if marker.ends_with('"') => rest,
                    _ => continue,
                };
                let Some(end) = quoted.find('"') else {
                    continue;
                };
                found.push(quoted[..end].to_string());
            }
        }
        found
    }

    let executor = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("executor");
    let mut files = vec![executor.join("mod.rs"), executor.join("prepared.rs")];
    for entry in std::fs::read_dir(executor.join("plan_exec")).expect("plan_exec is readable") {
        files.push(entry.expect("plan_exec entry is readable").path());
    }

    let mut labels = Vec::new();
    for file in files {
        if file.extension().and_then(|extension| extension.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&file)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", file.display()));
        labels.extend(labels_in(&source));
    }
    labels
}

/// Both halves fixed, and pinned so they stay fixed.
///
/// A `float` column holding `1.0` compared with the integer literal `1` used to
/// give three different answers depending on physical state alone:
///
///   * compiled predicate (no index): the row matches;
///   * generic evaluator: the row does not match;
///   * btree index on the column: the row did not match either.
///
/// The index half was fixed first. The probe coerces the literal to the
/// column's declared type before addressing the B-tree's type-tagged key lane
/// (`plan_exec::lowering::coerce_column_index_key`), so `X filter .fv = 1` no
/// longer starts returning nothing the moment someone runs
/// `alter X add index .fv`. That was the same failure mode as the v0.20.0
/// datetime bug.
///
/// The evaluator half is fixed now too: the comparison operators evaluated
/// generically read the same numeric order the compiled leaves and `Value::Ord`
/// use (`eval::cross_type_numeric_cmp`), rather than `Value::PartialEq`, which
/// stays strict per variant because it has to agree with `Value::Hash`. So the
/// generic path no longer answers false to `.fv = 1` while answering true to
/// both `.fv <= 1` and `.fv >= 1`.
///
/// Kept rather than deleted: the generated cases above vary catalog state and
/// physical path but compare each cell only against its own siblings, so a
/// change that moved all three answers together would still pass there. This
/// asserts the value.
#[test]
fn a_float_column_compared_to_an_int_literal_answers_the_same_on_every_path() {
    fn ids(engine: &mut Engine, query: &str) -> Vec<i64> {
        match run(engine, query) {
            Outcome::Rows { rows, .. } => rows
                .iter()
                .filter_map(|row| match row.first() {
                    Some(Value::Int(id)) => Some(*id),
                    _ => None,
                })
                .collect(),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    fn fixture(force_generic: bool, indexed: bool) -> Engine {
        let mut engine =
            Engine::new(&fresh_dir("floateq")).expect("engine opens over a fresh temp dir");
        exec(&mut engine, "type X { required unique id: int, fv: float }");
        exec(&mut engine, "insert X { id := 1, fv := 1.0 }");
        if indexed {
            exec(&mut engine, "alter X add index .fv");
        }
        engine.set_force_generic_path(force_generic);
        engine
    }

    let mut compiled = fixture(false, false);
    let mut generic = fixture(true, false);
    let mut indexed = fixture(false, true);

    assert_eq!(
        ids(&mut compiled, "X filter .fv = 1 { .id }"),
        vec![1],
        "the compiled predicate coerces the int literal to a float and matches"
    );
    assert_eq!(
        ids(&mut generic, "X filter .fv = 1 { .id }"),
        vec![1],
        "the generic evaluator compares a numeric pair as numbers, so it matches the \
         same row the compiled predicate does"
    );
    assert_eq!(
        ids(&mut indexed, "X filter .fv = 1 { .id }"),
        vec![1],
        "the index probe coerces the int literal to the column's float type, so it \
         addresses the same keys the compiled predicate reads and gives the same \
         answer adding an index must never change"
    );

    // And the generic answer agrees with its own ordering operators: equal
    // under `=`, under `<=` and under `>=`, and under nothing else.
    for engine in [&mut compiled, &mut generic, &mut indexed] {
        assert_eq!(ids(engine, "X filter .fv <= 1 { .id }"), vec![1]);
        assert_eq!(ids(engine, "X filter .fv >= 1 { .id }"), vec![1]);
        assert_eq!(ids(engine, "X filter .fv != 1 { .id }"), Vec::<i64>::new());
        assert_eq!(ids(engine, "X filter .fv < 1 { .id }"), Vec::<i64>::new());
        assert_eq!(ids(engine, "X filter .fv > 1 { .id }"), Vec::<i64>::new());
    }
}

/// Pinned because it is broken.
///
/// `materialize V as <query>` infers the view's column types from the rows the
/// query returns right now. When it returns none, every column is typed `str`,
/// and the wrong types are persisted: from then on any comparison against a
/// non-string literal is a hard type error, permanently, even after the source
/// table fills up.
///
/// This is the materialized-view arm of the catalog-state axis, so it belongs
/// with the rest of the access-path evidence. The runner above seeds its view
/// from a non-empty table and therefore never trips it; that is exactly why it
/// needs its own test rather than being left to chance.
#[test]
fn a_view_materialized_over_zero_rows_types_every_column_as_str() {
    let mut engine =
        Engine::new(&fresh_dir("emptyview")).expect("engine opens over a fresh temp dir");
    exec(&mut engine, "type S { required unique id: int, n: int }");
    exec(&mut engine, "insert S { id := 1 }");
    // `.n` is null on the only row, so the view is materialized over no rows.
    exec(&mut engine, "materialize EV as S filter .n > 0");

    let described = run(&mut engine, "describe EV");
    let Outcome::Rows { rows, .. } = &described else {
        panic!("expected rows from describe, got {described:?}");
    };
    let types: Vec<String> = rows
        .iter()
        .map(|row| match row.get(1) {
            Some(Value::Str(name)) => name.to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        types,
        vec!["str".to_string(), "str".to_string()],
        "an empty materialization should inherit the source column types; it currently \
         types everything as str"
    );

    // The consequence: the view can never be filtered on its int column again.
    let filtered = run(&mut engine, "EV filter .n = 1 { .id }");
    assert!(
        matches!(&filtered, Outcome::Error(message) if message.contains("type mismatch")),
        "expected the persisted str typing to make an int comparison a type error, \
         got {filtered:?}"
    );
}

/// Pinned because it is broken, and this one takes the process with it.
///
/// Once a view has been mistyped by the empty-materialization bug above, the
/// first refresh that has an actual row to write encodes an `int` into a column
/// the schema calls `str` and reaches
/// `unreachable!("variable column with non-variable value")` in the row
/// encoder. Under the release profile PowDB is built `panic = "abort"`, so this
/// is a process kill, and the refresh is automatic: a plain read of the view
/// after an unrelated insert is enough to trigger it. No unusual privilege is
/// needed, only `materialize` over a predicate that matches nothing yet.
///
/// Guarded on `panic = "unwind"` so a release-profile test run does not abort
/// the harness while asserting that a release build would abort.
#[cfg(panic = "unwind")]
#[test]
#[should_panic(expected = "variable column with non-variable value")]
fn refreshing_a_mistyped_view_aborts_the_process() {
    let mut engine =
        Engine::new(&fresh_dir("viewpanic")).expect("engine opens over a fresh temp dir");
    exec(&mut engine, "type S { required unique id: int, n: int }");
    exec(&mut engine, "insert S { id := 1 }");
    exec(&mut engine, "materialize EV as S filter .n > 0");
    // An ordinary insert into the source, then an ordinary read of the view.
    // Any read at all will do now that every shape honours the dirty flag
    // (`every_read_shape_refreshes_a_dirty_materialized_view`): `EV { .id }`,
    // `count(EV)` and `refresh EV` all abort the same way.
    exec(&mut engine, "insert S { id := 2, n := 5 }");
    let _ = run(&mut engine, "EV");
}

/// Every read shape must see a refreshed materialized view, whatever physical
/// path serves it.
///
/// A view is marked dirty when its source changes and refreshed on the next
/// read. The `SeqScan`, `count` and `GroupBy` arms did that; the projection
/// fast paths and the single-column aggregate fast path did not, and read the
/// stale copy straight off the heap. So the documented way to query a view
/// (`ActiveUsers { .name, .email }`, per docs/POWQL.md) silently returned data
/// from before the last write, while `count(ActiveUsers)` on the same engine
/// returned the fresh number.
///
/// The refresh now runs once at the boundary every statement and subquery
/// crosses (`Engine::refresh_dirty_views_read_by`) instead of at each arm that
/// remembers it, so a fast path added later cannot lose it again.
///
/// Every shape runs with the fast paths on AND forced off. The forced-off
/// column is what identified this as a fast-path defect rather than a refresh
/// policy, and keeping it is what would catch a repair that merely moved the
/// staleness elsewhere.
#[test]
fn every_read_shape_refreshes_a_dirty_materialized_view() {
    fn dirty_view(force_generic: bool) -> Engine {
        let mut engine =
            Engine::new(&fresh_dir("dirtyview")).expect("engine opens over a fresh temp dir");
        exec(&mut engine, "type S { required unique id: int, n: int }");
        exec(&mut engine, "insert S { id := 1, n := 1 }");
        exec(&mut engine, "materialize DV as S filter .n > 0");
        // Now the view is one row behind its source, and nothing has read it.
        exec(&mut engine, "insert S { id := 2, n := 5 }");
        engine.set_force_generic_path(force_generic);
        engine
    }

    fn ids(values: &[i64]) -> Outcome {
        Outcome::Rows {
            columns: vec!["id".into()],
            rows: values.iter().map(|v| vec![Value::Int(*v)]).collect(),
        }
    }

    // Each entry is a shape served by a different physical path: the bare scan,
    // the projection fast path, the projection-with-filter fast path, both
    // limit variants, the top-N heap, distinct, the single-column aggregates,
    // and the count block.
    let shapes: &[(&str, Outcome)] = &[
        (
            "DV",
            Outcome::Rows {
                columns: vec!["id".into(), "n".into()],
                rows: vec![
                    vec![Value::Int(1), Value::Int(1)],
                    vec![Value::Int(2), Value::Int(5)],
                ],
            },
        ),
        ("DV { .id }", ids(&[1, 2])),
        ("DV filter .n > 0 { .id }", ids(&[1, 2])),
        ("DV { .id } limit 5", ids(&[1, 2])),
        ("DV filter .n > 0 { .id } limit 5", ids(&[1, 2])),
        ("DV order .id { .id }", ids(&[1, 2])),
        ("DV order .id limit 5 { .id }", ids(&[1, 2])),
        ("DV filter .n > 0 order .id limit 5 { .id }", ids(&[1, 2])),
        (
            "DV distinct { .n }",
            Outcome::Rows {
                columns: vec!["n".into()],
                rows: vec![vec![Value::Int(1)], vec![Value::Int(5)]],
            },
        ),
        ("count(DV)", Outcome::Scalar(Value::Int(2))),
        ("sum(DV { .n })", Outcome::Scalar(Value::Int(6))),
        ("max(DV { .n })", Outcome::Scalar(Value::Int(5))),
        ("min(DV { .n })", Outcome::Scalar(Value::Int(1))),
    ];

    for (query, fresh) in shapes {
        for force_generic in [false, true] {
            let mut engine = dirty_view(force_generic);
            assert_eq!(
                &run(&mut engine, query).order_insensitive(),
                &fresh.order_insensitive(),
                "`{query}` answered from the stale backing table instead of refreshing \
                 the view first (fast paths {}). An access path is never a semantic, and \
                 a fast path that skips the refresh check is exactly that.",
                if force_generic { "forced off" } else { "on" }
            );
        }
    }
}

// ─── check-site accounting ──────────────────────────────────────────────────

/// The check sites the case corpus above actually diverts through.
///
/// The per-shape assertions prove a named guard fires for the shape that
/// targets it. This proves the converse for the corpus as a whole: a guard no
/// case reaches is a fast path the runner never compares against anything, and
/// a guard that stops being reached is a case that quietly stopped covering it.
/// Both surface here as a reviewable diff instead of as silence.
const SITES_REACHED_BY_THE_CORPUS: &[&str] = &[
    "agg-single-col",
    "count-fast-block",
    "delete-fused",
    "filter-index-residual",
    "filter-seqscan-raw",
    "fused-scan-update",
    "mutation-filter-seqscan:predicate",
    "mutation-index-residual",
    "project-filter-limit",
    "project-filter-sort-limit",
    "project-over-index-scan",
    "update-byte-patch",
    "update-var-shrink",
];

/// Declared check sites the corpus does not reach, each with the reason. A new
/// guard has to be added to one of these two lists, so it cannot be introduced
/// without someone deciding whether the runner covers it.
const UNCOVERED_SITES: &[(&str, &str)] = &[
    // Nested inside an outer guard: by the time compilation is attempted the
    // enclosing fast path has already declined, so these never record while the
    // switch is on. They still route through the single compile entry point,
    // which is what stops a future caller from bypassing the switch.
    ("agg-single-col:predicate", "nested inside agg-single-col"),
    ("count-filter:predicate", "nested inside count-fast-block"),
    ("delete-fused:predicate", "nested inside delete-fused"),
    (
        "filter-seqscan:predicate",
        "nested inside filter-seqscan-raw",
    ),
    (
        "fused-scan-update:predicate",
        "nested inside fused-scan-update",
    ),
    (
        "project-filter-limit:predicate",
        "nested inside project-filter-limit",
    ),
    (
        "project-filter-sort-limit:predicate",
        "nested inside project-filter-sort-limit",
    ),
    // Reached only through `Engine::open_readonly`, which serves plans from a
    // separate mirror of the dispatcher. Covered by the readonly suites, not by
    // this runner, whose fixtures all need to write before they can read.
    ("readonly:count-fast-block", "read-only engine mirror"),
    ("readonly:count-filter:predicate", "read-only engine mirror"),
    ("readonly:filter-seqscan-raw", "read-only engine mirror"),
    (
        "readonly:filter-seqscan:predicate",
        "read-only engine mirror",
    ),
    (
        "readonly:index-scan-scan-fallback:predicate",
        "read-only engine mirror",
    ),
    (
        "readonly:project-over-index-scan",
        "read-only engine mirror",
    ),
    (
        "readonly:range-scan-scan-fallback:predicate",
        "read-only engine mirror",
    ),
    // The planner emits IndexScan/RangeScan speculatively, but
    // `lower_unindexed_scans` rewrites the ones with no matching index into
    // Filter(SeqScan) before execution, so the in-executor scan fallbacks are
    // only reachable if that lowering pass regresses. They are guarded anyway:
    // an unguarded fallback would be a hole the day the lowering changes.
    (
        "index-scan-scan-fallback:predicate",
        "unreachable behind plan lowering",
    ),
    (
        "range-scan-scan-fallback:predicate",
        "unreachable behind plan lowering",
    ),
    (
        "mutation-index-scan-scan-fallback:predicate",
        "unreachable behind plan lowering",
    ),
    // Prepared statements have their own entry point; this runner drives
    // `execute_powql` only.
    ("prepared-insert", "prepared-statement entry point"),
    ("prepared-update-pk", "prepared-statement entry point"),
];

fn sites_reached_by_the_corpus() -> Vec<&'static str> {
    let mut reached: Vec<&'static str> = Vec::new();

    for access in ALL_ACCESS {
        let mut engine = build(access, true);
        for case in READ_CASES {
            let _ = run(&mut engine, &case.powql.replace("{t}", access.table()));
        }
        reached.extend(engine.forced_generic_sites());
    }
    for access in [
        Access::NoIndex,
        Access::Btree,
        Access::Unique,
        Access::ExprIndex,
    ] {
        for case in MUTATION_CASES {
            let mut engine = build(access, true);
            let _ = run(&mut engine, &case.mutation.replace("{t}", access.table()));
            let _ = run(&mut engine, &case.readback.replace("{t}", access.table()));
            reached.extend(engine.forced_generic_sites());
        }
    }

    reached.sort();
    reached.dedup();
    reached
}

#[test]
fn the_case_corpus_reaches_the_check_sites_it_is_recorded_as_reaching() {
    assert_eq!(
        sites_reached_by_the_corpus(),
        SITES_REACHED_BY_THE_CORPUS,
        "the set of check sites the case corpus reaches changed. A missing entry is \
         a fast path this runner stopped comparing; a new one should be added to the list."
    );
}

/// Nothing may exist in between: every guard written in the executor is either
/// exercised by the corpus or listed as deliberately not exercised, with a
/// reason. Without this, a new fast path could be guarded, never covered, and
/// never noticed, which is the same vacuum as not guarding it at all.
#[test]
fn every_declared_check_site_is_either_covered_or_excused() {
    let mut declared = declared_check_site_labels();
    declared.sort();
    declared.dedup();

    let mut accounted: Vec<String> = SITES_REACHED_BY_THE_CORPUS
        .iter()
        .map(|site| (*site).to_string())
        .chain(UNCOVERED_SITES.iter().map(|(site, _)| (*site).to_string()))
        .collect();
    accounted.sort();
    accounted.dedup();

    let unaccounted: Vec<&String> = declared.iter().filter(|s| !accounted.contains(s)).collect();
    assert!(
        unaccounted.is_empty(),
        "these executor check sites are neither reached by the corpus nor excused in \
         UNCOVERED_SITES: {unaccounted:?}"
    );

    let stale: Vec<&String> = accounted.iter().filter(|s| !declared.contains(s)).collect();
    assert!(
        stale.is_empty(),
        "these sites are listed but no longer exist in the executor: {stale:?}"
    );
}

/// A join must not mean two different things about the same comparison
/// depending on whether it is the key or a residual conjunct.
///
/// `plan_exec::join::hash_join` matches the key by bucketing the build side in
/// an `FxHashMap<Value, _>`, so a join key is compared by `Value`'s own `Hash`
/// and `PartialEq`, which are strict per variant. The residual conjuncts of the
/// same `on` clause are evaluated by `eval::eval_binop_mode` instead. Teaching
/// that evaluator cross-type numeric comparison without also giving the hash
/// side a canonical numeric key would have made `on a.n = b.f` match nothing as
/// a key and match numerically as a residual, in the same statement.
///
/// The assertion is that the two spellings AGREE, not which rule they agree on:
/// a later change that gives joins cross-type keys on both sides is a fix, and
/// this must not stand in its way. Only a split between them is a defect.
#[test]
fn a_join_key_and_a_join_residual_agree_about_a_cross_type_comparison() {
    for force_generic in [false, true] {
        let mut engine =
            Engine::new(&fresh_dir("joincross")).expect("engine opens over a fresh temp dir");
        exec(&mut engine, "type A { required unique id: int, n: int }");
        exec(&mut engine, "type B { required unique id: int, f: float }");
        exec(&mut engine, "insert A { id := 1, n := 1 }");
        exec(&mut engine, "insert A { id := 2, n := 2 }");
        exec(&mut engine, "insert B { id := 1, f := 1.0 }");
        exec(&mut engine, "insert B { id := 2, f := 2.5 }");
        engine.set_force_generic_path(force_generic);

        // `a.n = b.f` as the sole key, so the hash join buckets on it.
        let as_key = run(&mut engine, "A as a join B as b on a.n = b.f { a.id }");
        // The same comparison as a residual: `a.id = b.id` is the key, and
        // `a.n = b.f` is re-checked per matched pair. `a.id = b.id` pairs the
        // rows 1:1, so the residual alone decides the answer.
        let as_residual = run(
            &mut engine,
            "A as a join B as b on a.id = b.id and a.n = b.f { a.id }",
        );
        assert_eq!(
            as_key.order_insensitive(),
            as_residual.order_insensitive(),
            "`a.n = b.f` meant one thing as a join key and another as a join residual \
             (fast paths {}). Whatever the rule for cross-type keys is, both halves of \
             one `on` clause have to use it.",
            if force_generic { "forced off" } else { "on" }
        );
    }
}
