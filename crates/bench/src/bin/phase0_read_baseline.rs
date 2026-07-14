//! Reproducible remote-read baseline for the next read-performance cycle.
//!
//! This runner intentionally measures the currently shipped paths without
//! changing them. It compares equivalent ~2.5 KiB document and typed rows over
//! the real binary TCP protocol, then measures document-read throughput at
//! 1/2/5/10 concurrent client connections.
//!
//! Run:
//!   cargo run --release -p powdb-bench --bin phase0-read-baseline
//!
//! Optional bounds:
//!   POWDB_BASELINE_SAMPLES=500
//!   POWDB_BASELINE_WINDOW_MS=2000

use powdb_query::executor::Engine;
use powdb_server::handler::{
    handle_connection, new_tx_gate_with_permits, ConnOpts, DEFAULT_TX_GATE_READER_PERMITS,
};
use powdb_server::metrics::Metrics;
use powdb_server::protocol::Message;
use powdb_storage::pj1::parse_json_text;
use powdb_storage::types::Value;
use powdb_storage::wal::WalSyncMode;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

const DOCUMENT_BYTES: usize = 2_560;
const DEFAULT_SAMPLES: usize = 500;
const DEFAULT_WINDOW_MS: u64 = 2_000;
const CLIENT_COUNTS: [usize; 4] = [1, 2, 5, 10];
const OVERLAP_ROWS: usize = 10_000;
const OVERLAP_PAYLOAD_BYTES: usize = 256;
const DOCUMENT_QUERY: &str = "DocumentRow filter .id = 1 { .data }";
const TYPED_QUERY: &str =
    "TypedRow filter .id = 1 { .id, .status, .title, .score, .active, .payload }";
const OVERLAP_QUERY: &str = "ScanRow filter .bucket >= 0 { .id, .payload }";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "powdb_read_baseline_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create benchmark data directory");
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

struct BenchServer {
    addr: std::net::SocketAddr,
    shutdown: watch::Sender<bool>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl BenchServer {
    fn stop(self) {
        let _ = self.shutdown.send(true);
        self.accept_task.abort();
    }
}

struct BenchClient {
    stream: TcpStream,
}

struct RowResponse {
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
    encoded_bytes: usize,
}

impl BenchClient {
    async fn connect(addr: std::net::SocketAddr) -> Self {
        let mut stream = TcpStream::connect(addr)
            .await
            .expect("connect benchmark client");
        Message::Connect {
            db_name: "default".into(),
            password: None,
            username: None,
        }
        .write_to(&mut stream)
        .await
        .expect("write CONNECT");
        match Message::read_from(&mut stream)
            .await
            .expect("read CONNECT reply")
        {
            Some(Message::ConnectOk { .. }) => Self { stream },
            other => panic!("expected CONNECT_OK, got {other:?}"),
        }
    }

    async fn query(&mut self, query: &str) -> RowResponse {
        Message::Query {
            query: query.into(),
        }
        .write_to(&mut self.stream)
        .await
        .expect("write query");
        let message = Message::read_from(&mut self.stream)
            .await
            .expect("read query response")
            .expect("server closed before query response");
        let encoded_bytes = message.encode().len();
        match message {
            Message::ResultRows { columns, rows } => RowResponse {
                columns,
                rows,
                encoded_bytes,
            },
            Message::Error { message } => panic!("query failed: {message}"),
            other => panic!("expected row result, got {other:?}"),
        }
    }
}

#[derive(Clone, Copy)]
struct LatencyStats {
    count: usize,
    mean_us: f64,
    stddev_us: f64,
    min_us: f64,
    p50_us: f64,
    p90_us: f64,
    p95_us: f64,
    p99_us: f64,
    max_us: f64,
}

fn latency_stats(samples: &[Duration]) -> Option<LatencyStats> {
    if samples.is_empty() {
        return None;
    }
    let mut samples = samples.to_vec();
    samples.sort_unstable();
    let percentile = |pct: usize| {
        let index = ((samples.len() - 1) * pct) / 100;
        samples[index].as_secs_f64() * 1_000_000.0
    };
    let mean_us =
        samples.iter().map(Duration::as_secs_f64).sum::<f64>() * 1_000_000.0 / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| {
            let delta = sample.as_secs_f64() * 1_000_000.0 - mean_us;
            delta * delta
        })
        .sum::<f64>()
        / samples.len() as f64;
    Some(LatencyStats {
        count: samples.len(),
        mean_us,
        stddev_us: variance.sqrt(),
        min_us: percentile(0),
        p50_us: percentile(50),
        p90_us: percentile(90),
        p95_us: percentile(95),
        p99_us: percentile(99),
        max_us: percentile(100),
    })
}

