//! Remote (wire-protocol) execution: TLS, the handshake, one-shot and REPL.

use super::*;

// ─── Remote TLS support ─────────────────────────────────────────────────────

/// A remote connection stream: plaintext TCP or TLS over TCP. The TLS variant
/// is boxed because `TlsStream` is much larger than `TcpStream`.
pub(crate) enum RemoteStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

/// Build a rustls client connector on the same tokio-rustls stack the server
/// uses. With a `--tls-ca` bundle those certificates are the only trust roots
/// (the self-signed deployment case); otherwise the compiled-in webpki-roots
/// bundle (Mozilla's CA program) is used.
pub(crate) fn build_tls_connector(
    ca_path: Option<&str>,
) -> Result<tokio_rustls::TlsConnector, String> {
    use tokio_rustls::rustls;
    // See the matching note in powdb-server: `rustls-pemfile` is unmaintained
    // (RUSTSEC-2025-0134) and this API now lives in `rustls-pki-types`.
    use rustls::pki_types::pem::PemObject;

    let mut roots = rustls::RootCertStore::empty();
    match ca_path {
        Some(path) => {
            let file = std::fs::File::open(path)
                .map_err(|e| format!("failed to open TLS CA file {path}: {e}"))?;
            let certs: Vec<_> =
                rustls::pki_types::CertificateDer::pem_reader_iter(std::io::BufReader::new(file))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("failed to parse TLS CA file {path}: {e}"))?;
            if certs.is_empty() {
                return Err(format!("no certificates found in TLS CA file {path}"));
            }
            for cert in certs {
                roots
                    .add(cert)
                    .map_err(|e| format!("invalid certificate in TLS CA file {path}: {e}"))?;
            }
        }
        None => {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_rustls::TlsConnector::from(std::sync::Arc::new(
        config,
    )))
}

/// The name the server certificate is verified against: the
/// `--tls-server-name` override when set, else the host part of `host:port`
/// (with IPv6 brackets stripped). IP-address names are supported when the
/// certificate carries an IP SAN; certs issued for a hostname need
/// `--tls-server-name` when connecting by IP.
pub(crate) fn resolve_tls_server_name(
    addr: &str,
    override_name: Option<&str>,
) -> Result<tokio_rustls::rustls::pki_types::ServerName<'static>, String> {
    let name = match override_name {
        Some(n) => n.to_string(),
        None => {
            let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .to_string()
        }
    };
    tokio_rustls::rustls::pki_types::ServerName::try_from(name.clone())
        .map_err(|_| format!("invalid TLS server name: {name}"))
}

/// Open the remote connection, wrapping it in TLS when requested. With TLS
/// disabled this is exactly the plaintext `TcpStream::connect` path.
pub(crate) async fn connect_remote(addr: &str, tls: &TlsOpts) -> Result<RemoteStream, String> {
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connection failed: {e}"))?;
    if !tls.enabled {
        return Ok(RemoteStream::Plain(stream));
    }
    let connector = build_tls_connector(tls.ca_path.as_deref())?;
    let server_name = resolve_tls_server_name(addr, tls.server_name.as_deref())?;
    match connector.connect(server_name, stream).await {
        Ok(s) => Ok(RemoteStream::Tls(Box::new(s))),
        Err(e) => Err(format!(
            "TLS handshake with {addr} failed: {e} \
             (self-signed server? pass --tls-ca <ca.pem>; connecting by IP to a \
             hostname certificate? pass --tls-server-name <name>)"
        )),
    }
}

// ─── One-shot execution (remote) ────────────────────────────────────────────

pub(crate) async fn exec_remote(
    addr: String,
    db: String,
    password: Option<String>,
    username: Option<String>,
    query: String,
    session: SessionOpts,
    tls: &TlsOpts,
) -> i32 {
    match connect_remote(&addr, tls).await {
        Ok(RemoteStream::Plain(s)) => {
            exec_remote_on(s, db, password, username, query, session).await
        }
        Ok(RemoteStream::Tls(s)) => {
            exec_remote_on(*s, db, password, username, query, session).await
        }
        Err(msg) => {
            eprintln!("Error: {msg}");
            1
        }
    }
}

/// Why the client half of the wire handshake did not complete.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HandshakeFailure {
    /// The server answered and said no (bad credentials, unknown database,
    /// or a protocol version it cannot serve).
    Refused(String),
    /// No usable reply arrived: the connection closed, the read failed, or
    /// the frame was not a handshake reply at all. A plaintext connection to
    /// a TLS-only listener looks like this.
    NoReply(String),
}

