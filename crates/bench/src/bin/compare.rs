//! Bench regression gate comparator.
//!
//! Reads the most recent criterion run from `target/criterion/<workload>/new/estimates.json`
//! for every workload in `WORKLOADS`, then compares against two checked-in
//! baseline files:
//!
//! 1. `crates/bench/baseline/main.json`         — per-workload absolute (±7% default, ±10% for noisy)
//! 2. `crates/bench/baseline/thesis-ratios.json` — ratio ceilings (e.g. 2.5x)
//!
//! The workload list covers:
//!   - Storage-layer guards (insert_10k, btree_lookup, seq_scan_filter).
//!   - Legacy PowQL guards (powql_point, powql_filter_only,
//!     powql_filter_projection, powql_aggregation) — workloads 1 and 3 of
//!     PLAN-MISSION-A.md §1 reuse `powql_point` and `powql_aggregation`
//!     respectively for gate continuity.
//!   - Mission A expansion workloads 2, 4-15 from PLAN-MISSION-A.md §1
//!     (point_lookup_nonindexed, scan_filter_project_top100,
//!     scan_filter_sort_limit10, agg_sum/avg/min/max, multi_col_and_filter,
//!     insert_single, insert_batch_1k, update_by_pk, update_by_filter,
//!     delete_by_filter).
//!
//! Exits 0 on pass, 1 on regression. Tolerates `null` baseline values for
//! the absolute gate so the very first run on a fresh runner can capture
//! initial numbers without failing — the comparator prints what it observed
//! so the human can paste it into `main.json` for the real baseline.
//!
//! ## Environment fingerprint
//!
//! The baseline numbers are only meaningful on the machine and flags that
//! produced them. `main.json` records three fields, and this comparator
//! HARD-FAILS when the current environment disagrees, rather than silently
//! comparing arm64 laptop numbers against Depot x86 numbers and reporting a
//! 2x "improvement".
//!
//! Two of the three are SELF-ATTESTED; one is MEASURED:
//!
//! - `runner` and `rustflags` are read from the environment
//!   (`POWDB_BENCH_RUNNER`, `RUSTFLAGS`). Anyone can export them, so on their
//!   own they prove nothing: exporting the Depot pair on an arm64 laptop used
//!   to produce an unlabelled clean PASS indistinguishable from a real Depot
//!   run.
//! - `arch` is compared against [`std::env::consts::ARCH`], which is baked
//!   into this binary at compile time and cannot be altered by the environment
//!   it runs in. It is the part of the fingerprint that is actual evidence.
//!
//! `arch` therefore fails CLOSED: a baseline that does not record it counts as
//! a mismatch rather than as an absent expectation, because "this document is
//! too old to say" is precisely the case where the numbers could be from
//! anywhere. Set `POWDB_BENCH_ALLOW_ENV_MISMATCH=1` to proceed anyway; the run
//! is then labelled NOT AUTHORITATIVE in the output.
//!
//! ## Control mode
//!
//! `--control <criterion-dir>` compares this run against a criterion run of a
//! DIFFERENT commit measured on the SAME machine in the SAME job. That removes
//! runner-to-runner variance, which is the whole reason the absolute
//! thresholds had to be widened to +/-20%, so the control gate is tighter.
//! The absolute gate against `main.json` still runs alongside it.
//!
//! Usage:
//!
//! ```bash
//! cargo bench -p powdb-bench
//! cargo run -p powdb-bench --bin compare
//! # with a same-instance control run of another commit:
//! cargo run -p powdb-bench --bin compare -- --control /path/to/control/target/criterion
//! # print the arch this binary was compiled for (what the `arch` check uses):
//! cargo run -p powdb-bench --bin compare -- --print-arch
//! ```

use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Default ±7% per-workload tolerance. Matches the storage-layer and legacy
/// PowQL bench noise floor observed on ubuntu-24.04 and M1.
const DEFAULT_ABSOLUTE_THRESHOLD: f64 = 0.07;

/// Relaxed ±10% tolerance for the Mission A workloads that run a tight
/// per-iteration setup or hit paths with more jitter (insert/update/delete
/// loops, sort+limit over a filtered scan). These workloads have been
/// measured to fluctuate 3-8% across identical local runs on M1 — the extra
/// margin keeps the gate honest without flapping.
const NOISY_ABSOLUTE_THRESHOLD: f64 = 0.10;

/// ±20% tolerance for the sub-millisecond aggregation + point-probe workloads
/// that are dominated by Azure-pool ubuntu-24.04 runner-to-runner variance
/// rather than any structural property of the code. Evidence: two identical
/// `--bench` runs on the same merge commit (07fcaa6, runs 24213258406 and
/// 24213724490, fifteen minutes apart) produced agg_sum 385ms vs 504ms
/// (+31% spread), agg_min 362ms vs 467ms (+29%), powql_aggregation 545ms vs
/// 624ms (+14%) — with NO code change between runs. These workloads all
/// scan ~100K rows of ~5μs work each, so ~100μs of runner scheduling noise
/// is a 20-30% delta. Widening to 20% + pinning the baseline at the high
/// end of observed noise keeps the gate honest against real regressions
/// (which would have to exceed even the slowest runner by another 20%)
/// without flapping on every fresh Azure VM assignment.
const VERY_NOISY_ABSOLUTE_THRESHOLD: f64 = 0.20;

