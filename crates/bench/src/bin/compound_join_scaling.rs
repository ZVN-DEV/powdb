//! Bounded compound-join scaling benchmark.
//!
//! This runner answers two questions with release-build measurements:
//!
//! 1. How much wall time does a pure non-equi join spend evaluating every
//!    left/right pair as cardinality grows?
//! 2. How close is an `equi AND residual` ON predicate to the equivalent
//!    hashable equi join followed by the residual filter?
//!
//! Stdout is a single machine-readable JSON document. Progress goes to stderr.
//! Run with:
//!
//! ```text
//! cargo run --release -p powdb-bench --bin compound_join_scaling
//! ```
//!
//! Optional environment variables:
//! - `POWDB_JOIN_BENCH_SIZES=64,128,256,512,1024,2048,2560`
//! - `POWDB_JOIN_BENCH_WARMUPS=2`
//! - `POWDB_JOIN_BENCH_REPS=7`
//! - `POWDB_JOIN_BENCH_BUDGET_MS=250`

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use powdb_storage::wal::WalSyncMode;
use serde_json::{json, Value as JsonValue};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_SIZES: &[usize] = &[64, 128, 256, 512, 1024, 2048, 2560];
const DEFAULT_WARMUPS: usize = 2;
const DEFAULT_REPS: usize = 7;
const DEFAULT_BUDGET_MS: f64 = 250.0;
const BUCKETS: usize = 16;
const SAFETY_FACTOR: f64 = 1.25;

const PURE_NON_EQUI: &str =
    "JoinBenchL as l inner join JoinBenchR as r on l.scan_score < r.scan_score \
     { l.id, r.id }";
const COMPOUND_ON: &str = "JoinBenchL as l inner join JoinBenchR as r \
     on l.bucket = r.bucket and l.residual_score < r.residual_score \
     { l.id, r.id }";
const HASH_TARGET: &str = "JoinBenchL as l inner join JoinBenchR as r on l.bucket = r.bucket \
     filter l.residual_score < r.residual_score { l.id, r.id }";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "powdb_compound_join_bench_{}_{}",
            std::process::id(),
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
    observed_rows: usize,
}

fn env_usize(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(raw) => raw
            .parse::<usize>()
            .unwrap_or_else(|_| panic!("{name} must be a positive integer")),
        Err(_) => default,
    }
}

fn env_f64(name: &str, default: f64) -> f64 {
    match std::env::var(name) {
        Ok(raw) => raw
            .parse::<f64>()
            .unwrap_or_else(|_| panic!("{name} must be a positive number")),
        Err(_) => default,
    }
}

fn configured_sizes() -> Vec<usize> {
    let Some(raw) = std::env::var("POWDB_JOIN_BENCH_SIZES").ok() else {
        return DEFAULT_SIZES.to_vec();
    };
    let sizes: Vec<usize> = raw
        .split(',')
        .map(str::trim)
        .map(|part| {
            part.parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid cardinality in POWDB_JOIN_BENCH_SIZES"))
        })
        .collect();
    assert!(!sizes.is_empty(), "at least one cardinality is required");
    assert!(
        sizes.iter().all(|size| *size > 0 && *size <= 4096),
        "cardinalities must be in 1..=4096 to keep this benchmark bounded"
    );
    assert!(
        sizes.windows(2).all(|pair| pair[0] < pair[1]),
        "cardinalities must be strictly increasing"
    );
    sizes
}

fn setup(size: usize) -> (Engine, TempDir) {
    let temp = TempDir::new();
    let mut engine = Engine::new(temp.path()).expect("create benchmark engine");
    engine.catalog_mut().set_wal_sync_mode(WalSyncMode::Off);
    engine
        .execute_powql(
            "type JoinBenchL { required id: int, required bucket: int, \
             required scan_score: int, required residual_score: int }",
        )
        .expect("create left table");
    engine
        .execute_powql(
            "type JoinBenchR { required id: int, required bucket: int, \
             required scan_score: int, required residual_score: int }",
        )
        .expect("create right table");

    {
        let left = engine
            .catalog_mut()
            .get_table_mut("JoinBenchL")
            .expect("open left table");
        for id in 0..size {
            left.insert(&vec![
                Value::Int(id as i64),
                Value::Int((id % BUCKETS) as i64),
                Value::Int((size + id) as i64),
                Value::Int(((id * 37) % 257) as i64),
            ])
            .expect("insert left row");
        }
    }
    {
        let right = engine
            .catalog_mut()
            .get_table_mut("JoinBenchR")
            .expect("open right table");
        for id in 0..size {
            right
                .insert(&vec![
                    Value::Int(id as i64),
                    Value::Int((id % BUCKETS) as i64),
                    Value::Int(id as i64),
                    Value::Int(((id * 53 + 11) % 257) as i64),
                ])
                .expect("insert right row");
        }
    }
    (engine, temp)
}

fn row_count(result: QueryResult) -> usize {
    match result {
        QueryResult::Rows { rows, .. } => rows.len(),
        other => panic!("expected row result, got {other:?}"),
    }
}

fn expected_compound_rows(size: usize) -> usize {
    let mut count = 0usize;
    for left in 0..size {
        let left_score = (left * 37) % 257;
        for right in 0..size {
            let right_score = (right * 53 + 11) % 257;
            if left % BUCKETS == right % BUCKETS && left_score < right_score {
                count += 1;
            }
        }
    }
    count
}