fn make_document(target_len: usize) -> (String, String) {
    let head = r#"{"id":1,"status":"active","title":"synthetic record","score":42.5,"active":true,"payload":""#;
    let tail = r#""}"#;
    let filler_len = target_len.saturating_sub(head.len() + tail.len());
    let filler = "x".repeat(filler_len);
    let document = format!("{head}{filler}{tail}");
    assert_eq!(document.len(), target_len);
    (document, filler)
}

fn seed_engine(path: &Path) -> Engine {
    let mut engine = Engine::new(path).expect("create benchmark engine");
    engine.catalog_mut().set_wal_sync_mode(WalSyncMode::Off);
    engine
        .execute_powql("type DocumentRow { required unique id: int, data: json }")
        .expect("create document table");
    engine
        .execute_powql(
            "type TypedRow { required unique id: int, status: str, title: str, \
             score: float, active: bool, payload: str }",
        )
        .expect("create typed table");
    engine
        .execute_powql(
            "type ScanRow { required unique id: int, required bucket: int, payload: str }",
        )
        .expect("create overlap table");

    let (document, filler) = make_document(DOCUMENT_BYTES);
    let pj1 = parse_json_text(&document).expect("parse synthetic document");
    let document_row = vec![Value::Int(1), Value::Json(pj1.into_boxed_slice())];
    engine
        .catalog_mut()
        .get_table_mut("DocumentRow")
        .expect("document table")
        .insert(&document_row)
        .expect("insert document row");
    let typed_row = vec![
        Value::Int(1),
        Value::Str("active".into()),
        Value::Str("synthetic record".into()),
        Value::Float(42.5),
        Value::Bool(true),
        Value::Str(filler),
    ];
    engine
        .catalog_mut()
        .get_table_mut("TypedRow")
        .expect("typed table")
        .insert(&typed_row)
        .expect("insert typed row");
    {
        let table = engine
            .catalog_mut()
            .get_table_mut("ScanRow")
            .expect("overlap table");
        let payload = "s".repeat(OVERLAP_PAYLOAD_BYTES);
        for id in 0..OVERLAP_ROWS {
            table
                .insert(&vec![
                    Value::Int(id as i64),
                    Value::Int((id % 10) as i64),
                    Value::Str(payload.clone()),
                ])
                .expect("insert overlap row");
        }
    }

    // Fail before timing if either indexed point query is malformed or the
    // fixture is not visible through the normal executor.
    engine
        .execute_powql(DOCUMENT_QUERY)
        .expect("validate document point query");
    engine
        .execute_powql(TYPED_QUERY)
        .expect("validate typed point query");
    engine
        .execute_powql(OVERLAP_QUERY)
        .expect("validate overlap query");
    engine
}