/// Ceiling applied in `--control` mode. Every threshold above exists to absorb
/// variance BETWEEN machines: a fresh VM, a different CPU generation, a noisy
/// neighbour. A control run is the other commit measured on the same instance,
/// in the same job, minutes apart, so none of that applies and a 20% allowance
/// would let a real 15% regression through. Control deltas are gated at
/// `min(threshold_for(workload), 0.10)`.
///
/// 10% rather than something tighter because it is the highest same-machine
/// spread this repo has actually measured (the NOISY tier's evidence, taken
/// from repeated identical local runs). Tightening it further needs evidence
/// from a real Depot double-run, not optimism.
const CONTROL_ABSOLUTE_THRESHOLD: f64 = 0.10;

/// Env var that downgrades the environment fingerprint check to a warning.
/// Named loudly on purpose: a run that sets it is not evidence about the
/// baseline.
const ALLOW_ENV_MISMATCH: &str = "POWDB_BENCH_ALLOW_ENV_MISMATCH";

/// Env var carrying the runner label to compare against `main.json`'s
/// `runner` field. GitHub Actions does not expose the `runs-on` label to the
/// job, so `bench.yml` sets this explicitly.
const RUNNER_ENV: &str = "POWDB_BENCH_RUNNER";

/// Baseline document schema this comparator understands.
///
/// Schema 3 adds the measured `arch` fingerprint field. The bump is what makes
/// a schema-2 document (which cannot carry an `arch`) report the precise
/// reason it is rejected instead of an opaque "arch: baseline=(not recorded)".
/// Until this constant existed, `schema` was decoration: nothing read it.
const EXPECTED_BASELINE_SCHEMA: u64 = 3;

/// Placeholder shown for a fingerprint field the baseline never recorded.
/// Only reachable for fail-closed fields; the self-attested ones treat an
/// unrecorded field as "no expectation".
const UNRECORDED: &str = "(not recorded)";

/// The architecture this binary was COMPILED for.
///
/// The whole point of the `arch` fingerprint: unlike `POWDB_BENCH_RUNNER` and
/// `RUSTFLAGS`, this value is fixed at compile time and no environment
/// variable can change what it reports at run time.
fn compiled_arch() -> &'static str {
    std::env::consts::ARCH
}

/// Reject a baseline document whose schema this comparator does not implement.
///
/// Fail-closed on purpose: an unrecognised schema means the fields below might
/// mean something else, and a bench gate that guesses is not a gate.
fn check_baseline_schema(baseline: &Json) -> Result<(), String> {
    match baseline.get("schema").and_then(Json::as_u64) {
        Some(EXPECTED_BASELINE_SCHEMA) => Ok(()),
        Some(other) => Err(format!(
            "baseline schema is {other}, this comparator requires {EXPECTED_BASELINE_SCHEMA} \
             (schema {EXPECTED_BASELINE_SCHEMA} added the measured `arch` fingerprint)"
        )),
        None => Err(format!(
            "baseline records no `schema` field; this comparator requires schema \
             {EXPECTED_BASELINE_SCHEMA}"
        )),
    }
}

const WORKLOADS: &[&str] = &[
    // ── Storage layer (ratio denominator + existing guards) ──
    "insert_10k",
    "btree_lookup",
    "seq_scan_filter",
    // ── Legacy PowQL guards + Mission A workloads 1 & 3 ──
    "powql_point",             // MA#1 point_lookup_indexed
    "powql_filter_only",       // legacy 5a
    "powql_filter_projection", // legacy 5b
    "powql_aggregation",       // MA#3 scan_filter_count
    // ── Mission A reads (workloads 2, 4-10) ──
    "point_lookup_nonindexed",    // MA#2
    "scan_filter_project_top100", // MA#4
    "scan_filter_sort_limit10",   // MA#5
    "agg_sum",                    // MA#6
    "agg_avg",                    // MA#7
    "agg_min",                    // MA#8
    "agg_max",                    // MA#9
    "multi_col_and_filter",       // MA#10
    "conjunction_index_residual", // Lane A conjunction index selection
    // ── Mission A writes (workloads 11-15) ──
    "insert_single",    // MA#11
    "insert_batch_1k",  // MA#12
    "update_by_pk",     // MA#13
    "update_by_filter", // MA#14
    "delete_by_filter", // MA#15
];

