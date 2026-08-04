//! The query generator.
//!
//! A *shape* is a family of semantically equivalent queries written three
//! ways: PowQL, PowDB-SQL, and SQLite SQL. The shape name is the unit that
//! `known_divergences.toml` keys on, so shapes are deliberately narrow: a
//! shape that mixes two semantics would let an entry excusing one of them
//! suppress the other.
//!
//! Determinism: the whole case list is a pure function of the seed and the
//! budget. It does not depend on the fixture, so the identical case list runs
//! against every fixture and a divergence can always be reproduced by
//! (shape, case index, fixture).

use crate::fixture::predicate_literal;
use crate::model::{ColType, Column, Kind, Lit, COLUMNS, LIKE_COLUMN, LIKE_META_COLUMN, TABLE};
use crate::rng::Rng;

/// One generated case: the same question in three dialects.
#[derive(Debug, Clone)]
pub struct Case {
    pub shape: &'static str,
    pub powql: String,
    pub powdb_sql: String,
    pub sqlite_sql: String,
    /// Bound parameters for the SQLite side.
    ///
    /// SQLite always receives literals as bound parameters while PowDB
    /// receives literal *text*, because PowQL has no parameter syntax in a
    /// plain query string. That asymmetry is deliberate: SQLite holds the
    /// generator's exact intended value, so if PowDB's literal parsing loses
    /// or rounds a value, the oracle sees it instead of hiding it behind two
    /// identically-broken parsers.
    pub sqlite_params: Vec<Lit>,
    /// True when the query fixes a total row order, so the comparison must be
    /// sequence-sensitive. False means the comparison sorts both sides first.
    pub ordered: bool,
    /// One entry per output column. Drives the SQLite normalization.
    pub kinds: Vec<Kind>,
    /// True when PowDB answers with `QueryResult::Scalar`, which carries no
    /// column name. See `normalize::scalarize`.
    pub scalar: bool,
    /// False when SQLite is not a valid authority for this question, so the
    /// case runs two-way (PowQL vs PowDB-SQL) instead of three-way.
    ///
    /// Only one shape sets this today (`agg_sum_avg_float`); the reason is
    /// stated there. It is a *declared coverage gap*, counted and reported by
    /// `Report::two_way_comparisons`, not a silent skip and not a ledger
    /// entry: a difference SQLite cannot adjudicate should not be written down
    /// as if SQLite had adjudicated it.
    pub sqlite_comparable: bool,
}

/// Nullable columns (everything except the required `id` key).
fn nullable() -> Vec<&'static Column> {
    COLUMNS.iter().skip(1).collect()
}

/// Columns whose order SQLite and PowDB can be asked to agree on. Excludes
/// `json`: see [`ColType::orderable_against_sqlite`].
fn orderable() -> Vec<&'static Column> {
    COLUMNS
        .iter()
        .filter(|c| c.ty.orderable_against_sqlite())
        .collect()
}

const CMP_OPS: [&str; 6] = ["=", "!=", "<", "<=", ">", ">="];

/// SQLite's spelling of PowDB's engine-wide NULLS LAST ordering contract
/// (`crates/query/src/executor/plan_exec/mod.rs`, `compare_order_values`).
///
/// SQLite is NULLS FIRST ascending and NULLS LAST descending; PowDB is NULLS
/// LAST in both directions. `col IS NULL` as a leading key states PowDB's rule
/// in SQLite's own language, so the two engines can be compared on the
/// placement of the *non-null* values without the NULL block masking it.
fn sqlite_nulls_last(col: &str, desc: bool) -> String {
    let dir = if desc { "DESC" } else { "ASC" };
    format!("{col} IS NULL ASC, {col} {dir}, id ASC")
}

fn powql_order(col: &str, desc: bool) -> String {
    let dir = if desc { "desc" } else { "asc" };
    format!("order .{col} {dir}, .id asc")
}

fn sql_order(col: &str, desc: bool) -> String {
    let dir = if desc { "DESC" } else { "ASC" };
    format!("{col} {dir}, id ASC")
}

struct Builder {
    cases: Vec<Case>,
}

impl Builder {
    fn push(&mut self, case: Case) {
        self.cases.push(case);
    }
}

/// Generate the complete case list.
///
/// `per_shape` bounds each shape independently so a cheap shape cannot crowd
/// out an expensive one, and the total is bounded by `budget`. Both are fixed
/// in CI, so the run is byte-for-byte reproducible.
pub fn generate(seed: u64, per_shape: usize, budget: usize) -> Vec<Case> {
    let mut rng = Rng::new(seed);
    let mut groups: Vec<Vec<Case>> = Vec::new();

    for build in SHAPES {
        let mut b = Builder { cases: Vec::new() };
        build(&mut b, &mut rng, per_shape);
        groups.push(b.cases);
    }

    // Round-robin so a small budget still touches every shape.
    let mut out = Vec::new();
    let deepest = groups.iter().map(Vec::len).max().unwrap_or(0);
    'outer: for depth in 0..deepest {
        for group in &groups {
            if let Some(case) = group.get(depth) {
                if out.len() == budget {
                    break 'outer;
                }
                out.push(case.clone());
            }
        }
    }
    out
}

