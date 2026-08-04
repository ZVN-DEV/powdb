//! Lock-free Prometheus metrics for `powdb-server`.
//!
//! All counters/gauges are plain atomics behind an `Arc<Metrics>` shared into
//! every connection handler, so updating them never blocks and a `/metrics`
//! scrape never touches the `RwLock<Engine>`. Exposed over a small, separate
//! HTTP listener (own port, opt-in) so the binary wire protocol is untouched.
//!
//! Histogram consistency: `render()` derives `_count` and the `le="+Inf"`
//! bucket from the *same* in-render sum of the bucket atomics, so
//! `+Inf == _count` always holds in a single exposition regardless of
//! concurrent observers. At quiescence every counter is exact.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::debug;

/// Upper bounds (seconds) for the query-latency histogram, ascending. The
/// implicit final bucket is `le="+Inf"`.
const LATENCY_BUCKETS: [f64; 9] = [0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0];
const SYNC_OPERATION_COUNT: usize = 3;
const SYNC_OUTCOME_COUNT: usize = 2;
const SYNC_REPAIR_ACTION_COUNT: usize = 4;

/// Cap on bytes read from a metrics client before bailing. A scrape request
/// line + headers is tiny; anything larger is junk or hostile.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// A scrape must be snappy — do NOT reuse the 300s connection idle timeout.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on directory entries walked when sizing the data directory, so a scrape
/// stays bounded no matter how many files a data dir accumulates.
const MAX_SIZED_ENTRIES: usize = 10_000;

/// Name of the write-ahead log inside the data directory. Sized separately
/// from the rest of the database: WAL growth and database growth are different
/// operational signals (a WAL that stops shrinking means checkpoints are not
/// completing).
const WAL_FILE_NAME: &str = "wal.log";

/// How a finished query is classified for metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryOutcome {
    Ok,
    Error,
    Timeout,
    MemoryLimit,
}

/// Private sync protocol operation names. Keep this enum small and
/// low-cardinality: replica ids belong in authenticated responses, not labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncOperation {
    Status,
    Pull,
    Ack,
}

impl SyncOperation {
    const ALL: [Self; SYNC_OPERATION_COUNT] = [Self::Status, Self::Pull, Self::Ack];

    const fn idx(self) -> usize {
        match self {
            Self::Status => 0,
            Self::Pull => 1,
            Self::Ack => 2,
        }
    }

    const fn as_label(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Pull => "pull",
            Self::Ack => "ack",
        }
    }
}

/// How a finished sync protocol frame is classified for metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    Ok,
    Error,
}

impl SyncOutcome {
    const ALL: [Self; SYNC_OUTCOME_COUNT] = [Self::Ok, Self::Error];

    const fn idx(self) -> usize {
        match self {
            Self::Ok => 0,
            Self::Error => 1,
        }
    }

    const fn as_label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

/// Low-cardinality repair actions returned inside sync status payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncRepairLabel {
    None,
    Pull,
    AwaitArchive,
    Rebootstrap,
}

impl SyncRepairLabel {
    const ALL: [Self; SYNC_REPAIR_ACTION_COUNT] = [
        Self::None,
        Self::Pull,
        Self::AwaitArchive,
        Self::Rebootstrap,
    ];

    const fn idx(self) -> usize {
        match self {
            Self::None => 0,
            Self::Pull => 1,
            Self::AwaitArchive => 2,
            Self::Rebootstrap => 3,
        }
    }

    const fn as_label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pull => "pull",
            Self::AwaitArchive => "await_archive",
            Self::Rebootstrap => "rebootstrap",
        }
    }
}

/// All server metrics. Cheap to update (uncontended atomic add), lock-free to
/// render.
pub struct Metrics {
    start: Instant,
    version: &'static str,
    /// Data directory to size at scrape time; `None` disables the storage
    /// size gauges (they are the only metrics that touch the filesystem).
    data_dir: Option<PathBuf>,

    connections_active: AtomicU64,
    connections_accepted_total: AtomicU64,
    tls_handshake_failures_total: AtomicU64,

    queries_ok_total: AtomicU64,
    queries_error_total: AtomicU64,
    queries_in_flight: AtomicU64,
    query_timeouts_total: AtomicU64,
    query_memory_limit_exceeded_total: AtomicU64,
    tx_gate_timeouts_total: AtomicU64,
    tx_reaped_total: AtomicU64,

    auth_failures_total: AtomicU64,

    // Query-latency histogram: one counter per finite bucket plus one overflow
    // bucket for observations greater than the last bound.
    latency_buckets: [AtomicU64; LATENCY_BUCKETS.len() + 1],
    latency_sum_nanos: AtomicU64,

