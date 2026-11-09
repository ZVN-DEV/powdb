//! Quiescent-file experiment for late, page-grouped row-ID materialization.
//!
//! This is non-shipping benchmark code, not an engine access path. It compares
//! full/projected checked row fetches with checked page packets, preserving
//! request order, duplicate multiplicity and missing slots. The mmap reference
//! has the engine's different checksum policy and is labeled separately.
//! Files are checkpointed and never mutated during measurement. Dirty-page,
//! transaction, concurrent-writer and recovery integration are NOT implemented.
//!
//! `cargo run --release -p powdb-bench --bin page-gather-probe`
//! writes JSON to stdout. Optional settings: `POWDB_GATHER_ROWS` (8192),
//! `POWDB_GATHER_REQUESTS` (4096), `POWDB_GATHER_SAMPLES` (7), and
//! `POWDB_GATHER_PAYLOADS` (comma-separated bytes: 64,1024,8192).
//! `POWDB_GATHER_ORDER` rotates strategy order (0 by default). Place fixtures
//! on the intended filesystem with `TMPDIR=/path/on/ssd`.
//! These are warmed buffered-file measurements, not cold NVMe measurements.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::io;
use std::time::Instant;

use powdb_query::executor::Engine;
use powdb_storage::disk::DiskManager;
use powdb_storage::page::{Page, PAGE_SIZE};
use powdb_storage::row::{decode_column, validate_row_format, RowLayout};
use powdb_storage::table::Table;
use powdb_storage::types::{RowId, Value};
use serde_json::json;

type Gathered = Vec<Option<Value>>;

#[derive(Clone, Copy, Debug)]
enum Strategy {
    FullChecked,
    ProjectedChecked,
    PacketChecked,
    ProjectedMmap,
}

impl Strategy {
    fn label(self) -> &'static str {
        match self {
            Self::FullChecked => "full_row_checked",
            Self::ProjectedChecked => "projected_row_checked",
            Self::PacketChecked => "projected_page_packet_checked",
            Self::ProjectedMmap => "projected_mmap_reference",
        }
    }
}

#[derive(Default)]
struct Work {
    explicit_page_reads: Option<usize>,
    decoded_rows: usize,
    unused_payload_bytes: usize,
}

fn gather(
    table: &Table,
    disk: &DiskManager,
    requests: &[RowId],
    strategy: Strategy,
) -> io::Result<(Gathered, Work)> {
    if matches!(strategy, Strategy::PacketChecked) {
        return gather_packets(table, disk, requests);
    }
    let mut values = Vec::with_capacity(requests.len());
    let mut work = Work::default();
    for &rid in requests {
        let row = if matches!(strategy, Strategy::FullChecked) {
            table.get(rid)?
        } else {
            table.get_projected(rid, &[0])?
        };
        match row {
            Some(row) => {
                work.decoded_rows += 1;
                if matches!(strategy, Strategy::FullChecked) {
                    if let Some(Value::Str(payload)) = row.get(1) {
                        work.unused_payload_bytes += payload.len();
                    }
                }
                values.push(row.into_iter().next());
            }
            None => values.push(None),
        }
    }
    Ok((values, work))
}

