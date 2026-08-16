use powdb_query::executor::{Engine, WalSyncMode};
use powdb_server::handler;
use powdb_server::metrics::{serve_metrics, Metrics};
use std::io;
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{watch, Semaphore};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use zeroize::Zeroizing;

/// Maximum number of concurrent connections.
const MAX_CONNECTIONS: usize = 1024;

/// Hard deadline for the TLS handshake. A connection permit is held from the
/// moment the socket is accepted, so a peer that connects over TLS and then
/// stalls would otherwise pin one of the `MAX_CONNECTIONS` permits forever:
/// 1024 silent sockets take the server offline before a single byte of the
/// wire protocol is read. Ten seconds is far beyond any real handshake
/// (typically a few round trips) and far below the 300s connection idle
/// timeout, which does not start until the handshake returns.
const TLS_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

struct Args {
    port: u16,
    bind: String,
    data_dir: String,
    /// Client password, wrapped so it is zeroized from memory on drop.
    password: Option<Zeroizing<String>>,
    idle_timeout_secs: u64,
    query_timeout_secs: u64,
    /// How long an explicit `begin` waits for a concurrent explicit transaction
    /// on another connection before failing with a clear timeout error.
    tx_wait_timeout_ms: u64,
    /// Ceiling on how long one connection may hold the transaction gate inside
    /// an explicit transaction before the server rolls it back; env-only.
    /// `None` disables the bound (`POWDB_TX_MAX_LIFETIME_MS=0`).
    tx_max_lifetime: Option<std::time::Duration>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    query_memory_limit: usize,
    /// Fallback nested-loop join candidate-pair cap; env-only. `None` keeps the
    /// engine default (`MAX_NESTED_LOOP_PAIRS`).
    nested_loop_pair_limit: Option<usize>,
    /// Ceiling in bytes on unflushed heap pages held across every table;
    /// env-only. `None` keeps the storage default
    /// (`DEFAULT_DIRTY_PAGE_BUDGET`).
    dirty_page_budget: Option<usize>,
    require_tls: bool,
    /// `host:port` for the optional Prometheus metrics endpoint; `None` = off.
    metrics_addr: Option<String>,
    /// Filesystem path for an optional Unix-domain-socket listener; `None` =
    /// off. Additive: the TCP listener always runs. UDS removes the TCP/IP
    /// stack from the same-host path (~2× lower round-trip latency).
    socket: Option<String>,
    /// When `Some`, the single database name this server serves. A CONNECT that
    /// explicitly names a different database is rejected. `None` = accept any
    /// name (0.9.x behavior).
    db_name: Option<String>,
    /// Serve the data directory **read-only** (snapshot serving). The engine is
    /// opened read-only, no writer admission is ever taken, and mutating
    /// statements return a terminal error. Set via `--readonly` or
    /// `POWDB_READONLY=1`.
    read_only: bool,
}

/// Default explicit-transaction gate wait (ms) when `POWDB_TX_WAIT_TIMEOUT_MS`
/// is unset or unparseable. A `begin` that waits longer than this for a
/// concurrent explicit transaction fails with a clear timeout error instead of
/// queueing indefinitely.
const DEFAULT_TX_WAIT_TIMEOUT_MS: u64 = 5000;

/// Default per-query memory budget (bytes) when `POWDB_QUERY_MEMORY_LIMIT` is
/// unset or unparseable. Mirrors the query crate's default (256 MB).
const DEFAULT_QUERY_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

fn archive_wal_records_if_sync_enabled(
    data_dir: &Path,
    records: &[powdb_storage::wal::WalRecord],
) -> io::Result<()> {
    match powdb_sync::read_identity(data_dir) {
        Ok(identity) => powdb_sync::archive_wal_records_for_identity(data_dir, identity, records),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Parse the per-query memory limit from the `POWDB_QUERY_MEMORY_LIMIT`
/// environment value. Accepts a plain byte count; falls back to the default
/// when unset, empty, or unparseable. Pulled out as a free function so it can
/// be unit-tested without spawning the server.
fn parse_query_memory_limit(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_QUERY_MEMORY_LIMIT)
}

/// Parse the `POWDB_TX_MAX_LIFETIME_MS` environment value: the ceiling on how
/// long one connection may hold the transaction gate inside an explicit
/// transaction before the server rolls that transaction back. An explicit `0`
/// disables the bound and restores the pre-0.22 behavior where the client
/// chose the hold duration; unset, empty, or unparseable keeps the default
/// (`handler::DEFAULT_TX_MAX_LIFETIME`). Pulled out as a free function so it
/// can be unit-tested without spawning the server, mirroring
/// [`parse_query_memory_limit`].
fn parse_tx_max_lifetime(raw: Option<&str>) -> Option<std::time::Duration> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(value) => match value.parse::<u64>() {
            Ok(0) => None,
            Ok(ms) => Some(std::time::Duration::from_millis(ms)),
            Err(_) => Some(handler::DEFAULT_TX_MAX_LIFETIME),
        },
        None => Some(handler::DEFAULT_TX_MAX_LIFETIME),
    }
}

/// Parse the `POWDB_MAX_NESTED_LOOP_PAIRS` environment value. Accepts a plain
/// positive candidate-pair count; `None` (unset, empty, unparseable, or zero)
/// leaves the engine default (`MAX_NESTED_LOOP_PAIRS`) in place. Pulled out as
/// a free function so it can be unit-tested without spawning the server,
/// mirroring [`parse_query_memory_limit`].
fn parse_nested_loop_pair_limit(raw: Option<&str>) -> Option<usize> {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
}

/// Parse the `POWDB_DIRTY_PAGE_BUDGET` environment value. Accepts a plain
/// positive byte count for the ceiling on unflushed heap pages held across
/// every table; `None` (unset, empty, unparseable, or zero) leaves the storage
/// default (`DEFAULT_DIRTY_PAGE_BUDGET`, 256 MiB) in place. Mirrors
/// [`parse_nested_loop_pair_limit`].
fn parse_dirty_page_budget(raw: Option<&str>) -> Option<usize> {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
}