type ShapeFn = fn(&mut Builder, &mut Rng, usize);

const SHAPES: &[ShapeFn] = &[
    select_star,
    project_cols,
    filter_cmp,
    filter_is_null,
    filter_and,
    filter_or,
    filter_not,
    order_asc_naive,
    order_asc_nulls_last,
    order_desc,
    order_limit_offset,
    agg_count_star,
    agg_count_col,
    agg_sum_avg_int,
    agg_sum_avg_float,
    agg_min_max,
    group_count,
    group_having,
    distinct_col,
    like_ascii_case,
    like_wildcard_only,
    like_metachar_in_text,
    json_path_project,
    json_path_filter,
    json_whole_column_eq,
    sql_backslash_literal,
    // Policing twins, appended deliberately.
    //
    // `generate` walks this list with one shared `Rng`, so a shape's cases
    // depend on every shape before it. Appending leaves every existing shape's
    // draws byte-identical, which means a twin can be added without shifting
    // the recorded divergence profile of the shape it polices. Inserting one
    // next to its twin would reshuffle every later shape and could starve a
    // narrow ledger entry of its last matching case, failing the staleness
    // gate for a reason that has nothing to do with the engine.
    agg_sum_avg_int_zero_default,
    filter_not_non_null,
];

fn select_star(b: &mut Builder, _rng: &mut Rng, _n: usize) {
    b.push(Case {
        shape: "select_star",
        powql: TABLE.to_string(),
        powdb_sql: format!("SELECT * FROM {TABLE}"),
        sqlite_sql: format!("SELECT * FROM {TABLE}"),
        sqlite_params: Vec::new(),
        ordered: false,
        kinds: COLUMNS.iter().map(|c| Kind::Col(c.ty)).collect(),
        scalar: false,
        sqlite_comparable: true,
    });
    // Ordered by the unique key, so this one also pins row order end to end.
    b.push(Case {
        shape: "select_star",
        powql: format!("{TABLE} order .id asc"),
        powdb_sql: format!("SELECT * FROM {TABLE} ORDER BY id ASC"),
        sqlite_sql: format!("SELECT * FROM {TABLE} ORDER BY id ASC"),
        sqlite_params: Vec::new(),
        ordered: true,
        kinds: COLUMNS.iter().map(|c| Kind::Col(c.ty)).collect(),
        scalar: false,
        sqlite_comparable: true,
    });
}