    sync_operations_total: [[AtomicU64; SYNC_OUTCOME_COUNT]; SYNC_OPERATION_COUNT],
    sync_repair_actions_total: [[AtomicU64; SYNC_REPAIR_ACTION_COUNT]; SYNC_OPERATION_COUNT],
    sync_latency_buckets: [[AtomicU64; LATENCY_BUCKETS.len() + 1]; SYNC_OPERATION_COUNT],
    sync_latency_sum_nanos: [AtomicU64; SYNC_OPERATION_COUNT],
    sync_pull_units_total: AtomicU64,
    sync_pull_bytes_total: AtomicU64,
    sync_ack_advanced_total: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            version: env!("CARGO_PKG_VERSION"),
            data_dir: None,
            connections_active: AtomicU64::new(0),
            connections_accepted_total: AtomicU64::new(0),
            tls_handshake_failures_total: AtomicU64::new(0),
            queries_ok_total: AtomicU64::new(0),
            queries_error_total: AtomicU64::new(0),
            queries_in_flight: AtomicU64::new(0),
            query_timeouts_total: AtomicU64::new(0),
            query_memory_limit_exceeded_total: AtomicU64::new(0),
            tx_gate_timeouts_total: AtomicU64::new(0),
            tx_reaped_total: AtomicU64::new(0),
            auth_failures_total: AtomicU64::new(0),
            latency_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            latency_sum_nanos: AtomicU64::new(0),
            sync_operations_total: std::array::from_fn(|_| {
                std::array::from_fn(|_| AtomicU64::new(0))
            }),
            sync_latency_buckets: std::array::from_fn(|_| {
                std::array::from_fn(|_| AtomicU64::new(0))
            }),
            sync_latency_sum_nanos: std::array::from_fn(|_| AtomicU64::new(0)),
            sync_pull_units_total: AtomicU64::new(0),
            sync_pull_bytes_total: AtomicU64::new(0),
            sync_repair_actions_total: std::array::from_fn(|_| {
                std::array::from_fn(|_| AtomicU64::new(0))
            }),
            sync_ack_advanced_total: AtomicU64::new(0),
        }
    }

    /// Sample the data directory's size on each scrape. Without this the
    /// storage-size gauges are omitted from the exposition.
    pub fn with_data_dir(mut self, data_dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(data_dir.into());
        self
    }

    /// The server version reported by `/health` and `powdb_build_info`.
    pub fn version(&self) -> &'static str {
        self.version
    }

    /// Record a finished query: its result class and execution time.
    pub fn record_query(&self, elapsed: Duration, outcome: QueryOutcome) {
        match outcome {
            QueryOutcome::Ok => {
                self.queries_ok_total.fetch_add(1, Relaxed);
            }
            QueryOutcome::Error => {
                self.queries_error_total.fetch_add(1, Relaxed);
            }
            QueryOutcome::Timeout => {
                self.queries_error_total.fetch_add(1, Relaxed);
                self.query_timeouts_total.fetch_add(1, Relaxed);
            }
            QueryOutcome::MemoryLimit => {
                self.queries_error_total.fetch_add(1, Relaxed);
                self.query_memory_limit_exceeded_total.fetch_add(1, Relaxed);
            }
        }
        let secs = elapsed.as_secs_f64();
        let idx = LATENCY_BUCKETS
            .iter()
            .position(|&b| secs <= b)
            .unwrap_or(LATENCY_BUCKETS.len());
        self.latency_buckets[idx].fetch_add(1, Relaxed);
        self.latency_sum_nanos
            .fetch_add(elapsed.as_nanos() as u64, Relaxed);
    }

    /// Record a completed private sync protocol operation.
    pub fn record_sync_operation(
        &self,
        operation: SyncOperation,
        elapsed: Duration,
        outcome: SyncOutcome,
    ) {
        let op_idx = operation.idx();
        self.sync_operations_total[op_idx][outcome.idx()].fetch_add(1, Relaxed);

        let secs = elapsed.as_secs_f64();
        let bucket_idx = LATENCY_BUCKETS
            .iter()
            .position(|&b| secs <= b)
            .unwrap_or(LATENCY_BUCKETS.len());
        self.sync_latency_buckets[op_idx][bucket_idx].fetch_add(1, Relaxed);
        self.sync_latency_sum_nanos[op_idx].fetch_add(elapsed.as_nanos() as u64, Relaxed);
    }

    pub fn record_sync_pull_payload(&self, units: u64, bytes: u64) {
        self.sync_pull_units_total.fetch_add(units, Relaxed);
        self.sync_pull_bytes_total.fetch_add(bytes, Relaxed);
    }

    pub fn record_sync_repair_action(&self, operation: SyncOperation, repair: SyncRepairLabel) {
        self.sync_repair_actions_total[operation.idx()][repair.idx()].fetch_add(1, Relaxed);
    }

    pub fn inc_sync_ack_advanced(&self) {
        self.sync_ack_advanced_total.fetch_add(1, Relaxed);
    }

    pub fn inc_connection_accepted(&self) {
        self.connections_accepted_total.fetch_add(1, Relaxed);
    }

    pub fn inc_auth_failure(&self) {
        self.auth_failures_total.fetch_add(1, Relaxed);
    }

    /// Record a frame that gave up waiting on the transaction gate. Every
    /// frontend that waits on the gate reports here: an explicit `begin`, a
    /// bare autocommit statement, and a private-sync frame. Also counts as a
    /// failed query so `powdb_queries_total{result="error"}` stays truthful:
    /// the client saw the statement fail.
    pub fn inc_tx_gate_timeout(&self) {
        self.tx_gate_timeouts_total.fetch_add(1, Relaxed);
        self.queries_error_total.fetch_add(1, Relaxed);
    }

    /// Record an explicit transaction the server rolled back because it held
    /// the transaction gate for its whole permitted lifetime
    /// (`POWDB_TX_MAX_LIFETIME_MS`). Deliberately NOT counted as a gate
    /// timeout: this connection never waited on the gate, it held it.
    pub fn inc_tx_reaped(&self) {
        self.tx_reaped_total.fetch_add(1, Relaxed);
        self.queries_error_total.fetch_add(1, Relaxed);
    }

    pub fn inc_tls_failure(&self) {
        self.tls_handshake_failures_total.fetch_add(1, Relaxed);
    }

    /// RAII gauge: increments `connections_active` now, decrements on drop —
    /// correct across every early return or panic in the connection task.
    pub fn active_guard(self: &Arc<Self>) -> ActiveGuard {
        self.connections_active.fetch_add(1, Relaxed);
        ActiveGuard(self.clone())
    }

    /// RAII gauge: increments `queries_in_flight` now, decrements on drop.
    pub fn in_flight_guard(self: &Arc<Self>) -> InFlightGuard {
        self.queries_in_flight.fetch_add(1, Relaxed);
        InFlightGuard(self.clone())
    }

    /// Render the full exposition in Prometheus text format (v0.0.4).
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(2048);

        // build_info — value is always 1; the version is a label.
        out.push_str("# HELP powdb_build_info Build information.\n");
        out.push_str("# TYPE powdb_build_info gauge\n");
        let _ = writeln!(
            out,
            "powdb_build_info{{version=\"{}\"}} 1",
            escape_label(self.version)
        );

        gauge_f64(
            &mut out,
            "powdb_uptime_seconds",
            "Seconds since the server started.",
            self.start.elapsed().as_secs_f64(),
        );
        gauge_u64(
            &mut out,
            "powdb_connections_active",
            "Currently open client connections.",
            self.connections_active.load(Relaxed),
        );
        counter(
            &mut out,
            "powdb_connections_accepted_total",
            "Total client connections accepted.",
            self.connections_accepted_total.load(Relaxed),
        );
        counter(
            &mut out,
            "powdb_tls_handshake_failures_total",
            "Total TLS handshakes that failed.",
            self.tls_handshake_failures_total.load(Relaxed),
        );

        // queries_total{result} — always emit both label values.
        out.push_str("# HELP powdb_queries_total Total queries executed, by result.\n");
        out.push_str("# TYPE powdb_queries_total counter\n");
        let _ = writeln!(
            out,
            "powdb_queries_total{{result=\"ok\"}} {}",
            self.queries_ok_total.load(Relaxed)
        );
        let _ = writeln!(
            out,
            "powdb_queries_total{{result=\"error\"}} {}",
            self.queries_error_total.load(Relaxed)
        );

        gauge_u64(
            &mut out,
            "powdb_queries_in_flight",
            "Queries currently executing (saturation behind the engine lock).",
            self.queries_in_flight.load(Relaxed),
        );
        counter(
            &mut out,
            "powdb_query_timeouts_total",
            "Total queries whose execution exceeded the configured query timeout threshold.",
            self.query_timeouts_total.load(Relaxed),
        );
        counter(
            &mut out,
            "powdb_query_memory_limit_exceeded_total",
            "Total queries rejected by the per-query memory budget.",
            self.query_memory_limit_exceeded_total.load(Relaxed),
        );
        counter(
            &mut out,
            "powdb_tx_gate_timeouts_total",
            "Total frames that gave up waiting on the transaction gate, across every frontend that waits on it (explicit BEGIN, bare autocommit statement, private sync frame).",
            self.tx_gate_timeouts_total.load(Relaxed),
        );
        counter(
            &mut out,
            "powdb_tx_reaped_total",
            "Total explicit transactions rolled back for exceeding POWDB_TX_MAX_LIFETIME_MS.",
            self.tx_reaped_total.load(Relaxed),
        );
        counter(
            &mut out,
            "powdb_auth_failures_total",
            "Total authentication failures.",
            self.auth_failures_total.load(Relaxed),
        );

        // Latency histogram. Derive count and +Inf from the same bucket reads
        // so +Inf == _count in this exposition no matter what observers do.
        out.push_str("# HELP powdb_query_duration_seconds Query execution time in seconds.\n");
        out.push_str("# TYPE powdb_query_duration_seconds histogram\n");
        let mut cumulative = 0u64;
        for (i, &bound) in LATENCY_BUCKETS.iter().enumerate() {
            cumulative += self.latency_buckets[i].load(Relaxed);
            let _ = writeln!(
                out,
                "powdb_query_duration_seconds_bucket{{le=\"{bound}\"}} {cumulative}"
            );
        }
        cumulative += self.latency_buckets[LATENCY_BUCKETS.len()].load(Relaxed);
        let _ = writeln!(
            out,
            "powdb_query_duration_seconds_bucket{{le=\"+Inf\"}} {cumulative}"
        );
        let sum_secs = self.latency_sum_nanos.load(Relaxed) as f64 / 1e9;
        let _ = writeln!(out, "powdb_query_duration_seconds_sum {sum_secs}");
        let _ = writeln!(out, "powdb_query_duration_seconds_count {cumulative}");

        // Storage. The two gauges stat the data directory at scrape time; the
        // fsync counters are lock-free reads of the storage layer's
        // process-wide accounting. Neither touches the engine `RwLock`.
        if let Some(dir) = &self.data_dir {
            let sizes = DataDirSizes::sample(dir);
            gauge_u64(
                &mut out,
                "powdb_database_size_bytes",
                "Size on disk of the data directory excluding the write-ahead log.",
                sizes.database_bytes,
            );
            gauge_u64(
                &mut out,
                "powdb_wal_size_bytes",
                "Size on disk of the write-ahead log.",
                sizes.wal_bytes,
            );
        }

        let fsync = powdb_storage::wal::wal_fsync_stats();
        counter(
            &mut out,
            "powdb_wal_fsync_total",
            "Total WAL fsyncs issued (group-commit leaders plus background flushes).",
            fsync.count,
        );
        counter_f64(
            &mut out,
            "powdb_wal_fsync_seconds_total",
            "Total seconds spent inside WAL fsync. Divide its rate by the rate of powdb_wal_fsync_total for mean fsync latency.",
            fsync.nanos as f64 / 1e9,
        );
        counter(
            &mut out,
            "powdb_wal_fsync_failures_total",
            "Total WAL fsyncs that returned an error (commits may not be durable).",
            fsync.failures,
        );

        out.push_str("# HELP powdb_sync_operations_total Total private sync protocol operations, by operation and result.\n");
        out.push_str("# TYPE powdb_sync_operations_total counter\n");
        for operation in SyncOperation::ALL {
            for outcome in SyncOutcome::ALL {
                let _ = writeln!(
                    out,
                    "powdb_sync_operations_total{{operation=\"{}\",result=\"{}\"}} {}",
                    operation.as_label(),
                    outcome.as_label(),
                    self.sync_operations_total[operation.idx()][outcome.idx()].load(Relaxed)
                );
            }
        }

        counter(
            &mut out,
            "powdb_sync_pull_units_total",
            "Total retained units served by private sync pull responses.",
            self.sync_pull_units_total.load(Relaxed),
        );
        counter(
            &mut out,
            "powdb_sync_pull_bytes_total",
            "Total retained-unit wire payload bytes served by private sync pull responses.",
            self.sync_pull_bytes_total.load(Relaxed),
        );
        counter(
            &mut out,
            "powdb_sync_ack_advanced_total",
            "Total sync acknowledgements that advanced a replica cursor.",
            self.sync_ack_advanced_total.load(Relaxed),
        );

        out.push_str("# HELP powdb_sync_repair_actions_total Total sync status repair actions returned by operation.\n");
        out.push_str("# TYPE powdb_sync_repair_actions_total counter\n");
        for operation in SyncOperation::ALL {
            for repair in SyncRepairLabel::ALL {
                let _ = writeln!(
                    out,
                    "powdb_sync_repair_actions_total{{operation=\"{}\",repair_action=\"{}\"}} {}",
                    operation.as_label(),
                    repair.as_label(),
                    self.sync_repair_actions_total[operation.idx()][repair.idx()].load(Relaxed)
                );
            }
        }

        out.push_str("# HELP powdb_sync_operation_duration_seconds Private sync protocol operation time in seconds.\n");
        out.push_str("# TYPE powdb_sync_operation_duration_seconds histogram\n");
        for operation in SyncOperation::ALL {
            let mut cumulative = 0u64;
            for (i, &bound) in LATENCY_BUCKETS.iter().enumerate() {
                cumulative += self.sync_latency_buckets[operation.idx()][i].load(Relaxed);
                let _ = writeln!(
                    out,
                    "powdb_sync_operation_duration_seconds_bucket{{operation=\"{}\",le=\"{bound}\"}} {cumulative}",
                    operation.as_label()
                );
            }
            cumulative +=
                self.sync_latency_buckets[operation.idx()][LATENCY_BUCKETS.len()].load(Relaxed);
            let _ = writeln!(
                out,
                "powdb_sync_operation_duration_seconds_bucket{{operation=\"{}\",le=\"+Inf\"}} {cumulative}",
                operation.as_label()
            );
            let sum_secs = self.sync_latency_sum_nanos[operation.idx()].load(Relaxed) as f64 / 1e9;
            let _ = writeln!(
                out,
                "powdb_sync_operation_duration_seconds_sum{{operation=\"{}\"}} {sum_secs}",
                operation.as_label()
            );
            let _ = writeln!(
                out,
                "powdb_sync_operation_duration_seconds_count{{operation=\"{}\"}} {cumulative}",
                operation.as_label()
            );
        }

        out
    }
}

