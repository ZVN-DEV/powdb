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

/// Default ceiling on concurrent connections, overridable with
/// `--max-connections` / `POWDB_MAX_CONNECTIONS`.
const DEFAULT_MAX_CONNECTIONS: usize = 1024;

/// Whether the log should carry ANSI colour: only when stdout is a terminal
/// and `NO_COLOR` is unset.
fn use_ansi() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::IsTerminal::is_terminal(&std::io::stdout())
}

/// Mode the Unix-domain-socket file is created with: owner and group only.
/// The default umask leaves it 0755, so any local user could connect to a
/// server whose data directory is 0700.
#[cfg(unix)]
const UNIX_SOCKET_MODE: u32 = 0o660;

/// Default seconds a graceful shutdown waits for in-flight connections to
/// drain. Past it the process stops waiting and exits non-zero rather than
/// hanging forever, which is what an orchestrator's own kill timer would
/// otherwise have to resolve.
const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 30;

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
    /// WAL size in bytes at which a finished autocommit statement checkpoints.
    /// `None` keeps the storage default (`DEFAULT_WAL_CHECKPOINT_BYTES`); `0`
    /// is the documented opt-out and leaves the log to grow until close.
    wal_checkpoint_bytes: Option<u64>,
    require_tls: bool,
    /// `host:port` for the optional Prometheus metrics endpoint; `None` = off.
    metrics_addr: Option<String>,
    /// When set, write the actually-bound listener ports here after startup
    /// (`port=N`, plus `metrics=N` when the metrics endpoint is on). Pairs
    /// with `--port 0` so a supervisor or test harness can let the OS pick
    /// the port without racing to re-bind a probed one.
    port_file: Option<String>,
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
    /// Ceiling on concurrent connections; a peer past it waits for a slot.
    max_connections: usize,
    /// How long a graceful shutdown waits for in-flight connections to drain
    /// before cancelling them and exiting non-zero.
    shutdown_timeout_secs: u64,
    /// WAL durability mode, validated at startup.
    sync_mode: WalSyncMode,
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

/// A configuration value that was set but is not usable, phrased identically
/// whether it arrived as a flag or as an environment variable.
///
/// The two paths used to disagree: `--port abc` was refused, `POWDB_PORT=abc`
/// silently bound 5433. Eight variables behaved that way, so a typo in a
/// deployment's environment produced a server running on settings nobody
/// chose, with nothing in the log to say so.
fn invalid_setting(source: &str, value: &str, expected: &str) -> String {
    format!("invalid value for {source}: {value:?}; expected {expected}")
}

/// Refuse to start, naming the setting and the value.
fn refuse_setting(source: &str, value: &str, expected: &str) -> ! {
    eprintln!("{}", invalid_setting(source, value, expected));
    std::process::exit(2);
}

/// Read an environment variable through the same validator its flag uses.
///
/// Unset or blank means "not configured" and keeps the default. Anything else
/// must parse: a value that does not is a refusal to start, never a silent
/// fallback.
fn env_setting<T>(name: &str, parse: impl Fn(&str) -> Result<T, String>) -> Option<T> {
    let raw = std::env::var(name).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    match parse(&raw) {
        Ok(value) => Some(value),
        Err(expected) => refuse_setting(name, &raw, &expected),
    }
}

/// Read a command-line flag through the same validator its variable uses.
fn flag_setting<T>(flag: &str, raw: &str, parse: impl Fn(&str) -> Result<T, String>) -> T {
    match parse(raw) {
        Ok(value) => value,
        Err(expected) => refuse_setting(flag, raw, &expected),
    }
}

/// A TCP port. `0` is legal and means "let the OS choose".
fn parse_port(raw: &str) -> Result<u16, String> {
    raw.trim()
        .parse::<u16>()
        .map_err(|_| "a port number between 0 and 65535".to_string())
}

/// A timeout in whole seconds. Zero is refused rather than treated as
/// "disabled": a zero budget expires immediately, so it would close every
/// connection (or cancel every query) the moment it was armed.
fn parse_timeout_secs(raw: &str) -> Result<u64, String> {
    match raw.trim().parse::<u64>() {
        Ok(0) => Err(
            "a whole number of seconds greater than 0 (0 is not a way to disable the timeout)"
                .to_string(),
        ),
        Ok(secs) => Ok(secs),
        Err(_) => Err("a whole number of seconds greater than 0".to_string()),
    }
}

/// A wait budget in milliseconds. Zero is refused for the same reason.
fn parse_wait_ms(raw: &str) -> Result<u64, String> {
    match raw.trim().parse::<u64>() {
        Ok(0) => Err("a whole number of milliseconds greater than 0".to_string()),
        Ok(ms) => Ok(ms),
        Err(_) => Err("a whole number of milliseconds greater than 0".to_string()),
    }
}

/// The `POWDB_TX_MAX_LIFETIME_MS` ceiling on how long one connection may hold
/// the transaction gate inside an explicit transaction. An explicit `0`
/// disables the bound and restores the pre-0.22 behavior where the client
/// chose the hold duration.
fn parse_tx_max_lifetime(raw: &str) -> Result<Option<std::time::Duration>, String> {
    match raw.trim().parse::<u64>() {
        Ok(0) => Ok(None),
        Ok(ms) => Ok(Some(std::time::Duration::from_millis(ms))),
        Err(_) => Err("a whole number of milliseconds, or 0 to disable the bound".to_string()),
    }
}

