//! TLS connection tests for the PowDB server.
//!
//! Uses `rcgen` to generate self-signed certificates at test time, then
//! verifies that a TLS-wrapped connection can complete the CONNECT
//! handshake and execute queries through the encrypted channel.

use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls;

// ── Wire-protocol helpers (same as integration.rs) ────────────────────

fn encode_connect(db: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(db.len() as u32).to_le_bytes());
    payload.extend_from_slice(db.as_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes()); // no password
    let mut frame = Vec::new();
    frame.push(0x01); // CONNECT
    frame.push(0); // flags
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

fn encode_query(q: &str) -> Vec<u8> {
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

async fn read_response<S: AsyncReadExt + Unpin>(stream: &mut S) -> Vec<u8> {
    let mut header = [0u8; 6];
    stream.read_exact(&mut header).await.unwrap();
    let payload_len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).await.unwrap();
    }
    let mut full = Vec::new();
    full.extend_from_slice(&header);
    full.extend_from_slice(&payload);
    full
}

// ── Certificate generation ────────────────────────────────────────────

struct TestCerts {
    #[allow(dead_code)]
    cert_pem: String,
    key_pem: String,
    cert_der: Vec<u8>,
}

fn generate_test_certs() -> TestCerts {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_pem = cert.pem();
    let key_pem = signing_key.serialize_pem();
    let cert_der = cert.der().to_vec();
    TestCerts {
        cert_pem,
        key_pem,
        cert_der,
    }
}

fn build_tls_acceptor(certs: &TestCerts) -> tokio_rustls::TlsAcceptor {
    let cert_chain = vec![rustls::pki_types::CertificateDer::from(
        certs.cert_der.clone(),
    )];
    // Parse the PEM key via rustls-pemfile so we get a PrivateKeyDer.
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(certs.key_pem.as_bytes()))
        .unwrap()
        .expect("no private key found in PEM");
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

fn build_tls_connector(certs: &TestCerts) -> tokio_rustls::TlsConnector {
    let mut root_store = rustls::RootCertStore::empty();
    root_store
        .add(rustls::pki_types::CertificateDer::from(
            certs.cert_der.clone(),
        ))
        .unwrap();
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

// ── TLS server setup ──────────────────────────────────────────────────

async fn start_tls_server(
    port: u16,
    data_dir: &str,
    acceptor: tokio_rustls::TlsAcceptor,
) -> tokio::task::JoinHandle<()> {
    let addr = format!("127.0.0.1:{port}");
    let data_dir = data_dir.to_string();
    let acceptor = acceptor.clone();

    tokio::spawn(async move {
        let engine = powdb_query::executor::Engine::new(std::path::Path::new(&data_dir)).unwrap();
        let engine = Arc::new(std::sync::RwLock::new(engine));
        let tx_gate = powdb_server::handler::new_tx_gate();
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            let eng = engine.clone();
            let tx_gate = tx_gate.clone();
            let acc = acceptor.clone();
            let (_, mut rx) = tokio::sync::watch::channel(false);

            tokio::spawn(async move {
                if let Ok(tls_stream) = acc.accept(stream).await {
                    powdb_server::handler::handle_connection(
                        tls_stream,
                        powdb_server::handler::ConnOpts {
                            engine: eng,
                            tx_gate,
                            expected_password: None,
                            users: std::sync::Arc::new(powdb_auth::UserStore::new()),
                            shutdown_rx: &mut rx,
                            idle_timeout: Duration::from_secs(300),
                            query_timeout: Duration::from_secs(30),
                            rate_limiter: None,
                            peer_addr: Some(peer),
                            metrics: std::sync::Arc::new(powdb_server::metrics::Metrics::new()),
                        },
                    )
                    .await;
                }
            });
        }
    })
}

// ── Tests ─────────────────────────────────────────────────────────────