/// Bytes on disk under the data directory, split into the write-ahead log and
/// everything else (heaps, indexes, catalog, sync segments).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DataDirSizes {
    database_bytes: u64,
    wal_bytes: u64,
}

impl DataDirSizes {
    /// Walk `dir` and total the file sizes. Unreadable entries are skipped
    /// rather than failing the scrape: a metrics endpoint that 500s because a
    /// file was being renamed is worse than a gauge that is briefly low. The
    /// walk is bounded by `MAX_SIZED_ENTRIES` so the scrape cost stays flat.
    fn sample(dir: &Path) -> Self {
        let mut sizes = DataDirSizes::default();
        let mut stack = vec![dir.to_path_buf()];
        let mut visited = 0usize;
        while let Some(current) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            for entry in entries.flatten() {
                visited += 1;
                if visited > MAX_SIZED_ENTRIES {
                    return sizes;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    stack.push(entry.path());
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if entry.file_name() == WAL_FILE_NAME {
                    sizes.wal_bytes += meta.len();
                } else {
                    sizes.database_bytes += meta.len();
                }
            }
        }
        sizes
    }
}

/// Decrements `connections_active` when dropped.
pub struct ActiveGuard(Arc<Metrics>);
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.connections_active.fetch_sub(1, Relaxed);
    }
}

