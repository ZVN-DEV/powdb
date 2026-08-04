//! The harness: load every fixture into every engine, run every case, compare
//! every pair, adjudicate against the ledger.

use std::collections::BTreeMap;

use crate::compare::diff;
use crate::divergence::{adjudicate, check_owners, parse, repo_root, Divergence, Pair, Verdict};
use crate::engines::powdb::Powdb;
use crate::engines::sqlite::Sqlite;
use crate::fixture;
use crate::model::Outcome;
use crate::normalize::scalarize;
use crate::shapes::{self, Case};

/// The ledger, compiled in so the test binary needs no runtime file lookup for
/// the common path. The on-disk copy is the source of truth and is what a
/// human edits; `include_str!` guarantees they cannot drift.
pub const LEDGER: &str = include_str!("../known_divergences.toml");

/// A fixed seed and a fixed budget: the run is identical on every machine and
/// in every CI job, so a divergence recorded today reproduces tomorrow.
pub struct Config {
    pub seed: u64,
    pub per_shape: usize,
    pub budget: usize,
}

impl Default for Config {
    fn default() -> Self {
        // "PWDB" plus a run tag. Changing this changes every generated case,
        // so it is pinned rather than derived from the clock or the machine.
        Config {
            seed: 0x5057_4442_5157_4C00,
            per_shape: 8,
            budget: 900,
        }
    }
}

pub struct Report {
    pub cases: usize,
    pub comparisons: usize,
    /// Cases that ran two-way (PowQL vs PowDB-SQL) because SQLite cannot
    /// represent the fixture. Reported so degraded coverage is never silent.
    pub two_way_comparisons: usize,
    pub divergences: Vec<Divergence>,
    pub verdict: Verdict,
    pub ledger_problems: Vec<String>,
    /// Shapes whose PowQL leg never once returned a result set: every case, on
    /// every fixture, was an error.
    ///
    /// Such a shape agrees with SQLite for free, because two engines that both
    /// refuse a query are treated as agreeing (see [`crate::compare::diff`]).
    /// It therefore looks exactly like a passing shape while proving nothing,
    /// which is the "test that cannot fail" failure mode this crate exists to
    /// stop. A typo in generated PowQL is the usual cause.
    pub silent_shapes: Vec<&'static str>,
    /// Shapes where *no* engine ever returned a row.
    ///
    /// An empty result is trivially equal to another empty result, so such a
    /// shape passes without discriminating anything. The usual cause is a
    /// predicate the fixture data can never satisfy.
    ///
    /// Emptiness on the PowDB side alone is deliberately NOT counted here: a
    /// shape where PowDB returns nothing and SQLite returns rows is the
    /// strongest kind of finding, not a coverage hole. Only a shape where
    /// every leg was empty every time has compared nothing.
    pub always_empty_shapes: Vec<&'static str>,
    /// Every shape that actually ran, in sorted order.
    shapes: Vec<&'static str>,
}

/// Per-shape evidence that a shape did real work, accumulated during the run.
#[derive(Default, Clone, Copy)]
struct ShapeCoverage {
    /// The PowQL leg returned a result set (rather than an error) at least once.
    answered: bool,
    /// Some leg (PowQL, PowDB-SQL, or SQLite) returned at least one row.
    non_empty: bool,
}

impl ShapeCoverage {
    /// Fold one engine's outcome in.
    fn observe(&mut self, outcome: &Outcome) {
        if let Outcome::Rows(rs) = outcome {
            self.non_empty |= !rs.rows.is_empty();
        }
    }
}

impl Report {
    /// A run is clean when nothing unexplained diverged, nothing in the ledger
    /// has rotted, and every shape actually discriminated something.
    pub fn is_clean(&self) -> bool {
        self.verdict.is_clean()
            && self.ledger_problems.is_empty()
            && self.silent_shapes.is_empty()
            && self.always_empty_shapes.is_empty()
    }

    /// The shapes this run executed. Lets a caller assert a shape is really in
    /// the run rather than assume the generator still produces it.
    pub fn shapes_run(&self) -> Vec<&'static str> {
        self.shapes.clone()
    }

    /// Divergence counts grouped by shape and pair, for triage.
    pub fn by_shape(&self) -> BTreeMap<(&'static str, Pair), usize> {
        let mut map = BTreeMap::new();
        for d in &self.divergences {
            *map.entry((d.shape, d.pair)).or_insert(0) += 1;
        }
        map
    }

    pub fn summary(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "cases={} comparisons={} (two-way: {}) divergences={}\n",
            self.cases,
            self.comparisons,
            self.two_way_comparisons,
            self.divergences.len()
        ));
        for ((shape, pair), n) in self.by_shape() {
            out.push_str(&format!("  {shape} / {pair}: {n}\n"));
        }
        for (id, n) in &self.verdict.matched {
            out.push_str(&format!("  known: {id} covered {n}\n"));
        }
        for id in &self.verdict.stale_entries {
            out.push_str(&format!("  STALE LEDGER ENTRY (matched nothing): {id}\n"));
        }
        for problem in &self.ledger_problems {
            out.push_str(&format!("  LEDGER PROBLEM: {problem}\n"));
        }
        for shape in &self.silent_shapes {
            out.push_str(&format!(
                "  SHAPE PROVED NOTHING (PowQL errored on every case): {shape}\n"
            ));
        }
        for shape in &self.always_empty_shapes {
            out.push_str(&format!(
                "  SHAPE PROVED NOTHING (every answer was empty): {shape}\n"
            ));
        }
        for d in self.verdict.unexplained.iter().take(40) {
            out.push_str(&format!("  UNEXPLAINED {d}\n"));
        }
        if self.verdict.unexplained.len() > 40 {
            out.push_str(&format!(
                "  ... and {} more unexplained\n",
                self.verdict.unexplained.len() - 40
            ));
        }
        out
    }

    /// Every divergence, explained or not. Used by the manual runner during
    /// triage, where the point is to read them all.
    pub fn full(&self) -> String {
        let mut out = self.summary();
        for d in &self.divergences {
            out.push_str(&format!("  DIVERGENCE {d}\n"));
        }
        out
    }
}