async fn start_server(engine: Engine, gate_permits: u32) -> BenchServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind benchmark listener");
    let addr = listener.local_addr().expect("benchmark listener address");
    let engine = Arc::new(RwLock::new(engine));
    let tx_gate = new_tx_gate_with_permits(gate_permits);
    let metrics = Arc::new(Metrics::new());
    let users = Arc::new(powdb_auth::UserStore::new());
    let (shutdown, mut accept_shutdown) = watch::channel(false);

    let accept_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(pair) => pair,
                        Err(_) => break,
                    };
                    let engine = engine.clone();
                    let tx_gate = tx_gate.clone();
                    let metrics = metrics.clone();
                    let users = users.clone();
                    let mut connection_shutdown = accept_shutdown.clone();
                    tokio::spawn(async move {
                        handle_connection(
                            stream,
                            ConnOpts {
                                engine,
                                tx_gate,
                                expected_password: None,
                                users,
                                shutdown_rx: &mut connection_shutdown,
                                idle_timeout: Duration::from_secs(60),
                                query_timeout: Duration::from_secs(30),
                                rate_limiter: None,
                                peer_addr: Some(peer),
                                metrics,
                                tx_wait_timeout: Duration::from_secs(30),
                                db_name: None,
                            },
                        )
                        .await;
                    });
                }
                changed = accept_shutdown.changed() => {
                    if changed.is_err() || *accept_shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });

    BenchServer {
        addr,
        shutdown,
        accept_task,
    }
}

fn assert_point_equivalence(document: &RowResponse, typed: &RowResponse) {
    assert_eq!(document.columns, ["data"]);
    assert_eq!(document.rows.len(), 1);
    assert_eq!(typed.rows.len(), 1);
    let document_json: serde_json::Value =
        serde_json::from_str(&document.rows[0][0]).expect("canonical document JSON");
    let typed_json = serde_json::json!({
        "id": typed.rows[0][0].parse::<i64>().expect("typed id"),
        "status": typed.rows[0][1],
        "title": typed.rows[0][2],
        "score": typed.rows[0][3].parse::<f64>().expect("typed score"),
        "active": typed.rows[0][4].parse::<bool>().expect("typed active"),
        "payload": typed.rows[0][5],
    });
    assert_eq!(
        document_json, typed_json,
        "document and typed fixtures must be logically equivalent"
    );
}

struct PointMeasurement {
    document_stats: LatencyStats,
    typed_stats: LatencyStats,
    document_raw_us: Vec<f64>,
    typed_raw_us: Vec<f64>,
    document_response_bytes: usize,
    typed_response_bytes: usize,
}

async fn measure_point_reads(addr: std::net::SocketAddr, samples: usize) -> PointMeasurement {
    let mut client = BenchClient::connect(addr).await;
    let document_shape = client.query(DOCUMENT_QUERY).await;
    let typed_shape = client.query(TYPED_QUERY).await;
    assert_point_equivalence(&document_shape, &typed_shape);
    for _ in 0..50 {
        black_box(client.query(DOCUMENT_QUERY).await.encoded_bytes);
        black_box(client.query(TYPED_QUERY).await.encoded_bytes);
    }

    let mut document = Vec::with_capacity(samples);
    let mut typed = Vec::with_capacity(samples);
    for i in 0..samples {
        if i.is_multiple_of(2) {
            let started = Instant::now();
            let response = client.query(DOCUMENT_QUERY).await;
            document.push(started.elapsed());
            assert_eq!(response.encoded_bytes, document_shape.encoded_bytes);
            black_box(response.encoded_bytes);
            let started = Instant::now();
            let response = client.query(TYPED_QUERY).await;
            typed.push(started.elapsed());
            assert_eq!(response.encoded_bytes, typed_shape.encoded_bytes);
            black_box(response.encoded_bytes);
        } else {
            let started = Instant::now();
            let response = client.query(TYPED_QUERY).await;
            typed.push(started.elapsed());
            assert_eq!(response.encoded_bytes, typed_shape.encoded_bytes);
            black_box(response.encoded_bytes);
            let started = Instant::now();
            let response = client.query(DOCUMENT_QUERY).await;
            document.push(started.elapsed());
            assert_eq!(response.encoded_bytes, document_shape.encoded_bytes);
            black_box(response.encoded_bytes);
        }
    }
    PointMeasurement {
        document_stats: latency_stats(&document).expect("document point latency samples"),
        typed_stats: latency_stats(&typed).expect("typed point latency samples"),
        document_raw_us: document
            .iter()
            .map(|sample| sample.as_secs_f64() * 1_000_000.0)
            .collect(),
        typed_raw_us: typed
            .iter()
            .map(|sample| sample.as_secs_f64() * 1_000_000.0)
            .collect(),
        document_response_bytes: document_shape.encoded_bytes,
        typed_response_bytes: typed_shape.encoded_bytes,
    }
}

