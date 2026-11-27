//! Cold comparison of `HeapFile::scan` with one `read_pages_verified` of every page.
//!
//! The heap matches `cold-page-probe`: 16384 rows of 2048 incompressible bytes,
//! then a checkpoint. `HeapFile::open` reads every page, so the file is evicted
//! with `posix_fadvise(POSIX_FADV_DONTNEED)` after open and again before the
//! batch. The heap is not reopened after eviction.
//!
//! No `drop_caches`, `O_DIRECT`, or `io_uring`. The only advice is `DONTNEED`.
//!
//! ```text
//! TMPDIR=/path/on/disk cargo run --release -p powdb-bench --bin scan-compare
//! ```
//!
//! Optional: `POWDB_COLD_ROWS` (16384), `POWDB_COLD_PAYLOAD` (2048).
//! A tmpfs fixture is refused. `tempfile::tempdir` follows `TMPDIR`.

use std::error::Error;
use std::fs::File;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use powdb_query::executor::Engine;
use powdb_storage::disk::{read_pages_verified, DiskManager};
use powdb_storage::heap::HeapFile;
use powdb_storage::page::PAGE_SIZE;
use powdb_storage::types::Value;
use serde_json::json;

const DEFAULT_ROWS: usize = 16_384;
const DEFAULT_PAYLOAD: usize = 2_048;

fn main() -> Result<(), Box<dyn Error>> {
    let rows = parameter("POWDB_COLD_ROWS", DEFAULT_ROWS)?;
    let payload_bytes = parameter("POWDB_COLD_PAYLOAD", DEFAULT_PAYLOAD)?;
    if rows < 32 || rows > i64::MAX as usize || !(64..=3_000).contains(&payload_bytes) {
        return Err("invalid scan-compare settings".into());
    }

    let dir = tempfile::tempdir()?;
    disable_btrfs_compression(dir.path());
    let filesystem = filesystem(dir.path());
    if filesystem.as_deref() == Some("tmpfs") {
        return Err(
            "fixture is on tmpfs; set TMPDIR to the Btrfs volume that backs the NVMe device".into(),
        );
    }

    let heap_path = dir.path().join("Gather.heap");
    {
        let mut engine = Engine::new(dir.path())?;
        engine.execute_powql("type Gather { required id: int, payload: bytes }")?;
        {
            let table = engine
                .catalog_mut()
                .get_table_mut("Gather")
                .ok_or("missing Gather table")?;
            for id in 0..rows {
                table.insert(&vec![
                    Value::Int(id as i64),
                    Value::Bytes(incompressible(id, payload_bytes)),
                ])?;
            }
        }
        engine.catalog_mut().checkpoint()?;
    }

    let heap = HeapFile::open(&heap_path)?;
    let heap_pages = heap.num_pages();
    if heap_pages == 0 {
        return Err("heap has no pages".into());
    }
    let heap_bytes = (heap_pages as u64).saturating_mul(PAGE_SIZE as u64);

    evict_cache(&heap_path)?;
    let (scan_us, scan_read_bytes, scan_rows) = time_scan(&heap)?;
    if scan_rows != rows as u64 {
        return Err(format!("scan returned {scan_rows} rows, expected {rows}").into());
    }

    // `HeapFile` does not expose its disk. This handle reads the same path.
    let disk = DiskManager::open_read_only(&heap_path)?;
    if disk.num_pages() != heap_pages {
        return Err("heap page count changed after open".into());
    }
    evict_cache(&heap_path)?;
    let (batch_us, batch_read_bytes) = time_batch(&disk)?;

    let report = json!({
        "filesystem": filesystem,
        "heap_bytes": heap_bytes,
        "scan_us": scan_us,
        "scan_read_bytes": scan_read_bytes,
        "batch_us": batch_us,
        "batch_read_bytes": batch_read_bytes,
        "scan_rows": scan_rows,
    });
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn time_scan(heap: &HeapFile) -> io::Result<(u64, u64, u64)> {
    let before = physical_read_bytes();
    let start = Instant::now();
    let mut scan_rows = 0u64;
    let mut byte_sum = 0u64;
    for row in heap.scan() {
        let (_rid, bytes) = row?;
        scan_rows += 1;
        byte_sum = byte_sum.wrapping_add(bytes.len() as u64);
    }
    std::hint::black_box(byte_sum);
    let scan_us = elapsed_us(start);
    let after = physical_read_bytes();
    let scan_read_bytes = moved_bytes(before, after, "heap scan")?;
    Ok((scan_us, scan_read_bytes, scan_rows))
}

fn time_batch(disk: &DiskManager) -> io::Result<(u64, u64)> {
    let page_ids: Vec<u32> = (0..disk.num_pages()).collect();
    let before = physical_read_bytes();
    let start = Instant::now();
    let pages = read_pages_verified(disk, &page_ids, 1)?;
    // Already verified inside `read_pages_verified`. One byte per page keeps
    // the read live without a second checksum.
    let folded = pages
        .iter()
        .fold(0u8, |sum, page| sum.wrapping_add(page[0]));
    std::hint::black_box(folded);
    let batch_us = elapsed_us(start);
    let after = physical_read_bytes();
    let batch_read_bytes = moved_bytes(before, after, "batched read")?;
    Ok((batch_us, batch_read_bytes))
}

fn elapsed_us(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn moved_bytes(before: Option<u64>, after: Option<u64>, label: &str) -> io::Result<u64> {
    let Some((before, after)) = before.zip(after) else {
        return Err(io::Error::other(format!(
            "{label} could not read /proc/self/io read_bytes"
        )));
    };
    let delta = after.saturating_sub(before);
    if delta == 0 {
        return Err(io::Error::other(format!(
            "{label} moved no /proc/self/io read_bytes"
        )));
    }
    Ok(delta)
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

fn parameter(name: &str, default: usize) -> Result<usize, Box<dyn Error>> {
    match std::env::var(name) {
        Ok(value) => Ok(value.parse()?),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn disable_btrfs_compression(path: &Path) {
    // On ext4 this prints an error and continues. Failure is not fatal.
    let _ = std::process::Command::new("btrfs")
        .args(["property", "set"])
        .arg(path)
        .args(["compression", "none"])
        .status();
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
