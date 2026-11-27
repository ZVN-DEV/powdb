//! Cold buffered-page schedule for a quiescent heap file.
//!
//! Probe of the opt-in storage batch reader; default point reads are unchanged. Each cold
//! sample drops this file from the kernel page cache with `posix_fadvise`
//! (`POSIX_FADV_DONTNEED`) and then reads through [`DiskManager`], which is a
//! synchronous `pread`. The heap is not reopened after eviction: heap open
//! scans every page and would warm the cache before the timer starts.
//!
//! There is no `io_uring` or `O_DIRECT` path in the engine. Queue depth here
//! uses the storage library to run scoped threads calling `pread` on one file, the
//! scheduling comparison available on 2026-11-26.
//!
//! ```text
//! TMPDIR=/path/on/btrfs cargo run --release -p powdb-bench --bin cold-page-probe
//! ```
//!
//! Optional: `POWDB_COLD_ROWS` (16384), `POWDB_COLD_PAYLOAD` (2048),
//! `POWDB_COLD_SAMPLES` (7), `POWDB_COLD_RANDOM_PAGES` (4096),
//! `POWDB_COLD_FANOUT` (4096), `POWDB_COLD_DEPTHS` (1,4,16,32).
//! `/tmp` on this machine is tmpfs. A tmpfs fixture is refused.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::File;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use powdb_query::executor::Engine;
use powdb_storage::disk::{read_pages_verified, DiskManager};
use powdb_storage::page::{Page, PAGE_SIZE};
use powdb_storage::types::Value;
use serde_json::{json, Value as Json};

const DEFAULT_ROWS: usize = 16_384;
const DEFAULT_PAYLOAD: usize = 2_048;
const DEFAULT_SAMPLES: usize = 7;
const DEFAULT_RANDOM_PAGES: usize = 4_096;
const DEFAULT_FANOUT: usize = 4_096;