fn gather_packets(
    table: &Table,
    disk: &DiskManager,
    requests: &[RowId],
) -> io::Result<(Gathered, Work)> {
    // All planning, sorting and order restoration are inside measurement.
    let mut order: Vec<_> = requests.iter().copied().enumerate().collect();
    order.sort_unstable_by_key(|&(position, rid)| (rid.page_id, rid.slot_index, position));
    let mut output = vec![None; requests.len()];
    let layout = RowLayout::new(table.schema());
    let mut work = Work {
        explicit_page_reads: Some(0),
        ..Work::default()
    };
    let mut cursor = 0;
    while cursor < order.len() {
        let page_id = order[cursor].1.page_id;
        if page_id >= disk.num_pages() {
            while cursor < order.len() && order[cursor].1.page_id == page_id {
                cursor += 1;
            }
            continue;
        }
        let bytes = disk.read_page(page_id)?;
        // Same validate-if-present CRC policy as checked heap point reads.
        let page = Page::from_bytes_verified(&bytes).map_err(io::Error::from)?;
        if let Some(reads) = &mut work.explicit_page_reads {
            *reads += 1;
        }
        let mut previous: Option<(u16, Option<Value>)> = None;
        while cursor < order.len() && order[cursor].1.page_id == page_id {
            let (position, rid) = order[cursor];
            let value = match &previous {
                Some((slot, value)) if *slot == rid.slot_index => value.clone(),
                _ => {
                    let value = match page.get(rid.slot_index) {
                        Some(row) => {
                            validate_row_format(row)?;
                            work.decoded_rows += 1;
                            // This fixture projects only its non-spilled int
                            // id. General overflow projection is not an API
                            // promise of this experimental path.
                            Some(decode_column(table.schema(), &layout, row, 0))
                        }
                        None => None,
                    };
                    previous = Some((rid.slot_index, value.clone()));
                    value
                }
            };
            output[position] = value;
            cursor += 1;
        }
    }
    Ok((output, work))
}

fn physical_read_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/io").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("read_bytes: "))?
        .trim()
        .parse()
        .ok()
}

fn shuffle<T>(values: &mut [T]) {
    let mut seed = 0x98bc_724a_d512_6e03u64;
    for i in (1..values.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        values.swap(i, (seed % (i + 1) as u64) as usize);
    }
}

fn parameter(name: &str, default: usize) -> Result<usize, Box<dyn Error>> {
    match std::env::var(name) {
        Ok(value) => Ok(value.parse()?),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn fixture_payload(id: usize, width: usize) -> String {
    let mut state = (id as u64).wrapping_add(0x834b_2719_a653_02dd);
    (0..width)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (b'!' + (state % 94) as u8) as char
        })
        .collect()
}

