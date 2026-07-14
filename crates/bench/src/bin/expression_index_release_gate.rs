//! Fixed-seed v0.13 expression-index release gate.
//!
//! The benchmark compares the same nested JSON-path equality, range, and
//! bounded ordered queries before and after creating an expression index. It
//! checks the executed EXPLAIN shape, validates result parity, measures scaling,
//! reopens the largest fixture, and emits one machine-readable JSON report.
//!
//! Run the default release gate and refresh the checked-in evidence with:
//!
//! ```text
//! POWDB_EXPR_INDEX_OUTPUT=crates/bench/baseline/expression-index-v0.13.json \
//!   cargo run --release -p powdb-bench --bin expression-index-release-gate
//! ```
//!
//! Optional environment variables:
//! - `POWDB_EXPR_INDEX_SIZES=10000,40000,160000`
//! - `POWDB_EXPR_INDEX_WARMUPS=2`
//! - `POWDB_EXPR_INDEX_REPS=9`
//! - `POWDB_EXPR_INDEX_OUTPUT=/path/to/report.json`

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::pj1::parse_json_text;
use powdb_storage::types::Value;
use powdb_storage::wal::WalSyncMode;
use serde_json::{json, Value as JsonValue};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_SIZES: &[usize] = &[10_000, 40_000, 160_000];
const DEFAULT_WARMUPS: usize = 2;
const DEFAULT_REPETITIONS: usize = 9;
const SCORE_MODULUS: usize = 1_000_003;
const SCORE_MULTIPLIER: usize = 48_271;
const FIXTURE_SEED: usize = 73_951;
const EQUALITY_ROW: usize = 17;
const RANGE_LOW: usize = 250_000;
const RANGE_HIGH: usize = 260_000;
const ORDER_LIMIT: usize = 20;
const MIN_EQUALITY_SPEEDUP: f64 = 3.0;
const MIN_RANGE_SPEEDUP: f64 = 2.0;
const MIN_ORDER_SPEEDUP: f64 = 4.0;
const MAX_ORDER_GROWTH_FRACTION: f64 = 0.65;
const CANONICAL_ARTIFACT_PATH: &str = "crates/bench/baseline/expression-index-v0.13.json";

fn equality_score() -> usize {
    score_for(EQUALITY_ROW)
}

fn score_for(row: usize) -> usize {
    (row * SCORE_MULTIPLIER + FIXTURE_SEED) % SCORE_MODULUS
}

fn equality_query() -> String {
    format!(
        "Doc filter .data->metrics->score = {} {{ .id }}",
        equality_score()
    )
}

const RANGE_QUERY: &str =
    "Doc filter .data->metrics->score >= 250000 and .data->metrics->score < 260000 { .id }";
const ORDER_QUERY: &str =
    "Doc order .data->metrics->score desc limit 20 { .id, score: .data->metrics->score }";
const INDEX_DDL: &str = "alter Doc add index (.data->metrics->score)";

struct TempDir(PathBuf);

impl TempDir {
    fn new(cardinality: usize) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "powdb_expr_index_gate_{}_{}_{}",
            std::process::id(),
            cardinality,
            nonce
        ));
        std::fs::create_dir_all(&path).expect("create benchmark directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
struct Measurement {
    samples_ms: Vec<f64>,
    median_ms: f64,
    min_ms: f64,
    max_ms: f64,
    row_count: usize,
    digest: u64,
}

#[derive(Clone, Copy)]
struct QueryCase<'a> {
    name: &'a str,
    query: &'a str,
}

fn configured_sizes() -> Vec<usize> {
    let Some(raw) = std::env::var("POWDB_EXPR_INDEX_SIZES").ok() else {
        return DEFAULT_SIZES.to_vec();
    };
    let sizes: Vec<usize> = raw
        .split(',')
        .map(str::trim)
        .map(|part| {
            part.parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid cardinality in POWDB_EXPR_INDEX_SIZES"))
        })
        .collect();
    assert!(sizes.len() >= 2, "at least two cardinalities are required");
    assert!(
        sizes
            .iter()
            .all(|size| (1_000..SCORE_MODULUS).contains(size)),
        "cardinalities must be in 1,000..{SCORE_MODULUS}"
    );
    assert!(
        sizes.windows(2).all(|pair| pair[0] < pair[1]),
        "cardinalities must be strictly increasing"
    );
    sizes
}

