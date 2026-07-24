//! Measures the durable-write speedup from batching inserts in a transaction.
//!
//! The README, `docs/POWQL.md`, and the site all tell you to wrap bulk loads in
//! `begin` / `commit` because it collapses one fsync-per-row into one fsync for
//! the whole batch. This is the repro for that claim, so the number is
//! measured rather than asserted.
//!
//! Both legs run in `WalSyncMode::Full`, which is what the embedded `Engine`
//! and `powdb-server` use by default: every commit `fdatasync`s before
//! returning. That is the whole point. Running this in `WalSyncMode::Off` would
//! measure nothing, because there would be no fsync to amortize.
//!
//! ```bash
//! cargo run --release -p powdb-compare --example write_batching
//! ```
//!
//! The ratio you get is a property of **your disk**, not of PowDB: it is
//! roughly your fsync rate multiplied by how many rows you put in one
//! transaction. A slow spinning disk shows a far bigger speedup than a fast
//! NVMe drive. Measure your own hardware before quoting a number.

use std::time::Instant;

use powdb_query::executor::Engine;
use powdb_storage::wal::WalSyncMode;

/// Rows per leg. Kept small because the autocommit leg pays one fsync per row
/// and is genuinely slow on a durable disk.
const ROWS: usize = 500;

fn new_engine(dir: &std::path::Path) -> Engine {
    let mut engine = Engine::new(dir).expect("engine init");
    // Durable on purpose. This is the default the server and embedded engine
    // both ship with, and the only mode in which this measurement means
    // anything.
    engine.catalog_mut().set_wal_sync_mode(WalSyncMode::Full);
    engine
        .execute_powql("type W { required id: int, required name: str }")
        .expect("create type");
    engine
}

fn main() {
    let autocommit_dir = tempfile::tempdir().expect("tempdir");
    let mut engine = new_engine(autocommit_dir.path());

    let start = Instant::now();
    for i in 0..ROWS {
        engine
            .execute_powql(&format!("insert W {{ id := {i}, name := \"row\" }}"))
            .expect("autocommit insert");
    }
    let autocommit = start.elapsed();

    let batched_dir = tempfile::tempdir().expect("tempdir");
    let mut engine = new_engine(batched_dir.path());

    let start = Instant::now();
    engine.execute_powql("begin").expect("begin");
    for i in 0..ROWS {
        engine
            .execute_powql(&format!("insert W {{ id := {i}, name := \"row\" }}"))
            .expect("batched insert");
    }
    engine.execute_powql("commit").expect("commit");
    let batched = start.elapsed();

    let autocommit_rps = ROWS as f64 / autocommit.as_secs_f64();
    let batched_rps = ROWS as f64 / batched.as_secs_f64();

    println!("PowDB durable write batching ({ROWS} rows, WalSyncMode::Full)\n");
    println!(
        "  autocommit (one fsync per row):   {:>9.1} rows/sec  ({:.3}s total)",
        autocommit_rps,
        autocommit.as_secs_f64()
    );
    println!(
        "  one transaction (one fsync):      {:>9.1} rows/sec  ({:.3}s total)",
        batched_rps,
        batched.as_secs_f64()
    );
    println!("\n  speedup: {:.1}x", batched_rps / autocommit_rps);
    println!(
        "\nThis ratio is a property of your disk's fsync rate and your batch\n\
         size, not a fixed property of PowDB. Both legs are equally durable."
    );
}