/// A positive budget counted in `unit`. Plain digits only: a suffixed value
/// like `64MiB` used to be discarded in silence, leaving the default in force.
///
/// The unit is the caller's to name because these budgets are not all bytes.
/// One shared message told an operator who mistyped a CONNECTION ceiling to
/// give a byte count, which is not something they can act on.
fn parse_positive_count(unit: &'static str) -> impl Fn(&str) -> Result<usize, String> {
    move |raw: &str| match raw.trim().parse::<usize>() {
        Ok(0) => Err(format!(
            "a whole number of {unit} greater than 0 (unset the variable to keep the default)"
        )),
        Ok(n) => Ok(n),
        Err(_) => Err(format!(
            "a plain whole number of {unit}, with no unit suffix"
        )),
    }
}

/// A WAL checkpoint threshold in bytes. Unlike every other budget, `0` is a
/// real setting here and not a typo: it turns the automatic checkpoint off and
/// restores the pre-0.28 behaviour where the log grew until the catalog closed.
fn parse_wal_checkpoint_bytes(raw: &str) -> Result<u64, String> {
    raw.trim().parse::<u64>().map_err(|_| {
        "a plain whole number of bytes, with no unit suffix (0 disables the automatic checkpoint)"
            .to_string()
    })
}

/// Parse `POWDB_SYNC_MODE` (`full` | `normal` | `off`). `normal` trades a
/// bounded crash-loss window (OS-crash/power-loss only) for much faster
/// writes; `off` disables durability entirely and is bench-only.
fn parse_sync_mode(raw: &str) -> Result<WalSyncMode, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "full" => Ok(WalSyncMode::Full),
        "normal" => Ok(WalSyncMode::Normal),
        "off" => Ok(WalSyncMode::Off),
        _ => Err("one of full, normal, off".to_string()),
    }
}