/// Parse `POWDB_SYNC_MODE` (`full` | `normal` | `off`). Defaults to `Full` —
/// the safe, fully-durable mode — on unset/empty/unknown. `normal` trades a
/// bounded crash-loss window (OS-crash/power-loss only) for ~15–40× faster
/// writes; `off` disables durability entirely and is bench-only.
fn parse_sync_mode(raw: Option<&str>) -> WalSyncMode {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("normal") => WalSyncMode::Normal,
        Some("off") => WalSyncMode::Off,
        _ => WalSyncMode::Full,
    }
}

/// Parse the `POWDB_REQUIRE_TLS` env value. Truthy on `1`/`true` (any case);
/// default off (false) for backward compatibility.
fn parse_require_tls(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Enforce the TLS requirement at startup. When `require_tls` is set, a server
/// configured with any credential-based auth but no TLS cert/key would transmit
/// credentials in cleartext — refuse to start. Returns `Err` with a message
/// describing the misconfiguration; `Ok(())` otherwise.
fn check_tls_requirement(
    require_tls: bool,
    auth_configured: bool,
    tls_configured: bool,
) -> Result<(), String> {
    if require_tls && auth_configured && !tls_configured {
        return Err(
            "POWDB_REQUIRE_TLS is set but authentication is configured without TLS \
             (provide --tls-cert and --tls-key, or unset POWDB_REQUIRE_TLS)"
                .to_string(),
        );
    }
    Ok(())
}

fn parse_args() -> Args {
    // Defaults from env vars (preserve old behavior), then overridden by CLI flags.
    let mut port: u16 = std::env::var("POWDB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5433);
    let mut bind: String = std::env::var("POWDB_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let mut data_dir: String =
        std::env::var("POWDB_DATA").unwrap_or_else(|_| "./powdb_data".into());
    // Password is set exclusively via environment variable. Wrapped in
    // Zeroizing so the secret is wiped from memory on drop.
    let password: Option<Zeroizing<String>> = std::env::var("POWDB_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty())
        .map(Zeroizing::new);
    let mut idle_timeout_secs: u64 = std::env::var("POWDB_IDLE_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300); // 5 min default
    let mut query_timeout_secs: u64 = std::env::var("POWDB_QUERY_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30); // 30s default
    let mut tx_wait_timeout_ms: u64 = std::env::var("POWDB_TX_WAIT_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_TX_WAIT_TIMEOUT_MS);
    // Maximum explicit-transaction lifetime; env-only (no CLI flag), like the
    // other budgets.
    let tx_max_lifetime =
        parse_tx_max_lifetime(std::env::var("POWDB_TX_MAX_LIFETIME_MS").ok().as_deref());
    let mut db_name: Option<String> = std::env::var("POWDB_DB_NAME")
        .ok()
        .filter(|s| !s.is_empty());
    let mut tls_cert: Option<String> = std::env::var("POWDB_TLS_CERT")
        .ok()
        .filter(|s| !s.is_empty());
    let mut tls_key: Option<String> = std::env::var("POWDB_TLS_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    // Optional Prometheus metrics endpoint (host:port). Off unless set.
    let mut metrics_addr: Option<String> = std::env::var("POWDB_METRICS_ADDR")
        .ok()
        .filter(|s| !s.is_empty());
    // Optional Unix-domain-socket path. Off unless set.
    let mut socket: Option<String> = std::env::var("POWDB_SOCKET").ok().filter(|s| !s.is_empty());
    // Per-query memory budget; env-only (no CLI flag).
    let query_memory_limit =
        parse_query_memory_limit(std::env::var("POWDB_QUERY_MEMORY_LIMIT").ok().as_deref());
    // Fallback nested-loop join candidate-pair cap; env-only (no CLI flag).
    let nested_loop_pair_limit =
        parse_nested_loop_pair_limit(std::env::var("POWDB_MAX_NESTED_LOOP_PAIRS").ok().as_deref());
    // Dirty-page (unflushed heap page) budget; env-only (no CLI flag).
    let dirty_page_budget =
        parse_dirty_page_budget(std::env::var("POWDB_DIRTY_PAGE_BUDGET").ok().as_deref());
    // When set, refuse to start with a password but no TLS. Default off.
    let require_tls = parse_require_tls(std::env::var("POWDB_REQUIRE_TLS").ok().as_deref());
    // `POWDB_READONLY` reuses the same truthy grammar as `POWDB_REQUIRE_TLS`.
    let mut read_only = parse_require_tls(std::env::var("POWDB_READONLY").ok().as_deref());

    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--port" | "-p" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--port requires a value");
                    std::process::exit(2);
                }
                port = argv[i].parse().unwrap_or_else(|_| {
                    eprintln!("invalid port: {}", argv[i]);
                    std::process::exit(2);
                });
            }
            "--data-dir" | "-d" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--data-dir requires a value");
                    std::process::exit(2);
                }
                data_dir = argv[i].clone();
            }
            "--bind" | "-b" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--bind requires a value");
                    std::process::exit(2);
                }
                bind = argv[i].clone();
            }
            "--idle-timeout" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--idle-timeout requires a value");
                    std::process::exit(2);
                }
                idle_timeout_secs = argv[i].parse().unwrap_or_else(|_| {
                    eprintln!("invalid timeout: {}", argv[i]);
                    std::process::exit(2);
                });
            }
            "--query-timeout" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--query-timeout requires a value");
                    std::process::exit(2);
                }
                query_timeout_secs = argv[i].parse().unwrap_or_else(|_| {
                    eprintln!("invalid timeout: {}", argv[i]);
                    std::process::exit(2);
                });
            }
            "--tx-wait-timeout-ms" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--tx-wait-timeout-ms requires a value");
                    std::process::exit(2);
                }
                tx_wait_timeout_ms = argv[i].parse().unwrap_or_else(|_| {
                    eprintln!("invalid tx-wait-timeout-ms: {}", argv[i]);
                    std::process::exit(2);
                });
                if tx_wait_timeout_ms == 0 {
                    eprintln!("--tx-wait-timeout-ms must be greater than 0");
                    std::process::exit(2);
                }
            }
            "--db-name" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--db-name requires a value");
                    std::process::exit(2);
                }
                db_name = Some(argv[i].clone());
            }
            "--tls-cert" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--tls-cert requires a value");
                    std::process::exit(2);
                }
                tls_cert = Some(argv[i].clone());
            }
            "--tls-key" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--tls-key requires a value");
                    std::process::exit(2);
                }
                tls_key = Some(argv[i].clone());
            }
            "--metrics-addr" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--metrics-addr requires a value");
                    std::process::exit(2);
                }
                metrics_addr = Some(argv[i].clone());
            }
            "--socket" | "-s" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--socket requires a value");
                    std::process::exit(2);
                }
                socket = Some(argv[i].clone());
            }
            "--readonly" => {
                read_only = true;
            }
            "--version" | "-V" => {
                println!("powdb-server {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("powdb-server — PowDB wire-protocol server");
                println!();
                println!("USAGE:");
                println!("    powdb-server [OPTIONS]");
                println!();
                println!("OPTIONS:");
                println!("    -p, --port <PORT>          TCP port to listen on (default: 5433)");
                println!("    -b, --bind <ADDR>          Bind address (default: 127.0.0.1)");
                println!("    -s, --socket <PATH>        Also listen on a Unix domain socket (same-host, ~2x lower latency)");
                println!("    -d, --data-dir <PATH>      Data directory (default: ./powdb_data)");
                println!("        --tls-cert <PATH>      TLS certificate file (PEM)");
                println!("        --tls-key <PATH>       TLS private key file (PEM)");
                println!("        --idle-timeout <SECS>  Idle connection timeout (default: 300)");
                println!(
                    "        --query-timeout <SECS> Per-query timeout threshold metric (default: 30)"
                );
                println!("        --tx-wait-timeout-ms <MS>  Max wait for a concurrent explicit transaction before BEGIN fails (default: 5000)");
                println!("        --db-name <NAME>       Reject a CONNECT that explicitly names a different database (default: accept any)");
                println!("        --metrics-addr <ADDR>  Serve Prometheus /metrics on host:port (off by default)");
                println!("        --readonly             Serve the data directory read-only (snapshot serving; mutations are refused)");
                println!("    -V, --version              Print version and exit");
                println!("    -h, --help                 Print this message");
                println!();
                println!("ENVIRONMENT:");
                println!("    POWDB_PORT, POWDB_BIND, POWDB_DATA");
                println!("    POWDB_PASSWORD             Set password for client authentication");
                println!("    POWDB_TLS_CERT, POWDB_TLS_KEY");
                println!("    POWDB_REQUIRE_TLS          Refuse to start with a password but no TLS (default: off)");
                println!("    POWDB_IDLE_TIMEOUT, POWDB_QUERY_TIMEOUT");
                println!("    POWDB_TX_WAIT_TIMEOUT_MS   Max ms a BEGIN waits for a concurrent explicit transaction (default: 5000)");
                println!("    POWDB_TX_MAX_LIFETIME_MS   Max ms one connection may hold an open explicit transaction before the server rolls it back (default: 300000; 0 disables)");
                println!("    POWDB_DB_NAME              Reject a CONNECT that explicitly names a different database (default: accept any)");
                println!("    POWDB_QUERY_MEMORY_LIMIT   Per-query memory budget in bytes (default: 256 MiB)");
                println!("    POWDB_MAX_NESTED_LOOP_PAIRS  Fallback nested-loop join candidate-pair cap (default: 6,400,000)");
                println!("    POWDB_DIRTY_PAGE_BUDGET    Ceiling in bytes on unflushed heap pages inside an explicit transaction (default: 256 MiB)");
                println!("    POWDB_METRICS_ADDR         host:port for the Prometheus /metrics endpoint (unauthenticated)");
                println!("    POWDB_SOCKET               Path for an additional Unix-domain-socket listener (off by default)");
                println!("    POWDB_SYNC_MODE            WAL durability: full (default) | normal (bounded-loss, ~15-40x faster) | off (bench-only)");
                println!("    POWDB_READONLY             Serve read-only (snapshot serving) when truthy (1/true/yes/on)");
                println!("    RUST_LOG=info|debug|trace  (defaults to info)");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!("try --help");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    Args {
        port,
        bind,
        data_dir,
        password,
        idle_timeout_secs,
        query_timeout_secs,
        tx_wait_timeout_ms,
        tx_max_lifetime,
        tls_cert,
        tls_key,
        query_memory_limit,
        nested_loop_pair_limit,
        dirty_page_budget,
        require_tls,
        metrics_addr,
        socket,
        db_name,
        read_only,
    }
}

/// Serve one accepted connection to completion over any stream type — plain
/// TCP, TLS-over-TCP, or a Unix domain socket. Every accept arm funnels through
/// here so the per-connection `ConnOpts` wiring lives in exactly one place.
#[allow(clippy::too_many_arguments)]
async fn run_connection<S>(
    stream: S,
    peer_addr: Option<std::net::SocketAddr>,
    engine: Arc<RwLock<Engine>>,
    tx_gate: handler::TxGate,
    expected_password: Option<Zeroizing<String>>,
    users: Arc<powdb_auth::UserStore>,
    mut shutdown_rx: watch::Receiver<bool>,
    idle_timeout: std::time::Duration,
    query_timeout: std::time::Duration,
    tx_wait_timeout: std::time::Duration,
    rate_limiter: handler::AuthRateLimiter,
    metrics: Arc<Metrics>,
    db_name: Option<String>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    handler::handle_connection(
        stream,
        handler::ConnOpts {
            engine,
            tx_gate,
            expected_password,
            users,
            shutdown_rx: &mut shutdown_rx,
            idle_timeout,
            query_timeout,
            rate_limiter: Some(&rate_limiter),
            peer_addr,
            metrics,
            tx_wait_timeout,
            db_name,
        },
    )
    .await;
}

/// Load TLS certificate and key files, returning a configured `TlsAcceptor`.
fn build_tls_acceptor(
    cert_path: &str,
    key_path: &str,
) -> Result<tokio_rustls::TlsAcceptor, Box<dyn std::error::Error>> {
    use std::io::BufReader;
    use tokio_rustls::rustls;
    // PEM parsing comes from `rustls-pki-types` rather than `rustls-pemfile`,
    // which the rustls project marked unmaintained in RUSTSEC-2025-0134 after
    // folding this exact API into pki-types. Same maintainers, same parser, and
    // pki-types was already in the tree as a rustls dependency.
    use rustls::pki_types::pem::PemObject;

    let cert_file = std::fs::File::open(cert_path)
        .map_err(|e| format!("failed to open TLS cert {cert_path}: {e}"))?;
    let key_file = std::fs::File::open(key_path)
        .map_err(|e| format!("failed to open TLS key {key_path}: {e}"))?;

    let certs: Vec<_> =
        rustls::pki_types::CertificateDer::pem_reader_iter(BufReader::new(cert_file))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("failed to parse TLS certs: {e}"))?;

    // `from_pem_reader` returns `NoItemsFound` where the old `private_key`
    // returned `Ok(None)`, so the "no key in the file" case keeps its own
    // message instead of collapsing into the generic parse error.
    let key = rustls::pki_types::PrivateKeyDer::from_pem_reader(BufReader::new(key_file)).map_err(
        |e| match e {
            rustls::pki_types::pem::Error::NoItemsFound => {
                "no private key found in TLS key file".to_string()
            }
            other => format!("failed to parse TLS key: {other}"),
        },
    )?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS config error: {e}"))?;

    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// Perform the TLS handshake under a hard deadline.
///
/// The connection permit is taken before the handshake runs, and neither the
/// idle timeout (which only starts once a connection is running) nor the
/// per-IP auth rate limiter (which lives inside `run_connection`) applies yet.
/// Without a deadline, `MAX_CONNECTIONS` peers that connect and then send a
/// single byte, or nothing at all, hold every permit forever and the server
/// stops accepting: pre-auth connection-slot exhaustion that costs the
/// attacker nothing and hits exactly the deployments that enabled TLS.
///
/// A timed-out handshake returns `ErrorKind::TimedOut`; the caller counts it in
/// the TLS handshake failure metric and drops the permit like any other failed
/// handshake.
async fn accept_tls_with_timeout<S>(
    acceptor: &tokio_rustls::TlsAcceptor,
    stream: S,
    timeout: std::time::Duration,
) -> io::Result<tokio_rustls::server::TlsStream<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match tokio::time::timeout(timeout, acceptor.accept(stream)).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("TLS handshake did not complete within {timeout:?}"),
        )),
    }
}