fn env_usize(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(raw) => raw
            .parse::<usize>()
            .unwrap_or_else(|_| panic!("{name} must be a positive integer")),
        Err(_) => default,
    }
}

fn is_canonical_artifact_path(path: &Path) -> bool {
    path.ends_with(Path::new(CANONICAL_ARTIFACT_PATH))
}

fn make_document(row: usize) -> Box<[u8]> {
    let score = score_for(row);
    let state = match row % 4 {
        0 => "ready",
        1 => "pending",
        2 => "archived",
        _ => "draft",
    };
    let document = format!(
        r#"{{"metrics":{{"score":{score},"rank":{}}},"state":"{state}","payload":"fixed-seed-document-{:08x}"}}"#,
        row % 997,
        row ^ FIXTURE_SEED
    );
    parse_json_text(&document)
        .expect("fixed benchmark JSON must be valid")
        .into_boxed_slice()
}

fn expected_row_count(case_name: &str, cardinality: usize) -> usize {
    match case_name {
        "equality" => usize::from(EQUALITY_ROW < cardinality),
        "range" => (0..cardinality)
            .filter(|row| (RANGE_LOW..RANGE_HIGH).contains(&score_for(*row)))
            .count(),
        "order_limit" => cardinality.min(ORDER_LIMIT),
        _ => unreachable!("unknown benchmark case"),
    }
}

fn setup(cardinality: usize) -> (Engine, TempDir) {
    let temp = TempDir::new(cardinality);
    let mut engine = Engine::new(temp.path()).expect("create benchmark engine");
    engine.catalog_mut().set_wal_sync_mode(WalSyncMode::Off);
    engine
        .execute_powql("type Doc { required id: int, data: json }")
        .expect("create benchmark table");
    {
        let table = engine
            .catalog_mut()
            .get_table_mut("Doc")
            .expect("open benchmark table");
        for row in 0..cardinality {
            table
                .insert(&vec![
                    Value::Int(row as i64),
                    Value::Json(make_document(row)),
                ])
                .expect("insert fixed benchmark row");
        }
    }
    (engine, temp)
}

fn explain_text(engine: &mut Engine, query: &str) -> String {
    let QueryResult::Rows { rows, .. } = engine
        .execute_powql(&format!("explain {query}"))
        .unwrap_or_else(|error| panic!("EXPLAIN failed for `{query}`: {error}"))
    else {
        panic!("EXPLAIN did not return rows for `{query}`");
    };
    rows.into_iter()
        .filter_map(|row| match row.into_iter().next() {
            Some(Value::Str(line)) => Some(line),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn result_digest(result: QueryResult, preserve_order: bool) -> (usize, u64) {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("benchmark query must return rows, got {result:?}");
    };
    let mut encoded_rows: Vec<String> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| format!("{value:?}"))
                .collect::<Vec<_>>()
                .join("\u{1f}")
        })
        .collect();
    if !preserve_order {
        encoded_rows.sort_unstable();
    }
    let mut hash = FNV_OFFSET;
    for row in &encoded_rows {
        hash_bytes(&mut hash, row.as_bytes());
        hash_bytes(&mut hash, &[0x00]);
    }
    (rows.len(), hash)
}

fn run_once(engine: &mut Engine, query: &str, preserve_order: bool) -> (f64, usize, u64) {
    let started = Instant::now();
    let result = engine
        .execute_powql(query)
        .unwrap_or_else(|error| panic!("benchmark query failed `{query}`: {error}"));
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let (row_count, digest) = result_digest(result, preserve_order);
    std::hint::black_box((row_count, digest));
    (elapsed_ms, row_count, digest)
}