fn project_cols(b: &mut Builder, rng: &mut Rng, n: usize) {
    for _ in 0..n {
        let width = 1 + rng.below(4);
        let mut chosen: Vec<&Column> = Vec::new();
        for _ in 0..width {
            let c = *rng.pick(&COLUMNS);
            if !chosen.iter().any(|e| e.name == c.name) {
                chosen.push(COLUMNS.iter().find(|e| e.name == c.name).expect("column"));
            }
        }
        let powql_list = chosen
            .iter()
            .map(|c| format!(".{}", c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let sql_list = chosen
            .iter()
            .map(|c| c.name.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        b.push(Case {
            shape: "project_cols",
            powql: format!("{TABLE} {{ {powql_list} }}"),
            powdb_sql: format!("SELECT {sql_list} FROM {TABLE}"),
            sqlite_sql: format!("SELECT {sql_list} FROM {TABLE}"),
            sqlite_params: Vec::new(),
            ordered: false,
            kinds: chosen.iter().map(|c| Kind::Col(c.ty)).collect(),
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

fn filter_cmp(b: &mut Builder, rng: &mut Rng, n: usize) {
    let cols = orderable();
    for _ in 0..n {
        let col = *rng.pick(&cols);
        let op = *rng.pick(&CMP_OPS);
        let lit = predicate_literal(rng, col);
        let (Some(pq), Some(ps)) = (lit.powql(), lit.powdb_sql()) else {
            continue;
        };
        b.push(Case {
            shape: "filter_cmp",
            powql: format!("{TABLE} filter .{} {op} {pq} {{ .id }}", col.name),
            powdb_sql: format!("SELECT id FROM {TABLE} WHERE {} {op} {ps}", col.name),
            sqlite_sql: format!("SELECT id FROM {TABLE} WHERE {} {op} ?1", col.name),
            sqlite_params: vec![lit],
            ordered: false,
            kinds: vec![Kind::Col(ColType::Int)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

fn filter_is_null(b: &mut Builder, rng: &mut Rng, n: usize) {
    let cols = nullable();
    for _ in 0..n {
        let col = *rng.pick(&cols);
        let negated = rng.chance(1, 2);
        let (pq, sq) = if negated {
            ("is not null", "IS NOT NULL")
        } else {
            ("is null", "IS NULL")
        };
        b.push(Case {
            shape: "filter_is_null",
            powql: format!("{TABLE} filter .{} {pq} {{ .id }}", col.name),
            powdb_sql: format!("SELECT id FROM {TABLE} WHERE {} {sq}", col.name),
            sqlite_sql: format!("SELECT id FROM {TABLE} WHERE {} {sq}", col.name),
            sqlite_params: Vec::new(),
            ordered: false,
            kinds: vec![Kind::Col(ColType::Int)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

/// Build a `col OP lit` triple, or `None` when the literal has no PowDB
/// spelling.
fn cmp_term(rng: &mut Rng, col: &Column) -> Option<(String, String, String, Lit)> {
    let op = *rng.pick(&CMP_OPS);
    let lit = predicate_literal(rng, col);
    let pq = lit.powql()?;
    let ps = lit.powdb_sql()?;
    Some((
        format!(".{} {op} {pq}", col.name),
        format!("{} {op} {ps}", col.name),
        format!("{} {op}", col.name),
        lit,
    ))
}

fn filter_binary(
    b: &mut Builder,
    rng: &mut Rng,
    n: usize,
    shape: &'static str,
    pq_op: &str,
    sql_op: &str,
) {
    let cols = orderable();
    for _ in 0..n {
        let left = *rng.pick(&cols);
        let right = *rng.pick(&cols);
        let (Some((lp, ls, lsq, ll)), Some((rp, rs, rsq, rl))) =
            (cmp_term(rng, left), cmp_term(rng, right))
        else {
            continue;
        };
        b.push(Case {
            shape,
            powql: format!("{TABLE} filter {lp} {pq_op} {rp} {{ .id }}"),
            powdb_sql: format!("SELECT id FROM {TABLE} WHERE {ls} {sql_op} {rs}"),
            sqlite_sql: format!("SELECT id FROM {TABLE} WHERE {lsq} ?1 {sql_op} {rsq} ?2"),
            sqlite_params: vec![ll, rl],
            ordered: false,
            kinds: vec![Kind::Col(ColType::Int)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

fn filter_and(b: &mut Builder, rng: &mut Rng, n: usize) {
    filter_binary(b, rng, n, "filter_and", "and", "AND");
}

fn filter_or(b: &mut Builder, rng: &mut Rng, n: usize) {
    filter_binary(b, rng, n, "filter_or", "or", "OR");
}

/// `NOT (comparison)`. PowDB filter logic is two-valued by design, so a
/// comparison against a missing value is false and `NOT` makes it true;
/// standard SQL's three-valued logic keeps it unknown and drops the row. This
/// shape exists to hold that documented divergence in one place instead of
/// letting it leak into every boolean shape.
fn filter_not(b: &mut Builder, rng: &mut Rng, n: usize) {
    let cols = orderable();
    for _ in 0..n {
        let col = *rng.pick(&cols);
        let Some((pq, ps, sq, lit)) = cmp_term(rng, col) else {
            continue;
        };
        b.push(Case {
            shape: "filter_not",
            powql: format!("{TABLE} filter not ({pq}) {{ .id }}"),
            powdb_sql: format!("SELECT id FROM {TABLE} WHERE NOT ({ps})"),
            sqlite_sql: format!("SELECT id FROM {TABLE} WHERE NOT ({sq} ?1)"),
            sqlite_params: vec![lit],
            ordered: false,
            kinds: vec![Kind::Col(ColType::Int)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

/// The [`filter_not`] twin, restricted to the rows two-valued and three-valued
/// logic must agree about, so the shape can carry no ledger entry at all.
///
/// `filter_not` has one accepted difference, and it is entirely about missing
/// values: PowDB keeps a row whose column is NULL because the inner comparison
/// is false there and `NOT` makes it true, where SQL leaves it unknown and
/// drops it. Because an entry is keyed on the shape, that one reason blankets
/// every case in `filter_not`, including the rows where nothing is missing and
/// the two engines have no excuse to differ. A `NOT` that stopped negating
/// altogether would be absorbed there.
///
/// Adding `.c is not null` removes exactly the rows the two logics disagree
/// about (`not` binds tighter than `and`, so this is
/// `(.c is not null) and (not (.c OP lit))`). On what is left the comparison is
/// definite in both engines and `NOT` is the plain complement of it, so every
/// difference is a difference in the negation itself. Running it over every
/// orderable column polices negation for each column type rather than only for
/// the required key.
fn filter_not_non_null(b: &mut Builder, rng: &mut Rng, n: usize) {
    let cols = orderable();
    for _ in 0..n {
        let col = *rng.pick(&cols);
        let Some((pq, ps, sq, lit)) = cmp_term(rng, col) else {
            continue;
        };
        b.push(Case {
            shape: "filter_not_non_null",
            powql: format!(
                "{TABLE} filter .{0} is not null and not ({pq}) {{ .id }}",
                col.name
            ),
            powdb_sql: format!(
                "SELECT id FROM {TABLE} WHERE {0} IS NOT NULL AND NOT ({ps})",
                col.name
            ),
            sqlite_sql: format!(
                "SELECT id FROM {TABLE} WHERE {0} IS NOT NULL AND NOT ({sq} ?1)",
                col.name
            ),
            sqlite_params: vec![lit],
            ordered: false,
            kinds: vec![Kind::Col(ColType::Int)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

/// `ORDER BY col ASC` written the naive way in all three dialects.
///
/// PowDB places nulls last in both directions; SQLite places them first when
/// ascending. This shape is where that shows up. It is paired with
/// `order_asc_nulls_last`, which asks the same question with SQLite told
/// PowDB's rule, so the ordering of the non-null values is still policed by a
/// shape that carries no known divergence at all.
fn order_asc_naive(b: &mut Builder, _rng: &mut Rng, _n: usize) {
    for col in orderable() {
        b.push(Case {
            shape: "order_asc_naive",
            powql: format!(
                "{TABLE} {} {{ .id, .{} }}",
                powql_order(col.name, false),
                col.name
            ),
            powdb_sql: format!(
                "SELECT id, {} FROM {TABLE} ORDER BY {}",
                col.name,
                sql_order(col.name, false)
            ),
            sqlite_sql: format!(
                "SELECT id, {} FROM {TABLE} ORDER BY {}",
                col.name,
                sql_order(col.name, false)
            ),
            sqlite_params: Vec::new(),
            ordered: true,
            kinds: vec![Kind::Col(ColType::Int), Kind::Col(col.ty)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

fn order_asc_nulls_last(b: &mut Builder, _rng: &mut Rng, _n: usize) {
    for col in orderable() {
        b.push(Case {
            shape: "order_asc_nulls_last",
            powql: format!(
                "{TABLE} {} {{ .id, .{} }}",
                powql_order(col.name, false),
                col.name
            ),
            powdb_sql: format!(
                "SELECT id, {} FROM {TABLE} ORDER BY {}",
                col.name,
                sql_order(col.name, false)
            ),
            sqlite_sql: format!(
                "SELECT id, {} FROM {TABLE} ORDER BY {}",
                col.name,
                sqlite_nulls_last(col.name, false)
            ),
            sqlite_params: Vec::new(),
            ordered: true,
            kinds: vec![Kind::Col(ColType::Int), Kind::Col(col.ty)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

/// `ORDER BY col DESC`. Both engines put nulls last descending, so this shape
/// is compared naively and carries no known divergence.
fn order_desc(b: &mut Builder, _rng: &mut Rng, _n: usize) {
    for col in orderable() {
        b.push(Case {
            shape: "order_desc",
            powql: format!(
                "{TABLE} {} {{ .id, .{} }}",
                powql_order(col.name, true),
                col.name
            ),
            powdb_sql: format!(
                "SELECT id, {} FROM {TABLE} ORDER BY {}",
                col.name,
                sql_order(col.name, true)
            ),
            sqlite_sql: format!(
                "SELECT id, {} FROM {TABLE} ORDER BY {}",
                col.name,
                sql_order(col.name, true)
            ),
            sqlite_params: Vec::new(),
            ordered: true,
            kinds: vec![Kind::Col(ColType::Int), Kind::Col(col.ty)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

fn order_limit_offset(b: &mut Builder, rng: &mut Rng, n: usize) {
    let cols = orderable();
    for _ in 0..n {
        let col = *rng.pick(&cols);
        let desc = rng.chance(1, 2);
        let limit = 1 + rng.below(12);
        let offset = rng.below(5);
        b.push(Case {
            shape: "order_limit_offset",
            powql: format!(
                "{TABLE} {} limit {limit} offset {offset} {{ .id, .{} }}",
                powql_order(col.name, desc),
                col.name
            ),
            powdb_sql: format!(
                "SELECT id, {} FROM {TABLE} ORDER BY {} LIMIT {limit} OFFSET {offset}",
                col.name,
                sql_order(col.name, desc)
            ),
            // Told PowDB's NULLS LAST rule, so a LIMIT window that straddles
            // the null block is comparable in both directions.
            sqlite_sql: format!(
                "SELECT id, {} FROM {TABLE} ORDER BY {} LIMIT {limit} OFFSET {offset}",
                col.name,
                sqlite_nulls_last(col.name, desc)
            ),
            sqlite_params: Vec::new(),
            ordered: true,
            kinds: vec![Kind::Col(ColType::Int), Kind::Col(col.ty)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

/// An optional `WHERE`/`filter` clause shared by the aggregate shapes.
fn maybe_filter(rng: &mut Rng) -> (String, String, String, Vec<Lit>) {
    if rng.chance(1, 2) {
        return (String::new(), String::new(), String::new(), Vec::new());
    }
    let cols = orderable();
    let col = *rng.pick(&cols);
    match cmp_term(rng, col) {
        Some((pq, ps, sq, lit)) => (
            format!(" filter {pq}"),
            format!(" WHERE {ps}"),
            format!(" WHERE {sq} ?1"),
            vec![lit],
        ),
        None => (String::new(), String::new(), String::new(), Vec::new()),
    }
}

fn agg_count_star(b: &mut Builder, rng: &mut Rng, n: usize) {
    for _ in 0..n {
        let (fp, fs, fq, params) = maybe_filter(rng);
        b.push(Case {
            shape: "agg_count_star",
            powql: format!("count({TABLE}{fp})"),
            powdb_sql: format!("SELECT count(*) FROM {TABLE}{fs}"),
            sqlite_sql: format!("SELECT count(*) FROM {TABLE}{fq}"),
            sqlite_params: params,
            ordered: false,
            kinds: vec![Kind::Expr],
            scalar: true,
            sqlite_comparable: true,
        });
    }
}

fn agg_count_col(b: &mut Builder, rng: &mut Rng, n: usize) {
    for _ in 0..n {
        let col = *rng.pick(&COLUMNS);
        let (fp, fs, fq, params) = maybe_filter(rng);
        b.push(Case {
            shape: "agg_count_col",
            powql: format!("count({TABLE}{fp} {{ .{} }})", col.name),
            powdb_sql: format!("SELECT count({}) FROM {TABLE}{fs}", col.name),
            sqlite_sql: format!("SELECT count({}) FROM {TABLE}{fq}", col.name),
            sqlite_params: params,
            ordered: false,
            kinds: vec![Kind::Expr],
            scalar: true,
            sqlite_comparable: true,
        });
    }
}

/// `sum` / `avg` over an **integer** column. Integer accumulation is exact in
/// both engines, so every difference here is semantic and none of it is
/// rounding. Overflow is expected to be an error on both sides, and two errors
/// agree.
fn agg_sum_avg_int(b: &mut Builder, rng: &mut Rng, n: usize) {
    let cols: Vec<&Column> = COLUMNS
        .iter()
        .filter(|c| matches!(c.ty, ColType::Int))
        .collect();
    for _ in 0..n {
        let col = *rng.pick(&cols);
        let f = if rng.chance(1, 2) { "sum" } else { "avg" };
        let (fp, fs, fq, params) = maybe_filter(rng);
        b.push(Case {
            shape: "agg_sum_avg_int",
            powql: format!("{f}({TABLE}{fp} {{ .{} }})", col.name),
            powdb_sql: format!("SELECT {f}({}) FROM {TABLE}{fs}", col.name),
            sqlite_sql: format!("SELECT {f}({}) FROM {TABLE}{fq}", col.name),
            sqlite_params: params,
            ordered: false,
            kinds: vec![Kind::Expr],
            scalar: true,
            sqlite_comparable: true,
        });
    }
}

/// The `agg_sum_avg_int` twin, with PowDB's empty-sum rule spelled in SQLite's
/// own language so the shape can carry no ledger entry at all.
///
/// `agg_sum_avg_int` has one accepted difference: PowDB's `sum` over an input
/// with no numeric rows answers `Int(0)` where SQL answers NULL. An entry is
/// keyed on (shape, pair, fixture), so that one reason blankets every case in
/// the shape, and an integer total that is merely *wrong* is absorbed by it
/// without changing anything a gate can see.
///
/// This shape closes that. SQLite is told PowDB's rule with
/// `coalesce(sum(..), 0)`, exactly as [`order_asc_nulls_last`] tells SQLite
/// PowDB's NULLS LAST rule with a leading `col IS NULL` key. The empty-input
/// difference then cannot arise, the shape needs no entry, and every remaining
/// difference is arithmetic: a total that is off by any amount, on any input,
/// is unexplained and fails.
///
/// `avg` is included untouched and needs no such treatment, because PowDB
/// already answers `Empty` (SQL's NULL) over no numeric rows. It polices the
/// divisor, which `sum` alone would not.
///
/// The column pool includes the required `id`, whose values are dense, unique
/// and never null, so at least one case per fixture aggregates an input that is
/// non-empty and non-NULL on both sides without relying on the RNG.
fn agg_sum_avg_int_zero_default(b: &mut Builder, rng: &mut Rng, n: usize) {
    let cols: Vec<&Column> = COLUMNS
        .iter()
        .filter(|c| matches!(c.ty, ColType::Int))
        .collect();
    for _ in 0..n {
        let col = *rng.pick(&cols);
        let f = if rng.chance(1, 2) { "sum" } else { "avg" };
        let (fp, fs, fq, params) = maybe_filter(rng);
        // Only `sum` needs the zero default. Wrapping `avg` too would invent a
        // difference rather than remove one: both engines already answer NULL.
        let reference = if f == "sum" {
            format!("coalesce(sum({}), 0)", col.name)
        } else {
            format!("avg({})", col.name)
        };
        b.push(Case {
            shape: "agg_sum_avg_int_zero_default",
            powql: format!("{f}({TABLE}{fp} {{ .{} }})", col.name),
            powdb_sql: format!("SELECT {f}({}) FROM {TABLE}{fs}", col.name),
            sqlite_sql: format!("SELECT {reference} FROM {TABLE}{fq}"),
            sqlite_params: params,
            ordered: false,
            kinds: vec![Kind::Expr],
            scalar: true,
            sqlite_comparable: true,
        });
    }
}

/// `sum` / `avg` over a **float** column, compared between PowDB's two
/// frontends only.
///
/// SQLite is not an authority here and cannot be made one: since 3.43 its
/// `sum()`/`avg()` use Kahan-Babuska compensated summation, while a plain
/// left-to-right fold is a different (equally valid) answer under
/// floating-point non-associativity. On the boundary fixture the two disagree
/// by more than rounding: with `f64::MAX` and `f64::MIN` both present the
/// intermediate can overflow to an infinity in one order and stay finite in
/// the other. Writing that down as an accepted divergence would produce an
/// entry so broad it would excuse a genuinely wrong sum, so the SQLite leg is
/// declared out of scope instead and the PowQL-vs-PowDB-SQL leg still runs.
fn agg_sum_avg_float(b: &mut Builder, rng: &mut Rng, n: usize) {
    let cols: Vec<&Column> = COLUMNS
        .iter()
        .filter(|c| matches!(c.ty, ColType::Float))
        .collect();
    for _ in 0..n {
        let col = *rng.pick(&cols);
        let f = if rng.chance(1, 2) { "sum" } else { "avg" };
        let (fp, fs, fq, params) = maybe_filter(rng);
        b.push(Case {
            shape: "agg_sum_avg_float",
            powql: format!("{f}({TABLE}{fp} {{ .{} }})", col.name),
            powdb_sql: format!("SELECT {f}({}) FROM {TABLE}{fs}", col.name),
            sqlite_sql: format!("SELECT {f}({}) FROM {TABLE}{fq}", col.name),
            sqlite_params: params,
            ordered: false,
            kinds: vec![Kind::Expr],
            scalar: true,
            sqlite_comparable: false,
        });
    }
}

fn agg_min_max(b: &mut Builder, rng: &mut Rng, n: usize) {
    let cols = orderable();
    for _ in 0..n {
        let col = *rng.pick(&cols);
        let f = if rng.chance(1, 2) { "min" } else { "max" };
        let (fp, fs, fq, params) = maybe_filter(rng);
        b.push(Case {
            shape: "agg_min_max",
            powql: format!("{f}({TABLE}{fp} {{ .{} }})", col.name),
            powdb_sql: format!("SELECT {f}({}) FROM {TABLE}{fs}", col.name),
            sqlite_sql: format!("SELECT {f}({}) FROM {TABLE}{fq}", col.name),
            sqlite_params: params,
            // min/max return a value *of the column*, so the declared PowDB
            // type is what SQLite's carrier should be read back as.
            ordered: false,
            kinds: vec![Kind::Col(col.ty)],
            scalar: true,
            sqlite_comparable: true,
        });
    }
}

fn group_count(b: &mut Builder, _rng: &mut Rng, _n: usize) {
    for col in nullable() {
        b.push(Case {
            shape: "group_count",
            powql: format!("{TABLE} group .{0} {{ k: .{0}, n: count(*) }}", col.name),
            powdb_sql: format!(
                "SELECT {0} AS k, count(*) AS n FROM {TABLE} GROUP BY {0}",
                col.name
            ),
            sqlite_sql: format!(
                "SELECT {0} AS k, count(*) AS n FROM {TABLE} GROUP BY {0}",
                col.name
            ),
            sqlite_params: Vec::new(),
            ordered: false,
            kinds: vec![Kind::Col(col.ty), Kind::Expr],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

fn group_having(b: &mut Builder, _rng: &mut Rng, _n: usize) {
    for col in nullable() {
        b.push(Case {
            shape: "group_having",
            powql: format!(
                "{TABLE} group .{0} having count(*) > 1 {{ k: .{0}, n: count(*) }}",
                col.name
            ),
            powdb_sql: format!(
                "SELECT {0} AS k, count(*) AS n FROM {TABLE} GROUP BY {0} HAVING count(*) > 1",
                col.name
            ),
            sqlite_sql: format!(
                "SELECT {0} AS k, count(*) AS n FROM {TABLE} GROUP BY {0} HAVING count(*) > 1",
                col.name
            ),
            sqlite_params: Vec::new(),
            ordered: false,
            kinds: vec![Kind::Col(col.ty), Kind::Expr],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

fn distinct_col(b: &mut Builder, _rng: &mut Rng, _n: usize) {
    for col in COLUMNS.iter() {
        b.push(Case {
            shape: "distinct_col",
            powql: format!("{TABLE} distinct {{ .{} }}", col.name),
            powdb_sql: format!("SELECT DISTINCT {} FROM {TABLE}", col.name),
            sqlite_sql: format!("SELECT DISTINCT {} FROM {TABLE}", col.name),
            sqlite_params: Vec::new(),
            ordered: false,
            kinds: vec![Kind::Col(col.ty)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

/// LIKE patterns containing an ASCII letter. SQLite's default LIKE folds
/// ASCII case; PowDB's does not. Every case in this shape is affected, so the
/// shape carries exactly one accepted reason.
const LIKE_CASED_PATTERNS: [&str; 6] = ["a", "A", "a%", "A%", "%a%", "abc"];

/// LIKE patterns with no ASCII letter, so ASCII case folding cannot be the
/// explanation for any difference.
///
/// This is the shape that keeps the case-folding entry from becoming a blanket
/// excuse for LIKE: wildcard arity, UTF-8 character (not byte) counting for
/// `_`, and literal matching of multi-byte text are all still policed here by
/// a shape with no ledger entry at all.
const LIKE_UNCASED_PATTERNS: [&str; 8] = [
    "_",
    "__",
    "%",
    "%_",
    "_%",
    "\u{65e5}%",
    "%\u{1f600}%",
    "\u{e9}",
];

fn like_shape(
    b: &mut Builder,
    rng: &mut Rng,
    n: usize,
    shape: &'static str,
    pool: &[&str],
    column: &str,
) {
    for _ in 0..n {
        let pat = *rng.pick(pool);
        let lit = Lit::Str(pat.to_string());
        let (Some(pq), Some(ps)) = (lit.powql(), lit.powdb_sql()) else {
            continue;
        };
        b.push(Case {
            shape,
            powql: format!("{TABLE} filter .{column} like {pq} {{ .id }}"),
            powdb_sql: format!("SELECT id FROM {TABLE} WHERE {column} LIKE {ps}"),
            sqlite_sql: format!("SELECT id FROM {TABLE} WHERE {column} LIKE ?1"),
            sqlite_params: vec![lit],
            ordered: false,
            kinds: vec![Kind::Col(ColType::Int)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

fn like_ascii_case(b: &mut Builder, rng: &mut Rng, n: usize) {
    like_shape(
        b,
        rng,
        n,
        "like_ascii_case",
        &LIKE_CASED_PATTERNS,
        LIKE_COLUMN,
    );
}

fn like_wildcard_only(b: &mut Builder, rng: &mut Rng, n: usize) {
    like_shape(
        b,
        rng,
        n,
        "like_wildcard_only",
        &LIKE_UNCASED_PATTERNS,
        LIKE_COLUMN,
    );
}

/// LIKE against text that itself contains `%` or `_`.
///
/// Isolated in its own shape and its own column because PowDB's matcher gets
/// this wrong today: `like_match` tests the literal/underscore branch before
/// the `%` branch, so a pattern `%` that lands on a literal `%` in the text is
/// consumed as an ordinary character and no backtrack point is recorded. The
/// visible consequence is that `col LIKE '%'` does not match every non-null
/// row. Giving it a shape of its own keeps its ledger entry from excusing any
/// other LIKE difference; `like_ascii_case` and `like_wildcard_only` run over
/// metacharacter-free text and stay honest.
fn like_metachar_in_text(b: &mut Builder, rng: &mut Rng, n: usize) {
    const PATTERNS: [&str; 7] = ["%", "%_", "a%", "%a%", "%%", "_", "%a"];
    like_shape(
        b,
        rng,
        n,
        "like_metachar_in_text",
        &PATTERNS,
        LIKE_META_COLUMN,
    );
}

/// Keys the JSON pool guarantees are scalar-or-absent, so `->` and
/// `json_extract` are comparable (a sub-document would come back as PJ1
/// canonical text on one side and SQLite's own rendering on the other).
const JSON_SCALAR_KEYS: [&str; 3] = ["a", "b", "z"];

fn json_path_project(b: &mut Builder, _rng: &mut Rng, _n: usize) {
    for key in JSON_SCALAR_KEYS {
        b.push(Case {
            shape: "json_path_project",
            powql: format!("{TABLE} {{ .id, v: .j->{key} }}"),
            powdb_sql: format!("SELECT id, j->'{key}' AS v FROM {TABLE}"),
            sqlite_sql: format!("SELECT id, json_extract(j, '$.{key}') AS v FROM {TABLE}"),
            sqlite_params: Vec::new(),
            ordered: false,
            kinds: vec![Kind::Col(ColType::Int), Kind::Expr],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

fn json_path_filter(b: &mut Builder, rng: &mut Rng, n: usize) {
    // No boolean probe: see `fixture::json_pool` for why booleans are kept out
    // of the extracted keys entirely.
    let probes: [Lit; 6] = [
        Lit::Int(1),
        Lit::Int(0),
        Lit::Int(2),
        Lit::Int(-9_007_199_254_740_993),
        Lit::Str("x".into()),
        Lit::Str("".into()),
    ];
    for _ in 0..n {
        let key = *rng.pick(&JSON_SCALAR_KEYS);
        let lit = rng.pick(&probes).clone();
        let op = *rng.pick(&CMP_OPS);
        let (Some(pq), Some(ps)) = (lit.powql(), lit.powdb_sql()) else {
            continue;
        };
        b.push(Case {
            shape: "json_path_filter",
            powql: format!("{TABLE} filter .j->{key} {op} {pq} {{ .id }}"),
            powdb_sql: format!("SELECT id FROM {TABLE} WHERE j->'{key}' {op} {ps}"),
            sqlite_sql: format!("SELECT id FROM {TABLE} WHERE json_extract(j, '$.{key}') {op} ?1"),
            sqlite_params: vec![lit],
            ordered: false,
            kinds: vec![Kind::Col(ColType::Int)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

/// Equality against a whole json column. PowDB compares canonical PJ1 bytes;
/// SQLite compares the stored text. Both sides are given the same canonical
/// text, so they are asking the same question.
fn json_whole_column_eq(b: &mut Builder, rng: &mut Rng, n: usize) {
    for _ in 0..n {
        let col = COLUMNS
            .iter()
            .find(|c| c.ty == ColType::Json)
            .expect("json column");
        let lit = predicate_literal(rng, col);
        let (Some(pq), Some(ps)) = (lit.powql(), lit.powdb_sql()) else {
            continue;
        };
        b.push(Case {
            shape: "json_whole_column_eq",
            powql: format!("{TABLE} filter .j = {pq} {{ .id }}"),
            powdb_sql: format!("SELECT id FROM {TABLE} WHERE j = {ps}"),
            sqlite_sql: format!("SELECT id FROM {TABLE} WHERE j = ?1"),
            sqlite_params: vec![lit],
            ordered: false,
            kinds: vec![Kind::Col(ColType::Int)],
            scalar: false,
            sqlite_comparable: true,
        });
    }
}

/// A SQL string literal containing a backslash, spelled the way a user porting
/// SQL writes it.
///
/// PowDB's SQL lexer treats `\` as an escape introducer (`\X` collapses to
/// `X`); the SQL standard and SQLite do not, so `'back\slash'` is a different
/// string in the two engines. The fixture contains a row whose `s` really is
/// `back\slash`, so exactly one engine finds it.
fn sql_backslash_literal(b: &mut Builder, _rng: &mut Rng, _n: usize) {
    b.push(Case {
        shape: "sql_backslash_literal",
        // PowQL is given the *same naive spelling*, so the case measures the
        // escape rule and not the oracle's own quoting helper.
        powql: format!("{TABLE} filter .s = \"back\\slash\" {{ .id }}"),
        powdb_sql: format!("SELECT id FROM {TABLE} WHERE s = 'back\\slash'"),
        sqlite_sql: format!("SELECT id FROM {TABLE} WHERE s = 'back\\slash'"),
        sqlite_params: Vec::new(),
        ordered: false,
        kinds: vec![Kind::Col(ColType::Int)],
        scalar: false,
        sqlite_comparable: true,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic() {
        let a = generate(1234, 6, 500);
        let b = generate(1234, 6, 500);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.powql, y.powql);
            assert_eq!(x.sqlite_sql, y.sqlite_sql);
            assert_eq!(x.sqlite_params, y.sqlite_params);
        }
    }

    #[test]
    fn every_shape_is_represented() {
        let cases = generate(7, 6, 100_000);
        let mut seen: Vec<&str> = cases.iter().map(|c| c.shape).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            SHAPES.len(),
            "some shape generated no cases: {seen:?}"
        );
    }

    #[test]
    fn a_small_budget_still_touches_every_shape() {
        let cases = generate(7, 6, SHAPES.len());
        let mut seen: Vec<&str> = cases.iter().map(|c| c.shape).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), SHAPES.len());
    }

    #[test]
    fn parameter_count_matches_placeholders() {
        for case in generate(99, 6, 100_000) {
            let highest = (1..=9)
                .filter(|i| case.sqlite_sql.contains(&format!("?{i}")))
                .count();
            assert_eq!(
                highest,
                case.sqlite_params.len(),
                "placeholder/param mismatch in {}",
                case.sqlite_sql
            );
        }
    }

    #[test]
    fn kinds_are_declared_for_every_output_column() {
        for case in generate(5, 6, 100_000) {
            assert!(!case.kinds.is_empty(), "no kinds for {}", case.sqlite_sql);
        }
    }

    /// The json column must never reach an ORDER BY or a range comparison
    /// against SQLite: PJ1's total order and SQLite's text order are different
    /// orders, and comparing them would produce noise, not findings.
    #[test]
    fn json_never_reaches_an_ordering_shape() {
        for case in generate(11, 8, 100_000) {
            if matches!(
                case.shape,
                "order_asc_naive" | "order_asc_nulls_last" | "order_desc" | "order_limit_offset"
            ) {
                assert!(
                    !case.sqlite_sql.contains(" j "),
                    "json column reached ordering shape: {}",
                    case.sqlite_sql
                );
            }
        }
    }

    #[test]
    fn nulls_last_spelling_states_powdbs_rule() {
        assert_eq!(
            sqlite_nulls_last("s", false),
            "s IS NULL ASC, s ASC, id ASC"
        );
        assert_eq!(
            sqlite_nulls_last("s", true),
            "s IS NULL ASC, s DESC, id ASC"
        );
    }
}
