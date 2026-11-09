//! Fixed-output native index-page diagnostic, without test instrumentation.
//!
//! Run `cargo run --release -p powdb-bench --bin index-page-probe`.
//! `POWDB_INDEX_PAGE_SIZES` optionally sets comma-separated fixture sizes
//! (default 1000,10000,100000; each must be >= 100). JSON is written to stdout.
//! Compare separate binaries on the same host; these timings alone do not
//! establish isolation, cross-engine rankings or durable-write performance.

use std::error::Error;
use std::hint::black_box;
use std::time::Instant;

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use serde_json::json;

fn verify(result: &QueryResult, lower: i64) -> Result<(), Box<dyn Error>> {
    let QueryResult::Rows { columns, rows } = result else {
        return Err("expected row result".into());
    };
    if columns != &["id", "payload"] || rows.len() != 100 {
        return Err("page columns/count differ".into());
    }
    for (offset, row) in rows.iter().enumerate() {
        let id = lower + offset as i64;
        if row
            != &[
                Value::Int(id),
                Value::Str(format!("row_{id:08}_{}", "x".repeat(48))),
            ]
        {
            return Err(format!("incorrect row at {offset}").into());
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let sizes = std::env::var("POWDB_INDEX_PAGE_SIZES")
        .unwrap_or_else(|_| "1000,10000,100000".into())
        .split(',')
        .map(str::parse::<usize>)
        .collect::<Result<Vec<_>, _>>()?;
    if sizes.is_empty()
        || sizes
            .iter()
            .any(|&size| size < 100 || size > i64::MAX as usize)
    {
        return Err("fixture sizes must be between 100 and i64::MAX".into());
    }
    let mut observations = Vec::new();
    for size in sizes {
        let dir = tempfile::tempdir()?;
        let mut engine = Engine::new(dir.path())?;
        engine.execute_powql("type Page { required unique id: int, payload: str }")?;
        // Fixture loading is deliberately outside measurement. Checkpoint
        // before reads; all engine/query defaults remain unchanged.
        for id in (0..size as i64).rev() {
            engine
                .catalog_mut()
                .get_table_mut("Page")
                .ok_or("missing fixture")?
                .insert(&vec![
                    Value::Int(id),
                    Value::Str(format!("row_{id:08}_{}", "x".repeat(48))),
                ])?;
        }
        engine.catalog_mut().checkpoint()?;
        for (case, lower) in [("all_qualify", 0), ("last_100", size as i64 - 100)] {
            let query =
                format!("Page filter .id >= {lower} order .id limit 100 {{ .id, .payload }}");
            for readonly in [false, true] {
                for _ in 0..3 {
                    let result = if readonly {
                        engine.execute_powql_readonly(&query)?
                    } else {
                        engine.execute_powql(&query)?
                    };
                    verify(&result, lower)?;
                }
                let mut samples_us = Vec::new();
                for _ in 0..7 {
                    let start = Instant::now();
                    let result = if readonly {
                        engine.execute_powql_readonly(&query)?
                    } else {
                        engine.execute_powql(&query)?
                    };
                    let elapsed_us = start.elapsed().as_secs_f64() * 1e6;
                    verify(&result, lower)?;
                    black_box(result);
                    samples_us.push(elapsed_us);
                }
                let mut sorted = samples_us.clone();
                sorted.sort_by(f64::total_cmp);
                observations.push(json!({
                    "fixture_rows": size, "case": case, "readonly": readonly,
                    "output_rows": 100, "samples_us": samples_us, "median_us": sorted[sorted.len()/2],
                }));
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "profile": "native-index-page-diagnostic-v1", "instrumentation": false,
            "memory_limit_bytes": 256 * 1024 * 1024, "observations": observations,
        }))?
    );
    Ok(())
}
