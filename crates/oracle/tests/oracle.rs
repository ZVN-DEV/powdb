//! The CI gate.
//!
//! Fixed seed, fixed budget, so this test either passes on every machine or
//! fails on every machine. It fails on an unexplained divergence AND on a
//! ledger entry that has stopped matching anything, so `known_divergences.toml`
//! cannot decay into a suppression list.

use powdb_oracle::divergence::{check_owners, parse, repo_root};
use powdb_oracle::engines::powdb::Powdb;
use powdb_oracle::fixture;
use powdb_oracle::model::{OVal, Outcome};
use powdb_oracle::run::{run, Config, LEDGER};
use powdb_oracle::shapes::{generate, Case};

#[test]
fn the_ledger_parses_and_every_owner_resolves() {
    let entries = parse(LEDGER).expect("known_divergences.toml must parse");
    let problems = check_owners(&entries, &repo_root());
    assert!(
        problems.is_empty(),
        "ledger owner problems:\n{}",
        problems.join("\n")
    );
}

#[test]
fn powdb_agrees_with_sqlite_except_where_the_ledger_says_otherwise() {
    let report = run(&Config::default()).expect("oracle run");
    assert!(
        report.is_clean(),
        "differential oracle found a problem:\n{}",
        report.summary()
    );
}

/// The run must actually do work. A budget that silently collapsed to zero
/// cases would make the gate above pass for the wrong reason, which is the
/// exact failure mode this crate exists to prevent.
#[test]
fn the_gate_actually_compares_something() {
    let report = run(&Config::default()).expect("oracle run");
    assert!(report.cases >= 100, "only {} cases generated", report.cases);
    assert!(
        report.comparisons >= 1000,
        "only {} comparisons performed",
        report.comparisons
    );
    // Two-way (PowDB-only) comparisons exist, and are a minority: if they ever
    // became the bulk of the run, SQLite would have stopped being the oracle.
    assert!(report.two_way_comparisons > 0);
    assert!(report.two_way_comparisons * 4 < report.comparisons);
}

/// The policing twins must stay entry-free.
///
/// A ledger entry is keyed on (shape, pair, fixture), so it blankets its whole
/// shape: while `agg_sum_avg_int` carries the empty-sum entry, an integer total
/// that is simply wrong is absorbed by it, and while `filter_not` carries the
/// two-valued-logic entry, a `NOT` that never negates is absorbed by it. Both
/// were confirmed by mutation: the injected engine bug only moved a hit count
/// and the gate stayed green.
///
/// Each of those shapes therefore has a twin that asks the same question of the
/// same machinery with the accepted difference designed out (SQLite told
/// PowDB's zero-default rule; the rows the two logics disagree about excluded).
/// The twins are only load-bearing while they carry no entry at all, so the
/// moment someone writes one they would silently reopen the dead zone. That is
/// a ledger edit, not a code change, and nothing else would catch it.
///
/// `filter_is_null` joins them for the same reason without being a purpose-built
/// twin: it already asks, entry-free, exactly the question that
/// `cmp_against_null_literal`'s entry blankets. That entry excuses one spelling
/// (`col = NULL` desugaring to a null test), and it stays confined to the
/// spelling only while the test it desugars *to* is policed somewhere that
/// carries no entry.
#[test]
fn the_policing_twins_carry_no_ledger_entry() {
    const TWINS: [&str; 3] = [
        "agg_sum_avg_int_zero_default",
        "filter_not_non_null",
        "filter_is_null",
    ];
    let entries = parse(LEDGER).expect("known_divergences.toml must parse");
    for twin in TWINS {
        assert!(
            !entries.iter().any(|e| e.shape == twin),
            "shape `{twin}` exists to police a shape whose ledger entry blankets it; \
             an entry on the twin reopens the dead zone. Split the shape instead."
        );
    }
    // And the twins must really be in the run, or the assertion above is
    // vacuous: an entry cannot exist for a shape nobody generates.
    let report = run(&Config::default()).expect("oracle run");
    let shapes: Vec<&str> = report.shapes_run();
    for twin in TWINS {
        assert!(shapes.contains(&twin), "twin shape `{twin}` never ran");
    }
}