/// Run the whole oracle.
pub fn run(config: &Config) -> Result<Report, String> {
    let entries = parse(LEDGER).map_err(|e| format!("known_divergences.toml: {e}"))?;
    let mut ledger_problems = check_owners(&entries, &repo_root());

    let cases = shapes::generate(config.seed, config.per_shape, config.budget);
    let known_shapes: Vec<&str> = shapes::generate(config.seed, config.per_shape, usize::MAX)
        .iter()
        .map(|c| c.shape)
        .collect();
    for entry in &entries {
        if !known_shapes.contains(&entry.shape.as_str()) {
            ledger_problems.push(format!(
                "entry `{}` names shape `{}`, which no generator produces",
                entry.id, entry.shape
            ));
        }
    }

    let mut divergences = Vec::new();
    let mut comparisons = 0usize;
    let mut two_way_comparisons = 0usize;
    let mut coverage: BTreeMap<&'static str, ShapeCoverage> = BTreeMap::new();

    for fx in fixture::all() {
        let mut powdb = Powdb::open(&fx)?;
        let sqlite = if fx.sqlite_representable {
            Some(Sqlite::open(&fx)?)
        } else {
            None
        };

        for case in &cases {
            let powql = powdb.powql(&case.powql);
            let powdb_sql = powdb.sql(&case.powdb_sql);

            // Record what each leg actually managed to answer, so a shape that
            // only ever errors, or where every leg is always empty, is
            // reported instead of passing for free.
            {
                let seen = coverage.entry(case.shape).or_default();
                seen.answered |= matches!(powql, Outcome::Rows(_));
                seen.observe(&powql);
                seen.observe(&powdb_sql);
            }

            comparisons += 1;
            if let Some(detail) = diff(&powql, &powdb_sql, case.ordered) {
                divergences.push(Divergence {
                    fixture: fx.name,
                    shape: case.shape,
                    pair: Pair::PowqlVsPowdbSql,
                    query: describe(case),
                    detail,
                });
            }

            let sqlite = sqlite.as_ref().filter(|_| case.sqlite_comparable);
            let Some(sqlite) = sqlite else {
                // Either the fixture or the shape declared SQLite not to be a
                // valid authority here. Counted, not hidden.
                two_way_comparisons += 1;
                continue;
            };
            let mut reference = sqlite.query(&case.sqlite_sql, &case.sqlite_params, &case.kinds);
            if case.scalar {
                // Only a scalar case is relabelled, and only after the shape
                // declared it: see `normalize::scalarize`.
                if let Outcome::Rows(rs) = reference {
                    reference = Outcome::Rows(scalarize(rs));
                }
            }
            coverage.entry(case.shape).or_default().observe(&reference);
            comparisons += 1;
            if let Some(detail) = diff(&powql, &reference, case.ordered) {
                divergences.push(Divergence {
                    fixture: fx.name,
                    shape: case.shape,
                    pair: Pair::PowqlVsSqlite,
                    query: describe(case),
                    detail,
                });
            }
        }
    }

    let verdict = adjudicate(&divergences, &entries);
    let silent_shapes = coverage
        .iter()
        .filter(|(_, c)| !c.answered)
        .map(|(s, _)| *s)
        .collect::<Vec<_>>();
    let always_empty_shapes = coverage
        .iter()
        .filter(|(_, c)| c.answered && !c.non_empty)
        .map(|(s, _)| *s)
        .collect::<Vec<_>>();
    Ok(Report {
        cases: cases.len(),
        comparisons,
        two_way_comparisons,
        divergences,
        verdict,
        ledger_problems,
        silent_shapes,
        always_empty_shapes,
        shapes: coverage.keys().copied().collect(),
    })
}

fn describe(case: &Case) -> String {
    format!(
        "powql=`{}` powdb_sql=`{}` sqlite=`{}` params={:?}",
        case.powql, case.powdb_sql, case.sqlite_sql, case.sqlite_params
    )
}
