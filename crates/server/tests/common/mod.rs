//! Shared helpers for the powdb-server integration tests.
//!
//! Every integration-test binary compiles its own copy of this module, so
//! helpers unused by a given binary are expected; hence the module-wide
//! `allow(dead_code)`.

#![allow(dead_code)]

use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use powdb_query::executor::Engine;
use powdb_server::handler::{handle_connection, new_tx_gate, AuthRateLimiter, ConnOpts, TxGate};
use powdb_server::metrics::Metrics;
use powdb_server::protocol::Message;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Wire-protocol frame encoding
// ---------------------------------------------------------------------------

/// Encode a CONNECT frame by hand (raw framing, empty password = None) so the
/// tests exercise the byte-level wire format rather than only the encoder.
pub fn encode_connect(db: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(db.len() as u32).to_le_bytes());
    payload.extend_from_slice(db.as_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes()); // empty password => None
    let mut frame = Vec::new();
    frame.push(0x01); // CONNECT
    frame.push(0); // flags
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// Encode a CONNECT frame carrying db_name + password through the real wire
/// encoder.
pub fn encode_connect_with_password(db: &str, password: &str) -> Vec<u8> {
    Message::Connect {
        db_name: db.to_string(),
        password: Some(zeroize::Zeroizing::new(password.to_string())),
        username: None,
    }
    .encode()
}

/// Encode a CONNECT frame carrying db_name + password + username, exercising
/// the real wire encoder so the test depends on the actual protocol.
pub fn encode_connect_user(db: &str, password: &str, username: &str) -> Vec<u8> {
    Message::Connect {
        db_name: db.to_string(),
        password: Some(zeroize::Zeroizing::new(password.to_string())),
        username: Some(username.to_string()),
    }
    .encode()
}

/// Encode a QUERY frame by hand (raw framing).
pub fn encode_query(q: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(q.len() as u32).to_le_bytes());
    payload.extend_from_slice(q.as_bytes());
    let mut frame = Vec::new();
    frame.push(0x03); // QUERY
    frame.push(0);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// Encode a PING frame by hand (empty payload).
pub fn encode_ping() -> Vec<u8> {
    vec![0x11, 0, 0, 0, 0, 0]
}

// ---------------------------------------------------------------------------
// Wire-protocol frame reading
// ---------------------------------------------------------------------------

/// Read one full response frame (header + payload) from any stream type
/// (TcpStream, UnixStream, TLS-wrapped). Panics on I/O error.
pub async fn read_response<S: AsyncReadExt + Unpin>(stream: &mut S) -> Vec<u8> {
    let mut header = [0u8; 6];
    stream.read_exact(&mut header).await.unwrap();
    let payload_len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).await.unwrap();
    }
    let mut full = Vec::with_capacity(6 + payload_len);
    full.extend_from_slice(&header);
    full.extend_from_slice(&payload);
    full
}

/// Read one response frame and decode it. Panics on I/O or decode error.
pub async fn read_response_message<S: AsyncReadExt + Unpin>(stream: &mut S) -> Message {
    Message::decode(&read_response(stream).await).unwrap()
}

/// Read one response frame and decode it, returning `None` on EOF or any
/// read/decode failure (used by tests that treat a closed connection as an
/// acceptable outcome).
pub async fn read_message<S: AsyncReadExt + Unpin>(stream: &mut S) -> Option<Message> {
    let mut header = [0u8; 6];
    match stream.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return None,
        Err(_) => return None,
    }
    let payload_len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 && stream.read_exact(&mut payload).await.is_err() {
        return None;
    }
    let mut full = Vec::with_capacity(6 + payload_len);
    full.extend_from_slice(&header);
    full.extend_from_slice(&payload);
    Message::decode(&full).ok()
}

// ---------------------------------------------------------------------------
// Engine / temp-dir fixtures
// ---------------------------------------------------------------------------

/// A fresh engine on a fresh temp dir. Keep the TempDir alive for the test.
pub fn fresh_engine() -> (Arc<RwLock<Engine>>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let engine = Arc::new(RwLock::new(Engine::new(tmp.path()).unwrap()));
    (engine, tmp)
}

/// A uniquely-named temp dir path (pid + nanos) that survives until the test
/// removes it. For tests that manage the directory lifetime themselves.
pub fn unique_temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

// ---------------------------------------------------------------------------
// In-process test server
// ---------------------------------------------------------------------------