impl HandshakeFailure {
    pub(crate) fn message(&self) -> &str {
        match self {
            HandshakeFailure::Refused(m) | HandshakeFailure::NoReply(m) => m,
        }
    }
}

/// The CONNECT frame this CLI sends: credentials plus its protocol range,
/// catalog ceiling, and feature set.
pub(crate) fn client_connect_message(
    db: String,
    password: Option<String>,
    username: Option<String>,
) -> Message {
    Message::ConnectWithHello {
        db_name: db,
        password: password.map(Into::into),
        username,
        hello: ClientHello::current(),
    }
}

/// Classify a handshake reply and apply the client-side capability check.
///
/// Pure, so the whole matrix (negotiating server, pre-0.22.0 server, refusal,
/// wrong frame, closed socket) is testable without a socket. A server this
/// CLI cannot talk to is rejected here, at handshake time, rather than on the
/// first frame it turns out not to understand.
pub(crate) fn classify_handshake_reply(
    reply: Option<Message>,
) -> Result<(String, ServerHello), HandshakeFailure> {
    let (version, hello) = match reply {
        // A pre-0.22.0 server states nothing, which negotiates as protocol v1
        // with no named features. That is still a usable server.
        Some(Message::ConnectOk { version }) => (version, ServerHello::legacy()),
        Some(Message::ConnectOkWithHello { version, hello }) => (version, hello),
        Some(Message::Error { message }) => {
            return Err(HandshakeFailure::Refused(format!(
                "server rejected connection: {message}"
            )));
        }
        Some(other) => {
            return Err(HandshakeFailure::NoReply(format!(
                "unexpected handshake reply: {other:?}"
            )));
        }
        None => {
            return Err(HandshakeFailure::NoReply(
                "server closed connection during handshake".into(),
            ));
        }
    };
    require_server_capabilities(
        &hello,
        MIN_SUPPORTED_PROTOCOL_VERSION,
        &[],
        CLIENT_CATALOG_VERSION,
    )
    .map_err(HandshakeFailure::Refused)?;
    Ok((version, hello))
}

/// Whether this session should ask the server for typed results, noting once
/// (via `warned`, like [`note_continuation_when_piped`]) when `--format json`
/// cannot be typed because the server predates the typed wire frames.
///
/// `--format json` is sold as the scriptable output, but the legacy `Query`
/// frame stringifies every cell server-side, so an int came back as `"1"`
/// remotely and `1` embedded: a `jq` numeric comparison written against an
/// embedded run silently stopped matching once it was pointed at a server. The
/// typed frames carry storage values, so the CLI renders them with the same
/// code embedded mode uses.
///
/// `table` and `csv` deliberately stay on the legacy frame. Their cells are the
/// server's own display formatting, and this fix must not shift them.
pub(crate) fn negotiate_typed_json(
    output: OutputMode,
    hello: &ServerHello,
    warned: &mut bool,
) -> bool {
    if output != OutputMode::Json {
        return false;
    }
    if hello.has(FEATURE_NATIVE_TYPED) {
        return true;
    }
    if !*warned {
        *warned = true;
        eprintln!(
            "note: this server is too old to send typed results, so --format json renders \
             every cell as a JSON string; upgrade the server to get int, float, and bool types"
        );
    }
    false
}

/// The query frame for one statement: typed when the session negotiated typed
/// results, else the legacy stringly-typed frame.
pub(crate) fn query_message(query: String, dialect: Dialect, typed: bool) -> Message {
    match (dialect, typed) {
        (Dialect::Powql, false) => Message::Query { query },
        (Dialect::Powql, true) => Message::QueryNative { query },
        (Dialect::Sql, false) => Message::QuerySql { query },
        (Dialect::Sql, true) => Message::QuerySqlNative { query },
    }
}

/// The hint appended when the handshake produced no usable reply, which is
/// what a plaintext connection to a TLS-only listener looks like.
pub(crate) const TLS_HANDSHAKE_HINT: &str =
    "server may require TLS; try --tls, and --tls-ca <ca.pem> for self-signed certificates";