/// A boolean setting. Both spellings are accepted so a deployment can turn a
/// switch off explicitly; a value that is neither is refused rather than read
/// as false, which is how `POWDB_READONLY=ture` used to serve a writable
/// database.
fn parse_bool_setting(raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err("one of 1, true, yes, on, 0, false, no, off".to_string()),
    }
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
    let mut port: u16 = env_setting("POWDB_PORT", parse_port).unwrap_or(5433);
    let mut bind: String = std::env::var("POWDB_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let mut data_dir: String =
        std::env::var("POWDB_DATA").unwrap_or_else(|_| "./powdb_data".into());
    // Password is set exclusively via environment variable. Wrapped in
    // Zeroizing so the secret is wiped from memory on drop.
    let password: Option<Zeroizing<String>> = std::env::var("POWDB_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty())
        .map(Zeroizing::new);
    let mut idle_timeout_secs: u64 =
        env_setting("POWDB_IDLE_TIMEOUT", parse_timeout_secs).unwrap_or(300); // 5 min default
    let mut query_timeout_secs: u64 =
        env_setting("POWDB_QUERY_TIMEOUT", parse_timeout_secs).unwrap_or(30); // 30s default
    let mut tx_wait_timeout_ms: u64 = env_setting("POWDB_TX_WAIT_TIMEOUT_MS", parse_wait_ms)
        .unwrap_or(DEFAULT_TX_WAIT_TIMEOUT_MS);
    // Maximum explicit-transaction lifetime; env-only (no CLI flag), like the
    // other budgets. Unset keeps the default; an explicit 0 disables it.
    let tx_max_lifetime = env_setting("POWDB_TX_MAX_LIFETIME_MS", parse_tx_max_lifetime)
        .unwrap_or(Some(handler::DEFAULT_TX_MAX_LIFETIME));
    let mut max_connections: usize =
        env_setting("POWDB_MAX_CONNECTIONS", parse_positive_count("connections"))
            .unwrap_or(DEFAULT_MAX_CONNECTIONS);
    let mut shutdown_timeout_secs: u64 = env_setting("POWDB_SHUTDOWN_TIMEOUT", parse_timeout_secs)
        .unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_SECS);
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
    // Optional bound-port report file. Off unless set.
    let mut port_file: Option<String> = std::env::var("POWDB_PORT_FILE")
        .ok()
        .filter(|s| !s.is_empty());
    // Per-query memory budget; env-only (no CLI flag).
    let query_memory_limit = env_setting("POWDB_QUERY_MEMORY_LIMIT", parse_positive_count("bytes"))
        .unwrap_or(DEFAULT_QUERY_MEMORY_LIMIT);
    // Fallback nested-loop join candidate-pair cap; env-only (no CLI flag).
    let nested_loop_pair_limit = env_setting(
        "POWDB_MAX_NESTED_LOOP_PAIRS",
        parse_positive_count("candidate pairs"),
    );
    // Dirty-page (unflushed heap page) budget; env-only (no CLI flag).
    let dirty_page_budget = env_setting("POWDB_DIRTY_PAGE_BUDGET", parse_positive_count("bytes"));
    // WAL checkpoint threshold. Has a flag as well as a variable because it is
    // the one storage budget an operator may legitimately need to turn off.
    let mut wal_checkpoint_bytes =
        env_setting("POWDB_WAL_CHECKPOINT_BYTES", parse_wal_checkpoint_bytes);
    // When set, refuse to start with a password but no TLS. Default off.
    let require_tls = env_setting("POWDB_REQUIRE_TLS", parse_bool_setting).unwrap_or(false);
    // `POWDB_READONLY` reuses the same boolean grammar as `POWDB_REQUIRE_TLS`.
    let mut read_only = env_setting("POWDB_READONLY", parse_bool_setting).unwrap_or(false);
    // WAL durability mode; env-only. Validated here so a typo refuses startup
    // rather than silently serving a different durability contract.
    let sync_mode = env_setting("POWDB_SYNC_MODE", parse_sync_mode).unwrap_or(WalSyncMode::Full);

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
                port = flag_setting("--port", &argv[i], parse_port);
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
                idle_timeout_secs = flag_setting("--idle-timeout", &argv[i], parse_timeout_secs);
            }
            "--query-timeout" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--query-timeout requires a value");
                    std::process::exit(2);
                }
                query_timeout_secs = flag_setting("--query-timeout", &argv[i], parse_timeout_secs);
            }
            "--tx-wait-timeout-ms" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--tx-wait-timeout-ms requires a value");
                    std::process::exit(2);
                }
                tx_wait_timeout_ms = flag_setting("--tx-wait-timeout-ms", &argv[i], parse_wait_ms);
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
            "--port-file" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--port-file requires a value");
                    std::process::exit(2);
                }
                port_file = Some(argv[i].clone());
            }
            "--max-connections" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--max-connections requires a value");
                    std::process::exit(2);
                }
                max_connections = flag_setting(
                    "--max-connections",
                    &argv[i],
                    parse_positive_count("connections"),
                );
            }
            "--shutdown-timeout" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--shutdown-timeout requires a value");
                    std::process::exit(2);
                }
                shutdown_timeout_secs =
                    flag_setting("--shutdown-timeout", &argv[i], parse_timeout_secs);
            }
            "--wal-checkpoint-bytes" => {
                i += 1;
                if i >= argv.len() {
                    eprintln!("--wal-checkpoint-bytes requires a value");
                    std::process::exit(2);
                }
                wal_checkpoint_bytes = Some(flag_setting(
                    "--wal-checkpoint-bytes",
                    &argv[i],
                    parse_wal_checkpoint_bytes,
                ));
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
                println!("        --idle-timeout <SECS>  Idle connection timeout, must be > 0 (default: 300)");
                println!(
                    "        --query-timeout <SECS> Per-query deadline, must be > 0 (default: 30)"
                );
                println!("        --tx-wait-timeout-ms <MS>  Max wait for a concurrent explicit transaction before BEGIN fails (default: 5000)");
                println!("        --db-name <NAME>       Reject a CONNECT that explicitly names a different database (default: accept any)");
                println!("        --metrics-addr <ADDR>  Serve Prometheus /metrics on host:port (off by default)");
                println!("        --port-file <PATH>     Write the bound listener ports to PATH after startup (use with --port 0)");
                println!("        --max-connections <N>  Ceiling on concurrent connections (default: 1024)");
                println!("        --shutdown-timeout <SECS>  Seconds a graceful shutdown waits for connections to drain before exiting non-zero (default: 30)");
                println!("        --wal-checkpoint-bytes <BYTES>  WAL size at which a finished statement checkpoints (default: 64 MiB; 0 disables, leaving the log to grow until shutdown)");
                println!("        --readonly             Serve the data directory read-only (snapshot serving; mutations are refused)");
                println!("    -V, --version              Print version and exit");
                println!("    -h, --help                 Print this message");
                println!();
                println!("ENVIRONMENT:");
                println!("    POWDB_PORT, POWDB_BIND, POWDB_DATA");
                println!("    POWDB_PASSWORD             Set password for client authentication");
                println!("    POWDB_ADMIN_USER           Create this user with role admin at startup if absent (needs POWDB_ADMIN_PASSWORD)");
                println!(
                    "    POWDB_ADMIN_PASSWORD       Password for POWDB_ADMIN_USER; never logged"
                );
                println!("    POWDB_TLS_CERT, POWDB_TLS_KEY");
                println!("    POWDB_REQUIRE_TLS          Refuse to start with a password but no TLS (default: off)");
                println!("    POWDB_IDLE_TIMEOUT, POWDB_QUERY_TIMEOUT");
                println!("    POWDB_TX_WAIT_TIMEOUT_MS   Max ms a BEGIN waits for a concurrent explicit transaction (default: 5000)");
                println!("    POWDB_TX_MAX_LIFETIME_MS   Max ms one connection may hold an open explicit transaction before the server rolls it back (default: 300000; 0 disables)");
                println!("    POWDB_DB_NAME              Reject a CONNECT that explicitly names a different database (default: accept any)");
                println!("    POWDB_QUERY_MEMORY_LIMIT   Per-query memory budget in bytes (default: 256 MiB)");
                println!("    POWDB_MAX_NESTED_LOOP_PAIRS  Fallback nested-loop join candidate-pair cap (default: 6,400,000)");
                println!("    POWDB_DIRTY_PAGE_BUDGET    Ceiling in bytes on unflushed heap pages inside an explicit transaction (default: 256 MiB)");
                println!("    POWDB_WAL_CHECKPOINT_BYTES WAL size at which a finished statement checkpoints (default: 64 MiB; 0 disables)");
                println!("    POWDB_METRICS_ADDR         host:port for the Prometheus /metrics endpoint (unauthenticated)");
                println!("    POWDB_SOCKET               Path for an additional Unix-domain-socket listener (off by default)");
                println!("    POWDB_PORT_FILE            Write the bound listener ports here after startup (use with --port 0)");
                println!("    POWDB_SYNC_MODE            WAL durability: full (default) | normal (bounded-loss, ~15-40x faster) | off (bench-only)");
                println!("    POWDB_READONLY             Serve read-only (snapshot serving) when truthy (1/true/yes/on)");
                println!("    POWDB_MAX_CONNECTIONS      Ceiling on concurrent connections (default: 1024)");
                println!("    POWDB_SHUTDOWN_TIMEOUT     Seconds a graceful shutdown waits for connections to drain (default: 30)");
                println!("    NO_COLOR                   Disable ANSI colour in the log (also off automatically when stdout is not a terminal)");
                println!("    RUST_LOG=info|debug|trace  (defaults to info)");
                println!();
                println!("NOTES:");
                println!("    Every POWDB_* value above goes through the same validator as its flag: a value that");
                println!("    does not parse refuses startup naming the variable, it is never silently defaulted.");
                println!("    0 is not a way to disable --idle-timeout or --query-timeout; both are refused.");
                println!("    SIGHUP reloads auth.json; SIGINT/SIGTERM drain and checkpoint.");
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
        wal_checkpoint_bytes,
        require_tls,
        metrics_addr,
        port_file,
        socket,
        db_name,
        read_only,
        max_connections,
        shutdown_timeout_secs,
        sync_mode,
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
    users: Arc<handler::UserDirectory>,
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
            preauth_deadline: handler::DEFAULT_PREAUTH_DEADLINE,
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

/// A single DER node: its tag, its contents, and whatever follows it.
///
/// Enough of a reader to walk to a certificate's `notAfter`. Deliberately not
/// a general ASN.1 parser: it refuses anything it does not understand rather
/// than guessing, and every caller treats `None` as "say nothing".
fn der_read(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first_len, rest) = rest.split_first()?;
    let (len, rest) = if first_len < 0x80 {
        (usize::from(first_len), rest)
    } else {
        let count = usize::from(first_len & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let mut value = 0usize;
        for byte in &rest[..count] {
            value = (value << 8) | usize::from(*byte);
        }
        (value, &rest[count..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

/// The `notAfter` of an X.509 certificate, in unix seconds.
fn certificate_not_after_unix(cert_der: &[u8]) -> Option<i64> {
    let (tag, certificate, _) = der_read(cert_der)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, tbs, _) = der_read(certificate)?;
    if tag != 0x30 {
        return None;
    }
    // `[0] EXPLICIT version` is optional; everything after it is positional.
    let (tag, _, after_version) = der_read(tbs)?;
    let rest = if tag == 0xA0 { after_version } else { tbs };
    let (_, _, rest) = der_read(rest)?; // serialNumber
    let (_, _, rest) = der_read(rest)?; // signature
    let (_, _, rest) = der_read(rest)?; // issuer
    let (tag, validity, _) = der_read(rest)?;
    if tag != 0x30 {
        return None;
    }
    let (_, _, after_not_before) = der_read(validity)?;
    let (tag, not_after, _) = der_read(after_not_before)?;
    asn1_time_to_unix(tag, not_after)
}

/// An ASN.1 `UTCTime` (0x17) or `GeneralizedTime` (0x18) as unix seconds.
fn asn1_time_to_unix(tag: u8, bytes: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (year, rest) = match tag {
        // YYMMDDHHMMSSZ, with the RFC 5280 pivot at 50.
        0x17 => {
            let short: i64 = text.get(0..2)?.parse().ok()?;
            (
                if short >= 50 {
                    1900 + short
                } else {
                    2000 + short
                },
                text.get(2..)?,
            )
        }
        // YYYYMMDDHHMMSSZ.
        0x18 => (text.get(0..4)?.parse().ok()?, text.get(4..)?),
        _ => return None,
    };
    let month: i64 = rest.get(0..2)?.parse().ok()?;
    let day: i64 = rest.get(2..4)?.parse().ok()?;
    let hour: i64 = rest.get(4..6)?.parse().ok()?;
    let minute: i64 = rest.get(6..8)?.parse().ok()?;
    let second: i64 = match rest.get(8..10) {
        Some(text) => text.parse().ok()?,
        None => 0,
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`). Hand-rolled so reading a certificate's expiry does not
/// cost the server a date-library dependency.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// How far ahead of expiry a certificate is still worth a warning.
const CERT_EXPIRY_WARN_WINDOW_SECS: i64 = 30 * 24 * 60 * 60;

/// What to log about a certificate's remaining life, if anything.
///
/// A server started with an expired certificate logged `tls=true` and nothing
/// else. Every client then failed with `certificate expired` while the server
/// looked healthy, and nothing on the server side connected the two.
fn certificate_expiry_warning(not_after_unix: i64, now_unix: i64) -> Option<String> {
    let remaining = not_after_unix - now_unix;
    if remaining <= 0 {
        return Some(format!(
            "TLS certificate expired {} day(s) ago; every client will refuse this server with \
             `certificate expired`. Reissue it",
            (-remaining) / 86_400
        ));
    }
    if remaining <= CERT_EXPIRY_WARN_WINDOW_SECS {
        return Some(format!(
            "TLS certificate expires in {} day(s); reissue it before then or clients start \
             refusing this server",
            remaining / 86_400
        ));
    }
    None
}

/// The DER content bytes of the `prime-field` OID, 1.2.840.10045.1.1.
///
/// It appears in an EC key's `FieldID`, which only an EXPLICIT parameter
/// encoding carries: a named curve is one OID and has no `FieldID` at all.
const OID_PRIME_FIELD: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x01, 0x01];

/// Whether an EC private key states its curve as explicit parameters rather
/// than by name.
///
/// Stock macOS LibreSSL and OpenSSL 1.x write explicit parameters by default,
/// aws-lc refuses such a key, and rustls reports the refusal as `keys may not
/// be consistent: KeyMismatch`, which points at the one thing that is not
/// wrong: the key and the certificate match perfectly.
fn ec_key_uses_explicit_parameters(key_der: &[u8]) -> bool {
    key_der
        .windows(OID_PRIME_FIELD.len())
        .any(|window| window == OID_PRIME_FIELD)
}

/// Turn a rustls configuration failure into something an operator can act on.
fn tls_config_error(error: &str, key_path: &str, explicit_ec_parameters: bool) -> String {
    if explicit_ec_parameters {
        return format!(
            "TLS config error: {error}. The EC private key in {key_path} states its curve as \
             explicit parameters instead of by name, which this TLS stack refuses; the key and \
             the certificate do match. Regenerate it with `openssl genpkey -algorithm EC \
             -pkeyopt ec_paramgen_curve:P-256 -pkeyopt ec_param_enc:named_curve` (stock macOS \
             LibreSSL and OpenSSL 1.x write explicit parameters by default)"
        );
    }
    if error.contains("ExtensionValueInvalid") {
        return format!(
            "TLS config error: {error}. That is usually a DUPLICATE basicConstraints extension, \
             which OpenSSL 1.1 writes when a config sets it and `-addext` sets it again: reissue \
             the certificate with a single basicConstraints extension"
        );
    }
    format!("TLS config error: {error}")
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

    // Say something about the certificate the server is about to serve, while
    // there is still an operator reading the startup log.
    if let Some(leaf) = certs.first() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Some(message) =
            certificate_not_after_unix(leaf).and_then(|at| certificate_expiry_warning(at, now))
        {
            warn!(cert = %cert_path, "{message}");
        }
    }

    let explicit_ec_parameters = ec_key_uses_explicit_parameters(key.secret_der());
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| tls_config_error(&e.to_string(), key_path, explicit_ec_parameters))?;

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

/// The process-level signals the server acts on.
enum ProcessSignal {
    /// SIGINT (Ctrl-C) or SIGTERM: drain and exit.
    Shutdown,
    /// SIGHUP: reload what can be reloaded and keep serving.
    Reload,
}

/// Resolve when the process receives a signal it acts on: SIGINT (Ctrl-C) or,
/// on Unix, SIGTERM (the signal Docker `docker stop`, Kubernetes, and systemd
/// send) and SIGHUP. Awaiting only `ctrl_c()` would let SIGTERM fall through
/// to the kernel default and kill the process before the graceful drain +
/// checkpoint could run; SIGHUP had the same fate, so the conventional
/// "reload your configuration" signal killed the server without a checkpoint.
/// On non-Unix targets only Ctrl-C is available.
async fn process_signal() -> ProcessSignal {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => Some(s),
            Err(e) => {
                // Failing to install a handler is non-fatal: fall back to
                // SIGINT-only so the server still starts and Ctrl-C still drains.
                warn!(error = %e, "could not install SIGTERM handler; only Ctrl-C will drain");
                None
            }
        };
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(error = %e, "could not install SIGHUP handler; user reloads will not work");
                None
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => ProcessSignal::Shutdown,
            _ = async {
                match sigterm.as_mut() {
                    Some(s) => { s.recv().await; }
                    None => std::future::pending().await,
                }
            } => ProcessSignal::Shutdown,
            _ = async {
                match sighup.as_mut() {
                    Some(s) => { s.recv().await; }
                    None => std::future::pending().await,
                }
            } => ProcessSignal::Reload,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        ProcessSignal::Shutdown
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
    // ANSI escapes in a log nobody is reading as a terminal are noise: they
    // land in `docker logs`, in journald, and in whatever ships them onward.
    // `NO_COLOR` still forces them off for a real terminal.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_ansi(use_ansi())
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
    if let Some(bytes) = args.wal_checkpoint_bytes {
        engine.catalog_mut().set_wal_checkpoint_bytes(bytes);
        info!(
            wal_checkpoint_bytes = bytes,
            disabled = bytes == 0,
            "WAL checkpoint threshold (--wal-checkpoint-bytes / POWDB_WAL_CHECKPOINT_BYTES)"
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
        // WAL durability mode (POWDB_SYNC_MODE, validated in `parse_args`).
        // A read-only engine never writes, so durability configuration is moot.
        let sync_mode = args.sync_mode;
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
                // Display, not Debug: `user=Some("root")` is the shape of a
                // Rust value, not of a username.
                info!(
                    user = admin_user.as_deref().unwrap_or(""),
                    "bootstrapped admin user from environment"
                );
            }
            Err(e) => {
                error!(error = %e, "failed to persist bootstrapped admin user");
                std::process::exit(1);
            }
        }
    }
    let user_count = users.len();
    if user_count > 0 {
        info!(users = user_count, "multi-user authentication enabled");
    } else if args.password.is_none() {
        // TASK-09: Warn when neither a shared password nor users are configured.
        warn!("no password configured: all connections will be accepted without authentication");
    }
    // Serve from a directory that re-reads `auth.json` when it changes, so a
    // password rotated or a user deleted by `powdb-cli` against this running
    // server takes effect on the next login attempt instead of at the next
    // restart.
    let users = match handler::UserDirectory::load(std::path::Path::new(&args.data_dir)) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            error!(data_dir = %args.data_dir, error = %e, "failed to load user store (auth.json)");
            std::process::exit(1);
        }
    };

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

    let auth_configured = args.password.is_some() || user_count > 0;

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
    // The address actually bound — identical to `addr` unless the OS chose
    // the port (`--port 0`); logs and the port file report this one.
    let local_addr = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => {
            error!(addr = %addr, error = %e, "failed to read bound address");
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
    let metrics_local_addr = match metrics_listener.as_ref().map(|l| l.local_addr()) {
        Some(Ok(a)) => Some(a),
        Some(Err(e)) => {
            error!(error = %e, "failed to read bound metrics address");
            std::process::exit(1);
        }
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
                    // The socket carries the same access as the data
                    // directory, so it is created 0660 (owner and group)
                    // rather than the process umask's 0755, which let any
                    // local user open a connection.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Err(e) = std::fs::set_permissions(
                            path,
                            std::fs::Permissions::from_mode(UNIX_SOCKET_MODE),
                        ) {
                            error!(socket = %path, error = %e, "failed to restrict unix socket permissions");
                            std::process::exit(1);
                        }
                    }
                    info!(socket = %path, mode = format!("{UNIX_SOCKET_MODE:o}"), "unix domain socket listening");
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

    // Publish the bound ports before announcing readiness, so anything
    // watching the file connects to a listener that already exists. The
    // write is atomic (write + rename): a reader never sees a partial file.
    if let Some(path) = args.port_file.as_deref() {
        let mut contents = format!("port={}\n", local_addr.port());
        if let Some(maddr) = metrics_local_addr {
            contents.push_str(&format!("metrics={}\n", maddr.port()));
        }
        let tmp_path = format!("{path}.tmp");
        let written =
            std::fs::write(&tmp_path, contents).and_then(|()| std::fs::rename(&tmp_path, path));
        if let Err(e) = written {
            error!(path = %path, error = %e, "failed to write port file");
            std::process::exit(1);
        }
    }

    info!(
        addr = %local_addr, data_dir = %args.data_dir, auth = auth_configured,
        tls = tls_enabled,
        idle_timeout = args.idle_timeout_secs, query_timeout = args.query_timeout_secs,
        "powdb server listening"
    );

    let semaphore = Arc::new(Semaphore::new(args.max_connections));

    // Shutdown broadcast: `false` initially, flipped to `true` on SIGINT/SIGTERM.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Spawn the metrics endpoint now that the shutdown channel exists, so it
    // drains with the rest of the server on SIGINT/SIGTERM.
    if let Some(ml) = metrics_listener {
        if let Some(maddr) = metrics_local_addr {
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

    // One task owns the process signals. Handling them inside the accept loop
    // meant that whenever the loop was busy elsewhere, the signal was not
    // being polled: a server at its connection ceiling parked on the
    // connection semaphore and ignored SIGTERM entirely. Here the signal
    // always lands, and the accept loop only has to watch the flag.
    let signal_users = users.clone();
    tokio::spawn(async move {
        loop {
            match process_signal().await {
                ProcessSignal::Shutdown => {
                    warn!("received shutdown signal, draining connections...");
                    let _ = shutdown_tx.send(true);
                    return;
                }
                ProcessSignal::Reload => match signal_users.reload() {
                    Ok(count) => info!(signal = "SIGHUP", users = count, "reloaded auth.json"),
                    Err(e) => warn!(
                        signal = "SIGHUP",
                        error = %e,
                        "could not reload auth.json; keeping the loaded users"
                    ),
                },
            }
        }
    });

    // Two receivers: one the loop itself waits on, one the connection-slot
    // acquire races, since both must observe the flag and `changed()` needs
    // the receiver mutably.
    let mut loop_shutdown = shutdown_rx.clone();
    let mut slot_shutdown = shutdown_rx.clone();
    loop {
        tokio::select! {
            // Accept new connections.
            result = listener.accept() => {
                match result {
                    Ok((stream, peer)) => {
                        let permit = tokio::select! {
                            acquired = semaphore.clone().acquire_owned() => match acquired {
                                Ok(p) => p,
                                Err(_) => break,
                            },
                            _ = slot_shutdown.changed() => break,
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
                        let permit = tokio::select! {
                            acquired = semaphore.clone().acquire_owned() => match acquired {
                                Ok(p) => p,
                                Err(_) => break,
                            },
                            _ = slot_shutdown.changed() => break,
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

            // Stop accepting once the signal task has flipped the flag.
            _ = loop_shutdown.changed() => {
                if *loop_shutdown.borrow() {
                    break;
                }
            }
        }
    }

    // Wait for all in-flight connections to finish. The semaphore starts
    // at MAX_CONNECTIONS; each active connection holds one permit. When
    // all connections have closed, we can acquire all permits back.
    info!(
        "waiting for {} active connection(s) to drain",
        args.max_connections - semaphore.available_permits()
    );
    let drain_budget = std::time::Duration::from_secs(args.shutdown_timeout_secs);
    let permits = u32::try_from(args.max_connections).unwrap_or(u32::MAX);
    let drained = tokio::time::timeout(drain_budget, semaphore.acquire_many(permits))
        .await
        .is_ok();
    if !drained {
        // Waiting forever turns a stuck statement into a hung `docker stop`
        // that the orchestrator resolves with SIGKILL, which is the one exit
        // that skips the checkpoint. Give up on the budget, say so, and let
        // the exit code carry it.
        error!(
            shutdown_timeout_secs = args.shutdown_timeout_secs,
            still_active = args.max_connections - semaphore.available_permits(),
            "shutdown timeout elapsed with connections still in flight; cancelling and exiting non-zero"
        );
        drop(engine);
        if let Some(path) = args.socket.as_deref() {
            let _ = std::fs::remove_file(path);
        }
        std::process::exit(1);
    }
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

    /// One case per environment variable the audit found silently
    /// defaulting, checked through the exact validator `parse_args` uses.
    ///
    /// Each row is (variable, a value that must be refused). A refusal is an
    /// `Err` here and a non-zero exit in `parse_args`; the process test below
    /// pins that the two are actually wired together.
    #[test]
    fn every_env_setting_refuses_a_malformed_value() {
        assert!(parse_port("abc").is_err(), "POWDB_PORT");
        assert!(parse_port("70000").is_err(), "POWDB_PORT out of range");
        assert!(parse_timeout_secs("abc").is_err(), "POWDB_IDLE_TIMEOUT");
        assert!(
            parse_timeout_secs("0").is_err(),
            "POWDB_IDLE_TIMEOUT=0 expires immediately; it is not a way to disable the timeout"
        );
        assert!(parse_timeout_secs("abc").is_err(), "POWDB_QUERY_TIMEOUT");
        assert!(parse_timeout_secs("0").is_err(), "POWDB_QUERY_TIMEOUT=0");
        assert!(parse_wait_ms("abc").is_err(), "POWDB_TX_WAIT_TIMEOUT_MS");
        assert!(parse_wait_ms("0").is_err(), "POWDB_TX_WAIT_TIMEOUT_MS=0");
        assert!(
            parse_tx_max_lifetime("abc").is_err(),
            "POWDB_TX_MAX_LIFETIME_MS"
        );
        assert!(parse_sync_mode("bogus").is_err(), "POWDB_SYNC_MODE");
        assert!(parse_bool_setting("ture").is_err(), "POWDB_READONLY typo");
        assert!(
            parse_bool_setting("ture").is_err(),
            "POWDB_REQUIRE_TLS typo"
        );
        for value in ["1G", "64MiB", "64m", "0", "-1"] {
            assert!(
                parse_positive_count("bytes")(value).is_err(),
                "POWDB_QUERY_MEMORY_LIMIT={value} must be refused, not silently defaulted"
            );
        }
        assert!(
            parse_positive_count("candidate pairs")("nope").is_err(),
            "POWDB_MAX_NESTED_LOOP_PAIRS"
        );
        assert!(
            parse_positive_count("bytes")("nope").is_err(),
            "POWDB_DIRTY_PAGE_BUDGET"
        );
        assert!(
            parse_positive_count("connections")("0").is_err(),
            "POWDB_MAX_CONNECTIONS"
        );
        assert!(parse_timeout_secs("0").is_err(), "POWDB_SHUTDOWN_TIMEOUT");
    }

    /// A certificate's expiry is read straight off its DER, with no date
    /// library and no guessing.
    #[test]
    fn a_certificates_expiry_is_read_off_its_der() {
        let key = rcgen::KeyPair::generate().expect("generate key");
        let mut params =
            rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("cert params");
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2031, 6, 15);
        let cert = params.self_signed(&key).expect("self-signed cert");
        assert_eq!(
            certificate_not_after_unix(cert.der()),
            Some(1_939_248_000),
            "2031-06-15T00:00:00Z"
        );
    }

    /// Expired, nearly expired, and fine are three different answers, and only
    /// the first two are worth a log line.
    #[test]
    fn the_expiry_warning_fires_only_when_it_is_worth_reading() {
        let now = 1_800_000_000;
        let day = 86_400;
        let expired = certificate_expiry_warning(now - 3 * day, now).expect("expired");
        assert!(expired.contains("expired 3 day(s) ago"), "{expired}");
        assert!(expired.contains("certificate expired"), "{expired}");

        let soon = certificate_expiry_warning(now + 10 * day, now).expect("near expiry");
        assert!(soon.contains("expires in 10 day(s)"), "{soon}");

        assert!(certificate_expiry_warning(now + 31 * day, now).is_none());
    }

    /// A named-curve key is what a correct recipe produces, and must not be
    /// blamed for a failure it did not cause.
    #[test]
    fn a_named_curve_key_is_not_reported_as_explicit() {
        let key = rcgen::KeyPair::generate().expect("generate key");
        assert!(!ec_key_uses_explicit_parameters(&key.serialize_der()));
    }

    /// An explicit-parameter key carries the prime-field OID inside its
    /// FieldID. LibreSSL writes these by default and rustls reports them as
    /// `keys may not be consistent: KeyMismatch`, blaming the one thing that
    /// is not wrong.
    #[test]
    fn an_explicit_parameter_key_is_recognised_and_the_error_says_what_to_run() {
        let mut key = vec![0x30, 0x09, 0x06, 0x07];
        key.extend_from_slice(OID_PRIME_FIELD);
        assert!(ec_key_uses_explicit_parameters(&key));

        let message = tls_config_error(
            "keys may not be consistent: KeyMismatch",
            "/etc/powdb/key.pem",
            true,
        );
        assert!(message.contains("ec_param_enc:named_curve"), "{message}");
        assert!(message.contains("/etc/powdb/key.pem"), "{message}");
        assert!(
            message.contains("do match"),
            "the message must say the KeyMismatch is misleading: {message}"
        );
    }

    /// The other recipe failure worth naming.
    #[test]
    fn an_invalid_extension_error_names_the_duplicate_basic_constraints() {
        let message = tls_config_error(
            "invalid peer certificate: BadEncoding(ExtensionValueInvalid)",
            "/etc/powdb/key.pem",
            false,
        );
        assert!(message.contains("basicConstraints"), "{message}");
        assert!(message.contains("DUPLICATE"), "{message}");
    }

    /// The refusal names the setting and the value, so an operator can find
    /// the typo without reading the source.
    #[test]
    fn a_refusal_names_the_setting_and_the_value() {
        let message = invalid_setting("POWDB_PORT", "abc", "a port number between 0 and 65535");
        assert!(message.contains("POWDB_PORT"), "{message}");
        assert!(message.contains("abc"), "{message}");
        assert!(message.contains("65535"), "{message}");
    }

    #[test]
    fn every_env_setting_accepts_its_valid_values() {
        assert_eq!(parse_port("0").unwrap(), 0);
        assert_eq!(parse_port(" 5433 ").unwrap(), 5433);
        assert_eq!(parse_timeout_secs("300").unwrap(), 300);
        assert_eq!(parse_wait_ms("5000").unwrap(), 5000);
        assert_eq!(parse_positive_count("bytes")("  4096  ").unwrap(), 4096);
        assert_eq!(parse_sync_mode(" NORMAL ").unwrap(), WalSyncMode::Normal);
        assert_eq!(parse_sync_mode("off").unwrap(), WalSyncMode::Off);
        assert_eq!(parse_sync_mode("full").unwrap(), WalSyncMode::Full);
        for truthy in ["1", "true", "TRUE", "yes", "on"] {
            assert!(parse_bool_setting(truthy).unwrap(), "{truthy}");
        }
        for falsy in ["0", "false", "no", "off"] {
            assert!(!parse_bool_setting(falsy).unwrap(), "{falsy}");
        }
    }

    /// Only an explicit `0` turns the transaction-lifetime bound off. A typo
    /// must not restore the unbounded behavior that let one connection hold
    /// the write gate for as long as it liked; it now refuses startup.
    #[test]
    fn tx_max_lifetime_env_parsing_fails_safe() {
        assert_eq!(parse_tx_max_lifetime("0").unwrap(), None);
        assert_eq!(
            parse_tx_max_lifetime("  1500 ").unwrap(),
            Some(std::time::Duration::from_millis(1500))
        );
        assert!(parse_tx_max_lifetime("not-a-number").is_err());
        assert!(parse_tx_max_lifetime("-1").is_err());
    }

    /// The parsed value reaches the gate every connection is served through,
    /// which is the only place the bound can be enforced from.
    #[test]
    fn tx_max_lifetime_reaches_the_transaction_gate() {
        let gate = handler::new_tx_gate_with_max_tx_lifetime(
            parse_tx_max_lifetime("1500").expect("valid"),
        );
        assert_eq!(
            gate.max_tx_lifetime(),
            Some(std::time::Duration::from_millis(1500))
        );
        let default = handler::new_tx_gate();
        assert_eq!(
            default.max_tx_lifetime(),
            Some(handler::DEFAULT_TX_MAX_LIFETIME)
        );
        let disabled =
            handler::new_tx_gate_with_max_tx_lifetime(parse_tx_max_lifetime("0").expect("valid"));
        assert_eq!(disabled.max_tx_lifetime(), None);
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
        let limit = parse_positive_count("bytes")("2048").expect("valid");
        let dir = std::env::temp_dir().join(format!("powdb_srv_memlimit_{}", std::process::id()));
        // Hermetic: the path is pid-derived (not unique per run), so a stale dir
        // from an earlier run, or a reused pid, must not leak into this test.
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::with_memory_limit(&dir, limit).unwrap();
        assert_eq!(engine.query_memory_limit(), 2048);
    }

    /// 0 is the documented opt-out for the WAL checkpoint threshold, so the
    /// validator that refuses 0 everywhere else must accept it here, and a
    /// suffixed value must still be refused rather than defaulted.
    #[test]
    fn wal_checkpoint_bytes_accepts_the_opt_out_and_refuses_a_suffix() {
        assert_eq!(parse_wal_checkpoint_bytes("0"), Ok(0));
        assert_eq!(parse_wal_checkpoint_bytes("  1048576 "), Ok(1_048_576));
        let err = parse_wal_checkpoint_bytes("64MiB").expect_err("a suffix must be refused");
        assert!(err.contains("bytes"), "unexpected message: {err}");
        assert!(err.contains('0'), "the message must say what 0 does: {err}");
    }

    /// The parsed `POWDB_WAL_CHECKPOINT_BYTES` value reaches the engine's
    /// catalog and bounds the log there. The control half of the assertion is
    /// the point: without the wiring the catalog keeps
    /// `DEFAULT_WAL_CHECKPOINT_BYTES` (64 MiB), which no test-sized workload
    /// reaches, so a one-sided test would pass with the knob doing nothing.
    #[test]
    fn env_wal_checkpoint_bytes_is_applied_to_engine() {
        fn write_notes(dir: &std::path::Path, threshold: Option<u64>) -> u64 {
            let _ = std::fs::remove_dir_all(dir);
            let mut engine = Engine::with_memory_limit(dir, DEFAULT_QUERY_MEMORY_LIMIT).unwrap();
            if let Some(bytes) = threshold {
                engine.catalog_mut().set_wal_checkpoint_bytes(bytes);
            }
            engine
                .execute_powql("type Note { required body: string }")
                .expect("create type");
            for i in 0..400 {
                engine
                    .execute_powql(&format!(
                        "insert Note {{ body := \"{}{i}\" }}",
                        "x".repeat(64)
                    ))
                    .expect("insert");
            }
            std::fs::metadata(dir.join("wal.log")).expect("wal").len()
        }

        let base = std::env::temp_dir().join(format!("powdb_srv_walckpt_{}", std::process::id()));
        let bounded = write_notes(
            &base.join("bounded"),
            Some(parse_wal_checkpoint_bytes("4096").expect("valid")),
        );
        let unbounded = write_notes(&base.join("default"), None);
        assert!(
            bounded < unbounded,
            "the threshold did not bound the log: {bounded} bytes with it, {unbounded} without"
        );
        assert!(
            unbounded > 4096,
            "the workload is too small to prove anything: {unbounded} bytes"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The parsed `POWDB_DIRTY_PAGE_BUDGET` value reaches the engine's catalog.
    /// Without the wiring the catalog keeps `DEFAULT_DIRTY_PAGE_BUDGET`, which
    /// is what made the 256 MiB ceiling unoverridable.
    #[test]
    fn env_dirty_page_budget_is_applied_to_engine() {
        let budget = parse_positive_count("bytes")("32768").expect("valid");
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
