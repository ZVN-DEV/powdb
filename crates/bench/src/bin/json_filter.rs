//! JSON-path-filter workload (design doc 2026-07-13, section 4.4 + bench B2).
//!
//! Measures the v0.12 compiled inline-document JSON leaf against the paths it is
//! meant to beat, on a 100K-row table of ~1KB JSON documents with a ~10%
//! selective `status = "live"` filter:
//!
//!   (a) compiled inline leaf   `Post filter .data->status = "live"`   (leaf fires)
//!   (b) fallback decode, SAME  `Post filter (.data->status = "live" or .id < 0)`
//!       inline data            — the `or` makes `compile_predicate` decline, so
//!                                the query runs the generic decode + `pj1_scalarize`
//!                                + `eval_binop` path over the IDENTICAL rows
//!                                (`.id < 0` is never true, so the result set is
//!                                identical). This isolates the leaf's speedup.
//!   (c) flat-column ceiling    `Post filter .status = "live"`         (compiled StrEq)
//!   (d) out-of-line / spilled  same path filter on >4070B docs that spill into
//!       overflow pages — the reassemble-then-walk path (design B2's out-of-line
//!       case), which `table_has_overflow` routes to the decoded reader.
//!
//! Honesty rule: this prints only measured numbers from the machine it runs on.
//! It does NOT feed baseline/main.json (Depot-only policy). Numbers are laptop
//! numbers unless run on the Depot runner.

use powdb_query::executor::Engine;
use powdb_storage::pj1::parse_json_text;
use powdb_storage::types::*;
use powdb_storage::wal::WalSyncMode;
use std::path::PathBuf;
use std::time::Instant;

/// A throwaway temp directory that removes itself on drop (this bin has no
/// dev-dependency on `tempfile`, and a data dir must outlive the `Engine`'s
/// mmap pointers). Drop order matters: keep the guard alive alongside the engine.
struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "powdb_jsonbench_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const N_ROWS: usize = 100_000;
/// Filter selectivity target: 1 in 10 rows has status "live".
const LIVE_EVERY: usize = 10;
/// Repetitions per measured query (each is a full 100K-row scan).
const REPS: usize = 20;

/// Build a ~`target_len`-byte JSON document string carrying `status`, a couple
/// of scalar fields, and a filler `payload` string that pads to the target.
fn make_doc(i: usize, status: &str, target_len: usize) -> String {
    // Base fields (without filler). Keep them small and realistic.
    let head = format!(
        r#"{{"status":"{status}","id":{i},"name":"user_{i}","score":{},"active":{},"payload":""#,
        (i % 100) as f64 / 10.0,
        i.is_multiple_of(2)
    );
    let tail = r#""}"#;
    let filler_len = target_len.saturating_sub(head.len() + tail.len());
    let mut s = String::with_capacity(target_len + 8);
    s.push_str(&head);
    // ASCII filler, no JSON-special chars.
    for _ in 0..filler_len {
        s.push('x');
    }
    s.push_str(tail);
    s
}

fn status_for(i: usize) -> &'static str {
    if i.is_multiple_of(LIVE_EVERY) {
        "live"
    } else {
        match i % 3 {
            0 => "active",
            1 => "inactive",
            _ => "pending",
        }
    }
}

/// Build a `Post { id:int, status:str, data:json }` table with `n` rows whose
/// json docs are ~`doc_len` bytes. Direct `table.insert` bypasses parse/plan so
/// the fixture builds fast. WAL fsync off (bench convention).
fn setup(n: usize, doc_len: usize) -> (Engine, TempDir) {
    let tmp = TempDir::new();
    let mut engine = Engine::new(tmp.path()).expect("engine");
    engine.catalog_mut().set_wal_sync_mode(WalSyncMode::Off);
    engine
        .execute_powql("type Post { required id: int, required status: str, data: json }")
        .expect("create type");
    {
        let table = engine
            .catalog_mut()
            .get_table_mut("Post")
            .expect("get Post");
        for i in 0..n {
            let status = status_for(i);
            let doc = make_doc(i, status, doc_len);
            let pj1 = parse_json_text(&doc).expect("valid json");
            let row = vec![
                Value::Int(i as i64),
                Value::Str(status.to_string()),
                Value::Json(pj1.into_boxed_slice()),
            ];
            table.insert(&row).expect("insert");
        }
    }
    (engine, tmp)
}

/// Run `q` `REPS` times, returning (rows_matched, median_ms, mean_ms).
fn measure(engine: &mut Engine, q: &str) -> (usize, f64, f64) {
    let mut times = Vec::with_capacity(REPS);
    let mut matched = 0usize;
    for _ in 0..REPS {
        let t0 = Instant::now();
        let res = engine.execute_powql(q).expect("query ok");
        let dt = t0.elapsed();
        matched = match res {
            powdb_query::result::QueryResult::Rows { rows, .. } => rows.len(),
            other => panic!("expected rows, got {other:?}"),
        };
        std::hint::black_box(&matched);
        times.push(dt.as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = times[times.len() / 2];
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    (matched, median, mean)
}

fn report(label: &str, matched: usize, median_ms: f64, mean_ms: f64) {
    let rows_per_s = N_ROWS as f64 / (median_ms / 1000.0);
    println!(
        "{label:<44} matched={matched:>6}  median={median_ms:>8.2} ms  mean={mean_ms:>8.2} ms  ({:.1} M rows/s)",
        rows_per_s / 1_000_000.0
    );
}

fn main() {
    println!(
        "JSON-path-filter workload — {N_ROWS} rows, ~10% selective, {REPS} reps each\n\
         (laptop numbers; not a Depot baseline)\n"
    );

    // Inline docs (~1KB): the leaf's home turf.
    println!("== inline ~1KB documents ==");
    let (mut engine, _g) = setup(N_ROWS, 1024);
    let (m_a, med_a, mean_a) =
        measure(&mut engine, r#"Post filter .data->status = "live" { .id }"#);
    report("(a) compiled inline JSON leaf", m_a, med_a, mean_a);

    let (m_b, med_b, mean_b) = measure(
        &mut engine,
        r#"Post filter (.data->status = "live" or .id < 0) { .id }"#,
    );
    report("(b) decode fallback (same inline data)", m_b, med_b, mean_b);

    let (m_c, med_c, mean_c) = measure(&mut engine, r#"Post filter .status = "live" { .id }"#);
    report("(c) flat str column (ceiling)", m_c, med_c, mean_c);

    assert_eq!(m_a, m_b, "compiled and fallback must match the same rows");
    assert_eq!(
        m_a, m_c,
        "flat-column selectivity must match the json filter"
    );
    let speedup = med_b / med_a;
    println!(
        "    -> compiled leaf is {speedup:.2}x faster than the decode fallback on identical data"
    );
    let vs_flat = med_a / med_c;
    println!("    -> compiled leaf is {vs_flat:.2}x the cost of the flat-column ceiling\n");

    // Spilled docs (>4070B): out-of-line reassemble-then-walk (design B2).
    println!("== out-of-line documents (>4070B, spilled to overflow) ==");
    let (mut engine2, _g2) = setup(N_ROWS, 5000);
    let (m_d, med_d, mean_d) = measure(
        &mut engine2,
        r#"Post filter .data->status = "live" { .id }"#,
    );
    report("(d) spilled decode + reassemble walk", m_d, med_d, mean_d);
    assert_eq!(m_d, m_a, "spilled table must match the same rows");
    println!(
        "    -> out-of-line path is {:.2}x the inline compiled-leaf latency (design target: within a small multiple)\n",
        med_d / med_a
    );
}