fn main() -> Result<(), Box<dyn Error>> {
    let rows = parameter("POWDB_COLD_ROWS", DEFAULT_ROWS)?;
    let payload_bytes = parameter("POWDB_COLD_PAYLOAD", DEFAULT_PAYLOAD)?;
    let samples = parameter("POWDB_COLD_SAMPLES", DEFAULT_SAMPLES)?;
    let random_pages = parameter("POWDB_COLD_RANDOM_PAGES", DEFAULT_RANDOM_PAGES)?;
    let fanout = parameter("POWDB_COLD_FANOUT", DEFAULT_FANOUT)?;
    let depths = std::env::var("POWDB_COLD_DEPTHS")
        .unwrap_or_else(|_| "1,4,16,32".into())
        .split(',')
        .map(str::parse::<usize>)
        .collect::<Result<Vec<_>, _>>()?;
    if rows < 32
        || rows > i64::MAX as usize
        || !(64..=3_000).contains(&payload_bytes)
        || samples == 0
        || random_pages == 0
        || fanout == 0
        || depths.is_empty()
        || depths.contains(&0)
    {
        return Err("invalid cold-probe settings".into());
    }

    let dir = tempfile::tempdir()?;
    let compression_disabled = disable_btrfs_compression(dir.path());
    let fixture_filesystem = filesystem(dir.path());
    if fixture_filesystem.as_deref() == Some("tmpfs") {
        return Err(
            "fixture is on tmpfs; set TMPDIR to the Btrfs volume that backs the NVMe device".into(),
        );
    }
    let heap_path = dir.path().join("Gather.heap");
    let mut page_ids = Vec::new();
    {
        let mut engine = Engine::new(dir.path())?;
        engine.execute_powql("type Gather { required id: int, payload: bytes }")?;
        {
            let table = engine
                .catalog_mut()
                .get_table_mut("Gather")
                .ok_or("missing Gather table")?;
            for id in 0..rows {
                let rid = table.insert(&vec![
                    Value::Int(id as i64),
                    Value::Bytes(incompressible(id, payload_bytes)),
                ])?;
                page_ids.push(rid.page_id);
            }
        }
        engine.catalog_mut().checkpoint()?;
    }

    let disk = DiskManager::open_read_only(&heap_path)?;
    let page_count = disk.num_pages();
    if page_count == 0 {
        return Err("heap has no pages".into());
    }
    let fingerprints = fingerprints(&disk)?;
    let heap_bytes = (page_count as u64).saturating_mul(PAGE_SIZE as u64);
    let mut distinct_row_pages: Vec<u32> = page_ids;
    distinct_row_pages.sort_unstable();
    distinct_row_pages.dedup();
    let fan_pages: Vec<u32> = distinct_row_pages.iter().copied().take(16).collect();
    if fan_pages.is_empty() {
        return Err("fixture produced no row pages".into());
    }
    let fan_trace: Vec<u32> = (0..fanout)
        .map(|index| fan_pages[index % fan_pages.len()])
        .collect();
    let mut random = (0..page_count).collect::<Vec<_>>();
    shuffle(&mut random);
    random.truncate(random_pages.min(page_count as usize));
    let sequential: Vec<u32> = (0..page_count).collect();

    let mut observations = Vec::new();
    let run = |name: &str, pages: &[u32], depth: usize, cold: bool| {
        measure(Measure {
            disk: &disk,
            heap_path: &heap_path,
            fingerprints: &fingerprints,
            name,
            pages,
            depth,
            cold,
            samples,
        })
    };
    // The fingerprint pass left the file resident. These two runs are the warm
    // control: physical read bytes should stay at zero.
    for (name, pages) in [
        ("sequential_distinct", &sequential),
        ("random_distinct", &random),
    ] {
        observations.push(run(name, pages, 1, false)?);
    }
    for (name, pages) in [
        ("sequential_distinct", &sequential),
        ("random_distinct", &random),
    ] {
        for &depth in &depths {
            observations.push(run(name, pages, depth, true)?);
        }
    }
    observations.push(run("fanout_serial_reread", &fan_trace, 1, true)?);
    observations.push(run("fanout_packet", &fan_pages, 1, true)?);

    let report = json!({
        "profile": "cold-buffered-page-schedule-v1",
        "measured_on": "2026-11-27",
        "production_integration": true,
        "cold_nvme_claim": "scoped to this workstation, this file, and buffered pread",
        "method": {
            "eviction": "posix_fadvise POSIX_FADV_DONTNEED on the heap inode before each cold sample",
            "drop_caches": false,
            "o_direct": false,
            "io_uring": false,
            "queue_depth": "sequential_distinct at every depth, including depth 1: powdb_storage::disk::read_pages_verified. A contiguous run longer than 32 pages is read_page per page with Page::verify_bytes. Shorter runs are one pread. random_distinct depth 1, fanout_serial_reread, and fanout_packet: serial DiskManager::read_page. random_distinct depth >1: read_pages_verified",
            "heap_reopened_after_eviction": false,
            "checksum": "read_pages_verified checks with Page::verify_bytes in place. The read_page path still uses Page::from_bytes_verified",
            "why_not_heap_open": "HeapFile::open reads every page while discovering free space",
        },
        "host": {
            "cpu": cpu_model(),
            "mem_total_bytes": mem_total_bytes(),
            "filesystem": fixture_filesystem,
            "exclusive_hardware": false,
        },
        "fixture": {
            "btrfs_compression": if compression_disabled { "none" } else { "filesystem default" },
            "rows": rows,
            "payload_bytes": payload_bytes,
            "payload_pattern": "deterministic xorshift bytes, not ASCII",
            "heap_pages": page_count,
            "heap_bytes": heap_bytes,
            "distinct_row_pages": distinct_row_pages.len(),
            "random_pages": random.len(),
            "fanout_requests": fan_trace.len(),
            "fanout_distinct_pages": fan_pages.len(),
            "samples": samples,
            "depths": depths,
        },
        "observations": observations,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

struct Measure<'a> {
    disk: &'a DiskManager,
    heap_path: &'a Path,
    fingerprints: &'a [u64],
    name: &'a str,
    pages: &'a [u32],
    depth: usize,
    cold: bool,
    samples: usize,
}

fn measure(job: Measure<'_>) -> io::Result<Json> {
    let Measure {
        disk,
        heap_path,
        fingerprints,
        name,
        pages,
        depth,
        cold,
        samples,
    } = job;
    let expected = fingerprint_sum(fingerprints, pages);
    // Sequential pages are 0..n. Depth 1 of that case has to use the
    // coalesced reader or S1 times the one-pread path the change did not touch.
    // Random depth 1 and both fanout cases stay on read_page so R1's 4×
    // baseline and F1's serial reread stay one pread per request.
    let coalesce = name == "sequential_distinct" || depth > 1;
    let distinct = {
        let mut ids = pages.to_vec();
        ids.sort_unstable();
        ids.dedup();
        ids.len()
    };
    let mut timings = Vec::with_capacity(samples);
    let mut read_bytes = Vec::with_capacity(samples);
    let mut device_bytes = Vec::with_capacity(samples);
    for _ in 0..samples {
        if cold {
            evict_cache(heap_path)?;
        }
        let before_proc = physical_read_bytes();
        let before_device = disk_read_bytes();
        let start = Instant::now();
        let got = read_pages(disk, pages, depth, coalesce)?;
        let elapsed_us = start.elapsed().as_secs_f64() * 1e6;
        let after_proc = physical_read_bytes();
        let after_device = disk_read_bytes();
        if got != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{name} q{depth} cold={cold} fingerprint mismatch"),
            ));
        }
        let proc_delta = before_proc
            .zip(after_proc)
            .map(|(before, after)| after.saturating_sub(before));
        if cold && proc_delta == Some(0) {
            return Err(io::Error::other(format!(
                "{name} q{depth} cold sample moved no /proc/self/io read_bytes"
            )));
        }
        timings.push(elapsed_us);
        read_bytes.push(proc_delta);
        device_bytes.push(device_delta(&before_device, &after_device));
    }
    let mut sorted = timings.clone();
    sorted.sort_by(f64::total_cmp);
    Ok(json!({
        "case": name,
        "queue_depth": depth,
        "reader": if coalesce { "read_pages_verified" } else { "read_page" },
        "cold": cold,
        "requested_reads": pages.len(),
        "distinct_pages": distinct,
        "logical_bytes": (pages.len() as u64).saturating_mul(PAGE_SIZE as u64),
        "samples_us": timings,
        "min_us": sorted.first().copied(),
        "p50_us": percentile(&sorted, 0.50),
        "p95_us": percentile(&sorted, 0.95),
        "max_us": sorted.last().copied(),
        "physical_read_bytes": read_bytes,
        "device_read_bytes": device_bytes,
        "output_verified": true,
    }))
}

