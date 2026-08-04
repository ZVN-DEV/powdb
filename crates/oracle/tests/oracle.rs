//! The CI gate.
//!
//! Fixed seed, fixed budget, so this test either passes on every machine or
//! fails on every machine. It fails on an unexplained divergence AND on a
//! ledger entry that has stopped matching anything, so `known_divergences.toml`
//! cannot decay into a suppression list.

use powdb_oracle::divergence::{check_owners, parse, repo_root};
use powdb_oracle::run::{run, Config, LEDGER};

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
#[test]
fn the_policing_twins_carry_no_ledger_entry() {
    const TWINS: [&str; 2] = ["agg_sum_avg_int_zero_default", "filter_not_non_null"];
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