fn measure(
    engine: &mut Engine,
    query: &str,
    preserve_order: bool,
    warmups: usize,
    repetitions: usize,
    expected: Option<(usize, u64)>,
) -> Measurement {
    assert!(repetitions > 0, "at least one repetition is required");
    for _ in 0..warmups {
        let (_, row_count, digest) = run_once(engine, query, preserve_order);
        if let Some((expected_count, expected_digest)) = expected {
            assert_eq!(row_count, expected_count, "warmup row-count mismatch");
            assert_eq!(digest, expected_digest, "warmup result mismatch");
        }
    }

    let mut samples_ms = Vec::with_capacity(repetitions);
    let mut observed = None;
    for _ in 0..repetitions {
        let (elapsed_ms, row_count, digest) = run_once(engine, query, preserve_order);
        if let Some((expected_count, expected_digest)) = expected {
            assert_eq!(row_count, expected_count, "measured row-count mismatch");
            assert_eq!(digest, expected_digest, "measured result mismatch");
        }
        if let Some((prior_count, prior_digest)) = observed {
            assert_eq!(
                row_count, prior_count,
                "query row count changed between runs"
            );
            assert_eq!(digest, prior_digest, "query result changed between runs");
        }
        observed = Some((row_count, digest));
        samples_ms.push(elapsed_ms);
    }

    let mut sorted = samples_ms.clone();
    sorted.sort_by(f64::total_cmp);
    let (row_count, digest) = observed.expect("measured result");
    Measurement {
        median_ms: sorted[sorted.len() / 2],
        min_ms: sorted[0],
        max_ms: sorted[sorted.len() - 1],
        samples_ms,
        row_count,
        digest,
    }
}

fn measurement_json(measurement: &Measurement) -> JsonValue {
    json!({
        "samples_ms": measurement.samples_ms,
        "median_ms": measurement.median_ms,
        "min_ms": measurement.min_ms,
        "max_ms": measurement.max_ms,
        "row_count": measurement.row_count,
        "result_digest_fnv1a64": format!("{:016x}", measurement.digest),
    })
}

fn assert_sequential_plan(case: QueryCase<'_>, plan: &str) {
    assert!(
        plan.contains("SeqScan"),
        "{} sequential baseline must contain SeqScan:\n{plan}",
        case.name
    );
    match case.name {
        "equality" | "range" => assert!(
            plan.contains("Filter"),
            "{} sequential baseline must contain Filter:\n{plan}",
            case.name
        ),
        "order_limit" => assert!(
            plan.contains("Sort"),
            "ordered sequential baseline must contain Sort:\n{plan}"
        ),
        _ => unreachable!("unknown benchmark case"),
    }
}

fn assert_indexed_plan(case: QueryCase<'_>, plan: &str) {
    let required = match case.name {
        "equality" => "ExprIndexScan",
        "range" => "ExprRangeScan",
        "order_limit" => "OrderedExprIndexScan",
        _ => unreachable!("unknown benchmark case"),
    };
    assert!(
        plan.contains(required),
        "{} indexed plan must contain {required}:\n{plan}",
        case.name
    );
    assert!(
        !plan.contains("SeqScan"),
        "{} indexed plan must not contain SeqScan:\n{plan}",
        case.name
    );
    if case.name == "order_limit" {
        assert!(
            !plan.contains("Sort"),
            "ordered indexed plan must not contain a generic Sort:\n{plan}"
        );
    }
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

fn query_fingerprint(query: &str) -> String {
    let mut hash = FNV_OFFSET;
    hash_bytes(&mut hash, query.as_bytes());
    format!("fnv1a64:{hash:016x}")
}

fn repository_metadata(output_path: Option<&Path>) -> JsonValue {
    let commit =
        command_output("git", &["rev-parse", "HEAD"]).unwrap_or_else(|| "unavailable".to_string());
    let status = command_output("git", &["status", "--porcelain", "--untracked-files=all"])
        .unwrap_or_else(|| "unavailable".to_string());
    let diff = Command::new("git")
        .args(["diff", "--binary", "--no-ext-diff", "HEAD", "--", "."])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| output.stdout)
        .unwrap_or_default();
    let mut fingerprint = FNV_OFFSET;
    hash_bytes(&mut fingerprint, commit.as_bytes());
    hash_bytes(&mut fingerprint, status.as_bytes());
    hash_bytes(&mut fingerprint, &diff);
    let untracked = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| output.stdout)
        .unwrap_or_default();
    for raw_path in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let Ok(path_text) = std::str::from_utf8(raw_path) else {
            continue;
        };
        let path = Path::new(path_text);
        if output_path.is_some_and(|output| output == path) {
            continue;
        }
        hash_bytes(&mut fingerprint, raw_path);
        if let Ok(contents) = std::fs::read(path) {
            hash_bytes(&mut fingerprint, &contents);
        }
    }
    json!({
        "commit": commit,
        "dirty": !status.is_empty() && status != "unavailable",
        "working_tree_fingerprint": format!("fnv1a64:{fingerprint:016x}"),
        "fingerprint_inputs": "HEAD commit, porcelain status, tracked binary diff, and untracked file names/contents except the generated output artifact",
    })
}