fn read_pages(disk: &DiskManager, pages: &[u32], depth: usize, coalesce: bool) -> io::Result<u64> {
    if depth <= 1 && !coalesce {
        let mut sum = 0u64;
        for &page_id in pages {
            sum = sum.wrapping_add(consume_page(disk, page_id)?);
        }
        return Ok(sum);
    }
    let verified = read_pages_verified(disk, pages, depth)?;
    Ok(verified
        .iter()
        .fold(0u64, |sum, bytes| sum.wrapping_add(fingerprint(bytes))))
}

fn consume_page(disk: &DiskManager, page_id: u32) -> io::Result<u64> {
    let bytes = disk.read_page(page_id)?;
    let page = Page::from_bytes_verified(&bytes).map_err(io::Error::from)?;
    Ok(fingerprint(page.as_bytes()))
}

fn fingerprints(disk: &DiskManager) -> io::Result<Vec<u64>> {
    let mut out = Vec::with_capacity(disk.num_pages() as usize);
    for page_id in 0..disk.num_pages() {
        out.push(consume_page(disk, page_id)?);
    }
    Ok(out)
}

fn fingerprint_sum(fingerprints: &[u64], pages: &[u32]) -> u64 {
    pages.iter().fold(0u64, |sum, page_id| {
        sum.wrapping_add(fingerprints[*page_id as usize])
    })
}

fn fingerprint(bytes: &[u8; PAGE_SIZE]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for chunk in bytes.as_chunks::<8>().0 {
        hash ^= u64::from_le_bytes(*chunk);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn evict_cache(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let file = File::open(path)?;
        // Safety: the descriptor is a live file we just opened, and the advice
        // covers the whole file (offset 0, len 0). It only drops clean cache pages.
        let advise = |file: &File| -> io::Result<()> {
            let rc =
                unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
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
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "POSIX_FADV_DONTNEED is required to evict the file cache",
        ))
    }
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

fn shuffle(values: &mut [u32]) {
    let mut seed = 0x98bc_724a_d512_6e03u64;
    for index in (1..values.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        values.swap(index, (seed % (index as u64 + 1)) as usize);
    }
}

fn percentile(sorted: &[f64], quantile: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((sorted.len() - 1) as f64 * quantile).round();
    let index = usize::try_from(rank as u64).unwrap_or(0);
    sorted.get(index.min(sorted.len() - 1)).copied()
}

fn parameter(name: &str, default: usize) -> Result<usize, Box<dyn Error>> {
    match std::env::var(name) {
        Ok(value) => Ok(value.parse()?),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
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

fn filesystem(path: &Path) -> Option<String> {
    let result = std::process::Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", "-T"])
        .arg(path)
        .output()
        .ok()?;
    result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn cpu_model() -> Option<String> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    text.lines().find_map(|line| {
        let rest = line.strip_prefix("model name")?;
        rest.split(':').nth(1).map(|value| value.trim().to_string())
    })
}

fn mem_total_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kilobytes = text
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    Some(kilobytes.saturating_mul(1024))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn evicted_reread_matches_the_verified_fingerprint() {
        let dir = tempfile::tempdir().expect("fixture");
        let heap_path = dir.path().join("Gather.heap");
        {
            let mut engine = Engine::new(dir.path()).expect("engine");
            engine
                .execute_powql("type Gather { required id: int, payload: bytes }")
                .expect("schema");
            {
                let table = engine.catalog_mut().get_table_mut("Gather").expect("table");
                for id in 0..32_i64 {
                    table
                        .insert(&vec![
                            Value::Int(id),
                            Value::Bytes(incompressible(id as usize, 128)),
                        ])
                        .expect("insert");
                }
            }
            engine.catalog_mut().checkpoint().expect("checkpoint");
        }
        let disk = DiskManager::open_read_only(&heap_path).expect("disk");
        let fingerprints = fingerprints(&disk).expect("fingerprint");
        let pages: Vec<u32> = (0..disk.num_pages()).collect();
        let expected = fingerprint_sum(&fingerprints, &pages);
        evict_cache(&heap_path).expect("evict");
        let got = read_pages(&disk, &pages, 2, true).expect("read");
        assert_eq!(got, expected);
        let coalesced = read_pages(&disk, &pages, 1, true).expect("coalesced depth 1");
        assert_eq!(coalesced, expected);
    }
}