/// Decrements `queries_in_flight` when dropped.
pub struct InFlightGuard(Arc<Metrics>);
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.queries_in_flight.fetch_sub(1, Relaxed);
    }
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {value}");
}

fn counter_f64(out: &mut String, name: &str, help: &str, value: f64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {value}");
}

fn gauge_u64(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
    let _ = writeln!(out, "{name} {value}");
}

fn gauge_f64(out: &mut String, name: &str, help: &str, value: f64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
    let _ = writeln!(out, "{name} {value}");
}

/// Escape a Prometheus label value: backslash, double-quote, newline.
fn escape_label(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Serve `GET /metrics` over `listener` until the shutdown signal flips. Each
/// connection is handled on its own task so one slow client can't wedge the
/// accept loop; the listener drains on SIGINT/SIGTERM via the watch channel.
pub async fn serve_metrics(
    listener: TcpListener,
    metrics: Arc<Metrics>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        let m = metrics.clone();
                        tokio::spawn(async move { handle_scrape(stream, m).await; });
                    }
                    Err(e) => debug!(error = %e, "metrics accept error"),
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
}

/// Handle one scrape connection: read just the request line (capped + timed
/// out), answer `GET /metrics` with the exposition, everything else with 404.
async fn handle_scrape<S>(mut stream: S, metrics: Arc<Metrics>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0u8; 1024];

    let request_line = loop {
        if let Some(end) = find_line_end(&buf) {
            break String::from_utf8_lossy(&buf[..end]).into_owned();
        }
        if buf.len() >= MAX_REQUEST_BYTES {
            let _ = respond(&mut stream, 400, "text/plain", "request too large\n").await;
            return;
        }
        match tokio::time::timeout(READ_TIMEOUT, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => return, // EOF before a complete request line
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) => {
                debug!(error = %e, "metrics read error");
                return;
            }
            Err(_) => {
                debug!("metrics read timeout (slow client)");
                return;
            }
        }
    };

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    if method == "GET" && path == "/metrics" {
        let body = metrics.render();
        let _ = respond(
            &mut stream,
            200,
            "text/plain; version=0.0.4; charset=utf-8",
            &body,
        )
        .await;
    } else if method == "GET" && (path == "/health" || path == "/healthz") {
        // Liveness probe for k8s/Fly/Docker. Unauthenticated and deliberately
        // cheap: it answers from the metrics task alone and never touches the
        // engine lock, so a long-running query cannot make the process look
        // dead and get it restarted mid-write. `/healthz` is accepted as the
        // conventional Kubernetes spelling.
        let body = format!("ok powdb {}\n", metrics.version());
        let _ = respond(&mut stream, 200, "text/plain; charset=utf-8", &body).await;
    } else {
        let _ = respond(&mut stream, 404, "text/plain", "not found\n").await;
    }
}