/// Return the absolute-threshold that applies to a workload. Most workloads
/// use the ±7% default; a handful of write-heavy or sort-heavy workloads
/// get ±10% because their per-iter work is chunkier and the variance wider.
fn threshold_for(workload: &str) -> f64 {
    match workload {
        // Sub-millisecond aggregation + point-probe workloads where
        // Azure-pool GHA runner variance dominates over any structural
        // perf delta. See VERY_NOISY_ABSOLUTE_THRESHOLD comment above for
        // the evidence chain (back-to-back same-commit runs with +14 to
        // +31% spread across these workloads).
        "agg_sum"
        | "agg_avg"
        | "agg_min"
        | "agg_max"
        | "powql_aggregation"
        | "point_lookup_nonindexed"
        // powql_point is a microsecond-scale parse+plan+exec workload where
        // shared GHA runner CPU jitter produces >10% spread on identical
        // code. PR #21 back-to-back runs: +12.39% vs 0.65% with no code
        // change. Promoted from DEFAULT (7%) to VERY_NOISY (20%). The
        // thesis ratio powql_point_over_btree_lookup (ceiling 7.0x) still
        // guards against structural overhead growth.
        | "powql_point" => VERY_NOISY_ABSOLUTE_THRESHOLD,

        // GHA-variance-dominated workloads: back-to-back same-commit PR #9
        // runs showed scan_filter_sort_limit10 +11.9%, update_by_pk +86%,
        // delete_by_filter +17.7% — all with zero code change. Promoted
        // from NOISY (10%) to VERY_NOISY (20%). update_by_filter promoted
        // in PR #14 after +13.9% variance on identical code.
        "scan_filter_sort_limit10" | "update_by_pk" | "delete_by_filter" | "update_by_filter" => {
            VERY_NOISY_ABSOLUTE_THRESHOLD
        }

        // Bulk writes and multi-column scans: fixture growth, WAL sync,
        // btree splits — naturally more variance than point reads, but not
        // as extreme as the above. multi_col_and_filter promoted in PR #15
        // after +10.14% variance on identical code (four same-code runs
        // showed 2.89%–10.14% spread). seq_scan_filter promoted in PR #21
        // after +7.58% on identical code (other runs: +2.40%).
        // Full-table filter scans: runtime dominated by disk I/O and cache
        // effects on shared GHA runners. PR #56 back-to-back runs showed
        // powql_filter_only +10.76%, powql_filter_projection +7.18% on
        // identical code. Promoted from DEFAULT (7%) to NOISY (10%).
        "insert_single" | "insert_batch_1k" | "multi_col_and_filter" | "seq_scan_filter"
        | "powql_filter_only" | "powql_filter_projection" => {
            NOISY_ABSOLUTE_THRESHOLD
        }
        _ => DEFAULT_ABSOLUTE_THRESHOLD,
    }
}

/// Control-mode threshold: the absolute tier, capped by
/// [`CONTROL_ABSOLUTE_THRESHOLD`].
fn control_threshold_for(workload: &str) -> f64 {
    threshold_for(workload).min(CONTROL_ABSOLUTE_THRESHOLD)
}

/// One environment field's expected (baseline) and observed value.
#[derive(Debug, PartialEq)]
struct EnvMismatch {
    field: &'static str,
    expected: String,
    observed: String,
}

/// Compare the baseline's recorded environment against the current one.
///
/// For the SELF-ATTESTED fields (`runner`, `rustflags`) a missing baseline
/// field is not a mismatch (older baseline documents did not record
/// `rustflags`), but a field that IS recorded must match exactly.
///
/// For the MEASURED field (`arch`) a missing baseline field IS a mismatch.
/// These two rules differ because the risks differ: an unrecorded `rustflags`
/// costs some precision, whereas an unrecorded `arch` would let the one check
/// that cannot be spoofed be skipped by simply not writing it down. That is
/// the same "an absent observed value is a mismatch, not a pass" rule the
/// tests below pin, applied to the baseline side.
///
/// Normalising whitespace only, because `-C target-cpu=x86-64-v2` and
/// `  -C target-cpu=x86-64-v2 ` are the same flags, whereas
/// `-C target-cpu=native` is a different machine's benchmark.
fn env_mismatches(
    baseline: &Json,
    observed_runner: Option<&str>,
    observed_rustflags: Option<&str>,
    observed_arch: &str,
) -> Vec<EnvMismatch> {
    let mut out = vec![];
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");

    // (field, observed value, does an unrecorded baseline field fail closed?)
    let checks: [(&'static str, Option<&str>, bool); 3] = [
        ("runner", observed_runner, false),
        ("rustflags", observed_rustflags, false),
        ("arch", Some(observed_arch), true),
    ];
    for (field, observed, required) in checks {
        let expected = baseline.get(field).and_then(Json::as_str);
        let observed = observed.unwrap_or("");
        match expected {
            Some(expected) if norm(expected) != norm(observed) => out.push(EnvMismatch {
                field,
                expected: expected.to_string(),
                observed: observed.to_string(),
            }),
            Some(_) => {}
            None if required => out.push(EnvMismatch {
                field,
                expected: UNRECORDED.to_string(),
                observed: observed.to_string(),
            }),
            None => {}
        }
    }
    out
}

/// Parsed command line.
#[derive(Debug, Default, PartialEq)]
struct Args {
    /// `--control <dir>`: criterion output of another commit, same machine.
    control: Option<PathBuf>,
    /// `--print-arch`: print the compiled-in arch and exit. Exists so callers
    /// that need to agree with the `arch` fingerprint (the gate self-test) can
    /// ask THIS binary instead of asking `rustc` and hoping the two agree.
    print_arch: bool,
}

/// Parse the argument list.
fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Args, String> {
    let mut it = args.into_iter();
    let mut parsed = Args::default();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--control" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--control requires a criterion directory".to_string())?;
                parsed.control = Some(PathBuf::from(value));
            }
            other if other.starts_with("--control=") => {
                parsed.control = Some(PathBuf::from(&other["--control=".len()..]));
            }
            "--print-arch" => parsed.print_arch = true,
            other => return Err(format!("unrecognised argument: {other}")),
        }
    }
    Ok(parsed)
}