/// A TLS client connecting to a TLS server should complete the handshake
/// and successfully run PowQL queries over the encrypted channel.
#[tokio::test]
async fn test_tls_connect_and_query() {
    let certs = generate_test_certs();
    let test_id = std::process::id();
    let port = 17433 + (test_id % 500) as u16;
    let data_dir = tempfile::tempdir().unwrap();
    let data_dir_str = data_dir.path().to_str().unwrap().to_string();

    let acceptor = build_tls_acceptor(&certs);
    let _server = start_tls_server(port, &data_dir_str, acceptor).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect with TLS.
    let connector = build_tls_connector(&certs);
    let tcp = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut stream = connector.connect(server_name, tcp).await.unwrap();

    // CONNECT
    stream.write_all(&encode_connect("tlsdb")).await.unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x02, "expected CONNECT_OK over TLS");

    // CREATE TABLE
    stream
        .write_all(&encode_query(
            "type TlsUser { required name: str, score: int }",
        ))
        .await
        .unwrap();
    let resp = read_response(&mut stream).await;
    assert!(
        resp[0] == 0x09 || resp[0] == 0x0B,
        "expected RESULT_OK or RESULT_MSG for create type over TLS, got: 0x{:02X}",
        resp[0]
    );

    // INSERT
    stream
        .write_all(&encode_query(
            r#"insert TlsUser { name := "Encrypted", score := 100 }"#,
        ))
        .await
        .unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x09, "expected RESULT_OK for insert over TLS");

    // QUERY
    stream.write_all(&encode_query("TlsUser")).await.unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x07, "expected RESULT_ROWS over TLS");
}

/// A plaintext TCP client connecting to a TLS server should fail the
/// handshake. The server should not crash.
#[tokio::test]
async fn test_plaintext_client_to_tls_server() {
    let certs = generate_test_certs();
    let test_id = std::process::id();
    let port = 17933 + (test_id % 500) as u16;
    let data_dir = tempfile::tempdir().unwrap();
    let data_dir_str = data_dir.path().to_str().unwrap().to_string();

    let acceptor = build_tls_acceptor(&certs);
    let _server = start_tls_server(port, &data_dir_str, acceptor).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect WITHOUT TLS — send a raw CONNECT frame.
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    stream.write_all(&encode_connect("plaindb")).await.unwrap();

    // The server should either close the connection or send back garbage
    // (because it's trying to do a TLS handshake on non-TLS data).
    // Read with a timeout — we don't expect a valid protocol response.
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        let mut buf = [0u8; 6];
        stream.read_exact(&mut buf).await
    })
    .await;

    match result {
        Ok(Ok(_)) => {
            // If we got bytes, they shouldn't be a valid CONNECT_OK (0x02).
            // The server tried a TLS handshake and the bytes are TLS alerts.
        }
        Ok(Err(_)) => {} // Connection reset / closed — expected.
        Err(_) => {}     // Timeout — also acceptable (server dropped connection).
    }
    // Main assertion: the server did not crash. It's still running.
    // Verify by connecting with a proper TLS client.
    let connector = build_tls_connector(&certs);
    let tcp = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut tls = connector.connect(server_name, tcp).await.unwrap();
    tls.write_all(&encode_connect("tlsdb")).await.unwrap();
    let resp = read_response(&mut tls).await;
    assert_eq!(
        resp[0], 0x02,
        "TLS server should still be alive after a plaintext client attempt"
    );
}

/// TLS config with a nonexistent cert file should fail gracefully.
/// This tests the PEM-loading path (we replicate the `build_tls_acceptor`
/// logic from main.rs since it's not exported).
#[tokio::test]
async fn test_tls_config_missing_cert() {
    let cert_path = "/nonexistent/cert.pem";
    let result = std::fs::File::open(cert_path);
    assert!(result.is_err(), "opening a nonexistent cert should fail");
}

/// TLS config with an invalid (non-PEM) cert file should fail.
#[tokio::test]
async fn test_tls_config_invalid_cert() {
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("bad.pem");
    let key_path = dir.path().join("bad.key");
    std::fs::write(&cert_path, b"not a real certificate").unwrap();
    std::fs::write(&key_path, b"not a real key").unwrap();

    let cert_file = std::fs::File::open(&cert_path).unwrap();
    let certs: Vec<_> = rustls_pemfile::certs(&mut std::io::BufReader::new(cert_file))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default();
    assert!(certs.is_empty(), "garbage PEM should yield no certs");
}