/// Configuration for an in-process test server driving the real
/// `handle_connection` loop. `Default` matches the settings the plain
/// integration tests always used (no auth, generous timeouts).
pub struct InprocServer {
    pub tx_wait_timeout: Duration,
    pub idle_timeout: Duration,
    pub preauth_deadline: Duration,
    pub query_timeout: Duration,
    pub expected_password: Option<String>,
    pub users: Arc<powdb_auth::UserStore>,
    pub metrics: Arc<Metrics>,
    pub rate_limiter: Option<AuthRateLimiter>,
    /// External shutdown signal. When `None`, each connection gets its own
    /// channel whose sender is kept alive for the connection's lifetime (a
    /// dropped sender makes `changed()` resolve immediately forever,
    /// busy-spinning the connection loop and cancelling in-flight frame
    /// reads, which loses partially-read bytes).
    pub shutdown_rx: Option<tokio::sync::watch::Receiver<bool>>,
    /// Accept exactly one connection instead of looping.
    pub single_conn: bool,
}

impl Default for InprocServer {
    fn default() -> Self {
        Self {
            tx_wait_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(300),
            preauth_deadline: powdb_server::handler::DEFAULT_PREAUTH_DEADLINE,
            query_timeout: Duration::from_secs(30),
            expected_password: None,
            users: Arc::new(powdb_auth::UserStore::new()),
            metrics: Arc::new(Metrics::new()),
            rate_limiter: None,
            shutdown_rx: None,
            single_conn: false,
        }
    }
}

async fn serve_one(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    engine: Arc<RwLock<Engine>>,
    tx_gate: TxGate,
    cfg: &InprocServer,
) {
    // Keep a sender alive for the connection when no external signal is
    // provided (see the `shutdown_rx` field docs).
    let (own_tx, own_rx) = tokio::sync::watch::channel(false);
    let _keep_shutdown_alive = own_tx;
    let mut rx = cfg.shutdown_rx.clone().unwrap_or(own_rx);
    handle_connection(
        stream,
        ConnOpts {
            tx_wait_timeout: cfg.tx_wait_timeout,
            db_name: None,
            engine,
            tx_gate,
            expected_password: cfg.expected_password.clone().map(zeroize::Zeroizing::new),
            users: cfg.users.clone(),
            shutdown_rx: &mut rx,
            idle_timeout: cfg.idle_timeout,
            preauth_deadline: cfg.preauth_deadline,
            query_timeout: cfg.query_timeout,
            rate_limiter: cfg.rate_limiter.as_ref(),
            peer_addr: Some(peer),
            metrics: cfg.metrics.clone(),
        },
    )
    .await;
}

impl InprocServer {
    /// Bind an OS-assigned port and serve connections with the real
    /// `handle_connection` loop. Binding happens before this returns, so the
    /// address is immediately connectable (no bind-race sleeps needed).
    pub async fn start(
        self,
        engine: Arc<RwLock<Engine>>,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tx_gate = new_tx_gate();
        let cfg = self;
        let handle = tokio::spawn(async move {
            if cfg.single_conn {
                let (stream, peer) = listener.accept().await.unwrap();
                serve_one(stream, peer, engine, tx_gate, &cfg).await;
            } else {
                let cfg = Arc::new(cfg);
                loop {
                    let (stream, peer) = match listener.accept().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let engine = engine.clone();
                    let tx_gate = tx_gate.clone();
                    let cfg = cfg.clone();
                    tokio::spawn(async move {
                        serve_one(stream, peer, engine, tx_gate, &cfg).await;
                    });
                }
            }
        });
        (addr, handle)
    }
}

// ---------------------------------------------------------------------------
// Real-binary process helpers
// ---------------------------------------------------------------------------