/// Every shape must discriminate something.
///
/// Two engines that both refuse a query are treated as agreeing, and two empty
/// result sets are trivially equal, so a shape whose PowQL always errors (a
/// typo in the generated query) or whose predicate the fixture data can never
/// satisfy would agree with SQLite for free while proving nothing. That is the
/// same failure mode as a property suite that never exercises the path it was
/// written for, so it fails the gate rather than passing quietly.
#[test]
fn every_shape_actually_discriminates_something() {
    let report = run(&Config::default()).expect("oracle run");
    assert!(
        report.silent_shapes.is_empty(),
        "these shapes never returned a result set at all, so they agree with SQLite for free: {:?}",
        report.silent_shapes
    );
    assert!(
        report.always_empty_shapes.is_empty(),
        "no engine ever returned a row for these shapes, so they compare empty against empty: {:?}",
        report.always_empty_shapes
    );
}

/// The top-N shapes must reach the executor's bounded top-N fast path.
///
/// Every other gate in this file asks whether the answers agree. This one asks
/// whether the question ever arrives at the code it was written for, which is
/// a different failure and an invisible one: a shape can generate cases, run
/// on every fixture and compare full result sets while structurally unable to
/// enter the branch it exists to police.
///
/// That is not hypothetical. `order_limit_offset` spelled `offset 0` for one
/// case in five, the planner turned it into an `Offset` node between the
/// `Limit` and the `Sort`, and `project_filter_sort_limit_fast` matches
/// `Limit(Sort(..))` — so the descending top-N heap, and the tie-eviction rule
/// inside it, were never once executed by this crate. The shape looked healthy
/// by every other measure it had.
///
/// The `order_limit_offset` leg below is the control. Without it this test
/// would still pass against a generator that had quietly stopped producing an
/// `Offset` node anywhere, and "no plan contains an Offset" would be proving
/// nothing.
#[test]
fn the_top_n_shapes_reach_the_fast_path_plan_shape() {
    let config = Config::default();
    let cases = generate(config.seed, config.per_shape, usize::MAX);
    let pick = |shape: &str, wanted_offset: bool| -> Case {
        cases
            .iter()
            .find(|c| c.shape == shape && c.powql.contains("offset") == wanted_offset)
            .unwrap_or_else(|| panic!("no `{shape}` case with offset={wanted_offset}"))
            .clone()
    };

    let fixtures = fixture::all();
    let boundary = fixtures
        .iter()
        .find(|f| f.name == "boundary")
        .expect("boundary fixture");
    let mut engine = Powdb::open(boundary).expect("powdb");

    let plan = |engine: &mut Powdb, case: &Case| -> Vec<String> {
        match engine.powql(&format!("explain {}", case.powql)) {
            Outcome::Rows(rs) => rs
                .rows
                .iter()
                .map(|r| match r.first() {
                    Some(OVal::Str(text)) => text.clone(),
                    other => panic!("explain returned a non-text plan line: {other:?}"),
                })
                .collect(),
            Outcome::Error(e) => panic!("explain failed for `{}`: {e}", case.powql),
        }
    };

    for shape in ["order_desc_limit", "order_asc_limit"] {
        let case = pick(shape, false);
        let lines = plan(&mut engine, &case);
        let rendered = lines.join("\n");
        assert!(
            !rendered.contains("Offset"),
            "`{shape}` plans through an Offset node, which the top-N fast path \
             declines:\n{rendered}"
        );
        let sort = lines
            .iter()
            .find(|l| l.contains("Sort keys="))
            .unwrap_or_else(|| panic!("`{shape}` produced no Sort node:\n{rendered}"));
        assert!(
            !sort.contains(','),
            "`{shape}` sorts on more than one key, which the top-N fast path \
             declines: {sort}"
        );
        let limit = lines
            .iter()
            .position(|l| l.contains("Limit count="))
            .unwrap_or_else(|| panic!("`{shape}` produced no Limit node:\n{rendered}"));
        assert!(
            lines
                .get(limit + 1)
                .is_some_and(|l| l.contains("Sort keys=")),
            "the fast path matches Limit(Sort(..)) and nothing may sit between \
             them:\n{rendered}"
        );
    }

    // Control: a spelled-out offset really does produce the node this test
    // rejects above, so the assertions are discriminating rather than vacuous.
    let with_offset = pick("order_limit_offset", true);
    let rendered = plan(&mut engine, &with_offset).join("\n");
    assert!(
        rendered.contains("Offset"),
        "a case spelling `offset` must plan through an Offset node, otherwise \
         the checks above prove nothing:\n{rendered}"
    );
}