fn filesystem(path: &std::path::Path) -> Option<String> {
    let result = std::process::Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", "-T"])
        .arg(path)
        .output()
        .ok()?;
    result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

fn main() -> Result<(), Box<dyn Error>> {
    let rows = parameter("POWDB_GATHER_ROWS", 8192)?;
    let requests = parameter("POWDB_GATHER_REQUESTS", 4096)?;
    let samples = parameter("POWDB_GATHER_SAMPLES", 7)?;
    let order_shift = parameter("POWDB_GATHER_ORDER", 0)?;
    let payloads = std::env::var("POWDB_GATHER_PAYLOADS")
        .unwrap_or_else(|_| "64,1024,8192".into())
        .split(',')
        .map(str::parse::<usize>)
        .collect::<Result<Vec<_>, _>>()?;
    if rows < 128
        || rows > i64::MAX as usize
        || requests == 0
        || samples == 0
        || payloads.is_empty()
    {
        return Err("invalid fixture/request/sample sizes".into());
    }
    let mut observations = Vec::new();
    for (layout_index, payload_bytes) in payloads.into_iter().enumerate() {
        let dir = tempfile::tempdir()?;
        let fixture_filesystem = filesystem(dir.path());
        let mut engine = Engine::new(dir.path())?;
        engine.execute_powql("type Gather { required id: int, payload: str }")?;
        let mut rids = Vec::with_capacity(rows);
        for id in 0..rows {
            let rid = engine
                .catalog_mut()
                .get_table_mut("Gather")
                .ok_or("table")?
                .insert(&vec![
                    Value::Int(id as i64),
                    Value::Str(fixture_payload(id, payload_bytes)),
                ])?;
            rids.push(rid);
        }
        engine
            .catalog_mut()
            .get_table_mut("Gather")
            .ok_or("table")?
            .delete(rids[3])?;
        engine.catalog_mut().checkpoint()?;
        // Reopen to clear the trusted hot/dirty page caches before comparing
        // checked file reads. A flushed hot page may remain authoritative.
        drop(engine);
        let mut engine = Engine::new(dir.path())?;
        let disk = DiskManager::open_read_only(&dir.path().join("Gather.heap"))?;
        let expected_by_rid: BTreeMap<_, _> = rids
            .iter()
            .enumerate()
            .map(|(id, rid)| {
                (
                    (rid.page_id, rid.slot_index),
                    (id != 3).then_some(Value::Int(id as i64)),
                )
            })
            .collect();
        let hot_pages: BTreeSet<_> = rids
            .iter()
            .map(|rid| rid.page_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .take(16)
            .collect();
        let hot: Vec<_> = rids
            .iter()
            .copied()
            .filter(|rid| hot_pages.contains(&rid.page_id))
            .collect();
        let mut permuted = rids.clone();
        shuffle(&mut permuted);
        for (case_index, (case, source)) in [
            ("contiguous", &rids),
            ("spread", &permuted),
            ("join_fanout", &hot),
        ]
        .into_iter()
        .enumerate()
        {
            let mut trace: Vec<_> = (0..requests).map(|i| source[i % source.len()]).collect();
            if case == "join_fanout" {
                shuffle(&mut trace);
            }
            let expected: Gathered = trace
                .iter()
                .map(|rid| expected_by_rid[&(rid.page_id, rid.slot_index)].clone())
                .collect();
            let distinct_pages = trace
                .iter()
                .map(|rid| rid.page_id)
                .collect::<BTreeSet<_>>()
                .len();
            let mut strategies = [
                Strategy::FullChecked,
                Strategy::ProjectedChecked,
                Strategy::PacketChecked,
                Strategy::ProjectedMmap,
            ];
            let rotation = (layout_index % 4 + case_index % 4 + order_shift % 4) % 4;
            strategies.rotate_left(rotation);
            for strategy in strategies {
                let table = engine
                    .catalog_mut()
                    .get_table_mut("Gather")
                    .ok_or("table")?;
                if matches!(strategy, Strategy::ProjectedMmap) {
                    table.heap.enable_mmap();
                } else {
                    table.heap.disable_mmap();
                }
                for _ in 0..2 {
                    let (values, _) = gather(table, &disk, &trace, strategy)?;
                    if values != expected {
                        return Err("warmup result mismatch".into());
                    }
                }
                let mut timings = Vec::new();
                let mut reads = Vec::new();
                let mut recorded_work = None;
                for _ in 0..samples {
                    let before = physical_read_bytes();
                    let start = Instant::now();
                    let (values, work) = gather(table, &disk, &trace, strategy)?;
                    let elapsed_us = start.elapsed().as_secs_f64() * 1e6;
                    let after = physical_read_bytes();
                    if values != expected {
                        return Err("timed result mismatch".into());
                    }
                    if matches!(strategy, Strategy::PacketChecked)
                        && work.explicit_page_reads != Some(distinct_pages)
                    {
                        return Err("packet fetched a primary page more than once".into());
                    }
                    timings.push(elapsed_us);
                    reads.push(before.zip(after).map(|(a, b)| b.saturating_sub(a)));
                    recorded_work = Some(work);
                }
                let work = recorded_work.ok_or("missing sample")?;
                let mut sorted = timings.clone();
                sorted.sort_by(f64::total_cmp);
                observations.push(json!({
                    "payload_bytes":payload_bytes,"fixture_rows":rows,"case":case,"strategy":strategy.label(),
                    "fixture_filesystem":fixture_filesystem,
                    "requests":requests,"distinct_primary_pages":distinct_pages,"samples_us":timings,
                    "median_us":sorted[sorted.len()/2],"physical_read_bytes":reads,
                    "explicit_packet_page_reads":work.explicit_page_reads,"decoded_rows":work.decoded_rows,
                    "unused_payload_bytes_decoded":work.unused_payload_bytes,"output_verified":true,
                    "nominal_packet_primary_bytes":work.explicit_page_reads.map(|pages| pages*PAGE_SIZE),
                }));
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "profile":"quiescent-warm-page-gather-v1","boundary":"row-ID gather, not complete joins",
            "cache":"warmed buffered files, no cache eviction","cold_nvme_claim":false,
        "payload_pattern":"fixed-seed per-row ASCII","order_shift":order_shift,
            "checksum_policy":"checked row/page paths validate CRC if present; mmap reference retains existing different policy",
            "production_integration":false,"observations":observations,
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_and_row_checked_paths_both_refuse_corruption() {
        use std::io::{Read, Seek, SeekFrom, Write};
        let dir = tempfile::tempdir().expect("fixture");
        let mut engine = Engine::new(dir.path()).expect("open");
        engine
            .execute_powql("type Gather { required id: int, payload: str }")
            .expect("schema");
        let rid = engine
            .catalog_mut()
            .get_table_mut("Gather")
            .expect("table")
            .insert(&vec![Value::Int(1), Value::Str("payload".into())])
            .expect("row");
        engine.catalog_mut().checkpoint().expect("checkpoint");
        drop(engine);
        let mut engine = Engine::new(dir.path()).expect("reopen");
        let table = engine.catalog_mut().get_table_mut("Gather").expect("table");
        table.heap.disable_mmap();
        let path = dir.path().join("Gather.heap");
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("file");
        let offset = (rid.page_id as u64 + 1) * PAGE_SIZE as u64 - 1;
        file.seek(SeekFrom::Start(offset)).expect("seek");
        let mut byte = [0u8];
        file.read_exact(&mut byte).expect("read");
        byte[0] ^= 1;
        file.seek(SeekFrom::Start(offset)).expect("seek");
        file.write_all(&byte).expect("corrupt");
        file.sync_data().expect("sync");
        let disk = DiskManager::open_read_only(&path).expect("disk");
        assert!(gather(table, &disk, &[rid], Strategy::ProjectedChecked).is_err());
        assert!(gather(table, &disk, &[rid], Strategy::PacketChecked).is_err());
    }

    #[test]
    fn packets_preserve_duplicates_missing_slots_order_and_empty_input() {
        for width in [64, 8192] {
            let dir = tempfile::tempdir().expect("fixture");
            let mut engine = Engine::new(dir.path()).expect("open");
            engine
                .execute_powql("type Gather { required id: int, payload: str }")
                .expect("schema");
            let mut rids = Vec::new();
            for id in 0..100 {
                rids.push(
                    engine
                        .catalog_mut()
                        .get_table_mut("Gather")
                        .expect("table")
                        .insert(&vec![Value::Int(id), Value::Str("x".repeat(width))])
                        .expect("row"),
                );
            }
            engine
                .catalog_mut()
                .get_table_mut("Gather")
                .expect("table")
                .delete(rids[3])
                .expect("delete");
            engine.catalog_mut().checkpoint().expect("checkpoint");
            drop(engine);
            let mut engine = Engine::new(dir.path()).expect("reopen");
            let table = engine.catalog_mut().get_table_mut("Gather").expect("table");
            table.heap.disable_mmap();
            let disk = DiskManager::open_read_only(&dir.path().join("Gather.heap")).expect("disk");
            for trace in [
                vec![],
                vec![
                    rids[90],
                    rids[3],
                    rids[0],
                    rids[90],
                    rids[10],
                    rids[3],
                    RowId {
                        page_id: u32::MAX,
                        slot_index: 0,
                    },
                    RowId {
                        page_id: rids[0].page_id,
                        slot_index: u16::MAX,
                    },
                ],
            ] {
                let expected = gather(table, &disk, &trace, Strategy::ProjectedChecked)
                    .expect("baseline")
                    .0;
                let (actual, work) =
                    gather(table, &disk, &trace, Strategy::PacketChecked).expect("packet");
                assert_eq!(actual, expected);
                if !trace.is_empty() {
                    assert_eq!(
                        actual,
                        vec![
                            Some(Value::Int(90)),
                            None,
                            Some(Value::Int(0)),
                            Some(Value::Int(90)),
                            Some(Value::Int(10)),
                            None,
                            None,
                            None
                        ]
                    );
                }
                assert_eq!(
                    work.explicit_page_reads,
                    Some(
                        trace
                            .iter()
                            .map(|rid| rid.page_id)
                            .filter(|&page| page < disk.num_pages())
                            .collect::<BTreeSet<_>>()
                            .len()
                    )
                );
            }
        }
    }
}