pub(crate) async fn exec_remote_on<S>(
    stream: S,
    db: String,
    password: Option<String>,
    username: Option<String>,
    query: String,
    session: SessionOpts,
) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    let connect = client_connect_message(db, password, username);
    if connect.write_to(&mut writer).await.is_err()
        || tokio::io::AsyncWriteExt::flush(&mut writer).await.is_err()
    {
        eprintln!("Error: failed to send CONNECT");
        return 1;
    }
    let reply = Message::read_from(&mut reader).await.ok().flatten();
    let hello = match classify_handshake_reply(reply) {
        Ok((_version, hello)) => hello,
        Err(failure) => {
            match failure {
                HandshakeFailure::Refused(message) => eprintln!("Error: {message}"),
                HandshakeFailure::NoReply(message) => {
                    eprintln!("Error: handshake failed: {message} ({TLS_HANDSHAKE_HINT})");
                }
            }
            return 1;
        }
    };
    let typed = negotiate_typed_json(session.output, &hello, &mut false);

    // The wire protocol carries one statement per `Query` message, so split
    // client-side (#150) and send each in turn, stopping on the first error.
    let mut code = 0;
    let statements = split_statements_in(&query, session.dialect);
    let statement_count = statements.len();
    for stmt in statements {
        // Comment-only segments never reach the wire, same as embedded
        // one-shot and the REPL: they are not statements, and the server
        // would answer "expected statement, got end of input".
        if is_effectively_blank_in(stmt, session.dialect) {
            continue;
        }
        if stmt.starts_with('.') {
            let cmd = stmt.split_whitespace().next().unwrap_or(stmt);
            eprintln!(
                "Error: '{}' is a REPL-only command \u{2014} start the interactive REPL without -c to use it",
                cmd
            );
            code = 1;
            break;
        }

        let q = query_message(stmt.to_string(), session.dialect, typed);
        if q.write_to(&mut writer).await.is_err()
            || tokio::io::AsyncWriteExt::flush(&mut writer).await.is_err()
        {
            eprintln!("Error: write failed");
            code = 1;
            break;
        }

        match Message::read_from(&mut reader).await {
            Ok(Some(msg)) => {
                let is_error = matches!(msg, Message::Error { .. });
                print_remote_result(&msg, session.output);
                if is_error {
                    if let Some(hint) = missing_separator_hint(&query, statement_count) {
                        eprintln!("{hint}");
                    }
                    code = 1;
                    break;
                }
            }
            Ok(None) => {
                eprintln!("Error: server closed connection");
                code = 1;
                break;
            }
            Err(e) => {
                eprintln!("Error: read failed: {e}");
                code = 1;
                break;
            }
        }
    }

    let _ = Message::Disconnect.write_to(&mut writer).await;
    let _ = tokio::io::AsyncWriteExt::flush(&mut writer).await;
    code
}

// ─── Remote (wire protocol) mode ────────────────────────────────────────────

pub(crate) async fn run_remote(
    addr: String,
    db: String,
    password: Option<String>,
    username: Option<String>,
    session: SessionOpts,
    tls: &TlsOpts,
) {
    eprintln!("PowDB v{} — remote mode", env!("CARGO_PKG_VERSION"));
    eprintln!("Connecting to {addr} ...");

    match connect_remote(&addr, tls).await {
        Ok(RemoteStream::Plain(s)) => run_remote_on(s, db, password, username, session).await,
        Ok(RemoteStream::Tls(s)) => run_remote_on(*s, db, password, username, session).await,
        Err(msg) => {
            eprintln!("Error: {msg}");
            std::process::exit(1);
        }
    }
}

