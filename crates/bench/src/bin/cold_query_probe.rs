//! Cold readonly unique-range fetch through the production executor.
//!
//! The library probe times `read_pages_verified` directly. This one times
//! `Gather filter .rk >= 0` after the heap has been dropped from the page
//! cache. Read-only open scans every heap page, so the timed window starts
//! only after `posix_fadvise(DONTNEED)`. Index files are left cached.
//!
//! Keys are inserted in shuffled order and the range walks them in key order,
//! so the heap pages are not sequential. A sequential scan would return
//! insertion order. The probe requires the keys to come back as `0..n`.
//!
//! ```text
//! cargo run --release -p powdb-bench --bin cold-query-probe
//! ```
//!
//! The fixture is created under `/mnt/models/powdb-grade-q1` and removed
//! before exit. Optional: `POWDB_COLD_ROWS` (8192), `POWDB_COLD_PAYLOAD`
//! (2048), `POWDB_COLD_SAMPLES` (5).

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

const DEFAULT_ROWS: usize = 8_192;
const DEFAULT_PAYLOAD: usize = 2_048;
const DEFAULT_SAMPLES: usize = 5;
const FIXTURE_ROOT: &str = "/mnt/models/powdb-grade-q1";
const QUERY: &str = "Gather filter .rk >= 0";

fn main() -> Result<(), Box<dyn Error>> {
    let rows = parameter("POWDB_COLD_ROWS", DEFAULT_ROWS)?;
    let payload_bytes = parameter("POWDB_COLD_PAYLOAD", DEFAULT_PAYLOAD)?;
    let samples = parameter("POWDB_COLD_SAMPLES", DEFAULT_SAMPLES)?;
    if rows < 32
        || rows > i64::MAX as usize
        || !(64..=3_000).contains(&payload_bytes)
        || samples == 0
    {
        return Err("invalid cold-query settings".into());
    }

    let root = PathBuf::from(FIXTURE_ROOT);
    std::fs::create_dir_all(&root)?;
    let filesystem = filesystem(&root);
    if filesystem.as_deref() == Some("tmpfs") {
        return Err("fixture root is tmpfs".into());
    }
    let dir = tempfile::tempdir_in(&root)?;
    let compression_disabled = disable_btrfs_compression(dir.path());
    let heap_path = dir.path().join("Gather.heap");

    let mut insert_order: Vec<usize> = (0..rows).collect();
    shuffle(&mut insert_order);
    {
        let mut engine = Engine::new(dir.path())?;
        engine.execute_powql("type Gather { required unique rk: int, payload: bytes }")?;
        {
            let table = engine
                .catalog_mut()
                .get_table_mut("Gather")
                .ok_or("missing Gather table")?;
            for &key in &insert_order {
                table.insert(&vec![
                    Value::Int(key as i64),
                    Value::Bytes(incompressible(key, payload_bytes)),
                ])?;
            }
        }
        engine.catalog_mut().checkpoint()?;
    }

    let engine = Engine::open_read_only(dir.path())?;
    let heap_bytes = std::fs::metadata(&heap_path)?.len();
    let mut observations = Vec::with_capacity(samples + 1);
    for index in 0..samples {
        observations.push(sample(
            &engine,
            &heap_path,
            rows,
            payload_bytes,
            true,
            index,
        )?);
    }
    observations.push(sample(
        &engine,
        &heap_path,
        rows,
        payload_bytes,
        false,
        samples,
    )?);

    let report = serde_json::json!({
        "probe": "cold-query-probe",
        "query": QUERY,
        "filesystem": filesystem,
        "compression_disabled": compression_disabled,
        "rows": rows,
        "payload_bytes": payload_bytes,
        "heap_bytes": heap_bytes,
        "fixture": dir.path(),
        "observations": observations,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    drop(engine);
    drop(dir);
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn sample(
    engine: &Engine,
    heap_path: &Path,
    rows: usize,
    payload_bytes: usize,
    cold: bool,
    index: usize,
) -> io::Result<serde_json::Value> {
    if cold {
        evict_cache(heap_path)?;
    }
    let before_proc = physical_read_bytes();
    let before_device = disk_read_bytes();
    let start = Instant::now();
    let result = engine
        .execute_powql_readonly(QUERY)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let elapsed_us = start.elapsed().as_secs_f64() * 1e6;
    let after_proc = physical_read_bytes();
    let after_device = disk_read_bytes();
    verify_rows(&result, rows, payload_bytes)?;
    let proc_delta = before_proc
        .zip(after_proc)
        .map(|(before, after)| after.saturating_sub(before));
    if cold && proc_delta == Some(0) {
        return Err(io::Error::other(format!(
            "cold sample {index} moved no /proc/self/io read_bytes"
        )));
    }
    if !cold && proc_delta.is_some_and(|delta| delta != 0) {
        return Err(io::Error::other(format!(
            "warm sample moved {proc_delta:?} read_bytes"
        )));
    }
    Ok(serde_json::json!({
        "case": "readonly_unique_range",
        "cold": cold,
        "sample": index,
        "elapsed_us": elapsed_us,
        "physical_read_bytes": proc_delta,
        "device_read_bytes": device_delta(&before_device, &after_device),
        "rows_verified": rows,
    }))
}

fn verify_rows(result: &QueryResult, rows: usize, payload_bytes: usize) -> io::Result<()> {
    let QueryResult::Rows { rows: got, .. } = result else {
        return Err(io::Error::other("query did not return rows"));
    };
    if got.len() != rows {
        return Err(io::Error::other(format!(
            "row count {} != {rows}",
            got.len()
        )));
    }
    for (index, row) in got.iter().enumerate() {
        let Some(Value::Int(key)) = row.first() else {
            return Err(io::Error::other(format!("row {index} has no int key")));
        };
        if *key != index as i64 {
            return Err(io::Error::other(format!(
                "row {index} key {key} is not index order"
            )));
        }
        let Some(Value::Bytes(payload)) = row.get(1) else {
            return Err(io::Error::other(format!("row {index} has no payload")));
        };
        if payload.as_slice() != incompressible(index, payload_bytes).as_slice() {
            return Err(io::Error::other(format!("row {index} payload mismatch")));
        }
    }
    Ok(())
}

fn evict_cache(path: &Path) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let file = File::open(path)?;
    let advise = |file: &File| -> io::Result<()> {
        // Safety: the descriptor is a file just opened, and the advice covers
        // the whole file. It drops clean cache pages only.
        let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc))
        }
    };
    advise(&file)?;
    std::thread::sleep(Duration::from_millis(50));
    advise(&file)?;
    Ok(())
}

