//! Reproducible remote-read release gate for the v0.13 read-performance cycle.
//!
//! This runner compares the same ~2.5 KiB indexed document point read over the
//! legacy text and native typed TCP frames, plus an equivalent typed-row
//! baseline. It also measures both document wire paths and an overlap-sized
//! scan at 1/2/5/10 concurrent client connections.
//!
//! Run:
//!   POWDB_HISTORICAL_LEGACY_ARTIFACT=/path/to/isolated-v0.12.json \
//!     cargo run --release -p powdb-bench --bin phase0-read-baseline
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
const DEFAULT_CONVERSION_SAMPLES: usize = 20_000;
const CLIENT_COUNTS: [usize; 4] = [1, 2, 5, 10];
const OVERLAP_ROWS: usize = 10_000;
const OVERLAP_PAYLOAD_BYTES: usize = 256;
const DOCUMENT_QUERY: &str = "DocumentRow filter .id = 1 { .data }";
const TYPED_QUERY: &str =
    "TypedRow filter .id = 1 { .id, .status, .title, .score, .active, .payload }";
const OVERLAP_QUERY: &str = "ScanRow filter .bucket >= 0 { .id, .payload }";
const MIN_LEGACY_TO_NATIVE_P50_REDUCTION_PCT: f64 = 80.0;
const MAX_NATIVE_DOCUMENT_TO_TYPED_P95_RATIO: f64 = 1.5;
const MIN_ISOLATED_RENDER_TO_FRAME_P50_REDUCTION_PCT: f64 = 90.0;
const MIN_OVERLAP_TEN_CLIENT_SCALING: f64 = 5.0;
const MAX_OVERLAP_ONE_CLIENT_P50_REGRESSION_PCT: f64 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryWireMode {
    LegacyText,
    NativeTyped,
}

impl QueryWireMode {
    fn label(self) -> &'static str {
        match self {
            Self::LegacyText => "legacy_text",
            Self::NativeTyped => "native_typed",
        }
    }
}

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

enum WireRows {
    Legacy(Vec<Vec<String>>),
    Native(Vec<Vec<Value>>),
}

impl WireRows {
    fn len(&self) -> usize {
        match self {
            Self::Legacy(rows) => rows.len(),
            Self::Native(rows) => rows.len(),
        }
    }
}

struct RowResponse {
    columns: Vec<String>,
    rows: WireRows,
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

    async fn query(&mut self, query: &str, wire_mode: QueryWireMode) -> RowResponse {
        self.query_inner(query, wire_mode, false).await
    }

    async fn query_with_encoded_size(
        &mut self,
        query: &str,
        wire_mode: QueryWireMode,
    ) -> RowResponse {
        self.query_inner(query, wire_mode, true).await
    }