pub(crate) async fn run_remote_on<S>(
    stream: S,
    db: String,
    password: Option<String>,
    username: Option<String>,
    session: SessionOpts,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    // Send CONNECT
    let connect = client_connect_message(db.clone(), password, username);
    if let Err(e) = connect.write_to(&mut writer).await {
        eprintln!("Error: failed to send CONNECT: {e}");
        std::process::exit(1);
    }
    if let Err(e) = tokio::io::AsyncWriteExt::flush(&mut writer).await {
        eprintln!("Error: flush failed: {e}");
        std::process::exit(1);
    }

    // Read CONNECT_OK or ERROR
    let reply = match Message::read_from(&mut reader).await {
        Ok(reply) => reply,
        Err(e) => {
            eprintln!("Error: handshake read failed: {e}");
            std::process::exit(1);
        }
    };
    let server_hello = match classify_handshake_reply(reply) {
        Ok((version, hello)) => {
            eprintln!(
                "Connected to db `{db}` (server v{version}, wire protocol v{})",
                hello.protocol
            );
            eprintln!("Type PowQL queries. Use Ctrl-D to exit.\n");
            hello
        }
        Err(failure) => {
            eprintln!("Error: {}", failure.message());
            std::process::exit(1);
        }
    };

    let mut rl = match Editor::new() {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("Error: failed to initialize readline: {e}");
            std::process::exit(1);
        }
    };
    rl.set_helper(Some(PowqlHelper));

    let hist = history_path();
    rl.load_history(&hist).ok();

    let mut state = ReplState {
        timing: false,
        dialect: session.dialect,
        output: session.output,
    };
    let mut buffer = String::new();
    let interactive = std::io::IsTerminal::is_terminal(&io::stdin());
    let mut continuation_noted = false;
    let mut untyped_json_warned = false;
    let mut one_off_sql: Option<String>;

    loop {
        let prompt = if buffer.is_empty() {
            state.dialect.prompt()
        } else {
            "  ...> "
        };
        let line = match rl.readline(prompt) {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(rustyline::error::ReadlineError::Interrupted) => {
                buffer.clear();
                continuation_noted = false;
                continue;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        };

        // `.cancel` works mid-continuation, unlike every other meta-command.
        if is_cancel_line(&line) {
            rl.add_history_entry(line.trim()).ok();
            cancel_buffer(&mut buffer);
            continuation_noted = false;
            continue;
        }

        one_off_sql = None;

        // Meta-commands are only recognized at the start of a statement.
        if buffer.is_empty() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Handle local-only meta-commands in remote mode
            if trimmed.starts_with('.') {
                rl.add_history_entry(trimmed).ok();
                match handle_shared_meta(trimmed, &mut state) {
                    MetaOutcome::Quit => break,
                    MetaOutcome::Handled => continue,
                    MetaOutcome::RunSql(stmt) => one_off_sql = Some(stmt),
                    MetaOutcome::Unhandled => {
                        if trimmed == ".help" {
                            println!("Meta-commands (remote mode):");
                            println!("  .sql [STMT]      Run STMT as SQL, or switch to SQL mode");
                            println!("  .powql           Switch back to PowQL");
                            println!("  .mode <FMT>      Render results as table, json, or csv");
                            println!("  .cancel          Discard an unterminated statement");
                            println!("  .timing          Toggle query timing on/off");
                            println!("  .help            Show this help");
                            println!("  .quit / .exit    Exit the REPL");
                            println!();
                            println!(
                                "Note: .tables and .schema are only available in embedded mode."
                            );
                        } else {
                            eprintln!(
                                "Meta-commands (.tables, .schema) are not supported in remote mode."
                            );
                            eprintln!("Type .help for available commands.");
                        }
                        continue;
                    }
                }
            }
        }

        // Accumulate input until braces/parens balance outside strings.
        let (statement, statement_dialect) = match one_off_sql {
            Some(stmt) => (stmt, Dialect::Sql),
            None => {
                buffer.push_str(&line);
                buffer.push('\n');
                if needs_continuation(&buffer) {
                    note_continuation_when_piped(interactive, &mut continuation_noted);
                    continue;
                }
                continuation_noted = false;
                let statement = buffer.trim().to_string();
                buffer.clear();
                if is_effectively_blank_in(&statement, session.dialect) {
                    continue;
                }
                rl.add_history_entry(&statement).ok();
                (statement, state.dialect)
            }
        };

        // Re-negotiated per statement because `.mode json` can switch the
        // session's rendering mid-REPL.
        let typed = negotiate_typed_json(state.output, &server_hello, &mut untyped_json_warned);
        let q = query_message(statement, statement_dialect, typed);
        if q.write_to(&mut writer).await.is_err() {
            eprintln!("Error: write failed — disconnected");
            break;
        }
        if tokio::io::AsyncWriteExt::flush(&mut writer).await.is_err() {
            eprintln!("Error: flush failed — disconnected");
            break;
        }

        let start = Instant::now();
        match Message::read_from(&mut reader).await {
            Ok(Some(msg)) => {
                print_remote_result(&msg, state.output);
                if state.timing {
                    let elapsed = start.elapsed();
                    if elapsed.as_secs() >= 1 {
                        println!("Time: {:.2}s", elapsed.as_secs_f64());
                    } else {
                        println!("Time: {:.2}ms", elapsed.as_secs_f64() * 1000.0);
                    }
                }
            }
            Ok(None) => {
                eprintln!("Error: server closed connection");
                break;
            }
            Err(e) => {
                eprintln!("Error: read failed: {e}");
                break;
            }
        }
    }

    // Best-effort goodbye
    let _ = Message::Disconnect.write_to(&mut writer).await;
    let _ = tokio::io::AsyncWriteExt::flush(&mut writer).await;

    warn_unterminated_at_eof(&buffer);
    rl.save_history(&hist).ok();
    eprintln!("\nBye!");
}