fn cpu_description() -> String {
    if let Some(cpu) = command_output("sysctl", &["-n", "machdep.cpu.brand_string"]) {
        return cpu;
    }
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    cpuinfo
        .lines()
        .find_map(|line| line.strip_prefix("model name\t: "))
        .unwrap_or("unavailable")
        .to_string()
}

fn environment_metadata() -> JsonValue {
    json!({
        "os": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "cpu": cpu_description(),
        "logical_parallelism": std::thread::available_parallelism().map_or(0, usize::from),
        "rustc": command_output("rustc", &["--version", "--verbose"]).unwrap_or_else(|| "unavailable".to_string()),
        "build_profile": "release",
        "process_page_cache_cleared": false,
    })
}

fn main() {
    if cfg!(debug_assertions) {
        panic!("expression-index release gate must run with --release");
    }

    let sizes = configured_sizes();
    let output_path = std::env::var("POWDB_EXPR_INDEX_OUTPUT")
        .ok()
        .map(PathBuf::from);
    let warmups = env_usize("POWDB_EXPR_INDEX_WARMUPS", DEFAULT_WARMUPS);
    let repetitions = env_usize("POWDB_EXPR_INDEX_REPS", DEFAULT_REPETITIONS);
    assert!(warmups <= 20, "warmups must be at most 20");
    assert!(
        (3..=50).contains(&repetitions),
        "repetitions must be in 3..=50"
    );
    if output_path
        .as_deref()
        .is_some_and(is_canonical_artifact_path)
    {
        assert_eq!(
            sizes, DEFAULT_SIZES,
            "the canonical release artifact requires default cardinalities"
        );
        assert_eq!(
            warmups, DEFAULT_WARMUPS,
            "the canonical release artifact requires default warmups"
        );
        assert_eq!(
            repetitions, DEFAULT_REPETITIONS,
            "the canonical release artifact requires default repetitions"
        );
    }

    let equality = equality_query();
    let cases = [
        QueryCase {
            name: "equality",
            query: &equality,
        },
        QueryCase {
            name: "range",
            query: RANGE_QUERY,
        },
        QueryCase {
            name: "order_limit",
            query: ORDER_QUERY,
        },
    ];

    let mut cardinality_reports = Vec::with_capacity(sizes.len());
    let mut medians: BTreeMap<&str, Vec<(f64, f64)>> = BTreeMap::new();
    let mut reopen_report = JsonValue::Null;

    for cardinality in sizes.iter().copied() {
        eprintln!("building fixed-seed {cardinality}-row document fixture");
        let (mut engine, temp) = setup(cardinality);
        let mut sequential_plans = BTreeMap::new();
        let mut sequential = BTreeMap::new();
        for case in cases {
            let plan = explain_text(&mut engine, case.query);
            assert_sequential_plan(case, &plan);
            let measurement = measure(
                &mut engine,
                case.query,
                case.name == "order_limit",
                warmups,
                repetitions,
                None,
            );
            assert_eq!(
                measurement.row_count,
                expected_row_count(case.name, cardinality),
                "{} fixed-seed expected row count mismatch",
                case.name
            );
            sequential_plans.insert(case.name, plan);
            sequential.insert(case.name, measurement);
        }

        eprintln!("creating expression index for {cardinality} rows");
        engine
            .execute_powql(INDEX_DDL)
            .expect("create expression index");
        let mut indexed_plans = BTreeMap::new();
        let mut indexed = BTreeMap::new();
        for case in cases {
            let plan = explain_text(&mut engine, case.query);
            assert_indexed_plan(case, &plan);
            let baseline = sequential.get(case.name).expect("sequential measurement");
            let measurement = measure(
                &mut engine,
                case.query,
                case.name == "order_limit",
                warmups,
                repetitions,
                Some((baseline.row_count, baseline.digest)),
            );
            medians
                .entry(case.name)
                .or_default()
                .push((baseline.median_ms, measurement.median_ms));
            indexed_plans.insert(case.name, plan);
            indexed.insert(case.name, measurement);
        }

        let query_reports: Vec<JsonValue> = cases
            .iter()
            .map(|case| {
                let sequential_measurement =
                    sequential.get(case.name).expect("sequential measurement");
                let indexed_measurement = indexed.get(case.name).expect("indexed measurement");
                json!({
                    "name": case.name,
                    "query": case.query,
                    "query_fingerprint": query_fingerprint(case.query),
                    "sequential_plan": sequential_plans.get(case.name),
                    "indexed_plan": indexed_plans.get(case.name),
                    "sequential": measurement_json(sequential_measurement),
                    "indexed": measurement_json(indexed_measurement),
                    "indexed_speedup": sequential_measurement.median_ms / indexed_measurement.median_ms,
                })
            })
            .collect();

        if cardinality == *sizes.last().expect("at least two sizes") {
            drop(engine);
            eprintln!("reopening largest indexed fixture");
            let mut reopened = Engine::new(temp.path()).expect("reopen indexed fixture");
            let reopened_cases: Vec<JsonValue> = cases
                .iter()
                .map(|case| {
                    let plan = explain_text(&mut reopened, case.query);
                    assert_indexed_plan(*case, &plan);
                    let baseline = indexed.get(case.name).expect("indexed measurement");
                    let (first_ms, first_count, first_digest) =
                        run_once(&mut reopened, case.query, case.name == "order_limit");
                    assert_eq!(first_count, baseline.row_count, "reopen row-count mismatch");
                    assert_eq!(first_digest, baseline.digest, "reopen result mismatch");
                    let warm = measure(
                        &mut reopened,
                        case.query,
                        case.name == "order_limit",
                        warmups,
                        repetitions,
                        Some((baseline.row_count, baseline.digest)),
                    );
                    json!({
                        "name": case.name,
                        "plan": plan,
                        "first_query_ms": first_ms,
                        "warm": measurement_json(&warm),
                    })
                })
                .collect();
            reopen_report = json!({
                "cardinality": cardinality,
                "engine_reopened": true,
                "process_page_cache_cleared": false,
                "limitation": "Reopen proves durable index discovery and a cold Engine, but does not evict the operating-system page cache.",
                "queries": reopened_cases,
            });
        }

        cardinality_reports.push(json!({
            "rows": cardinality,
            "queries": query_reports,
        }));
    }

    let largest = |name: &str| -> (f64, f64) {
        *medians
            .get(name)
            .and_then(|measurements| measurements.last())
            .expect("largest-cardinality measurement")
    };
    let equality_largest = largest("equality");
    let range_largest = largest("range");
    let order_largest = largest("order_limit");
    let order_all = medians.get("order_limit").expect("order measurements");
    let order_first = order_all.first().expect("smallest order measurement");
    let sequential_order_growth = order_largest.0 / order_first.0;
    let indexed_order_growth = order_largest.1 / order_first.1;
    let order_growth_fraction = indexed_order_growth / sequential_order_growth;

    let gates = json!({
        "largest_equality_speedup": {
            "minimum": MIN_EQUALITY_SPEEDUP,
            "observed": equality_largest.0 / equality_largest.1,
            "pass": equality_largest.0 / equality_largest.1 >= MIN_EQUALITY_SPEEDUP,
        },
        "largest_range_speedup": {
            "minimum": MIN_RANGE_SPEEDUP,
            "observed": range_largest.0 / range_largest.1,
            "pass": range_largest.0 / range_largest.1 >= MIN_RANGE_SPEEDUP,
        },
        "largest_order_limit_speedup": {
            "minimum": MIN_ORDER_SPEEDUP,
            "observed": order_largest.0 / order_largest.1,
            "pass": order_largest.0 / order_largest.1 >= MIN_ORDER_SPEEDUP,
        },
        "order_limit_scaling": {
            "smallest_rows": sizes.first(),
            "largest_rows": sizes.last(),
            "sequential_growth": sequential_order_growth,
            "indexed_growth": indexed_order_growth,
            "maximum_indexed_to_sequential_growth_fraction": MAX_ORDER_GROWTH_FRACTION,
            "observed_indexed_to_sequential_growth_fraction": order_growth_fraction,
            "pass": order_growth_fraction <= MAX_ORDER_GROWTH_FRACTION,
        },
        "plan_shape_and_result_parity": {
            "pass": true,
            "enforced_by": "hard assertions before report generation",
        },
        "reopen_result_parity": {
            "pass": true,
            "enforced_by": "hard assertions before report generation",
        },
    });
    let overall_pass = equality_largest.0 / equality_largest.1 >= MIN_EQUALITY_SPEEDUP
        && range_largest.0 / range_largest.1 >= MIN_RANGE_SPEEDUP
        && order_largest.0 / order_largest.1 >= MIN_ORDER_SPEEDUP
        && order_growth_fraction <= MAX_ORDER_GROWTH_FRACTION;

    let generated_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis();
    let report = json!({
        "schema_version": 1,
        "benchmark": "expression_index_release_gate",
        "generated_unix_ms": generated_unix_ms,
        "overall_pass": overall_pass,
        "repository": repository_metadata(output_path.as_deref()),
        "environment": environment_metadata(),
        "configuration": {
            "fixed_seed": FIXTURE_SEED,
            "score_modulus": SCORE_MODULUS,
            "score_multiplier": SCORE_MULTIPLIER,
            "cardinalities": sizes,
            "warmups": warmups,
            "repetitions": repetitions,
            "range": { "low_inclusive": RANGE_LOW, "high_exclusive": RANGE_HIGH },
            "order_limit": ORDER_LIMIT,
            "index_ddl": INDEX_DDL,
        },
        "methodology": {
            "comparison": "Identical table and queries measured before and after expression-index creation at each cardinality.",
            "statistic": "Median wall time across measured repetitions after warmups.",
            "threshold_policy": "Relative speedup and relative scaling only; no absolute latency threshold.",
            "known_limitations": [
                "Single-process embedded benchmark; server framing and network latency are out of scope.",
                "The reopen pass constructs a cold Engine but does not evict the operating-system page cache.",
                "Results describe this recorded runner and must be refreshed on the release candidate."
            ],
        },
        "queries": {
            "equality": { "text": equality, "fingerprint": query_fingerprint(&equality) },
            "range": { "text": RANGE_QUERY, "fingerprint": query_fingerprint(RANGE_QUERY) },
            "order_limit": { "text": ORDER_QUERY, "fingerprint": query_fingerprint(ORDER_QUERY) },
        },
        "cardinalities": cardinality_reports,
        "reopen": reopen_report,
        "release_gates": gates,
    });

    let serialized = serde_json::to_string_pretty(&report).expect("serialize benchmark report");
    if let Some(path) = output_path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create benchmark artifact directory");
        }
        std::fs::write(&path, format!("{serialized}\n")).unwrap_or_else(|error| {
            panic!("write benchmark artifact `{}`: {error}", path.display())
        });
        eprintln!("wrote benchmark artifact to {}", path.display());
    }
    println!("{serialized}");

    assert!(
        overall_pass,
        "expression-index release gate failed; inspect release_gates in the JSON report"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_generator_is_unique_over_supported_fixture_range() {
        let mut scores = std::collections::HashSet::new();
        for row in 0..160_000 {
            assert!(scores.insert(score_for(row)));
        }
    }

    #[test]
    fn query_fingerprints_are_stable_and_distinct() {
        assert_eq!(query_fingerprint("abc"), "fnv1a64:e71fa2190541574b");
        assert_ne!(
            query_fingerprint(RANGE_QUERY),
            query_fingerprint(ORDER_QUERY)
        );
    }

    #[test]
    fn canonical_artifact_path_is_detected_for_relative_and_absolute_paths() {
        assert!(is_canonical_artifact_path(Path::new(
            CANONICAL_ARTIFACT_PATH
        )));
        assert!(is_canonical_artifact_path(Path::new(
            "/tmp/worktree/crates/bench/baseline/expression-index-v0.13.json"
        )));
        assert!(!is_canonical_artifact_path(Path::new(
            "/tmp/expression-index-v0.13.json"
        )));
    }
}