/// Resolve when the process receives a termination signal: SIGINT (Ctrl-C) or,
/// on Unix, SIGTERM — the signal Docker (`docker stop`), Kubernetes (pod
/// termination), and systemd send on stop. Awaiting only `ctrl_c()` would let
/// SIGTERM fall through to the kernel default and kill the process before the
/// graceful drain + checkpoint could run. On non-Unix targets only Ctrl-C is
/// available.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(e) => {
                // Failing to install the SIGTERM handler is non-fatal: fall back
                // to SIGINT-only so the server still starts and Ctrl-C still drains.
                warn!(error = %e, "could not install SIGTERM handler; only Ctrl-C will drain");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Bootstrap an admin user from `POWDB_ADMIN_USER` / `POWDB_ADMIN_PASSWORD`.
///
/// Creates the user with role "admin" only when both values are present AND the
/// user does not already exist. Returns `true` when a user was created (so the
/// caller can persist + log). The password is never returned or logged.
fn ensure_bootstrap_admin(
    store: &mut powdb_auth::UserStore,
    user: Option<String>,
    pass: Option<String>,
) -> bool {
    let (Some(user), Some(pass)) = (user, pass) else {
        return false;
    };
    if user.is_empty() || pass.is_empty() {
        return false;
    }
    // Already present? Don't clobber an existing credential.
    if store.list_users().iter().any(|(n, _)| n == &user) {
        return false;
    }
    match store.create_user(&user, &pass, "admin") {
        Ok(()) => true,
        Err(e) => {
            error!(error = %e, user = %user, "failed to bootstrap admin user");
            false
        }
    }
}