#[derive(Debug)]
struct WorkloadResult {
    name: String,
    current_ns: Option<f64>,
    baseline_ns: Option<f64>,
}

#[derive(Debug)]
struct RatioCheck {
    name: String,
    numerator: String,
    denominator: String,
    ceiling: f64,
    observed: Option<f64>,
    /// False when either endpoint has a null baseline entry. Used to keep the
    /// CRITERION→FASTPATH race quiet during FIRST-RUN CAPTURE: if the
    /// baseline can't tell us what "good" looks like yet, we print the
    /// current ratio for humans but don't fail the gate.
    enforced: bool,
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("usage: compare [--control <criterion-dir>] [--print-arch]");
            return ExitCode::from(2);
        }
    };
    if args.print_arch {
        println!("{}", compiled_arch());
        return ExitCode::SUCCESS;
    }
    let control_dir = args.control;

    let manifest_dir = env_manifest_dir();
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    // The three inputs are overridable so the gate itself can be tested
    // against synthetic fixtures (scripts/ci/bench-gate-selftest.sh) without a
    // 60-second bench run, and so a two-checkout control job can point at
    // another workspace's criterion output. CI leaves them unset.
    let criterion_dir = env_path("POWDB_BENCH_CRITERION_DIR")
        .unwrap_or_else(|| workspace_root.join("target/criterion"));
    let baseline_path =
        env_path("POWDB_BENCH_BASELINE").unwrap_or_else(|| manifest_dir.join("baseline/main.json"));
    let ratios_path = env_path("POWDB_BENCH_RATIOS")
        .unwrap_or_else(|| manifest_dir.join("baseline/thesis-ratios.json"));

    println!("PowDB bench regression gate");
    println!("  criterion dir : {}", criterion_dir.display());
    println!("  baseline      : {}", baseline_path.display());
    println!("  ratios        : {}", ratios_path.display());
    match &control_dir {
        Some(c) => println!("  control dir   : {}", c.display()),
        None => println!("  control dir   : (none, absolute gate only)"),
    }
    println!();

    // ── Load current run estimates ─────────────────────────────────────────
    let mut current: BTreeMap<&'static str, f64> = BTreeMap::new();
    let mut missing: Vec<&'static str> = vec![];
    for &workload in WORKLOADS {
        match read_estimate_median(&criterion_dir, workload) {
            Ok(ns) => {
                current.insert(workload, ns);
            }
            Err(e) => {
                eprintln!("warning: no criterion estimate for {workload}: {e}");
                missing.push(workload);
            }
        }
    }

    if !missing.is_empty() {
        eprintln!();
        eprintln!(
            "error: {} workload(s) missing from criterion output",
            missing.len()
        );
        eprintln!("       did `cargo bench -p powdb-bench` run all benches?");
        return ExitCode::from(1);
    }

    // ── Load baseline (allow nulls for first-run capture) ──────────────────
    // A baseline that exists must declare a schema this comparator implements.
    // A baseline that does not exist at all stays tolerated for first-run
    // capture on a fresh runner; that path then fails closed anyway, because
    // an absent document records no `arch`.
    let baseline_json = match read_json(&baseline_path) {
        Ok(json) => {
            if let Err(e) = check_baseline_schema(&json) {
                eprintln!("error: {}: {e}", baseline_path.display());
                return ExitCode::from(1);
            }
            json
        }
        Err(e) => {
            eprintln!("warning: could not read baseline ({e}); treating as first-run capture");
            Json::Null
        }
    };

    // ── Environment fingerprint ────────────────────────────────────────────
    // Numbers are only comparable to the machine and flags that produced them.
    // Without this, running the comparator on an arm64 laptop against
    // Depot-x86 numbers reports a uniform 2x "improvement" and exits 0, which
    // is how a laptop rebaseline got as far as a PR once already.
    // `runner` and `rustflags` are self-attested (anyone can export them);
    // `arch` is read out of this binary and is the check that a spoofed
    // environment cannot get past.
    let observed_runner = std::env::var(RUNNER_ENV).ok();
    let observed_rustflags = std::env::var("RUSTFLAGS").ok();
    let mismatches = env_mismatches(
        &baseline_json,
        observed_runner.as_deref(),
        observed_rustflags.as_deref(),
        compiled_arch(),
    );
    let env_override = std::env::var(ALLOW_ENV_MISMATCH)
        .map(|v| v == "1")
        .unwrap_or(false);
    if !mismatches.is_empty() {
        eprintln!("ENVIRONMENT MISMATCH vs {}", baseline_path.display());
        for m in &mismatches {
            eprintln!(
                "  {:<10} baseline={:?}  this run={:?}",
                m.field, m.expected, m.observed
            );
        }
        if env_override {
            eprintln!();
            eprintln!(
                "  {ALLOW_ENV_MISMATCH}=1 is set: continuing, but this run is NOT AUTHORITATIVE."
            );
            eprintln!("  Do not rebaseline main.json from it.");
            eprintln!();
        } else {
            eprintln!();
            eprintln!("  The baseline numbers were measured elsewhere; comparing against them");
            eprintln!("  here measures the machine, not the code. Run the bench workflow on the");
            eprintln!("  recorded runner, or set {ALLOW_ENV_MISMATCH}=1 to proceed with an");
            eprintln!("  explicitly non-authoritative run.");
            eprintln!();
            eprintln!("  ({RUNNER_ENV} carries the runner label and RUSTFLAGS is read directly:");
            eprintln!("   both are self-attested. `arch` is compiled into this binary, so it is");
            eprintln!("   the one field the environment cannot talk its way past.)");
            return ExitCode::from(1);
        }
    }

    let baseline_workloads = baseline_json
        .get("workloads")
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();

    let mut results: Vec<WorkloadResult> = Vec::with_capacity(WORKLOADS.len());
    for &workload in WORKLOADS {
        let baseline_ns = baseline_workloads
            .get(workload)
            .and_then(|w| w.get("ns_per_iter"))
            .and_then(Json::as_f64);
        results.push(WorkloadResult {
            name: workload.to_string(),
            current_ns: current.get(workload).copied(),
            baseline_ns,
        });
    }

    // ── Print absolute gate table ──────────────────────────────────────────
    println!(
        "{:<28} {:>14} {:>14} {:>10} {:>6} {:>8}",
        "workload", "baseline", "current", "delta", "thr", "gate"
    );
    println!("{}", "─".repeat(86));

    let mut absolute_failed = false;
    let mut first_run_capture = false;
    for r in &results {
        let baseline_str = r
            .baseline_ns
            .map(|ns| format!("{:>10.0} ns", ns))
            .unwrap_or_else(|| "        null".to_string());
        let current_str = r
            .current_ns
            .map(|ns| format!("{:>10.0} ns", ns))
            .unwrap_or_else(|| "        n/a".to_string());

        let threshold = threshold_for(&r.name);
        let threshold_str = format!("{:>4.0}%", threshold * 100.0);

        let (delta_str, gate_str) = match (r.baseline_ns, r.current_ns) {
            (Some(b), Some(c)) => {
                let delta = (c - b) / b;
                let pct = format!("{:+>9.2}%", delta * 100.0);
                if delta > threshold {
                    absolute_failed = true;
                    (pct, "FAIL".to_string())
                } else {
                    (pct, "PASS".to_string())
                }
            }
            (None, Some(_)) => {
                first_run_capture = true;
                ("       —".to_string(), "CAPTURE".to_string())
            }
            _ => ("       —".to_string(), "—".to_string()),
        };

        println!(
            "{:<28} {:>14} {:>14} {:>10} {:>6} {:>8}",
            r.name, baseline_str, current_str, delta_str, threshold_str, gate_str
        );
    }
    println!();

    // ── Ratio gate ─────────────────────────────────────────────────────────
    let ratio_json = read_json(&ratios_path).unwrap_or_else(|e| {
        eprintln!("warning: could not read thesis-ratios.json ({e}); skipping ratio gate");
        Json::Null
    });

    // Build the baseline-ns map keyed by workload name for the ratio gate's
    // "only enforce when both endpoints have non-null baselines" rule. This
    // keeps the CRITERION→FASTPATH race (§4) quiet: pre-FASTPATH, any ratio
    // whose endpoints are still null in main.json will CAPTURE rather than
    // FAIL, even if the observed ratio exceeds the ceiling. Once FASTPATH
    // lands and the rebaseline commit populates the baseline numbers, the
    // ratio switches to enforcing mode automatically.
    let baseline_ns_map: BTreeMap<String, Option<f64>> = results
        .iter()
        .map(|r| (r.name.clone(), r.baseline_ns))
        .collect();

    let ratio_checks = parse_ratios(&ratio_json, &current, &baseline_ns_map);

    let mut ratio_failed = false;
    if !ratio_checks.is_empty() {
        println!(
            "{:<36} {:>10} {:>12} {:>10}",
            "ratio", "ceiling", "current", "gate"
        );
        println!("{}", "─".repeat(74));
        for check in &ratio_checks {
            let observed_str = check
                .observed
                .map(|v| format!("{:>10.3}x", v))
                .unwrap_or_else(|| "          —".to_string());
            let gate_str = match check.observed {
                Some(v) if v > check.ceiling => {
                    if check.enforced {
                        ratio_failed = true;
                        "FAIL"
                    } else {
                        // Endpoint baselines still null: CAPTURE mode.
                        first_run_capture = true;
                        "CAPTURE"
                    }
                }
                Some(_) => {
                    if check.enforced {
                        "PASS"
                    } else {
                        "CAPTURE"
                    }
                }
                None => "—",
            };
            println!(
                "{:<36} {:>9.3}x {:>12} {:>10}",
                check.name, check.ceiling, observed_str, gate_str
            );
            println!("  ({} / {})", check.numerator, check.denominator);
        }
        println!();
    }

    // ── Control gate (same instance, same job, other commit) ───────────────
    let mut control_failed = false;
    if let Some(control_dir) = &control_dir {
        println!("Same-instance control run: {}", control_dir.display());
        println!(
            "{:<28} {:>14} {:>14} {:>10} {:>6} {:>8}",
            "workload", "control", "head", "delta", "thr", "gate"
        );
        println!("{}", "─".repeat(86));

        let mut control_missing: Vec<&'static str> = vec![];
        for &workload in WORKLOADS {
            let head_ns = current.get(workload).copied();
            let control_ns = match read_estimate_median(control_dir, workload) {
                Ok(ns) => Some(ns),
                Err(_) => {
                    control_missing.push(workload);
                    None
                }
            };
            let threshold = control_threshold_for(workload);
            let (delta_str, gate_str) = match (control_ns, head_ns) {
                (Some(c), Some(h)) if c > 0.0 => {
                    let delta = (h - c) / c;
                    let gate = if delta > threshold {
                        control_failed = true;
                        "FAIL"
                    } else {
                        "PASS"
                    };
                    (format!("{:+>9.2}%", delta * 100.0), gate)
                }
                _ => ("       -".to_string(), "MISSING"),
            };
            println!(
                "{:<28} {:>14} {:>14} {:>10} {:>5.0}% {:>8}",
                workload,
                control_ns
                    .map(|ns| format!("{:>10.0} ns", ns))
                    .unwrap_or_else(|| "         n/a".to_string()),
                head_ns
                    .map(|ns| format!("{:>10.0} ns", ns))
                    .unwrap_or_else(|| "         n/a".to_string()),
                delta_str,
                threshold * 100.0,
                gate_str
            );
        }
        println!();

        // A control run that silently covered nothing is the miri failure
        // mode: an empty comparison exits 0 and reads as a pass.
        if !control_missing.is_empty() {
            eprintln!(
                "error: control run is missing {} workload(s): {}",
                control_missing.len(),
                control_missing.join(", ")
            );
            eprintln!("       the control commit must run the SAME bench suite as head;");
            eprintln!("       a partial control comparison is not a gate.");
            control_failed = true;
        }
    }

    // ── Verdict ────────────────────────────────────────────────────────────
    if control_failed {
        eprintln!("REGRESSION: head is slower than the same-instance control run.");
        eprintln!("  This comparison has no cross-machine noise in it: both numbers came");
        eprintln!("  from this job, on this instance, minutes apart. Treat it as real.");
    }
    if absolute_failed || ratio_failed || control_failed {
        eprintln!("REGRESSION: gate failed.");
        if absolute_failed {
            eprintln!(
                "  - one or more workloads exceeded their absolute threshold ({:.0}% default, {:.0}% for noisy write/sort workloads)",
                DEFAULT_ABSOLUTE_THRESHOLD * 100.0,
                NOISY_ABSOLUTE_THRESHOLD * 100.0,
            );
        }
        if ratio_failed {
            eprintln!("  - one or more thesis ratios exceeded their ceiling");
        }
        eprintln!();
        eprintln!("If this regression is intentional:");
        eprintln!("  - rerun ./scripts/update-bench-baseline.sh and commit the new main.json");
        eprintln!("  - or hand-edit thesis-ratios.json with a justification commit");
        return ExitCode::from(1);
    }

    if first_run_capture {
        println!("FIRST-RUN CAPTURE: baseline had null values for some workloads.");
        println!("  Paste the current values above into crates/bench/baseline/main.json");
        println!("  to set the real baseline, then commit.");
    } else if control_dir.is_some() {
        println!("OK: within absolute threshold, within ratio ceiling, and no regression");
        println!("    against the same-instance control run.");
    } else {
        println!("OK: all workloads within threshold, all ratios within ceiling.");
    }
    if !mismatches.is_empty() {
        println!();
        println!("NOT AUTHORITATIVE: the environment did not match the baseline's.");
    }
    ExitCode::SUCCESS
}

