use powdb_query::executor::Engine;
use powdb_server::handler;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Maximum number of concurrent connections.
const MAX_CONNECTIONS: usize = 1024;

struct Args {
    port: u16,
    bind: String,
    data_dir: String,
    password: Option<String>,
    idle_timeout_secs: u64,
    query_timeout_secs: u64,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    query_memory_limit: usize,
}

/// Default per-query memory budget (bytes) when `POWDB_QUERY_MEMORY_LIMIT` is
/// unset or unparseable. Mirrors the query crate's default (256 MB).
const DEFAULT_QUERY_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// Parse the per-query memory limit from the `POWDB_QUERY_MEMORY_LIMIT`
/// environment value. Accepts a plain byte count; falls back to the default
/// when unset, empty, or unparseable. Pulled out as a free function so it can
/// be unit-tested without spawning the server.
fn parse_query_memory_limit(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_QUERY_MEMORY_LIMIT)
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
    // Password is set exclusively via environment variable.
    let password: Option<String> = std::env::var("POWDB_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());
    let mut idle_timeout_secs: u64 = std::env::var("POWDB_IDLE_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300); // 5 min default
    let mut query_timeout_secs: u64 = std::env::var("POWDB_QUERY_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30); // 30s default
    let mut tls_cert: Option<String> = std::env::var("POWDB_TLS_CERT")
        .ok()
        .filter(|s| !s.is_empty());
    let mut tls_key: Option<String> = std::env::var("POWDB_TLS_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    // WS2: per-query memory budget. Env-only (no CLI flag) for now.
    let query_memory_limit =
        parse_query_memory_limit(std::env::var("POWDB_QUERY_MEMORY_LIMIT").ok().as_deref());

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
                println!("    -d, --data-dir <PATH>      Data directory (default: ./powdb_data)");
                println!("        --tls-cert <PATH>      TLS certificate file (PEM)");
                println!("        --tls-key <PATH>       TLS private key file (PEM)");
                println!("        --idle-timeout <SECS>  Idle connection timeout (default: 300)");
                println!(
                    "        --query-timeout <SECS> Per-query execution timeout (default: 30)"
                );
                println!("    -V, --version              Print version and exit");
                println!("    -h, --help                 Print this message");
                println!();
                println!("ENVIRONMENT:");
                println!("    POWDB_PORT, POWDB_BIND, POWDB_DATA");
                println!("    POWDB_PASSWORD             Set password for client authentication");
                println!("    POWDB_TLS_CERT, POWDB_TLS_KEY");
                println!("    POWDB_IDLE_TIMEOUT, POWDB_QUERY_TIMEOUT");
                println!("    POWDB_QUERY_MEMORY_LIMIT   Per-query memory budget in bytes (default: 256 MiB)");
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
        tls_cert,
        tls_key,
        query_memory_limit,
    }
}

/// Load TLS certificate and key files, returning a configured `TlsAcceptor`.
fn build_tls_acceptor(
    cert_path: &str,
    key_path: &str,
) -> Result<tokio_rustls::TlsAcceptor, Box<dyn std::error::Error>> {
    use std::io::BufReader;
    use tokio_rustls::rustls;

    let cert_file = std::fs::File::open(cert_path)
        .map_err(|e| format!("failed to open TLS cert {cert_path}: {e}"))?;
    let key_file = std::fs::File::open(key_path)
        .map_err(|e| format!("failed to open TLS key {key_path}: {e}"))?;

    let certs: Vec<_> = rustls_pemfile::certs(&mut BufReader::new(cert_file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to parse TLS certs: {e}"))?;

    let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
        .map_err(|e| format!("failed to parse TLS key: {e}"))?
        .ok_or("no private key found in TLS key file")?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS config error: {e}"))?;

    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
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

    // TASK-09: Warn when no password is configured.
    if args.password.is_none() {
        warn!("no password configured — all connections will be accepted without authentication");
    }

    let engine = match Engine::with_memory_limit(
        std::path::Path::new(&args.data_dir),
        args.query_memory_limit,
    ) {
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
    let engine = Arc::new(RwLock::new(engine));

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

    // CRITICAL: warn when password auth is enabled without TLS encryption.
    if args.password.is_some() && tls_acceptor.is_none() {
        warn!("WARNING: Password authentication enabled without TLS. Credentials will be sent in plaintext.");
        eprintln!("!!! CRITICAL: Password authentication enabled without TLS. Credentials will be sent in plaintext. !!!");
    }

    let addr = format!("{}:{}", args.bind, args.port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(addr = %addr, error = %e, "failed to bind");
            std::process::exit(1);
        }
    };

    info!(
        addr = %addr, data_dir = %args.data_dir, auth = %args.password.is_some(),
        tls = tls_enabled,
        idle_timeout = args.idle_timeout_secs, query_timeout = args.query_timeout_secs,
        "powdb server listening"
    );

    let semaphore = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    // Shutdown broadcast: `false` initially, flipped to `true` on SIGINT/SIGTERM.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let idle_timeout = std::time::Duration::from_secs(args.idle_timeout_secs);
    let query_timeout = std::time::Duration::from_secs(args.query_timeout_secs);

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
                        let pw = args.password.clone();
                        let mut rx = shutdown_rx.clone();
                        let idle = idle_timeout;
                        let qtimeout = query_timeout;
                        let rl = rate_limiter.clone();
                        let tls = tls_acceptor.clone();
                        tokio::spawn(async move {
                            let peer_addr = Some(peer);
                            if let Some(acceptor) = tls {
                                match acceptor.accept(stream).await {
                                    Ok(tls_stream) => {
                                        handler::handle_connection(
                                            tls_stream,
                                            handler::ConnOpts {
                                                engine: eng, expected_password: pw,
                                                shutdown_rx: &mut rx, idle_timeout: idle,
                                                query_timeout: qtimeout, rate_limiter: Some(&rl),
                                                peer_addr,
                                            },
                                        ).await;
                                    }
                                    Err(e) => {
                                        warn!(peer = %peer, error = %e, "TLS handshake failed");
                                    }
                                }
                            } else {
                                handler::handle_connection(
                                    stream,
                                    handler::ConnOpts {
                                        engine: eng, expected_password: pw,
                                        shutdown_rx: &mut rx, idle_timeout: idle,
                                        query_timeout: qtimeout, rate_limiter: Some(&rl),
                                        peer_addr,
                                    },
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

            // Graceful shutdown on SIGINT (Ctrl-C).
            _ = tokio::signal::ctrl_c() => {
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
    info!("clean shutdown complete");
}

#[cfg(test)]
mod tests {
    use super::*;
    use powdb_query::executor::Engine;

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

    /// The parsed env limit is actually applied to the constructed Engine.
    #[test]
    fn env_limit_is_applied_to_engine() {
        let limit = parse_query_memory_limit(Some("2048"));
        let dir = std::env::temp_dir().join(format!("powdb_srv_memlimit_{}", std::process::id()));
        let engine = Engine::with_memory_limit(&dir, limit).unwrap();
        assert_eq!(engine.query_memory_limit(), 2048);
    }
}