    async fn query_inner(
        &mut self,
        query: &str,
        wire_mode: QueryWireMode,
        measure_encoded_size: bool,
    ) -> RowResponse {
        let request = match wire_mode {
            QueryWireMode::LegacyText => Message::Query {
                query: query.into(),
            },
            QueryWireMode::NativeTyped => Message::QueryNative {
                query: query.into(),
            },
        };
        request
            .write_to(&mut self.stream)
            .await
            .expect("write query");
        let message = Message::read_from(&mut self.stream)
            .await
            .expect("read query response")
            .expect("server closed before query response");
        // Re-encoding a decoded response is not client work in the actual
        // protocol. It is especially costly for the 2.7 MiB overlap fixture
        // and would make the concurrency measurement include an artificial
        // second serialization on every client. Measure the stable frame size
        // once during fixture/warmup validation, outside timed samples.
        let encoded_bytes = if measure_encoded_size {
            message.encode().len()
        } else {
            0
        };
        match message {
            Message::ResultRows { columns, rows } => RowResponse {
                columns,
                rows: WireRows::Legacy(rows),
                encoded_bytes,
            },
            Message::ResultRowsNative { columns, rows } => RowResponse {
                columns,
                rows: WireRows::Native(rows),
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

struct ConversionMeasurement {
    legacy_text: LatencyStats,
    native_clone: LatencyStats,
    legacy_render_and_frame: LatencyStats,
    native_frame: LatencyStats,
    legacy_to_native_clone_p50_reduction_pct: f64,
    legacy_render_to_native_frame_p50_reduction_pct: f64,
}

fn measure_conversion_cost(samples: usize) -> ConversionMeasurement {
    let (document, _) = make_document(DOCUMENT_BYTES);
    let pj1 = parse_json_text(&document).expect("parse conversion fixture");
    let value = Value::Json(pj1.clone().into_boxed_slice());
    let native_message = Message::ResultRowsNative {
        columns: vec!["data".into()],
        rows: vec![vec![value.clone()]],
    };

    for _ in 0..1_000 {
        black_box(value.to_wire_string());
        black_box(pj1.clone());
        black_box(native_message.encode());
    }

    let mut legacy_text = Vec::with_capacity(samples);
    let mut native_clone = Vec::with_capacity(samples);
    let mut legacy_render_and_frame = Vec::with_capacity(samples);
    let mut native_frame = Vec::with_capacity(samples);
    for i in 0..samples {
        if i.is_multiple_of(2) {
            let started = Instant::now();
            black_box(value.to_wire_string());
            legacy_text.push(started.elapsed());

            let started = Instant::now();
            black_box(pj1.clone());
            native_clone.push(started.elapsed());
        } else {
            let started = Instant::now();
            black_box(pj1.clone());
            native_clone.push(started.elapsed());

            let started = Instant::now();
            black_box(value.to_wire_string());
            legacy_text.push(started.elapsed());
        }

        if i.is_multiple_of(2) {
            let started = Instant::now();
            let legacy_message = Message::ResultRows {
                columns: vec!["data".into()],
                rows: vec![vec![value.to_wire_string()]],
            };
            black_box(legacy_message.encode());
            legacy_render_and_frame.push(started.elapsed());

            let started = Instant::now();
            black_box(native_message.encode());
            native_frame.push(started.elapsed());
        } else {
            let started = Instant::now();
            black_box(native_message.encode());
            native_frame.push(started.elapsed());

            let started = Instant::now();
            let legacy_message = Message::ResultRows {
                columns: vec!["data".into()],
                rows: vec![vec![value.to_wire_string()]],
            };
            black_box(legacy_message.encode());
            legacy_render_and_frame.push(started.elapsed());
        }
    }

    let legacy_text = latency_stats(&legacy_text).expect("legacy conversion samples");
    let native_clone = latency_stats(&native_clone).expect("native clone samples");
    let legacy_render_and_frame =
        latency_stats(&legacy_render_and_frame).expect("legacy render and frame samples");
    let native_frame = latency_stats(&native_frame).expect("native frame samples");
    ConversionMeasurement {
        legacy_to_native_clone_p50_reduction_pct: (legacy_text.p50_us - native_clone.p50_us)
            / legacy_text.p50_us
            * 100.0,
        legacy_render_to_native_frame_p50_reduction_pct: (legacy_render_and_frame.p50_us
            - native_frame.p50_us)
            / legacy_render_and_frame.p50_us
            * 100.0,
        legacy_text,
        native_clone,
        legacy_render_and_frame,
        native_frame,
    }
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

fn assert_point_equivalence(
    legacy_document: &RowResponse,
    native_document: &RowResponse,
    legacy_typed: &RowResponse,
    native_typed: &RowResponse,
) {
    assert_eq!(legacy_document.columns, ["data"]);
    assert_eq!(native_document.columns, ["data"]);
    assert_eq!(legacy_document.rows.len(), 1);
    assert_eq!(native_document.rows.len(), 1);
    assert_eq!(legacy_typed.rows.len(), 1);
    assert_eq!(native_typed.rows.len(), 1);

    let WireRows::Legacy(legacy_document_rows) = &legacy_document.rows else {
        panic!("legacy document query returned a native frame")
    };
    let WireRows::Native(native_document_rows) = &native_document.rows else {
        panic!("native document query returned a legacy frame")
    };
    let WireRows::Legacy(legacy_typed_rows) = &legacy_typed.rows else {
        panic!("legacy typed query returned a native frame")
    };
    let WireRows::Native(native_typed_rows) = &native_typed.rows else {
        panic!("native typed query returned a legacy frame")
    };

    let document_text = &legacy_document_rows[0][0];
    let expected_pj1 = parse_json_text(document_text).expect("canonical document JSON");
    assert_eq!(
        native_document_rows[0],
        [Value::Json(expected_pj1.into_boxed_slice())],
        "native document result must preserve the stored PJ1 bytes"
    );

    let document_json: serde_json::Value =
        serde_json::from_str(document_text).expect("canonical document JSON");
    let typed = &legacy_typed_rows[0];
    let typed_json = serde_json::json!({
        "id": typed[0].parse::<i64>().expect("typed id"),
        "status": typed[1],
        "title": typed[2],
        "score": typed[3].parse::<f64>().expect("typed score"),
        "active": typed[4].parse::<bool>().expect("typed active"),
        "payload": typed[5],
    });
    assert_eq!(
        document_json, typed_json,
        "document and typed fixtures must be logically equivalent"
    );

    assert_eq!(
        native_typed_rows[0],
        [
            Value::Int(typed[0].parse().expect("typed id")),
            Value::Str(typed[1].clone()),
            Value::Str(typed[2].clone()),
            Value::Float(typed[3].parse().expect("typed score")),
            Value::Bool(typed[4].parse().expect("typed active")),
            Value::Str(typed[5].clone()),
        ],
        "legacy and native typed rows must be logically equivalent"
    );
}

struct PointMeasurement {
    legacy_document_stats: LatencyStats,
    native_document_stats: LatencyStats,
    legacy_typed_stats: LatencyStats,
    native_typed_stats: LatencyStats,
    legacy_document_raw_us: Vec<f64>,
    native_document_raw_us: Vec<f64>,
    legacy_typed_raw_us: Vec<f64>,
    native_typed_raw_us: Vec<f64>,
    legacy_document_response_bytes: usize,
    native_document_response_bytes: usize,
    legacy_typed_response_bytes: usize,
    native_typed_response_bytes: usize,
}

async fn record_point_sample(
    client: &mut BenchClient,
    query: &str,
    wire_mode: QueryWireMode,
    samples: &mut Vec<Duration>,
) {
    let started = Instant::now();
    let response = client.query(query, wire_mode).await;
    samples.push(started.elapsed());
    black_box(response.rows.len());
}

async fn measure_point_reads(addr: std::net::SocketAddr, samples: usize) -> PointMeasurement {
    let mut client = BenchClient::connect(addr).await;
    let legacy_document_shape = client
        .query_with_encoded_size(DOCUMENT_QUERY, QueryWireMode::LegacyText)
        .await;
    let native_document_shape = client
        .query_with_encoded_size(DOCUMENT_QUERY, QueryWireMode::NativeTyped)
        .await;
    let legacy_typed_shape = client
        .query_with_encoded_size(TYPED_QUERY, QueryWireMode::LegacyText)
        .await;
    let native_typed_shape = client
        .query_with_encoded_size(TYPED_QUERY, QueryWireMode::NativeTyped)
        .await;
    assert_point_equivalence(
        &legacy_document_shape,
        &native_document_shape,
        &legacy_typed_shape,
        &native_typed_shape,
    );
    for _ in 0..50 {
        for (query, wire_mode) in [
            (DOCUMENT_QUERY, QueryWireMode::LegacyText),
            (DOCUMENT_QUERY, QueryWireMode::NativeTyped),
            (TYPED_QUERY, QueryWireMode::LegacyText),
            (TYPED_QUERY, QueryWireMode::NativeTyped),
        ] {
            black_box(client.query(query, wire_mode).await.encoded_bytes);
        }
    }

    let mut legacy_document = Vec::with_capacity(samples);
    let mut native_document = Vec::with_capacity(samples);
    let mut legacy_typed = Vec::with_capacity(samples);
    let mut native_typed = Vec::with_capacity(samples);
    for i in 0..samples {
        for slot in 0..4 {
            match (i + slot) % 4 {
                0 => {
                    record_point_sample(
                        &mut client,
                        DOCUMENT_QUERY,
                        QueryWireMode::LegacyText,
                        &mut legacy_document,
                    )
                    .await
                }
                1 => {
                    record_point_sample(
                        &mut client,
                        DOCUMENT_QUERY,
                        QueryWireMode::NativeTyped,
                        &mut native_document,
                    )
                    .await
                }
                2 => {
                    record_point_sample(
                        &mut client,
                        TYPED_QUERY,
                        QueryWireMode::LegacyText,
                        &mut legacy_typed,
                    )
                    .await
                }
                _ => {
                    record_point_sample(
                        &mut client,
                        TYPED_QUERY,
                        QueryWireMode::NativeTyped,
                        &mut native_typed,
                    )
                    .await
                }
            }
        }
    }
    PointMeasurement {
        legacy_document_stats: latency_stats(&legacy_document)
            .expect("legacy document point latency samples"),
        native_document_stats: latency_stats(&native_document)
            .expect("native document point latency samples"),
        legacy_typed_stats: latency_stats(&legacy_typed)
            .expect("legacy typed point latency samples"),
        native_typed_stats: latency_stats(&native_typed)
            .expect("native typed point latency samples"),
        legacy_document_raw_us: legacy_document
            .iter()
            .map(|sample| sample.as_secs_f64() * 1_000_000.0)
            .collect(),
        native_document_raw_us: native_document
            .iter()
            .map(|sample| sample.as_secs_f64() * 1_000_000.0)
            .collect(),
        legacy_typed_raw_us: legacy_typed
            .iter()
            .map(|sample| sample.as_secs_f64() * 1_000_000.0)
            .collect(),
        native_typed_raw_us: native_typed
            .iter()
            .map(|sample| sample.as_secs_f64() * 1_000_000.0)
            .collect(),
        legacy_document_response_bytes: legacy_document_shape.encoded_bytes,
        native_document_response_bytes: native_document_shape.encoded_bytes,
        legacy_typed_response_bytes: legacy_typed_shape.encoded_bytes,
        native_typed_response_bytes: native_typed_shape.encoded_bytes,
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
    wire_mode: QueryWireMode,
    expected_rows: usize,
    encoded_response_bytes: usize,
) -> ConcurrencyResult {
    let (ready_tx, mut ready_rx) = mpsc::channel(clients);
    let (start_tx, start_rx) = watch::channel(None::<Instant>);
    let mut tasks = Vec::with_capacity(clients);

    for _ in 0..clients {
        let ready_tx = ready_tx.clone();
        let mut start_rx = start_rx.clone();
        tasks.push(tokio::spawn(async move {
            let mut client = BenchClient::connect(addr).await;
            let warmups = if expected_rows == 1 { 10 } else { 1 };
            for _ in 0..warmups {
                let response = client.query(query, wire_mode).await;
                assert_eq!(response.rows.len(), expected_rows);
                black_box(response.rows.len());
            }
            ready_tx.send(()).await.expect("signal ready client");
            while start_rx.borrow().is_none() {
                start_rx.changed().await.expect("receive benchmark start");
            }
            let deadline = start_rx.borrow().expect("benchmark deadline");
            let mut latencies = Vec::new();
            while Instant::now() < deadline {
                let started = Instant::now();
                let response = client.query(query, wire_mode).await;
                let completed = Instant::now();
                assert_eq!(response.rows.len(), expected_rows);
                black_box(response.rows.len());
                if completed <= deadline {
                    latencies.push(completed.duration_since(started));
                } else {
                    break;
                }
            }
            latencies
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
    for task in tasks {
        let latencies = task.await.expect("join benchmark client");
        all_latencies.extend(latencies);
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
        encoded_response_bytes,
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

#[derive(Debug)]
struct HistoricalLegacyBaseline {
    label: String,
    artifact_fingerprint: String,
    git_commit: String,
    dirty_tree_fingerprint: Option<String>,
    package_version: String,
    p50_us: f64,
    p95_us: f64,
    encoded_response_bytes: u64,
    overlap_one_client_p50_us: f64,
}

fn json_pointer_str<'a>(value: &'a serde_json::Value, pointer: &str) -> &'a str {
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("historical artifact is missing string field {pointer}"))
}

fn json_pointer_f64(value: &serde_json::Value, pointer: &str) -> f64 {
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("historical artifact is missing number field {pointer}"))
}

fn load_historical_legacy_baseline() -> HistoricalLegacyBaseline {
    let path = std::env::var("POWDB_HISTORICAL_LEGACY_ARTIFACT")
        .expect("POWDB_HISTORICAL_LEGACY_ARTIFACT must name the isolated pre-v0.13 JSON artifact");
    let text = std::fs::read_to_string(&path).expect("read historical legacy benchmark artifact");
    let report: serde_json::Value =
        serde_json::from_str(&text).expect("parse historical legacy benchmark artifact");
    assert_eq!(
        report
            .pointer("/schema_version")
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "unsupported historical artifact schema"
    );
    assert_eq!(
        json_pointer_str(&report, "/artifact_kind"),
        "historical_read_baseline_summary",
        "historical artifact has the wrong kind"
    );
    assert_eq!(
        report
            .pointer("/fixture/document_json_bytes")
            .and_then(serde_json::Value::as_u64),
        Some(DOCUMENT_BYTES as u64),
        "historical artifact used a different document size"
    );
    assert_eq!(
        json_pointer_str(&report, "/fixture/document_query"),
        DOCUMENT_QUERY,
        "historical artifact used a different document query"
    );
    assert_eq!(
        report
            .pointer("/fixture/overlap_rows")
            .and_then(serde_json::Value::as_u64),
        Some(OVERLAP_ROWS as u64),
        "historical artifact used a different overlap row count"
    );
    assert_eq!(
        report
            .pointer("/fixture/overlap_payload_bytes")
            .and_then(serde_json::Value::as_u64),
        Some(OVERLAP_PAYLOAD_BYTES as u64),
        "historical artifact used a different overlap payload size"
    );
    assert_eq!(
        json_pointer_str(&report, "/fixture/overlap_query"),
        OVERLAP_QUERY,
        "historical artifact used a different overlap query"
    );
    let corrected_methodology = json_pointer_str(
        &report,
        "/overlap_concurrency/corrected_no_reencode_summary/methodology",
    );
    assert!(
        corrected_methodology.contains("not re-encoded inside timed samples"),
        "historical overlap comparator must exclude decoded-response re-encoding"
    );
    assert_eq!(
        json_pointer_str(
            &report,
            "/overlap_concurrency/corrected_no_reencode_summary/runner_source",
        ),
        "docs/benchmarks/harnesses/v0.12-overlap-no-reencode.patch",
        "historical overlap comparator must name its reproducible harness patch"
    );
    assert_eq!(
        json_pointer_str(&report, "/run/profile"),
        "release",
        "historical comparator must be a release build"
    );

    HistoricalLegacyBaseline {
        label: std::env::var("POWDB_HISTORICAL_BASELINE_LABEL")
            .unwrap_or_else(|_| "pre-v0.13-isolated".into()),
        artifact_fingerprint: stable_fingerprint(&[&text]),
        git_commit: json_pointer_str(&report, "/run/git_commit").to_owned(),
        dirty_tree_fingerprint: report
            .pointer("/run/dirty_tree_fingerprint")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        package_version: json_pointer_str(&report, "/run/package_version").to_owned(),
        p50_us: json_pointer_f64(&report, "/point_reads/document/latency/p50_us"),
        p95_us: json_pointer_f64(&report, "/point_reads/document/latency/p95_us"),
        encoded_response_bytes: report
            .pointer("/point_reads/document/encoded_response_bytes")
            .and_then(serde_json::Value::as_u64)
            .expect("historical artifact is missing document encoded bytes"),
        overlap_one_client_p50_us: json_pointer_f64(
            &report,
            "/overlap_concurrency/corrected_no_reencode_summary/one_client_p50_us",
        ),
    }
}

fn assert_native_pj1_has_no_text_serialization() {
    const HANDLER: &str = include_str!("../../../server/src/handler/wire.rs");
    const PROTOCOL: &str = include_str!("../../../server/src/protocol.rs");

    let conversion_start = HANDLER
        .find("fn query_result_to_message(")
        .expect("server result conversion function");
    let conversion_end = HANDLER[conversion_start..]
        .find("fn value_to_display(")
        .map(|offset| conversion_start + offset)
        .expect("legacy display conversion function");
    let conversion = &HANDLER[conversion_start..conversion_end];
    assert!(conversion.contains("Message::ResultRowsNative { columns, rows }"));
    assert!(conversion.contains("Message::ResultScalarNative { value }"));
    assert!(conversion.contains("WireResultMode::LegacyText"));
    assert!(conversion.contains("value_to_display"));
    assert!(!conversion.contains("pj1_to_text"));

    let native_rows_start = conversion
        .find("WireResultMode::Native => {")
        .expect("native row conversion branch");
    let legacy_rows_start = conversion[native_rows_start..]
        .find("WireResultMode::LegacyText => {")
        .map(|offset| native_rows_start + offset)
        .expect("legacy row conversion branch");
    let native_rows = &conversion[native_rows_start..legacy_rows_start];
    assert!(!native_rows.contains("value_to_display"));
    assert!(!native_rows.contains("pj1_to_text"));

    let native_scalar_start = conversion
        .rfind("WireResultMode::Native => {")
        .expect("native scalar conversion branch");
    let legacy_scalar_start = conversion[native_scalar_start..]
        .find("WireResultMode::LegacyText =>")
        .map(|offset| native_scalar_start + offset)
        .expect("legacy scalar conversion branch");
    let native_scalar = &conversion[native_scalar_start..legacy_scalar_start];
    assert!(!native_scalar.contains("value_to_display"));
    assert!(!native_scalar.contains("pj1_to_text"));

    let encode_start = PROTOCOL
        .find("fn encode_typed_value(")
        .expect("native value encoder");
    let encode_end = PROTOCOL[encode_start..]
        .find("fn decode_typed_value(")
        .map(|offset| encode_start + offset)
        .expect("native value decoder");
    let encoder = &PROTOCOL[encode_start..encode_end];
    assert!(encoder.contains("Value::Json(value) => out.extend_from_slice(value)"));
    assert!(!encoder.contains("pj1_to_text"));
    assert!(!encoder.contains("value_to_display"));
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
    let response = client
        .query_with_encoded_size(OVERLAP_QUERY, QueryWireMode::LegacyText)
        .await;
    assert_eq!(response.columns, ["id", "payload"]);
    let encoded_bytes = response.encoded_bytes;
    let WireRows::Legacy(rows) = response.rows else {
        panic!("legacy overlap query returned a native frame")
    };
    assert_eq!(rows.len(), OVERLAP_ROWS);
    assert_eq!(rows.first().expect("first overlap row")[0], "0");
    assert_eq!(
        rows.last().expect("last overlap row")[0],
        (OVERLAP_ROWS - 1).to_string()
    );
    assert!(rows.iter().all(|row| row[1].len() == OVERLAP_PAYLOAD_BYTES));
    encoded_bytes
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
    let conversion_samples =
        env_usize("POWDB_CONVERSION_SAMPLES", DEFAULT_CONVERSION_SAMPLES).max(100);
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
        println!("conversion_samples_per_shape={conversion_samples}");
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

    let historical = load_historical_legacy_baseline();
    assert_native_pj1_has_no_text_serialization();
    let temp = TempDir::new();
    let engine = seed_engine(temp.path());
    let server = start_server(engine, gate_permits).await;

    // Measure the overlap gate first on a fresh server, matching the isolated
    // historical harness. Point/concurrency loads would otherwise add a long,
    // client-count-dependent thermal phase before the blocking comparison.
    let mut overlap_concurrency = Vec::new();
    for clients in CLIENT_COUNTS {
        overlap_concurrency.push(
            measure_concurrency(
                server.addr,
                clients,
                window,
                OVERLAP_QUERY,
                QueryWireMode::LegacyText,
                OVERLAP_ROWS,
                0,
            )
            .await,
        );
    }
    // Validate and measure the stable frame size after the timed tiers. This
    // keeps pre-timing warmups identical to the historical harness: exactly
    // one warmup on each measured client connection, with no extra remote scan
    // on the server beforehand.
    let overlap_response_bytes = validate_overlap_fixture(server.addr).await;
    for result in &mut overlap_concurrency {
        result.encoded_response_bytes = overlap_response_bytes;
    }

    let point = measure_point_reads(server.addr, samples).await;

    let mut legacy_point_concurrency = Vec::new();
    for clients in CLIENT_COUNTS {
        legacy_point_concurrency.push(
            measure_concurrency(
                server.addr,
                clients,
                window,
                DOCUMENT_QUERY,
                QueryWireMode::LegacyText,
                1,
                point.legacy_document_response_bytes,
            )
            .await,
        );
    }
    let mut native_point_concurrency = Vec::new();
    for clients in CLIENT_COUNTS {
        native_point_concurrency.push(
            measure_concurrency(
                server.addr,
                clients,
                window,
                DOCUMENT_QUERY,
                QueryWireMode::NativeTyped,
                1,
                point.native_document_response_bytes,
            )
            .await,
        );
    }
    let conversion = measure_conversion_cost(conversion_samples);

    let historical_to_native_p50_reduction_pct =
        (historical.p50_us - point.native_document_stats.p50_us) / historical.p50_us * 100.0;
    let current_legacy_to_native_p50_reduction_pct = (point.legacy_document_stats.p50_us
        - point.native_document_stats.p50_us)
        / point.legacy_document_stats.p50_us
        * 100.0;
    let native_document_to_typed_p95_ratio =
        point.native_document_stats.p95_us / point.native_typed_stats.p95_us;
    let historical_reduction_gate_passed =
        historical_to_native_p50_reduction_pct >= MIN_LEGACY_TO_NATIVE_P50_REDUCTION_PCT;
    let typed_ratio_gate_passed =
        native_document_to_typed_p95_ratio <= MAX_NATIVE_DOCUMENT_TO_TYPED_P95_RATIO;
    let e2e_non_regression_gate_passed =
        point.native_document_stats.p50_us <= point.legacy_document_stats.p50_us;
    let conversion_gate_passed = conversion.legacy_render_to_native_frame_p50_reduction_pct
        >= MIN_ISOLATED_RENDER_TO_FRAME_P50_REDUCTION_PCT;
    let overlap_one =
        overlap_concurrency[0].operations as f64 / overlap_concurrency[0].window.as_secs_f64();
    let overlap_ten_result = overlap_concurrency
        .last()
        .expect("ten-client overlap result");
    let overlap_ten =
        overlap_ten_result.operations as f64 / overlap_ten_result.window.as_secs_f64();
    let overlap_ten_client_scaling = overlap_ten / overlap_one;
    let overlap_scaling_gate_passed = overlap_ten_client_scaling >= MIN_OVERLAP_TEN_CLIENT_SCALING;
    let overlap_one_client_p50_us = overlap_concurrency[0]
        .latency
        .expect("one-client overlap latency")
        .p50_us;
    let overlap_one_client_p50_regression_pct = (overlap_one_client_p50_us
        - historical.overlap_one_client_p50_us)
        / historical.overlap_one_client_p50_us
        * 100.0;
    let overlap_one_client_latency_gate_passed =
        overlap_one_client_p50_regression_pct <= MAX_OVERLAP_ONE_CLIENT_P50_REGRESSION_PCT;
    let release_profile_gate_passed = !cfg!(debug_assertions);
    let release_gate_passed = typed_ratio_gate_passed
        && e2e_non_regression_gate_passed
        && conversion_gate_passed
        && overlap_scaling_gate_passed
        && overlap_one_client_latency_gate_passed
        && release_profile_gate_passed;

    if !json_only {
        println!("== remote indexed point reads ==");
        print_latency("legacy document", point.legacy_document_stats);
        println!(
            "legacy_document_response_bytes={}",
            point.legacy_document_response_bytes
        );
        print_latency("native document", point.native_document_stats);
        println!(
            "native_document_response_bytes={}",
            point.native_document_response_bytes
        );
        print_latency("legacy typed row", point.legacy_typed_stats);
        println!(
            "legacy_typed_response_bytes={}",
            point.legacy_typed_response_bytes
        );
        print_latency("native typed row", point.native_typed_stats);
        println!(
            "native_typed_response_bytes={}",
            point.native_typed_response_bytes
        );
        println!(
            "historical_legacy_to_native_p50_reduction={historical_to_native_p50_reduction_pct:.2}%"
        );
        println!(
            "historical_80_percent_gate={} (retained diagnostic; not a release gate)",
            if historical_reduction_gate_passed {
                "PASS"
            } else {
                "FAIL"
            }
        );
        println!(
            "current_legacy_to_native_p50_reduction={current_legacy_to_native_p50_reduction_pct:.2}%"
        );
        println!("native_document_to_typed_p95_ratio={native_document_to_typed_p95_ratio:.3}x");
        println!(
            "native_server_pj1_to_text_calls=0 (structural source-path assertion outside timing)"
        );
        println!(
            "overlap_ten_client_scaling={overlap_ten_client_scaling:.2}x (minimum {:.2}x)",
            MIN_OVERLAP_TEN_CLIENT_SCALING
        );
        println!(
            "overlap_one_client_p50_regression={overlap_one_client_p50_regression_pct:.2}% \
             (maximum {:.2}%)",
            MAX_OVERLAP_ONE_CLIENT_P50_REGRESSION_PCT
        );
        println!(
            "release_gate={}\n",
            if release_gate_passed { "PASS" } else { "FAIL" }
        );
        println!("== isolated PJ1 conversion and framing ==");
        print_latency("legacy PJ1 to text", conversion.legacy_text);
        print_latency("native PJ1 clone", conversion.native_clone);
        print_latency("legacy render+frame", conversion.legacy_render_and_frame);
        print_latency("native frame", conversion.native_frame);
        println!(
            "legacy_text_to_native_clone_p50_reduction={:.2}%",
            conversion.legacy_to_native_clone_p50_reduction_pct
        );
        println!(
            "legacy_render_to_native_frame_p50_reduction={:.2}%\n",
            conversion.legacy_render_to_native_frame_p50_reduction_pct
        );
        print_concurrency(
            "legacy remote document point-read concurrency",
            &legacy_point_concurrency,
        );
        print_concurrency(
            "native remote document point-read concurrency",
            &native_point_concurrency,
        );
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
    let legacy_point_one = legacy_point_concurrency[0].operations as f64
        / legacy_point_concurrency[0].window.as_secs_f64();
    let native_point_one = native_point_concurrency[0].operations as f64
        / native_point_concurrency[0].window.as_secs_f64();
    let report = serde_json::json!({
        "schema_version": 2,
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
            "conversion_samples_per_shape": conversion_samples,
            "conversion_warmups_per_shape": 1000,
            "concurrency_window_ms": window.as_millis(),
            "concurrency_clients": CLIENT_COUNTS,
            "gate_permits": gate_permits,
            "inside_window_completions_only": true,
            "point_wire_paths": [
                QueryWireMode::LegacyText.label(),
                QueryWireMode::NativeTyped.label(),
            ],
            "allocation_metrics": "unavailable",
            "storage_utilization_metrics": "unavailable",
            "saturation_inference": "not inferred from throughput alone",
        },
        "historical_legacy_baseline": {
            "label": historical.label,
            "artifact_fingerprint": historical.artifact_fingerprint,
            "git_commit": historical.git_commit,
            "dirty_tree_fingerprint": historical.dirty_tree_fingerprint,
            "package_version": historical.package_version,
            "document_p50_us": historical.p50_us,
            "document_p95_us": historical.p95_us,
            "encoded_response_bytes": historical.encoded_response_bytes,
            "overlap_one_client_p50_us": historical.overlap_one_client_p50_us,
            "isolation": "separate git worktree; current branch was not mutated",
        },
        "point_reads": {
            "legacy_document_current_tree": {
                "wire_mode": QueryWireMode::LegacyText.label(),
                "encoded_response_bytes": point.legacy_document_response_bytes,
                "latency": stats_json(Some(point.legacy_document_stats)),
                "raw_latency_us": point.legacy_document_raw_us,
            },
            "native_document": {
                "wire_mode": QueryWireMode::NativeTyped.label(),
                "encoded_response_bytes": point.native_document_response_bytes,
                "latency": stats_json(Some(point.native_document_stats)),
                "raw_latency_us": point.native_document_raw_us,
            },
            "legacy_typed_current_tree": {
                "wire_mode": QueryWireMode::LegacyText.label(),
                "encoded_response_bytes": point.legacy_typed_response_bytes,
                "latency": stats_json(Some(point.legacy_typed_stats)),
                "raw_latency_us": point.legacy_typed_raw_us,
            },
            "native_typed": {
                "wire_mode": QueryWireMode::NativeTyped.label(),
                "encoded_response_bytes": point.native_typed_response_bytes,
                "latency": stats_json(Some(point.native_typed_stats)),
                "raw_latency_us": point.native_typed_raw_us,
            },
            "current_legacy_to_native_document_p50_reduction_pct": current_legacy_to_native_p50_reduction_pct,
            "historical_legacy_to_native_document_p50_reduction_pct": historical_to_native_p50_reduction_pct,
            "native_document_to_native_typed_p95_ratio": native_document_to_typed_p95_ratio,
        },
        "point_concurrency": {
            "legacy_document": legacy_point_concurrency
                .iter()
                .map(|result| concurrency_json(result, legacy_point_one))
                .collect::<Vec<_>>(),
            "native_document": native_point_concurrency
                .iter()
                .map(|result| concurrency_json(result, native_point_one))
                .collect::<Vec<_>>(),
        },
        "overlap_concurrency": {
            "encoded_response_bytes": overlap_response_bytes,
            "results": overlap_concurrency
                .iter()
                .map(|result| concurrency_json(result, overlap_one))
                .collect::<Vec<_>>(),
        },
        "native_pj1_text_serialization": {
            "server_calls_per_native_result": 0,
            "proof_kind": "structural source-path assertion",
            "proof_scope": "query_result_to_message native rows/scalar branches and encode_typed_value",
            "timing_distortion": "none; assertion runs before timed measurements",
            "runtime_counter_available": false,
        },
        "isolated_conversion_cost": {
            "fixture_pj1_bytes": parse_json_text(&make_document(DOCUMENT_BYTES).0)
                .expect("conversion fixture")
                .len(),
            "legacy_pj1_to_canonical_text": stats_json(Some(conversion.legacy_text)),
            "native_pj1_clone": stats_json(Some(conversion.native_clone)),
            "legacy_render_and_frame": stats_json(Some(conversion.legacy_render_and_frame)),
            "native_frame": stats_json(Some(conversion.native_frame)),
            "legacy_text_to_native_clone_p50_reduction_pct": conversion.legacy_to_native_clone_p50_reduction_pct,
            "legacy_render_to_native_frame_p50_reduction_pct": conversion.legacy_render_to_native_frame_p50_reduction_pct,
            "scope": "single-process conversion/copy microbenchmark; excludes query execution, sockets, and scheduling",
        },
        "release_gate": {
            "passed": release_gate_passed,
            "historical_80_percent_gate_passed": historical_reduction_gate_passed,
            "historical_legacy_to_native_p50_reduction_diagnostic": {
                "observed_pct": historical_to_native_p50_reduction_pct,
                "minimum_pct": MIN_LEGACY_TO_NATIVE_P50_REDUCTION_PCT,
                "passed": historical_reduction_gate_passed,
                "release_blocking": false,
            },
            "native_document_to_native_typed_p95_ratio": {
                "observed": native_document_to_typed_p95_ratio,
                "maximum": MAX_NATIVE_DOCUMENT_TO_TYPED_P95_RATIO,
                "passed": typed_ratio_gate_passed,
            },
            "native_e2e_p50_not_worse_than_current_legacy": {
                "native_document_p50_us": point.native_document_stats.p50_us,
                "current_legacy_document_p50_us": point.legacy_document_stats.p50_us,
                "passed": e2e_non_regression_gate_passed,
            },
            "isolated_legacy_render_to_native_frame_p50_reduction": {
                "observed_pct": conversion.legacy_render_to_native_frame_p50_reduction_pct,
                "minimum_pct": MIN_ISOLATED_RENDER_TO_FRAME_P50_REDUCTION_PCT,
                "passed": conversion_gate_passed,
            },
            "native_server_pj1_to_text_calls": {
                "observed_per_result": 0,
                "maximum_per_result": 0,
                "passed": true,
                "proof_kind": "structural source-path assertion",
            },
            "overlap_ten_client_scaling": {
                "observed": overlap_ten_client_scaling,
                "minimum": MIN_OVERLAP_TEN_CLIENT_SCALING,
                "passed": overlap_scaling_gate_passed,
            },
            "overlap_one_client_p50_regression": {
                "historical_p50_us": historical.overlap_one_client_p50_us,
                "current_p50_us": overlap_one_client_p50_us,
                "observed_pct": overlap_one_client_p50_regression_pct,
                "maximum_pct": MAX_OVERLAP_ONE_CLIENT_P50_REGRESSION_PCT,
                "passed": overlap_one_client_latency_gate_passed,
            },
            "release_profile": {
                "passed": release_profile_gate_passed,
            },
            "failure_exit_code": 2,
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
    if !release_gate_passed {
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_window_has_no_latency_distribution() {
        assert!(latency_stats(&[]).is_none());
    }

    #[test]
    fn native_result_path_cannot_render_pj1_text() {
        assert_native_pj1_has_no_text_serialization();
    }
}