/// Index just past the request line (position of `\n`, with a trailing `\r`
/// trimmed). Handles `\r\n` and bare `\n`.
fn find_line_end(buf: &[u8]) -> Option<usize> {
    buf.iter().position(|&b| b == b'\n').map(|nl| {
        if nl > 0 && buf[nl - 1] == b'\r' {
            nl - 1
        } else {
            nl
        }
    })
}

async fn respond<S>(
    stream: &mut S,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_has_required_families_and_help_type() {
        let m = Metrics::new();
        let out = m.render();
        for name in [
            "powdb_build_info",
            "powdb_uptime_seconds",
            "powdb_connections_active",
            "powdb_connections_accepted_total",
            "powdb_tls_handshake_failures_total",
            "powdb_queries_total",
            "powdb_queries_in_flight",
            "powdb_query_timeouts_total",
            "powdb_query_memory_limit_exceeded_total",
            "powdb_tx_gate_timeouts_total",
            "powdb_tx_reaped_total",
            "powdb_auth_failures_total",
            "powdb_query_duration_seconds",
            "powdb_sync_operations_total",
            "powdb_sync_pull_units_total",
            "powdb_sync_pull_bytes_total",
            "powdb_sync_ack_advanced_total",
            "powdb_sync_repair_actions_total",
            "powdb_sync_operation_duration_seconds",
            "powdb_wal_fsync_total",
            "powdb_wal_fsync_seconds_total",
            "powdb_wal_fsync_failures_total",
        ] {
            assert!(
                out.contains(&format!("# HELP {name}")),
                "missing HELP {name}"
            );
            assert!(
                out.contains(&format!("# TYPE {name}")),
                "missing TYPE {name}"
            );
        }
        // Both label values always present.
        assert!(out.contains("powdb_queries_total{result=\"ok\"} 0"));
        assert!(out.contains("powdb_queries_total{result=\"error\"} 0"));
        assert!(out.contains("powdb_sync_operations_total{operation=\"status\",result=\"ok\"} 0"));
        assert!(out.contains("powdb_sync_operations_total{operation=\"pull\",result=\"error\"} 0"));
        assert!(out.contains("powdb_sync_operations_total{operation=\"ack\",result=\"ok\"} 0"));
        assert!(out.contains(
            "powdb_sync_repair_actions_total{operation=\"status\",repair_action=\"pull\"} 0"
        ));
        assert!(out.contains(
            "powdb_sync_repair_actions_total{operation=\"pull\",repair_action=\"rebootstrap\"} 0"
        ));
        assert!(out.contains("powdb_build_info{version=\""));
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_inf_equals_count() {
        let m = Metrics::new();
        m.record_query(Duration::from_micros(300), QueryOutcome::Ok); // <= 0.0005
        m.record_query(Duration::from_millis(3), QueryOutcome::Ok); // <= 0.005
        m.record_query(Duration::from_secs(10), QueryOutcome::Error); // overflow (> 5s)
        let out = m.render();

        // le="0.0005" has 1; le="0.005" is cumulative => 2; +Inf => 3 == count.
        assert!(out.contains("powdb_query_duration_seconds_bucket{le=\"0.0005\"} 1"));
        assert!(out.contains("powdb_query_duration_seconds_bucket{le=\"0.005\"} 2"));
        assert!(out.contains("powdb_query_duration_seconds_bucket{le=\"+Inf\"} 3"));
        assert!(out.contains("powdb_query_duration_seconds_count 3"));
        // queries_total split: 2 ok, 1 error.
        assert!(out.contains("powdb_queries_total{result=\"ok\"} 2"));
        assert!(out.contains("powdb_queries_total{result=\"error\"} 1"));
    }

    #[test]
    fn outcome_counters_route_correctly() {
        let m = Metrics::new();
        m.record_query(Duration::from_millis(1), QueryOutcome::Timeout);
        m.record_query(Duration::from_millis(1), QueryOutcome::MemoryLimit);
        let out = m.render();
        // Both count as errors, plus their specific counters.
        assert!(out.contains("powdb_queries_total{result=\"error\"} 2"));
        assert!(out.contains("powdb_query_timeouts_total 1"));
        assert!(out.contains("powdb_query_memory_limit_exceeded_total 1"));
    }

    #[test]
    fn tx_gate_timeout_counts_as_error_and_dedicated_counter() {
        let m = Metrics::new();
        m.inc_tx_gate_timeout();
        m.inc_tx_gate_timeout();
        let out = m.render();
        assert!(out.contains("powdb_tx_gate_timeouts_total 2"));
        // Each timed-out begin is also a failed query.
        assert!(out.contains("powdb_queries_total{result=\"error\"} 2"));
    }

    /// A reaped transaction and a gate timeout are opposite events (one held
    /// the gate, the other waited for it) and must never share a counter.
    #[test]
    fn reaped_transactions_have_their_own_counter() {
        let m = Metrics::new();
        m.inc_tx_reaped();
        let out = m.render();
        assert!(out.contains("powdb_tx_reaped_total 1"));
        assert!(out.contains("powdb_tx_gate_timeouts_total 0"));
        assert!(out.contains("powdb_queries_total{result=\"error\"} 1"));
    }

    #[test]
    fn sync_operation_metrics_route_and_bucket_correctly() {
        let m = Metrics::new();
        m.record_sync_operation(
            SyncOperation::Status,
            Duration::from_micros(400),
            SyncOutcome::Ok,
        );
        m.record_sync_operation(
            SyncOperation::Pull,
            Duration::from_millis(2),
            SyncOutcome::Ok,
        );
        m.record_sync_operation(
            SyncOperation::Ack,
            Duration::from_secs(7),
            SyncOutcome::Error,
        );
        m.record_sync_pull_payload(3, 1234);
        m.record_sync_repair_action(SyncOperation::Status, SyncRepairLabel::Pull);
        m.record_sync_repair_action(SyncOperation::Pull, SyncRepairLabel::Rebootstrap);
        m.inc_sync_ack_advanced();

        let out = m.render();
        assert!(out.contains("powdb_sync_operations_total{operation=\"status\",result=\"ok\"} 1"));
        assert!(out.contains("powdb_sync_operations_total{operation=\"pull\",result=\"ok\"} 1"));
        assert!(out.contains("powdb_sync_operations_total{operation=\"ack\",result=\"error\"} 1"));
        assert!(out.contains("powdb_sync_pull_units_total 3"));
        assert!(out.contains("powdb_sync_pull_bytes_total 1234"));
        assert!(out.contains("powdb_sync_ack_advanced_total 1"));
        assert!(out.contains(
            "powdb_sync_repair_actions_total{operation=\"status\",repair_action=\"pull\"} 1"
        ));
        assert!(out.contains(
            "powdb_sync_repair_actions_total{operation=\"pull\",repair_action=\"rebootstrap\"} 1"
        ));
        assert!(out.contains(
            "powdb_sync_operation_duration_seconds_bucket{operation=\"status\",le=\"0.0005\"} 1"
        ));
        assert!(out.contains(
            "powdb_sync_operation_duration_seconds_bucket{operation=\"ack\",le=\"+Inf\"} 1"
        ));
        assert!(out.contains("powdb_sync_operation_duration_seconds_count{operation=\"pull\"} 1"));
    }

    #[test]
    fn guards_increment_and_decrement() {
        let m = Arc::new(Metrics::new());
        {
            let _a = m.active_guard();
            let _b = m.active_guard();
            let _f = m.in_flight_guard();
            assert!(m.render().contains("powdb_connections_active 2"));
            assert!(m.render().contains("powdb_queries_in_flight 1"));
        }
        assert!(m.render().contains("powdb_connections_active 0"));
        assert!(m.render().contains("powdb_queries_in_flight 0"));
    }

    #[test]
    fn escape_label_escapes_special_chars() {
        assert_eq!(escape_label(r#"a\b"c"#), r#"a\\b\"c"#);
        assert_eq!(escape_label("a\nb"), "a\\nb");
        assert_eq!(escape_label("0.5.1"), "0.5.1");
    }

    #[test]
    fn find_line_end_handles_crlf_lf_and_none() {
        assert_eq!(find_line_end(b"GET / HTTP/1.1\r\n"), Some(14));
        assert_eq!(find_line_end(b"GET / HTTP/1.1\n"), Some(14));
        assert_eq!(find_line_end(b"GET / HTTP/1.1"), None);
        assert_eq!(find_line_end(b"\n"), Some(0));
    }

    #[test]
    fn concurrent_record_query_is_consistent_at_quiescence() {
        use std::thread;
        let m = Arc::new(Metrics::new());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let m = m.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    m.record_query(Duration::from_millis(2), QueryOutcome::Ok);
                }
            }));
        }
        // Concurrent scrapes must never panic or produce +Inf < count.
        for _ in 0..50 {
            let _ = m.render();
        }
        for h in handles {
            h.join().unwrap();
        }
        let out = m.render();
        assert!(out.contains("powdb_queries_total{result=\"ok\"} 8000"));
        assert!(out.contains("powdb_query_duration_seconds_count 8000"));
        assert!(out.contains("powdb_query_duration_seconds_bucket{le=\"+Inf\"} 8000"));
    }

    #[tokio::test]
    async fn handle_scrape_serves_metrics_and_404s_other_paths() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // GET /metrics -> 200 + exposition.
        let (mut client, server) = tokio::io::duplex(8192);
        let m = Arc::new(Metrics::new());
        let task = tokio::spawn(handle_scrape(server, m));
        client
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut resp = String::new();
        client.read_to_string(&mut resp).await.unwrap();
        task.await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "resp: {resp}");
        assert!(resp.contains("powdb_build_info"));

        // Unknown path -> 404.
        let (mut client, server) = tokio::io::duplex(8192);
        let m = Arc::new(Metrics::new());
        let task = tokio::spawn(handle_scrape(server, m));
        client
            .write_all(b"GET /nope HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut resp = String::new();
        client.read_to_string(&mut resp).await.unwrap();
        task.await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 404 Not Found"), "resp: {resp}");
    }

    #[test]
    fn storage_size_gauges_appear_only_with_a_data_dir() {
        let m = Metrics::new();
        let out = m.render();
        assert!(
            !out.contains("powdb_database_size_bytes"),
            "size gauges must be omitted when no data dir is configured"
        );

        let dir = std::env::temp_dir().join(format!("powdb_metrics_size_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("T.heap"), vec![7u8; 4096]).unwrap();
        std::fs::write(dir.join(WAL_FILE_NAME), vec![7u8; 100]).unwrap();

        let m = Metrics::new().with_data_dir(&dir);
        let out = m.render();
        assert!(out.contains("powdb_database_size_bytes 4096"), "out: {out}");
        assert!(out.contains("powdb_wal_size_bytes 100"), "out: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn data_dir_sizes_split_wal_from_the_rest_and_recurse() {
        let dir = std::env::temp_dir().join(format!("powdb_metrics_walk_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sync")).unwrap();
        std::fs::write(dir.join("catalog.bin"), vec![0u8; 10]).unwrap();
        std::fs::write(dir.join(WAL_FILE_NAME), vec![0u8; 20]).unwrap();
        std::fs::write(dir.join("sync").join("segment-1"), vec![0u8; 30]).unwrap();

        let sizes = DataDirSizes::sample(&dir);
        assert_eq!(sizes.wal_bytes, 20);
        assert_eq!(sizes.database_bytes, 40, "catalog plus nested sync segment");

        // A missing directory must not panic or fail the scrape.
        assert_eq!(
            DataDirSizes::sample(&dir.join("nope")),
            DataDirSizes::default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn handle_scrape_serves_health_for_liveness_probes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for path in ["/health", "/healthz"] {
            let (mut client, server) = tokio::io::duplex(8192);
            let m = Arc::new(Metrics::new());
            let task = tokio::spawn(handle_scrape(server, m));
            client
                .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut resp = String::new();
            client.read_to_string(&mut resp).await.unwrap();
            task.await.unwrap();
            assert!(resp.starts_with("HTTP/1.1 200 OK"), "{path} resp: {resp}");
            assert!(resp.contains("ok powdb "), "{path} resp: {resp}");
        }
    }

    #[tokio::test]
    async fn handle_scrape_rejects_garbage_without_panicking() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client, server) = tokio::io::duplex(8192);
        let m = Arc::new(Metrics::new());
        let task = tokio::spawn(handle_scrape(server, m));
        client
            .write_all(b"\x00\x01\x02 garbage\r\n\r\n")
            .await
            .unwrap();
        let mut resp = String::new();
        client.read_to_string(&mut resp).await.unwrap();
        task.await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 404"), "resp: {resp}");
    }
}