/// Read a path from the environment, treating an empty value as unset.
fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn env_manifest_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is set when run via cargo. When run as a standalone
    // binary, fall back to the current dir.
    std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn read_json(path: &Path) -> Result<Json, String> {
    let content = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&content).map_err(|e| format!("{}: {e}", path.display()))
}

fn read_estimate_median(criterion_dir: &Path, workload: &str) -> Result<f64, String> {
    let estimates_path = criterion_dir
        .join(workload)
        .join("new")
        .join("estimates.json");
    let json = read_json(&estimates_path)?;
    json.get("median")
        .and_then(|m| m.get("point_estimate"))
        .and_then(Json::as_f64)
        .ok_or_else(|| {
            format!(
                "missing median.point_estimate in {}",
                estimates_path.display()
            )
        })
}

fn parse_ratios(
    ratio_json: &Json,
    current: &BTreeMap<&'static str, f64>,
    baseline_ns_map: &BTreeMap<String, Option<f64>>,
) -> Vec<RatioCheck> {
    let Some(ratios) = ratio_json.get("ratios").and_then(Json::as_object) else {
        return vec![];
    };
    ratios
        .iter()
        .filter_map(|(name, def)| {
            let numerator = def.get("numerator")?.as_str()?.to_string();
            let denominator = def.get("denominator")?.as_str()?.to_string();
            let ceiling = def.get("ceiling")?.as_f64()?;

            let observed = match (
                current.get(numerator.as_str()),
                current.get(denominator.as_str()),
            ) {
                (Some(&n), Some(&d)) if d > 0.0 => Some(n / d),
                _ => None,
            };

            // Enforce only when BOTH endpoints have non-null baselines. This
            // implements the CRITERION→FASTPATH race resolution from
            // PLAN-MISSION-A.md §4: pre-FASTPATH, ratios with null endpoints
            // CAPTURE rather than FAIL, and the rebaseline commit flips them
            // to enforcing mode by populating the baseline values.
            let num_baseline = baseline_ns_map.get(&numerator).copied().unwrap_or(None);
            let den_baseline = baseline_ns_map.get(&denominator).copied().unwrap_or(None);
            let enforced = num_baseline.is_some() && den_baseline.is_some();

            Some(RatioCheck {
                name: name.clone(),
                numerator,
                denominator,
                ceiling,
                observed,
                enforced,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shell self-test (`scripts/ci/bench-gate-selftest.sh`) drives the
    /// built binary and proves every VERDICT is reachable. It cannot reach the
    /// branches below, because every fixture baseline it writes records both
    /// environment fields. These cover the rest of the decision table.
    /// The arch this test binary was compiled for, i.e. what a baseline has to
    /// record for the measured check to pass here.
    const HOST_ARCH: &str = std::env::consts::ARCH;

    #[test]
    fn missing_baseline_field_is_not_a_mismatch() {
        // Schema-1 baselines predate `rustflags`. For a SELF-ATTESTED field an
        // absent entry carries no expectation, so it must not fail a run; only
        // a RECORDED field does. (`arch` is recorded here because it is the
        // one field whose absence does fail; see the fail-closed test below.)
        let baseline = json!({ "runner": "depot-ubuntu-24.04-4", "arch": HOST_ARCH });
        let found = env_mismatches(
            &baseline,
            Some("depot-ubuntu-24.04-4"),
            Some("-C target-cpu=whatever"),
            HOST_ARCH,
        );
        assert!(
            found.is_empty(),
            "an unrecorded baseline field must not be enforced, got {found:?}"
        );
    }

    #[test]
    fn recorded_field_that_differs_is_a_mismatch() {
        let baseline = json!({ "runner": "depot-ubuntu-24.04-4", "arch": HOST_ARCH });
        let found = env_mismatches(&baseline, Some("ubuntu-24.04"), None, HOST_ARCH);
        assert_eq!(
            found,
            vec![EnvMismatch {
                field: "runner",
                expected: "depot-ubuntu-24.04-4".to_string(),
                observed: "ubuntu-24.04".to_string(),
            }]
        );
    }

    #[test]
    fn absent_observed_value_is_a_mismatch_not_a_pass() {
        // The dangerous default: a runner that simply does not set the env var
        // must not be read as "matches".
        let baseline = json!({ "rustflags": "-C target-cpu=x86-64-v2", "arch": HOST_ARCH });
        let found = env_mismatches(&baseline, None, None, HOST_ARCH);
        assert_eq!(found.len(), 1, "unset RUSTFLAGS must be a mismatch");
        assert_eq!(found[0].observed, "");
    }

    #[test]
    fn only_whitespace_is_normalised() {
        let baseline = json!({ "rustflags": "-C target-cpu=x86-64-v2", "arch": HOST_ARCH });
        // Same flags, different spacing: not a mismatch.
        assert!(env_mismatches(
            &baseline,
            None,
            Some("  -C   target-cpu=x86-64-v2 "),
            HOST_ARCH
        )
        .is_empty());
        // Different flags: a mismatch, even though it is a near-miss.
        assert_eq!(
            env_mismatches(&baseline, None, Some("-C target-cpu=native"), HOST_ARCH).len(),
            1
        );
    }

    #[test]
    fn matching_arch_is_not_a_mismatch() {
        // The whole fingerprint agreeing is the ordinary Depot case: an x86_64
        // baseline compared by an x86_64 binary exporting the recorded env.
        let baseline = json!({
            "runner": "depot-ubuntu-24.04-4",
            "rustflags": "-C target-cpu=x86-64-v2",
            "arch": HOST_ARCH,
        });
        let found = env_mismatches(
            &baseline,
            Some("depot-ubuntu-24.04-4"),
            Some("-C target-cpu=x86-64-v2"),
            HOST_ARCH,
        );
        assert!(
            found.is_empty(),
            "a matching fingerprint must pass: {found:?}"
        );
    }

    #[test]
    fn arch_mismatch_survives_a_fully_spoofed_environment() {
        // The defect this check exists for. Both self-attested fields are set
        // to exactly what the baseline records, which is all it used to take
        // for an arm64 laptop to produce an unlabelled clean PASS against
        // Depot x86 numbers. `arch` is not readable from the environment, so
        // it still reports the mismatch.
        let baseline = json!({
            "runner": "depot-ubuntu-24.04-4",
            "rustflags": "-C target-cpu=x86-64-v2",
            "arch": "x86_64",
        });
        let found = env_mismatches(
            &baseline,
            Some("depot-ubuntu-24.04-4"),
            Some("-C target-cpu=x86-64-v2"),
            "aarch64",
        );
        assert_eq!(
            found,
            vec![EnvMismatch {
                field: "arch",
                expected: "x86_64".to_string(),
                observed: "aarch64".to_string(),
            }]
        );
    }

    #[test]
    fn baseline_without_arch_fails_closed() {
        // An unrecorded `arch` must not read as "no expectation". Otherwise
        // the one unspoofable check could be disabled by deleting one line
        // from the JSON, which is not a security property at all.
        let baseline = json!({
            "runner": "depot-ubuntu-24.04-4",
            "rustflags": "-C target-cpu=x86-64-v2",
        });
        let found = env_mismatches(
            &baseline,
            Some("depot-ubuntu-24.04-4"),
            Some("-C target-cpu=x86-64-v2"),
            HOST_ARCH,
        );
        assert_eq!(
            found,
            vec![EnvMismatch {
                field: "arch",
                expected: UNRECORDED.to_string(),
                observed: HOST_ARCH.to_string(),
            }]
        );
    }

    #[test]
    fn observed_arch_comes_from_the_binary_not_the_environment() {
        // Guards the wiring, not the constant: main() must pass
        // compiled_arch(), and compiled_arch() must be the compile-time value.
        // If someone ever routes this through std::env::var, this fails.
        assert_eq!(compiled_arch(), std::env::consts::ARCH);
        assert!(!compiled_arch().is_empty());
    }

    #[test]
    fn baseline_schema_must_be_the_expected_one() {
        assert!(check_baseline_schema(&json!({ "schema": EXPECTED_BASELINE_SCHEMA })).is_ok());
        // The pre-`arch` document shape: rejected with its own reason rather
        // than surfacing as a confusing arch mismatch.
        let old = check_baseline_schema(&json!({ "schema": 2 })).unwrap_err();
        assert!(
            old.contains('2') && old.contains("arch"),
            "unhelpful: {old}"
        );
        // A future shape this binary does not implement is also refused.
        assert!(check_baseline_schema(&json!({ "schema": 4 })).is_err());
        // And the field is no longer decoration: absent is an error.
        assert!(check_baseline_schema(&json!({ "runner": "x" })).is_err());
    }

    #[test]
    fn control_threshold_caps_the_noisiest_tier() {
        // agg_sum sits in the VERY_NOISY (20%) absolute tier; on one machine
        // it must still be gated at the control ceiling.
        assert_eq!(threshold_for("agg_sum"), VERY_NOISY_ABSOLUTE_THRESHOLD);
        assert_eq!(control_threshold_for("agg_sum"), CONTROL_ABSOLUTE_THRESHOLD);
        // A workload already tighter than the ceiling keeps its own threshold.
        let tight = threshold_for("btree_lookup");
        assert!(tight < CONTROL_ABSOLUTE_THRESHOLD);
        assert_eq!(control_threshold_for("btree_lookup"), tight);
    }

    #[test]
    fn control_arg_parses_both_spellings() {
        let one = parse_args(["--control".to_string(), "/tmp/c".to_string()]).unwrap();
        assert_eq!(one.control, Some(PathBuf::from("/tmp/c")));
        let two = parse_args(["--control=/tmp/c".to_string()]).unwrap();
        assert_eq!(two.control, Some(PathBuf::from("/tmp/c")));
        assert_eq!(parse_args([]).unwrap(), Args::default());
    }

    #[test]
    fn control_arg_rejects_bad_input_instead_of_ignoring_it() {
        // A dropped value would silently turn a control run into an
        // absolute-only run, i.e. the gate the control mode exists to replace.
        assert!(parse_args(["--control".to_string()]).is_err());
        assert!(parse_args(["--contorl=/tmp/c".to_string()]).is_err());
    }

    #[test]
    fn print_arch_flag_parses() {
        let parsed = parse_args(["--print-arch".to_string()]).unwrap();
        assert!(parsed.print_arch);
        assert_eq!(parsed.control, None);
    }
}