#[tokio::main]
async fn main() {
    // Initialize tracing. RUST_LOG overrides; default is info.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args = parse_args();

    let build_engine = || {
        if args.read_only {
            Engine::open_read_only_with_memory_limit(
                Path::new(&args.data_dir),
                args.query_memory_limit,
            )
        } else {
            Engine::with_memory_limit_and_wal_archive(
                Path::new(&args.data_dir),
                args.query_memory_limit,
                archive_wal_records_if_sync_enabled,
            )
        }
    };
    let mut engine = match build_engine() {
        Ok(e) => e,
        Err(e) => {
            error!(data_dir = %args.data_dir, error = %e, "failed to initialize storage engine");
            std::process::exit(1);
        }
    };
    info!(
        query_memory_limit = args.query_memory_limit,
        "per-query memory budget"
    );
    if let Some(limit) = args.nested_loop_pair_limit {
        engine.set_nested_loop_pair_limit(limit);
        info!(
            nested_loop_pair_limit = limit,
            "fallback nested-loop join candidate-pair cap (POWDB_MAX_NESTED_LOOP_PAIRS)"
        );
    }
    if let Some(limit) = args.dirty_page_budget {
        engine.catalog_mut().set_dirty_page_budget_bytes(limit);
        info!(
            dirty_page_budget_bytes = limit,
            "unflushed heap-page budget (POWDB_DIRTY_PAGE_BUDGET)"
        );
    }

    if args.read_only {
        info!(
            data_dir = %args.data_dir,
            "READ-ONLY snapshot serving: the directory is opened read-only, no writer admission \
             is taken, and mutating statements are refused. Refresh materialized views before \
             snapshotting. This mode is stale-by-design between snapshot swaps"
        );
    } else {
        // WAL durability mode (POWDB_SYNC_MODE). Default Full is fully durable.
        // A read-only engine never writes, so durability configuration is moot.
        let sync_mode = parse_sync_mode(std::env::var("POWDB_SYNC_MODE").ok().as_deref());
        engine.set_wal_sync_mode(sync_mode);
        match sync_mode {
            WalSyncMode::Full => info!("WAL sync mode: full (fsync every commit: fully durable)"),
            WalSyncMode::Normal => warn!(
                "WAL sync mode: NORMAL: commits fsync on a background interval; an OS crash or \
                 power loss may lose up to the last ~10ms of writes (process crashes lose nothing)"
            ),
            WalSyncMode::Off => warn!(
                "WAL sync mode: OFF: NO durability; a crash loses all writes since the last \
                 checkpoint. Bench/test use only, never production"
            ),
        }
    }

    let engine = Arc::new(RwLock::new(engine));
    let tx_gate = handler::new_tx_gate_with_max_tx_lifetime(args.tx_max_lifetime);
    match args.tx_max_lifetime {
        Some(max) => info!(
            tx_max_lifetime_ms = max.as_millis(),
            "maximum explicit-transaction lifetime (POWDB_TX_MAX_LIFETIME_MS)"
        ),
        None => warn!(
            "maximum explicit-transaction lifetime DISABLED (POWDB_TX_MAX_LIFETIME_MS=0): one \
             connection can hold the write-admission gate for as long as it likes, and every \
             other connection, readers included, waits behind it"
        ),
    }

    // Load the multi-user store from the same data dir. When it has users, the
    // handshake authenticates (username, password) against it; when empty the
    // server falls back to the shared-password behavior.
    let mut users = match powdb_auth::UserStore::load(std::path::Path::new(&args.data_dir)) {
        Ok(u) => u,
        Err(e) => {
            error!(data_dir = %args.data_dir, error = %e, "failed to load user store (auth.json)");
            std::process::exit(1);
        }
    };
    // Zero-CLI bootstrap: create an admin from POWDB_ADMIN_USER/PASSWORD when it
    // doesn't already exist, then persist it. The password is never logged.
    let admin_user = std::env::var("POWDB_ADMIN_USER")
        .ok()
        .filter(|s| !s.is_empty());
    let admin_pass = std::env::var("POWDB_ADMIN_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());
    if ensure_bootstrap_admin(&mut users, admin_user.clone(), admin_pass) {
        match users.save(std::path::Path::new(&args.data_dir)) {
            Ok(()) => {
                info!(user = ?admin_user, "bootstrapped admin user from environment");
            }
            Err(e) => {
                error!(error = %e, "failed to persist bootstrapped admin user");
                std::process::exit(1);
            }
        }
    }
    if !users.is_empty() {
        info!(users = users.len(), "multi-user authentication enabled");
    } else if args.password.is_none() {
        // TASK-09: Warn when neither a shared password nor users are configured.
        warn!("no password configured — all connections will be accepted without authentication");
    }
    let users = Arc::new(users);

    // Build TLS acceptor if both cert and key are provided.
    let tls_acceptor = match (&args.tls_cert, &args.tls_key) {
        (Some(cert), Some(key)) => match build_tls_acceptor(cert, key) {
            Ok(acceptor) => Some(acceptor),
            Err(e) => {
                error!(error = %e, "failed to configure TLS");
                std::process::exit(1);
            }
        },
        (Some(_), None) => {
            error!("--tls-cert provided without --tls-key");
            std::process::exit(2);
        }
        (None, Some(_)) => {
            error!("--tls-key provided without --tls-cert");
            std::process::exit(2);
        }
        (None, None) => None,
    };

    let tls_enabled = tls_acceptor.is_some();

    let auth_configured = args.password.is_some() || !users.is_empty();

    // Enforce TLS when required. Refuse to start (rather than silently
    // transmitting credentials in cleartext) if any auth mode is enabled without
    // TLS. This covers shared-password auth, persisted named users, and
    // just-bootstrapped admins.
    if let Err(msg) = check_tls_requirement(args.require_tls, auth_configured, tls_enabled) {
        error!("{msg}");
        std::process::exit(2);
    }

    // CRITICAL: warn when credential auth is enabled without TLS encryption.
    if auth_configured && tls_acceptor.is_none() {
        warn!(
            "WARNING: Authentication enabled without TLS. Credentials will be sent in plaintext."
        );
        eprintln!("!!! CRITICAL: Authentication enabled without TLS. Credentials will be sent in plaintext. !!!");
    }

    let addr = format!("{}:{}", args.bind, args.port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(addr = %addr, error = %e, "failed to bind");
            std::process::exit(1);
        }
    };

    // Optional metrics endpoint. Construct the registry now (handlers always
    // hold an Arc<Metrics>) and bind eagerly so a port conflict fails fast at
    // startup, consistent with the main listener above.
    let metrics = Arc::new(Metrics::new().with_data_dir(&args.data_dir));
    let metrics_listener = match args.metrics_addr.as_deref() {
        Some(maddr) => match TcpListener::bind(maddr).await {
            Ok(l) => Some(l),
            Err(e) => {
                error!(addr = %maddr, error = %e, "failed to bind metrics endpoint");
                std::process::exit(1);
            }
        },
        None => None,
    };

    // Optional Unix-domain-socket listener (same-host clients). Additive: the
    // TCP listener above always runs. Remove a stale socket file from a prior
    // unclean exit first — `bind` fails if the path already exists.
    let unix_listener = match args.socket.as_deref() {
        Some(path) => {
            let _ = std::fs::remove_file(path);
            match UnixListener::bind(path) {
                Ok(l) => {
                    info!(socket = %path, "unix domain socket listening");
                    Some(l)
                }
                Err(e) => {
                    error!(socket = %path, error = %e, "failed to bind unix socket");
                    std::process::exit(1);
                }
            }
        }
        None => None,
    };

    info!(
        addr = %addr, data_dir = %args.data_dir, auth = auth_configured,
        tls = tls_enabled,
        idle_timeout = args.idle_timeout_secs, query_timeout = args.query_timeout_secs,
        "powdb server listening"
    );

    let semaphore = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    // Shutdown broadcast: `false` initially, flipped to `true` on SIGINT/SIGTERM.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Spawn the metrics endpoint now that the shutdown channel exists, so it
    // drains with the rest of the server on SIGINT/SIGTERM.
    if let Some(ml) = metrics_listener {
        if let Some(maddr) = args.metrics_addr.as_deref() {
            info!(addr = %maddr, "metrics endpoint listening");
        }
        tokio::spawn(serve_metrics(ml, metrics.clone(), shutdown_rx.clone()));
    }

    let idle_timeout = std::time::Duration::from_secs(args.idle_timeout_secs);
    let query_timeout = std::time::Duration::from_secs(args.query_timeout_secs);
    let tx_wait_timeout = std::time::Duration::from_millis(args.tx_wait_timeout_ms);
    if let Some(name) = args.db_name.as_deref() {
        info!(db_name = %name, "serving a single named database; foreign CONNECT db names will be rejected");
    }

    // Shared auth rate limiter.
    let rate_limiter = handler::new_rate_limiter();

    loop {
        tokio::select! {
            // Accept new connections.
            result = listener.accept() => {
                match result {
                    Ok((stream, peer)) => {
                        let permit = match semaphore.clone().acquire_owned().await {
                            Ok(p) => p,
                            Err(_) => break,
                        };
                        info!(peer = %peer, "accepted connection");
                        let eng = engine.clone();
                        let tx_gate = tx_gate.clone();
                        let pw = args.password.clone();
                        let users = users.clone();
                        let rx = shutdown_rx.clone();
                        let idle = idle_timeout;
                        let qtimeout = query_timeout;
                        let txwait = tx_wait_timeout;
                        let rl = rate_limiter.clone();
                        let tls = tls_acceptor.clone();
                        let m = metrics.clone();
                        let dbn = args.db_name.clone();
                        tokio::spawn(async move {
                            let peer_addr = Some(peer);
                            m.inc_connection_accepted();
                            // RAII gauge: decremented when this task ends, even
                            // on an early return or panic.
                            let _active = m.active_guard();
                            if let Some(acceptor) = tls {
                                match accept_tls_with_timeout(&acceptor, stream, TLS_HANDSHAKE_TIMEOUT).await {
                                    Ok(tls_stream) => {
                                        run_connection(
                                            tls_stream, peer_addr, eng, tx_gate, pw, users, rx,
                                            idle, qtimeout, txwait, rl, m.clone(), dbn,
                                        ).await;
                                    }
                                    Err(e) => {
                                        m.inc_tls_failure();
                                        warn!(peer = %peer, error = %e, "TLS handshake failed");
                                    }
                                }
                            } else {
                                run_connection(
                                    stream, peer_addr, eng, tx_gate, pw, users, rx, idle,
                                    qtimeout, txwait, rl, m.clone(), dbn,
                                ).await;
                            }
                            drop(permit);
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "accept error");
                    }
                }
            }

            // Accept new connections on the optional Unix domain socket. When
            // no socket is configured this future never resolves, so the arm is
            // inert. UDS is same-host and local-only, so no TLS and no
            // IP-based rate limiting (peer_addr = None).
            result = async {
                match &unix_listener {
                    Some(l) => l.accept().await,
                    None => std::future::pending().await,
                }
            } => {
                match result {
                    Ok((stream, _addr)) => {
                        let permit = match semaphore.clone().acquire_owned().await {
                            Ok(p) => p,
                            Err(_) => break,
                        };
                        info!("accepted unix-socket connection");
                        let eng = engine.clone();
                        let tx_gate = tx_gate.clone();
                        let pw = args.password.clone();
                        let users = users.clone();
                        let rx = shutdown_rx.clone();
                        let idle = idle_timeout;
                        let qtimeout = query_timeout;
                        let txwait = tx_wait_timeout;
                        let rl = rate_limiter.clone();
                        let m = metrics.clone();
                        let dbn = args.db_name.clone();
                        tokio::spawn(async move {
                            m.inc_connection_accepted();
                            let _active = m.active_guard();
                            run_connection(
                                stream, None, eng, tx_gate, pw, users, rx, idle, qtimeout, txwait,
                                rl, m.clone(), dbn,
                            ).await;
                            drop(permit);
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "unix accept error");
                    }
                }
            }

            // Graceful shutdown on SIGINT (Ctrl-C) or SIGTERM (docker/k8s/systemd).
            _ = shutdown_signal() => {
                warn!("received shutdown signal, draining connections...");
                let _ = shutdown_tx.send(true);
                break;
            }
        }
    }

    // Wait for all in-flight connections to finish. The semaphore starts
    // at MAX_CONNECTIONS; each active connection holds one permit. When
    // all connections have closed, we can acquire all permits back.
    info!(
        "waiting for {} active connection(s) to drain",
        MAX_CONNECTIONS - semaphore.available_permits()
    );
    let _ = semaphore.acquire_many(MAX_CONNECTIONS as u32).await;
    info!("all connections drained, shutting down");

    // Engine `Drop` calls `catalog.checkpoint()` which flushes heap pages
    // and truncates the WAL.
    drop(engine);

    // Remove the socket file so a restart can re-bind cleanly (bind fails if
    // the path already exists).
    if let Some(path) = args.socket.as_deref() {
        let _ = std::fs::remove_file(path);
    }
    info!("clean shutdown complete");
}

#[cfg(test)]
mod tests {
    use super::*;
    use powdb_query::executor::Engine;

    /// Build a self-signed acceptor for handshake tests.
    fn test_acceptor() -> tokio_rustls::TlsAcceptor {
        use tokio_rustls::rustls;
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
        let key_der =
            rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der().to_vec())
                .unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    }

    /// A peer that opens a TLS connection and then says nothing must
    /// not pin its connection permit. Without the deadline this future never
    /// resolves and the test hangs (the permit is held for the process's life).
    #[tokio::test]
    async fn tls_handshake_times_out_on_a_silent_peer() {
        // Witness the bug first: the bare acceptor never resolves against a
        // silent peer, so the connection permit it holds is never released.
        let (_bug_client, bug_server) = tokio::io::duplex(4096);
        let acceptor = test_acceptor();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(150),
                acceptor.accept(bug_server)
            )
            .await
            .is_err(),
            "an unbounded handshake against a silent peer must not resolve"
        );

        let (_client, server) = tokio::io::duplex(4096);
        let started = std::time::Instant::now();
        let err = accept_tls_with_timeout(
            &test_acceptor(),
            server,
            std::time::Duration::from_millis(150),
        )
        .await
        .expect_err("a silent peer must not complete the handshake");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "err: {err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the handshake must give up promptly, took {:?}",
            started.elapsed()
        );
        // The client half is still open: the timeout, not an EOF, ended it.
        drop(_client);
    }

    /// A peer that dribbles one byte and stalls is the same attack with a
    /// pulse: the partial handshake must still hit the deadline.
    #[tokio::test]
    async fn tls_handshake_times_out_on_a_one_byte_peer() {
        use tokio::io::AsyncWriteExt;
        let (mut client, server) = tokio::io::duplex(4096);
        client.write_all(&[0x16]).await.unwrap();
        let err = accept_tls_with_timeout(
            &test_acceptor(),
            server,
            std::time::Duration::from_millis(150),
        )
        .await
        .expect_err("a stalled partial handshake must not complete");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "err: {err}");
    }

    /// The deadline must not break a real handshake.
    #[tokio::test]
    async fn tls_handshake_succeeds_within_the_deadline() {
        use tokio_rustls::rustls;
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
        let key_der =
            rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der().to_vec())
                .unwrap();
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

        let (client, server) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            accept_tls_with_timeout(&acceptor, server, TLS_HANDSHAKE_TIMEOUT).await
        });
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let client_result = connector.connect(name, client).await;
        assert!(client_result.is_ok(), "client handshake failed");
        assert!(
            server_task.await.unwrap().is_ok(),
            "server handshake must succeed inside the deadline"
        );
    }

    #[test]
    fn require_tls_rejects_password_without_tls() {
        // POWDB_REQUIRE_TLS=1 + password set + no TLS cert/key → hard error.
        let err = check_tls_requirement(true, true, false);
        assert!(err.is_err(), "expected startup refusal");
    }

    #[test]
    fn require_tls_rejects_named_user_auth_without_tls() {
        // The caller passes auth_configured=true for either shared-password or
        // named-user auth. Named users must not silently bypass REQUIRE_TLS.
        let err = check_tls_requirement(true, true, false);
        assert!(err.is_err(), "expected startup refusal");
    }

    #[test]
    fn require_tls_allows_password_with_tls() {
        assert!(check_tls_requirement(true, true, true).is_ok());
    }

    #[test]
    fn require_tls_allows_no_password() {
        // No password means nothing to leak; TLS not required.
        assert!(check_tls_requirement(true, false, false).is_ok());
    }

    #[test]
    fn require_tls_off_is_backward_compatible() {
        // Default off: password without TLS is allowed (just warned).
        assert!(check_tls_requirement(false, true, false).is_ok());
    }

    #[test]
    fn parse_sync_mode_env() {
        assert_eq!(parse_sync_mode(Some("normal")), WalSyncMode::Normal);
        assert_eq!(parse_sync_mode(Some("NORMAL")), WalSyncMode::Normal);
        assert_eq!(parse_sync_mode(Some(" normal ")), WalSyncMode::Normal);
        assert_eq!(parse_sync_mode(Some("off")), WalSyncMode::Off);
        assert_eq!(parse_sync_mode(Some("full")), WalSyncMode::Full);
        // Unset / empty / unknown all fall back to the safe Full default.
        assert_eq!(parse_sync_mode(None), WalSyncMode::Full);
        assert_eq!(parse_sync_mode(Some("")), WalSyncMode::Full);
        assert_eq!(parse_sync_mode(Some("bogus")), WalSyncMode::Full);
    }

    #[test]
    fn parse_require_tls_env() {
        assert!(parse_require_tls(Some("1")));
        assert!(parse_require_tls(Some("true")));
        assert!(parse_require_tls(Some("TRUE")));
        assert!(!parse_require_tls(Some("0")));
        assert!(!parse_require_tls(Some("")));
        assert!(!parse_require_tls(None));
    }

    #[test]
    fn memory_limit_defaults_when_unset() {
        assert_eq!(parse_query_memory_limit(None), DEFAULT_QUERY_MEMORY_LIMIT);
    }

    #[test]
    fn memory_limit_defaults_on_garbage() {
        assert_eq!(
            parse_query_memory_limit(Some("not-a-number")),
            DEFAULT_QUERY_MEMORY_LIMIT
        );
        assert_eq!(
            parse_query_memory_limit(Some("")),
            DEFAULT_QUERY_MEMORY_LIMIT
        );
        assert_eq!(
            parse_query_memory_limit(Some("0")),
            DEFAULT_QUERY_MEMORY_LIMIT
        );
    }

    #[test]
    fn memory_limit_parses_explicit_value() {
        assert_eq!(parse_query_memory_limit(Some("1048576")), 1_048_576);
        assert_eq!(parse_query_memory_limit(Some("  4096  ")), 4096);
    }

    #[test]
    fn nested_loop_pair_limit_env_parsing() {
        // Unset / empty / garbage / zero all leave the engine default (None).
        assert_eq!(parse_nested_loop_pair_limit(None), None);
        assert_eq!(parse_nested_loop_pair_limit(Some("")), None);
        assert_eq!(parse_nested_loop_pair_limit(Some("not-a-number")), None);
        assert_eq!(parse_nested_loop_pair_limit(Some("0")), None);
        // A positive count (including a small one for testing) overrides.
        assert_eq!(parse_nested_loop_pair_limit(Some("4")), Some(4));
        assert_eq!(
            parse_nested_loop_pair_limit(Some("  6400000 ")),
            Some(6_400_000)
        );
    }

    #[test]
    fn bootstrap_admin_creates_when_both_set_and_absent() {
        let mut store = powdb_auth::UserStore::new();
        let created =
            ensure_bootstrap_admin(&mut store, Some("root".into()), Some("secret".into()));
        assert!(created);
        assert!(store.authenticate("root", "secret").is_some());
        assert_eq!(store.authenticate("root", "secret").unwrap().role, "admin");
    }

    #[test]
    fn bootstrap_admin_noop_when_missing_inputs() {
        let mut store = powdb_auth::UserStore::new();
        assert!(!ensure_bootstrap_admin(&mut store, None, Some("p".into())));
        assert!(!ensure_bootstrap_admin(&mut store, Some("u".into()), None));
        assert!(!ensure_bootstrap_admin(&mut store, None, None));
        assert!(!ensure_bootstrap_admin(
            &mut store,
            Some("".into()),
            Some("p".into())
        ));
        assert!(store.is_empty());
    }

    #[test]
    fn bootstrap_admin_does_not_clobber_existing() {
        let mut store = powdb_auth::UserStore::new();
        store.create_user("root", "original", "readonly").unwrap();
        let created =
            ensure_bootstrap_admin(&mut store, Some("root".into()), Some("different".into()));
        assert!(!created);
        // Existing credential + role preserved.
        assert!(store.authenticate("root", "original").is_some());
        assert_eq!(
            store.authenticate("root", "original").unwrap().role,
            "readonly"
        );
    }

    /// The parsed env limit is actually applied to the constructed Engine.
    #[test]
    fn env_limit_is_applied_to_engine() {
        let limit = parse_query_memory_limit(Some("2048"));
        let dir = std::env::temp_dir().join(format!("powdb_srv_memlimit_{}", std::process::id()));
        // Hermetic: the path is pid-derived (not unique per run), so a stale dir
        // from an earlier run — or a reused pid — must not leak into this test.
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::with_memory_limit(&dir, limit).unwrap();
        assert_eq!(engine.query_memory_limit(), 2048);
    }

    #[test]
    fn dirty_page_budget_env_parsing() {
        // Unset / empty / garbage / zero all leave the storage default (None).
        assert_eq!(parse_dirty_page_budget(None), None);
        assert_eq!(parse_dirty_page_budget(Some("")), None);
        assert_eq!(parse_dirty_page_budget(Some("not-a-number")), None);
        assert_eq!(parse_dirty_page_budget(Some("0")), None);
        // A positive byte count (including a small one for testing) overrides.
        assert_eq!(parse_dirty_page_budget(Some("32768")), Some(32_768));
        assert_eq!(
            parse_dirty_page_budget(Some("  268435456 ")),
            Some(268_435_456)
        );
    }

    /// The transaction-lifetime bound fails SAFE: only an explicit `0` turns
    /// it off. A typo must not silently restore the unbounded behavior that
    /// let one connection hold the write gate for as long as it liked.
    #[test]
    fn tx_max_lifetime_env_parsing_fails_safe() {
        assert_eq!(
            parse_tx_max_lifetime(None),
            Some(handler::DEFAULT_TX_MAX_LIFETIME)
        );
        assert_eq!(
            parse_tx_max_lifetime(Some("")),
            Some(handler::DEFAULT_TX_MAX_LIFETIME)
        );
        assert_eq!(
            parse_tx_max_lifetime(Some("not-a-number")),
            Some(handler::DEFAULT_TX_MAX_LIFETIME)
        );
        assert_eq!(
            parse_tx_max_lifetime(Some("-1")),
            Some(handler::DEFAULT_TX_MAX_LIFETIME)
        );
        // Only an explicit zero opts out.
        assert_eq!(parse_tx_max_lifetime(Some("0")), None);
        assert_eq!(
            parse_tx_max_lifetime(Some("  1500 ")),
            Some(std::time::Duration::from_millis(1500))
        );
    }

    /// The parsed value reaches the gate every connection is served through,
    /// which is the only place the bound can be enforced from.
    #[test]
    fn tx_max_lifetime_reaches_the_transaction_gate() {
        let gate = handler::new_tx_gate_with_max_tx_lifetime(parse_tx_max_lifetime(Some("1500")));
        assert_eq!(
            gate.max_tx_lifetime(),
            Some(std::time::Duration::from_millis(1500))
        );
        let default = handler::new_tx_gate();
        assert_eq!(
            default.max_tx_lifetime(),
            Some(handler::DEFAULT_TX_MAX_LIFETIME)
        );
        let disabled = handler::new_tx_gate_with_max_tx_lifetime(parse_tx_max_lifetime(Some("0")));
        assert_eq!(disabled.max_tx_lifetime(), None);
    }

    /// The parsed `POWDB_DIRTY_PAGE_BUDGET` value reaches the engine's catalog.
    /// Without the wiring the catalog keeps `DEFAULT_DIRTY_PAGE_BUDGET`, which
    /// is what made the 256 MiB ceiling unoverridable.
    #[test]
    fn env_dirty_page_budget_is_applied_to_engine() {
        let budget = parse_dirty_page_budget(Some("32768")).unwrap();
        let dir =
            std::env::temp_dir().join(format!("powdb_srv_dirtybudget_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut engine = Engine::with_memory_limit(&dir, DEFAULT_QUERY_MEMORY_LIMIT).unwrap();
        assert_eq!(
            engine.catalog().dirty_page_budget_bytes(),
            powdb_storage::heap::DEFAULT_DIRTY_PAGE_BUDGET
        );
        engine.catalog_mut().set_dirty_page_budget_bytes(budget);
        assert_eq!(engine.catalog().dirty_page_budget_bytes(), 32_768);
    }
}