fn measure(
    engine: &mut Engine,
    query: &str,
    expected_rows: usize,
    warmups: usize,
    repetitions: usize,
) -> Measurement {
    assert!(
        repetitions > 0,
        "at least one measured repetition is required"
    );
    for _ in 0..warmups {
        let observed = row_count(engine.execute_powql(query).expect("warmup query"));
        assert_eq!(observed, expected_rows, "warmup row count mismatch");
    }

    let mut samples_ms = Vec::with_capacity(repetitions);
    let mut observed_rows = 0usize;
    for _ in 0..repetitions {
        let started = Instant::now();
        observed_rows = row_count(engine.execute_powql(query).expect("measured query"));
        let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        assert_eq!(observed_rows, expected_rows, "measured row count mismatch");
        std::hint::black_box(observed_rows);
        samples_ms.push(elapsed_ms);
    }

    let mut sorted = samples_ms.clone();
    sorted.sort_by(f64::total_cmp);
    Measurement {
        median_ms: sorted[sorted.len() / 2],
        min_ms: sorted[0],
        max_ms: sorted[sorted.len() - 1],
        samples_ms,
        observed_rows,
    }
}

fn measurement_json(measurement: &Measurement, expected_rows: usize) -> JsonValue {
    json!({
        "expected_rows": expected_rows,
        "observed_rows": measurement.observed_rows,
        "samples_ms": measurement.samples_ms,
        "median_ms": measurement.median_ms,
        "min_ms": measurement.min_ms,
        "max_ms": measurement.max_ms,
    })
}

fn round_down_significant(value: u64) -> u64 {
    if value < 10 {
        return value;
    }
    let magnitude = 10u64.pow(value.ilog10());
    let quantum = (magnitude / 10).max(1);
    value / quantum * quantum
}

fn main() {
    if cfg!(debug_assertions) {
        panic!("this benchmark must run with --release");
    }

    let sizes = configured_sizes();
    let warmups = env_usize("POWDB_JOIN_BENCH_WARMUPS", DEFAULT_WARMUPS);
    let repetitions = env_usize("POWDB_JOIN_BENCH_REPS", DEFAULT_REPS);
    let budget_ms = env_f64("POWDB_JOIN_BENCH_BUDGET_MS", DEFAULT_BUDGET_MS);
    assert!(warmups <= 20, "warmups must be at most 20");
    assert!(
        (1..=50).contains(&repetitions),
        "repetitions must be in 1..=50"
    );
    assert!(
        budget_ms.is_finite() && budget_ms > 0.0,
        "budget must be positive"
    );

    let mut cases = Vec::with_capacity(sizes.len());
    let mut largest_nested_ns_per_pair = 0.0f64;

    for size in sizes.iter().copied() {
        eprintln!("measuring {size} x {size} joins");
        let pair_count = size
            .checked_mul(size)
            .expect("cardinality pair count overflow");
        let expected_compound = expected_compound_rows(size);
        let (mut engine, _temp) = setup(size);

        let pure = measure(&mut engine, PURE_NON_EQUI, 0, warmups, repetitions);
        let compound = measure(
            &mut engine,
            COMPOUND_ON,
            expected_compound,
            warmups,
            repetitions,
        );
        let hash_target = measure(
            &mut engine,
            HASH_TARGET,
            expected_compound,
            warmups,
            repetitions,
        );

        let nested_ns_per_pair = pure.median_ms * 1_000_000.0 / pair_count as f64;
        largest_nested_ns_per_pair = nested_ns_per_pair;
        let compound_to_target = compound.median_ms / hash_target.median_ms;
        cases.push(json!({
            "left_rows": size,
            "right_rows": size,
            "candidate_pairs": pair_count,
            "pure_non_equi": measurement_json(&pure, 0),
            "pure_non_equi_ns_per_pair": nested_ns_per_pair,
            "compound_on": measurement_json(&compound, expected_compound),
            "hash_target": measurement_json(&hash_target, expected_compound),
            "compound_to_hash_target_ratio": compound_to_target,
        }));
    }

    let conservative_ns_per_pair = largest_nested_ns_per_pair * SAFETY_FACTOR;
    let unrounded_limit = (budget_ms * 1_000_000.0 / conservative_ns_per_pair).floor() as u64;
    let recommended_limit = round_down_significant(unrounded_limit).max(1);
    let generated_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis();

    let report = json!({
        "schema_version": 1,
        "benchmark": "compound_join_scaling",
        "generated_unix_ms": generated_unix_ms,
        "build_profile": "release",
        "configuration": {
            "cardinalities": sizes,
            "warmups": warmups,
            "repetitions": repetitions,
            "bucket_count": BUCKETS,
            "query_time_budget_ms": budget_ms,
            "threshold_safety_factor": SAFETY_FACTOR,
        },
        "queries": {
            "pure_non_equi": PURE_NON_EQUI,
            "compound_on": COMPOUND_ON,
            "hash_target": HASH_TARGET,
        },
        "cases": cases,
        "threshold_recommendation": {
            "basis": "largest-cardinality pure non-equi median cost per pair with safety factor",
            "largest_case_ns_per_pair": largest_nested_ns_per_pair,
            "conservative_ns_per_pair": conservative_ns_per_pair,
            "unrounded_max_pairs_for_budget": unrounded_limit,
            "recommended_max_nested_loop_pairs": recommended_limit,
        },
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("serialize benchmark report")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compound_expected_count_matches_direct_enumeration_examples() {
        assert_eq!(expected_compound_rows(1), 1);
        assert_eq!(expected_compound_rows(16), 9);
        assert_eq!(expected_compound_rows(64), 137);
    }

    #[test]
    fn significant_rounding_is_conservative() {
        assert_eq!(round_down_significant(9), 9);
        assert_eq!(round_down_significant(19), 19);
        assert_eq!(round_down_significant(123), 120);
        assert_eq!(round_down_significant(12_345), 12_000);
    }
}