fn physical_read_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/io").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("read_bytes: "))?
        .trim()
        .parse()
        .ok()
}

fn disk_read_bytes() -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string("/proc/diskstats") else {
        return out;
    };
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.nth(2) else {
            continue;
        };
        if !(name.starts_with("nvme") && !name.contains('p')) {
            continue;
        }
        let Some(sectors) = parts.nth(2).and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        out.insert(name.to_string(), sectors.saturating_mul(512));
    }
    out
}

fn device_delta(
    before: &BTreeMap<String, u64>,
    after: &BTreeMap<String, u64>,
) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for (name, after_bytes) in after {
        let before_bytes = before.get(name).copied().unwrap_or(0);
        out.insert(name.clone(), after_bytes.saturating_sub(before_bytes));
    }
    out
}

fn incompressible(id: usize, width: usize) -> Vec<u8> {
    let mut state = (id as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0x6a09_e667_f3bc_c909);
    (0..width)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn shuffle(values: &mut [usize]) {
    let mut seed = 0x98bc_724a_d512_6e03u64;
    for index in (1..values.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        values.swap(index, (seed % (index as u64 + 1)) as usize);
    }
}

fn filesystem(path: &Path) -> Option<String> {
    let result = std::process::Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", "-T"])
        .arg(path)
        .output()
        .ok()?;
    if !result.status.success() {
        return None;
    }
    let text = String::from_utf8(result.stdout).ok()?;
    let name = text.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn disable_btrfs_compression(path: &Path) -> bool {
    std::process::Command::new("btrfs")
        .args(["property", "set"])
        .arg(path)
        .args(["compression", "none"])
        .status()
        .is_ok_and(|status| status.success())
}

fn parameter(name: &str, default: usize) -> Result<usize, Box<dyn Error>> {
    match std::env::var(name) {
        Ok(value) => Ok(value.parse()?),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}