struct ConcurrencyResult {
    clients: usize,
    operations: usize,
    window: Duration,
    drain_time: Duration,
    latency: Option<LatencyStats>,
    encoded_response_bytes: usize,
    process_cpu_seconds: Option<f64>,
    process_cpu_utilization_pct: Option<f64>,
}

#[cfg(unix)]
fn process_cpu_seconds() -> Option<f64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `getrusage` initializes the provided rusage on success. The
    // pointer is valid for writes and is only assumed initialized after a
    // zero return code.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: guarded by the successful `getrusage` return above.
    let usage = unsafe { usage.assume_init() };
    let timeval_seconds =
        |value: libc::timeval| value.tv_sec as f64 + value.tv_usec as f64 / 1_000_000.0;
    Some(timeval_seconds(usage.ru_utime) + timeval_seconds(usage.ru_stime))
}

#[cfg(not(unix))]
fn process_cpu_seconds() -> Option<f64> {
    None
}

async fn measure_concurrency(
    addr: std::net::SocketAddr,
    clients: usize,
    window: Duration,
    query: &'static str,
    expected_rows: usize,
) -> ConcurrencyResult {
    let (ready_tx, mut ready_rx) = mpsc::channel(clients);
    let (start_tx, start_rx) = watch::channel(None::<Instant>);
    let mut tasks = Vec::with_capacity(clients);

    for _ in 0..clients {
        let ready_tx = ready_tx.clone();
        let mut start_rx = start_rx.clone();
        tasks.push(tokio::spawn(async move {
            let mut client = BenchClient::connect(addr).await;
            let mut encoded_response_bytes = 0;
            let warmups = if expected_rows == 1 { 10 } else { 1 };
            for _ in 0..warmups {
                let response = client.query(query).await;
                assert_eq!(response.rows.len(), expected_rows);
                encoded_response_bytes = response.encoded_bytes;
                black_box(response.encoded_bytes);
            }
            ready_tx.send(()).await.expect("signal ready client");
            while start_rx.borrow().is_none() {
                start_rx.changed().await.expect("receive benchmark start");
            }
            let deadline = start_rx.borrow().expect("benchmark deadline");
            let mut latencies = Vec::new();
            while Instant::now() < deadline {
                let started = Instant::now();
                let response = client.query(query).await;
                let completed = Instant::now();
                assert_eq!(response.rows.len(), expected_rows);
                assert_eq!(response.encoded_bytes, encoded_response_bytes);
                black_box(response.encoded_bytes);
                if completed <= deadline {
                    latencies.push(completed.duration_since(started));
                } else {
                    break;
                }
            }
            (latencies, encoded_response_bytes)
        }));
    }
    drop(ready_tx);
    for _ in 0..clients {
        ready_rx.recv().await.expect("wait for ready client");
    }

    let started = Instant::now();
    let deadline = started + window;
    let cpu_started = process_cpu_seconds();
    start_tx.send(Some(deadline)).expect("start clients");
    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    let cpu_finished = process_cpu_seconds();
    let mut all_latencies = Vec::new();
    let mut encoded_response_bytes = None;
    for task in tasks {
        let (latencies, bytes) = task.await.expect("join benchmark client");
        all_latencies.extend(latencies);
        assert_eq!(*encoded_response_bytes.get_or_insert(bytes), bytes);
    }
    let drain_time = started.elapsed().saturating_sub(window);
    let cpu_delta = cpu_started
        .zip(cpu_finished)
        .map(|(start, end)| end - start);

    ConcurrencyResult {
        clients,
        operations: all_latencies.len(),
        window,
        drain_time,
        latency: latency_stats(&all_latencies),
        encoded_response_bytes: encoded_response_bytes.expect("response byte size"),
        process_cpu_seconds: cpu_delta,
        process_cpu_utilization_pct: cpu_delta.map(|cpu| cpu / window.as_secs_f64() * 100.0),
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn print_latency(label: &str, stats: LatencyStats) {
    println!(
        "{label:<22} mean={:>9.2} us  p50={:>9.2} us  p95={:>9.2} us  p99={:>9.2} us",
        stats.mean_us, stats.p50_us, stats.p95_us, stats.p99_us
    );
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn working_tree_fingerprint() -> Option<String> {
    let tracked = std::process::Command::new("git")
        .args(["diff", "--binary", "--no-ext-diff", "HEAD", "--"])
        .output()
        .ok()?;
    if !tracked.status.success() {
        return None;
    }
    let untracked = std::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .ok()?;
    if !untracked.status.success() {
        return None;
    }

    let mut hash = 0xcbf29ce484222325u64;
    let mut update = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    };
    update(b"tracked-diff");
    update(&tracked.stdout);

    let mut paths = untracked
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
        .collect::<Vec<_>>();
    paths.sort();
    update(b"untracked-files");
    for path in paths {
        update(path.to_string_lossy().as_bytes());
        update(&std::fs::read(&path).ok()?);
    }
    Some(format!("fnv1a64:{hash:016x}"))
}

fn stable_fingerprint(parts: &[&str]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn stats_json(stats: Option<LatencyStats>) -> serde_json::Value {
    stats.map_or(serde_json::Value::Null, |stats| {
        serde_json::json!({
            "count": stats.count,
            "mean_us": stats.mean_us,
            "stddev_us": stats.stddev_us,
            "min_us": stats.min_us,
            "p50_us": stats.p50_us,
            "p90_us": stats.p90_us,
            "p95_us": stats.p95_us,
            "p99_us": stats.p99_us,
            "max_us": stats.max_us,
        })
    })
}

fn concurrency_json(result: &ConcurrencyResult, one_client_throughput: f64) -> serde_json::Value {
    let throughput = result.operations as f64 / result.window.as_secs_f64();
    let scaling = (one_client_throughput > 0.0).then(|| throughput / one_client_throughput);
    serde_json::json!({
        "clients": result.clients,
        "completed_inside_window": result.operations,
        "window_ms": result.window.as_millis(),
        "drain_time_ms": result.drain_time.as_secs_f64() * 1000.0,
        "throughput_ops_s": throughput,
        "scaling_vs_one_client": scaling,
        "encoded_response_bytes": result.encoded_response_bytes,
        "latency": stats_json(result.latency),
        "process_cpu_seconds_inside_window": result.process_cpu_seconds,
        "process_cpu_utilization_pct": result.process_cpu_utilization_pct,
    })
}

async fn validate_overlap_fixture(addr: std::net::SocketAddr) -> usize {
    let mut client = BenchClient::connect(addr).await;
    let response = client.query(OVERLAP_QUERY).await;
    assert_eq!(response.columns, ["id", "payload"]);
    assert_eq!(response.rows.len(), OVERLAP_ROWS);
    assert_eq!(response.rows.first().expect("first overlap row")[0], "0");
    assert_eq!(
        response.rows.last().expect("last overlap row")[0],
        (OVERLAP_ROWS - 1).to_string()
    );
    assert!(response
        .rows
        .iter()
        .all(|row| row[1].len() == OVERLAP_PAYLOAD_BYTES));
    response.encoded_bytes
}

fn print_concurrency(label: &str, results: &[ConcurrencyResult]) {
    println!("== {label} ==");
    let one_client = results[0].operations as f64 / results[0].window.as_secs_f64();
    for result in results {
        let throughput = result.operations as f64 / result.window.as_secs_f64();
        let scaling = if one_client > 0.0 {
            throughput / one_client
        } else {
            f64::NAN
        };
        let (p50_us, p95_us) = result.latency.map_or((f64::NAN, f64::NAN), |latency| {
            (latency.p50_us, latency.p95_us)
        });
        println!(
            "clients={:<2} completed={:<7} window={:>5.2}s throughput={:>10.1} ops/s \
             scaling={:>5.2}x p50={:>9.2} us p95={:>9.2} us cpu={:>6.1}% drain={:>6.2}ms",
            result.clients,
            result.operations,
            result.window.as_secs_f64(),
            throughput,
            scaling,
            p50_us,
            p95_us,
            result.process_cpu_utilization_pct.unwrap_or(f64::NAN),
            result.drain_time.as_secs_f64() * 1000.0,
        );
    }
    println!();
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let samples = env_usize("POWDB_BASELINE_SAMPLES", DEFAULT_SAMPLES).max(10);
    let window_ms = env_usize("POWDB_BASELINE_WINDOW_MS", DEFAULT_WINDOW_MS as usize) as u64;
    let window = Duration::from_millis(window_ms.max(100));
    let logical_cpus = std::thread::available_parallelism().map_or(1, usize::from);
    let json_only = std::env::args().any(|arg| arg == "--json");
    let run_label =
        std::env::var("POWDB_BASELINE_RUN_LABEL").unwrap_or_else(|_| "unlabeled".into());
    let repetition = env_usize("POWDB_BASELINE_REPETITION", 1);
    let gate_permits = env_usize(
        "POWDB_BASELINE_GATE_PERMITS",
        DEFAULT_TX_GATE_READER_PERMITS as usize,
    )
    .min(u32::MAX as usize) as u32;
    let profile = if cfg!(debug_assertions) {
        "debug (not suitable for performance claims)"
    } else {
        "release"
    };

    if !json_only {
        println!("PowDB synthetic remote-read baseline");
        println!("run_label={run_label}");
        println!("repetition={repetition}");
        println!("gate_permits={gate_permits}");
        println!("build_profile={profile}");
        println!("logical_cpus={logical_cpus}");
        println!("document_json_bytes={DOCUMENT_BYTES}");
        println!("overlap_rows={OVERLAP_ROWS}");
        println!("overlap_payload_bytes={OVERLAP_PAYLOAD_BYTES}");
        println!("point_samples_per_shape={samples}");
        println!("concurrency_window_ms={}", window.as_millis());
        println!("transport=TCP loopback; server and clients share one process\n");
    }
    if cfg!(debug_assertions) && !json_only {
        eprintln!("warning: rerun with --release before comparing performance");
    }
    if logical_cpus < *CLIENT_COUNTS.last().expect("client counts") && !json_only {
        eprintln!(
            "warning: the 10-client tier exceeds detected logical CPU parallelism; \
             scheduler saturation may limit scaling"
        );
    }

    let temp = TempDir::new();
    let engine = seed_engine(temp.path());
    let server = start_server(engine, gate_permits).await;

    let point = measure_point_reads(server.addr, samples).await;
    let overlap_response_bytes = validate_overlap_fixture(server.addr).await;

    let mut point_concurrency = Vec::new();
    for clients in CLIENT_COUNTS {
        point_concurrency
            .push(measure_concurrency(server.addr, clients, window, DOCUMENT_QUERY, 1).await);
    }
    let mut overlap_concurrency = Vec::new();
    for clients in CLIENT_COUNTS {
        overlap_concurrency.push(
            measure_concurrency(server.addr, clients, window, OVERLAP_QUERY, OVERLAP_ROWS).await,
        );
    }

    if !json_only {
        println!("== remote indexed point reads ==");
        print_latency("document JSON", point.document_stats);
        println!("document_response_bytes={}", point.document_response_bytes);
        print_latency("typed row", point.typed_stats);
        println!("typed_response_bytes={}", point.typed_response_bytes);
        println!(
            "document_to_typed_p50_ratio={:.3}x\n",
            point.document_stats.p50_us / point.typed_stats.p50_us
        );
        print_concurrency("remote document point-read concurrency", &point_concurrency);
        print_concurrency(
            "remote overlap-sized scan concurrency",
            &overlap_concurrency,
        );
        println!("overlap_response_bytes={overlap_response_bytes}");
        println!("allocation_metrics=unavailable");
        println!("storage_utilization_metrics=unavailable");
        println!("saturation_inference=not inferred from throughput alone");
        println!(
            "\nInterpretation note: loopback results include client/server scheduling in one process. \
             Repeat release runs on an otherwise idle machine before treating small deltas as signal."
        );
    }

    let git_commit = command_output("git", &["rev-parse", "HEAD"]);
    let git_status = command_output("git", &["status", "--porcelain"]);
    let dirty_tree_fingerprint = working_tree_fingerprint();
    let rustc = command_output("rustc", &["--version"]);
    let fixture_descriptor = format!(
        "document_bytes={DOCUMENT_BYTES};overlap_rows={OVERLAP_ROWS};overlap_payload_bytes={OVERLAP_PAYLOAD_BYTES}"
    );
    let fingerprint = stable_fingerprint(&[
        DOCUMENT_QUERY,
        TYPED_QUERY,
        OVERLAP_QUERY,
        &fixture_descriptor,
    ]);
    let point_one =
        point_concurrency[0].operations as f64 / point_concurrency[0].window.as_secs_f64();
    let overlap_one =
        overlap_concurrency[0].operations as f64 / overlap_concurrency[0].window.as_secs_f64();
    let report = serde_json::json!({
        "schema_version": 1,
        "run": {
            "label": run_label,
            "repetition": repetition,
            "unix_time_ns": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
                .to_string(),
            "profile": profile,
            "package_version": env!("CARGO_PKG_VERSION"),
            "git_commit": git_commit,
            "git_dirty": git_status.as_ref().map(|status| !status.is_empty()),
            "dirty_tree_fingerprint": dirty_tree_fingerprint,
            "dirty_tree_fingerprint_scope": "git diff --binary HEAD plus full contents of every untracked, non-ignored file, sorted by path",
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "logical_cpus": logical_cpus,
            "rustc": rustc,
            "transport": "TCP loopback; server and clients share one process",
        },
        "fixture": {
            "document_json_bytes": DOCUMENT_BYTES,
            "overlap_rows": OVERLAP_ROWS,
            "overlap_payload_bytes": OVERLAP_PAYLOAD_BYTES,
            "fingerprint": fingerprint,
            "document_query": DOCUMENT_QUERY,
            "typed_query": TYPED_QUERY,
            "overlap_query": OVERLAP_QUERY,
            "logical_equivalence_asserted": true,
        },
        "method": {
            "point_samples_per_shape": samples,
            "point_warmups_per_shape": 50,
            "concurrency_window_ms": window.as_millis(),
            "concurrency_clients": CLIENT_COUNTS,
            "gate_permits": gate_permits,
            "inside_window_completions_only": true,
            "allocation_metrics": "unavailable",
            "storage_utilization_metrics": "unavailable",
            "saturation_inference": "not inferred from throughput alone",
        },
        "point_reads": {
            "document": {
                "encoded_response_bytes": point.document_response_bytes,
                "latency": stats_json(Some(point.document_stats)),
                "raw_latency_us": point.document_raw_us,
            },
            "typed": {
                "encoded_response_bytes": point.typed_response_bytes,
                "latency": stats_json(Some(point.typed_stats)),
                "raw_latency_us": point.typed_raw_us,
            },
            "document_to_typed_p50_ratio": point.document_stats.p50_us / point.typed_stats.p50_us,
        },
        "point_concurrency": point_concurrency
            .iter()
            .map(|result| concurrency_json(result, point_one))
            .collect::<Vec<_>>(),
        "overlap_concurrency": {
            "encoded_response_bytes": overlap_response_bytes,
            "results": overlap_concurrency
                .iter()
                .map(|result| concurrency_json(result, overlap_one))
                .collect::<Vec<_>>(),
        },
    });
    if json_only {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("serialize report")
        );
    } else {
        println!("\n== machine-readable JSON ==");
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("serialize report")
        );
    }
    server.stop();
    drop(temp);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_window_has_no_latency_distribution() {
        assert!(latency_stats(&[]).is_none());
    }
}