/// Sequence number for unique per-spawn port files.
static PORT_FILE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Spawn the real `powdb-server` binary on an OS-assigned port (`--port 0` +
/// `--port-file`) and return it together with the port it actually bound.
///
/// Never probe a free port and re-bind it: ephemeral ports are machine-global,
/// so with many test binaries spawning servers concurrently the released port
/// can be handed to another test's server and this test's connection lands on
/// a listener that is about to be killed (the `wire_error_class_from_type`
/// ConnectionReset flake on CI). Letting the server bind port 0 and report the
/// result removes the race entirely.
pub fn spawn_server_bound_env(
    data_dir: &std::path::Path,
    extra_args: &[&str],
    extra_env: &[(&str, &str)],
) -> (Child, u16) {
    let (child, port, _) = spawn_server_bound_inner(data_dir, extra_args, extra_env, false);
    (child, port)
}

/// [`spawn_server_bound_env`] without extra environment variables.
pub fn spawn_server_bound(data_dir: &std::path::Path, extra_args: &[&str]) -> (Child, u16) {
    spawn_server_bound_env(data_dir, extra_args, &[])
}

/// [`spawn_server_bound_env`] plus a metrics endpoint on its own OS-assigned
/// port; returns `(child, port, metrics_port)`.
pub fn spawn_server_bound_with_metrics(
    data_dir: &std::path::Path,
    extra_args: &[&str],
    extra_env: &[(&str, &str)],
) -> (Child, u16, u16) {
    let (child, port, metrics) = spawn_server_bound_inner(data_dir, extra_args, extra_env, true);
    (
        child,
        port,
        metrics.expect("metrics endpoint was requested"),
    )
}

fn spawn_server_bound_inner(
    data_dir: &std::path::Path,
    extra_args: &[&str],
    extra_env: &[(&str, &str)],
    metrics: bool,
) -> (Child, u16, Option<u16>) {
    let seq = PORT_FILE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let port_file =
        std::env::temp_dir().join(format!("powdb_test_ports_{}_{seq}", std::process::id()));
    let port_file_arg = port_file.to_str().unwrap().to_string();
    let mut args: Vec<&str> = vec!["--port-file", &port_file_arg];
    if metrics {
        args.extend(["--metrics-addr", "127.0.0.1:0"]);
    }
    args.extend_from_slice(extra_args);
    let mut child = spawn_server_bin_env(0, data_dir, &args, extra_env);

    let start = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(&port_file) {
            // The write is atomic (write + rename), so any readable content
            // is complete.
            let mut port = None;
            let mut metrics_port = None;
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("port=") {
                    port = v.parse::<u16>().ok();
                } else if let Some(v) = line.strip_prefix("metrics=") {
                    metrics_port = v.parse::<u16>().ok();
                }
            }
            let port = port.expect("port file must carry the bound port");
            std::fs::remove_file(&port_file).ok();
            return (child, port, metrics_port);
        }
        if let Ok(Some(status)) = child.try_wait() {
            std::fs::remove_file(&port_file).ok();
            panic!("powdb-server exited before publishing its bound port: {status}");
        }
        assert!(
            start.elapsed() <= Duration::from_secs(30),
            "powdb-server did not publish its bound port within 30s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Spawn the real `powdb-server` binary on `port`/`data_dir` with any extra
/// CLI arguments (e.g. `--socket`, `--metrics-addr`, `--readonly`) and extra
/// environment variables.
pub fn spawn_server_bin_env(
    port: u16,
    data_dir: &std::path::Path,
    extra_args: &[&str],
    extra_env: &[(&str, &str)],
) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_powdb-server"));
    cmd.arg("--port")
        .arg(port.to_string())
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--data-dir")
        .arg(data_dir)
        .args(extra_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.spawn().expect("spawn powdb-server binary")
}

/// Spawn the real `powdb-server` binary with extra CLI arguments only.
pub fn spawn_server_bin(port: u16, data_dir: &std::path::Path, extra_args: &[&str]) -> Child {
    spawn_server_bin_env(port, data_dir, extra_args, &[])
}

/// Poll-connect until the server accepts on `port`, failing loudly after
/// `within`.
pub async fn wait_for_bind(port: u16, within: Duration) -> TcpStream {
    let start = Instant::now();
    loop {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)).await {
            return stream;
        }
        assert!(
            start.elapsed() <= within,
            "server did not accept connections on port {port} within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll-connect until the server accepts on the Unix socket at `path`.
#[cfg(unix)]
pub async fn wait_for_socket(path: &std::path::Path, within: Duration) -> tokio::net::UnixStream {
    let start = Instant::now();
    loop {
        if let Ok(stream) = tokio::net::UnixStream::connect(path).await {
            return stream;
        }
        assert!(
            start.elapsed() <= within,
            "server did not accept connections on socket {path:?} within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Send SIGTERM to the child (the signal Docker/Kubernetes/systemd use).
#[cfg(unix)]
pub fn send_sigterm(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("invoke kill");
    assert!(status.success(), "kill -TERM failed");
}

/// Send SIGKILL to the child: no drop handlers, no graceful drain. This is
/// the real crash signal for durability tests.
#[cfg(unix)]
pub fn send_sigkill(child: &Child) {
    let status = Command::new("kill")
        .arg("-9")
        .arg(child.id().to_string())
        .status()
        .expect("invoke kill");
    assert!(status.success(), "kill -9 failed");
}

/// Poll for process exit with a hard timeout so a hang fails the test loudly
/// instead of wedging CI.
pub fn wait_with_timeout(child: &mut Child, timeout: Duration) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            panic!("server did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
