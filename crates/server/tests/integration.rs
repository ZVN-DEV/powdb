use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn encode_connect(db: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(db.len() as u32).to_le_bytes());
    payload.extend_from_slice(db.as_bytes());
    // Empty password (len=0) means None
    payload.extend_from_slice(&0u32.to_le_bytes());
    let mut frame = Vec::new();
    frame.push(0x01); // CONNECT
    frame.push(0); // flags
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// Encode a CONNECT frame carrying db_name + password + username, exercising
/// the real wire encoder so the test depends on the actual protocol.
fn encode_connect_user(db: &str, password: &str, username: &str) -> Vec<u8> {
    powdb_server::protocol::Message::Connect {
        db_name: db.to_string(),
        password: Some(zeroize::Zeroizing::new(password.to_string())),
        username: Some(username.to_string()),
    }
    .encode()
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

async fn read_response(stream: &mut TcpStream) -> Vec<u8> {
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

#[tokio::test]
async fn test_full_lifecycle() {
    // Use a unique port and temp dir to avoid conflicts with parallel tests
    let test_id = std::process::id();
    let port = 15433 + (test_id % 1000) as u16;
    let data_dir = std::env::temp_dir().join(format!("powdb_integ_{test_id}"));
    std::fs::create_dir_all(&data_dir).unwrap();
    let data_dir_str = data_dir.to_str().unwrap().to_string();

    let addr = format!("127.0.0.1:{port}");
    let bind_addr = addr.clone();

    // Start server in background
    let handle = tokio::spawn(async move {
        let engine =
            powdb_query::executor::Engine::new(std::path::Path::new(&data_dir_str)).unwrap();
        let engine = std::sync::Arc::new(std::sync::RwLock::new(engine));
        let tx_gate = powdb_server::handler::new_tx_gate();
        let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();

        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let eng = engine.clone();
            let tx_gate = tx_gate.clone();
            let (_, mut rx) = tokio::sync::watch::channel(false);
            let peer_addr = Some(peer);
            tokio::spawn(async move {
                powdb_server::handler::handle_connection(
                    stream,
                    powdb_server::handler::ConnOpts {
                        engine: eng,
                        tx_gate,
                        expected_password: None,
                        users: std::sync::Arc::new(powdb_auth::UserStore::new()),
                        shutdown_rx: &mut rx,
                        idle_timeout: Duration::from_secs(300),
                        query_timeout: Duration::from_secs(30),
                        rate_limiter: None,
                        peer_addr,
                    },
                )
                .await;
            });
        }
    });

    // Give server time to bind
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x02, "expected CONNECT_OK");

    // Create table
    stream
        .write_all(&encode_query("type User { required name: str, age: int }"))
        .await
        .unwrap();
    let resp = read_response(&mut stream).await;
    assert!(
        resp[0] == 0x09 || resp[0] == 0x0B,
        "expected RESULT_OK or RESULT_MSG for create type, got: 0x{:02X}",
        resp[0]
    );

    // Insert row
    stream
        .write_all(&encode_query(
            r#"insert User { name := "Alice", age := 30 }"#,
        ))
        .await
        .unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x09, "expected RESULT_OK for insert");

    // Insert another row
    stream
        .write_all(&encode_query(r#"insert User { name := "Bob", age := 25 }"#))
        .await
        .unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x09, "expected RESULT_OK for second insert");

    // Query all rows
    stream.write_all(&encode_query("User")).await.unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x07, "expected RESULT_ROWS");

    // Count
    stream
        .write_all(&encode_query("count(User)"))
        .await
        .unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x08, "expected RESULT_SCALAR for count");

    // Filter query
    stream
        .write_all(&encode_query("User filter .age > 27"))
        .await
        .unwrap();
    let resp = read_response(&mut stream).await;
    assert_eq!(resp[0], 0x07, "expected RESULT_ROWS for filter");

    // Decode the filtered rows to verify content
    let decoded = powdb_server::protocol::Message::decode(&resp).unwrap();
    match decoded {
        powdb_server::protocol::Message::ResultRows { columns: _, rows } => {
            assert_eq!(rows.len(), 1, "filter should return only Alice");
            assert_eq!(rows[0][0], "Alice");
        }
        other => panic!("expected ResultRows, got {other:?}"),
    }

    // Cleanup
    handle.abort();
    std::fs::remove_dir_all(&data_dir).ok();
}

#[tokio::test]
async fn explicit_transaction_blocks_other_connections_until_closed() {
    use powdb_server::protocol::Message;
    use std::sync::{Arc, RwLock};

    let unique = format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let data_dir = std::env::temp_dir().join(format!("powdb_tx_gate_{unique}"));
    std::fs::create_dir_all(&data_dir).unwrap();

    let data_dir_str = data_dir.to_str().unwrap().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let engine =
            powdb_query::executor::Engine::new(std::path::Path::new(&data_dir_str)).unwrap();
        let engine = Arc::new(RwLock::new(engine));
        let tx_gate = powdb_server::handler::new_tx_gate();
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let eng = engine.clone();
            let tx_gate = tx_gate.clone();
            let (_, mut rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                powdb_server::handler::handle_connection(
                    stream,
                    powdb_server::handler::ConnOpts {
                        engine: eng,
                        tx_gate,
                        expected_password: None,
                        users: Arc::new(powdb_auth::UserStore::new()),
                        shutdown_rx: &mut rx,
                        idle_timeout: Duration::from_secs(300),
                        query_timeout: Duration::from_secs(30),
                        rate_limiter: None,
                        peer_addr: Some(peer),
                    },
                )
                .await;
            });
        }
    });

    let mut tx_client = TcpStream::connect(addr).await.unwrap();
    tx_client
        .write_all(&encode_connect("testdb"))
        .await
        .unwrap();
    assert_eq!(read_response(&mut tx_client).await[0], 0x02);

    tx_client
        .write_all(&encode_query("type Item { required name: str }"))
        .await
        .unwrap();
    let resp = read_response(&mut tx_client).await;
    assert!(resp[0] == 0x09 || resp[0] == 0x0B);

    tx_client.write_all(&encode_query("begin")).await.unwrap();
    let resp = read_response(&mut tx_client).await;
    assert_eq!(resp[0], 0x0B, "begin should return a message");

    tx_client
        .write_all(&encode_query(r#"insert Item { name := "uncommitted" }"#))
        .await
        .unwrap();
    let resp = read_response(&mut tx_client).await;
    assert_eq!(resp[0], 0x09, "insert inside transaction should succeed");

    let mut other = TcpStream::connect(addr).await.unwrap();
    other.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(read_response(&mut other).await[0], 0x02);
    other.write_all(&encode_query("count(Item)")).await.unwrap();

    let blocked = tokio::time::timeout(Duration::from_millis(200), read_response(&mut other)).await;
    assert!(
        blocked.is_err(),
        "another connection must not receive a query response while BEGIN is open"
    );

    tx_client
        .write_all(&encode_query("rollback"))
        .await
        .unwrap();
    let resp = read_response(&mut tx_client).await;
    assert_eq!(resp[0], 0x0B, "rollback should return a message");

    let resp = tokio::time::timeout(Duration::from_secs(5), read_response(&mut other))
        .await
        .expect("blocked query should resume after rollback");
    assert_eq!(resp[0], 0x08, "count should complete after rollback");
    match Message::decode(&resp).unwrap() {
        Message::ResultScalar { value } => assert_eq!(value, "0"),
        other => panic!("expected count scalar, got {other:?}"),
    }

    handle.abort();
    std::fs::remove_dir_all(&data_dir).ok();
}

/// Drive the REAL handshake with a populated UserStore: a valid (user,password)
/// connects; wrong password, unknown user, and a missing username all reject.
#[tokio::test]
async fn test_user_auth_handshake() {
    use powdb_auth::UserStore;
    use std::sync::{Arc, RwLock};

    let test_id = std::process::id();
    let port = 16500 + (test_id % 1000) as u16;
    let data_dir = std::env::temp_dir().join(format!("powdb_userauth_{test_id}"));
    std::fs::create_dir_all(&data_dir).unwrap();

    // Seed a user store with one user.
    let mut store = UserStore::new();
    store.create_user("alice", "pw", "readwrite").unwrap();
    let users = Arc::new(store);

    let data_dir_str = data_dir.to_str().unwrap().to_string();
    let addr = format!("127.0.0.1:{port}");
    let bind_addr = addr.clone();

    let handle = tokio::spawn(async move {
        let engine =
            powdb_query::executor::Engine::new(std::path::Path::new(&data_dir_str)).unwrap();
        let engine = Arc::new(RwLock::new(engine));
        let tx_gate = powdb_server::handler::new_tx_gate();
        let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let eng = engine.clone();
            let tx_gate = tx_gate.clone();
            let users = users.clone();
            let (_, mut rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                powdb_server::handler::handle_connection(
                    stream,
                    powdb_server::handler::ConnOpts {
                        engine: eng,
                        tx_gate,
                        expected_password: None,
                        users,
                        shutdown_rx: &mut rx,
                        idle_timeout: Duration::from_secs(300),
                        query_timeout: Duration::from_secs(30),
                        rate_limiter: None,
                        peer_addr: Some(peer),
                    },
                )
                .await;
            });
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Valid user → CONNECT_OK.
    {
        let mut stream = TcpStream::connect(&addr).await.unwrap();
        stream
            .write_all(&encode_connect_user("testdb", "pw", "alice"))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x02, "valid user should get CONNECT_OK");
    }

    // Wrong password → ERROR.
    {
        let mut stream = TcpStream::connect(&addr).await.unwrap();
        stream
            .write_all(&encode_connect_user("testdb", "wrong", "alice"))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x0A, "wrong password should get ERROR");
    }

    // Unknown user → ERROR.
    {
        let mut stream = TcpStream::connect(&addr).await.unwrap();
        stream
            .write_all(&encode_connect_user("testdb", "pw", "mallory"))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x0A, "unknown user should get ERROR");
    }

    // Missing username (old-style connect with password only) → ERROR because
    // the store has users.
    {
        let mut stream = TcpStream::connect(&addr).await.unwrap();
        let frame = powdb_server::protocol::Message::Connect {
            db_name: "testdb".into(),
            password: Some(zeroize::Zeroizing::new("pw".into())),
            username: None,
        }
        .encode();
        stream.write_all(&frame).await.unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x0A, "missing username should get ERROR");
    }

    handle.abort();
    std::fs::remove_dir_all(&data_dir).ok();
}

/// Empty UserStore → legacy shared-password path is byte-identical: right
/// password connects, wrong password is rejected.
#[tokio::test]
async fn test_empty_store_shared_password_fallback() {
    use powdb_auth::UserStore;
    use std::sync::{Arc, RwLock};

    let test_id = std::process::id();
    let port = 17500 + (test_id % 1000) as u16;
    let data_dir = std::env::temp_dir().join(format!("powdb_sharedpw_{test_id}"));
    std::fs::create_dir_all(&data_dir).unwrap();

    let users = Arc::new(UserStore::new()); // empty → fallback
    let data_dir_str = data_dir.to_str().unwrap().to_string();
    let addr = format!("127.0.0.1:{port}");
    let bind_addr = addr.clone();

    let handle = tokio::spawn(async move {
        let engine =
            powdb_query::executor::Engine::new(std::path::Path::new(&data_dir_str)).unwrap();
        let engine = Arc::new(RwLock::new(engine));
        let tx_gate = powdb_server::handler::new_tx_gate();
        let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let eng = engine.clone();
            let tx_gate = tx_gate.clone();
            let users = users.clone();
            let (_, mut rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                powdb_server::handler::handle_connection(
                    stream,
                    powdb_server::handler::ConnOpts {
                        engine: eng,
                        tx_gate,
                        expected_password: Some(zeroize::Zeroizing::new("sekret".into())),
                        users,
                        shutdown_rx: &mut rx,
                        idle_timeout: Duration::from_secs(300),
                        query_timeout: Duration::from_secs(30),
                        rate_limiter: None,
                        peer_addr: Some(peer),
                    },
                )
                .await;
            });
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Right shared password, no username → CONNECT_OK.
    {
        let mut stream = TcpStream::connect(&addr).await.unwrap();
        let frame = powdb_server::protocol::Message::Connect {
            db_name: "testdb".into(),
            password: Some(zeroize::Zeroizing::new("sekret".into())),
            username: None,
        }
        .encode();
        stream.write_all(&frame).await.unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x02, "correct shared password should connect");

        // Shared-password mode has no per-user role: writes must keep
        // working exactly as before role enforcement existed.
        stream
            .write_all(&encode_query("type Item { required name: str }"))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert!(
            resp[0] == 0x09 || resp[0] == 0x0B,
            "shared-password DDL must succeed, got 0x{:02X}",
            resp[0]
        );
        stream
            .write_all(&encode_query(r#"insert Item { name := "widget" }"#))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x09, "shared-password insert must succeed");
    }

    // Wrong shared password → ERROR.
    {
        let mut stream = TcpStream::connect(&addr).await.unwrap();
        let frame = powdb_server::protocol::Message::Connect {
            db_name: "testdb".into(),
            password: Some(zeroize::Zeroizing::new("nope".into())),
            username: None,
        }
        .encode();
        stream.write_all(&frame).await.unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x0A, "wrong shared password should be rejected");
    }

    handle.abort();
    std::fs::remove_dir_all(&data_dir).ok();
}

/// Fix 2 (authorization bypass): the `readonly` role must be enforced at the
/// server dispatch boundary. A readonly user can run read statements but every
/// write (insert/update/delete/DDL/transaction control) is rejected with a
/// clean "permission denied" error — and the connection stays alive.
/// `readwrite` and `admin` users keep full query access.
#[tokio::test]
async fn test_readonly_role_enforced_over_tcp() {
    use powdb_auth::UserStore;
    use powdb_server::protocol::Message;
    use std::sync::{Arc, RwLock};

    let test_id = std::process::id();
    let port = 18500 + (test_id % 1000) as u16;
    let data_dir = std::env::temp_dir().join(format!("powdb_rbac_{test_id}"));
    std::fs::create_dir_all(&data_dir).unwrap();

    let mut store = UserStore::new();
    store.create_user("root", "pw", "admin").unwrap();
    store.create_user("rw", "pw", "readwrite").unwrap();
    store.create_user("ro", "pw", "readonly").unwrap();
    let users = Arc::new(store);

    let data_dir_str = data_dir.to_str().unwrap().to_string();
    let addr = format!("127.0.0.1:{port}");
    let bind_addr = addr.clone();

    let handle = tokio::spawn(async move {
        let engine =
            powdb_query::executor::Engine::new(std::path::Path::new(&data_dir_str)).unwrap();
        let engine = Arc::new(RwLock::new(engine));
        let tx_gate = powdb_server::handler::new_tx_gate();
        let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let eng = engine.clone();
            let tx_gate = tx_gate.clone();
            let users = users.clone();
            let (_, mut rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                powdb_server::handler::handle_connection(
                    stream,
                    powdb_server::handler::ConnOpts {
                        engine: eng,
                        tx_gate,
                        expected_password: None,
                        users,
                        shutdown_rx: &mut rx,
                        idle_timeout: Duration::from_secs(300),
                        query_timeout: Duration::from_secs(30),
                        rate_limiter: None,
                        peer_addr: Some(peer),
                    },
                )
                .await;
            });
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Admin seeds the schema + one row.
    {
        let mut stream = TcpStream::connect(&addr).await.unwrap();
        stream
            .write_all(&encode_connect_user("testdb", "pw", "root"))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x02, "admin should connect");

        stream
            .write_all(&encode_query("type User { required name: str, age: int }"))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert!(
            resp[0] == 0x09 || resp[0] == 0x0B,
            "admin DDL should succeed, got 0x{:02X}",
            resp[0]
        );

        stream
            .write_all(&encode_query(
                r#"insert User { name := "Alice", age := 30 }"#,
            ))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x09, "admin insert should succeed");
    }

    // Readonly user: reads OK, every write shape rejected, connection alive.
    {
        let mut stream = TcpStream::connect(&addr).await.unwrap();
        stream
            .write_all(&encode_connect_user("testdb", "pw", "ro"))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x02, "readonly user should connect");

        // Reads succeed.
        stream.write_all(&encode_query("User")).await.unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x07, "readonly select should return rows");

        stream
            .write_all(&encode_query("count(User)"))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x08, "readonly count should return scalar");

        // Every write statement is rejected with a clean permission error.
        let writes = [
            r#"insert User { name := "Mallory", age := 1 }"#,
            r#"User filter .name = "Alice" update { age := 99 }"#,
            r#"User filter .name = "Alice" delete"#,
            "drop User",
            "alter User add column hacked: str",
            "begin",
        ];
        for q in writes {
            stream.write_all(&encode_query(q)).await.unwrap();
            let resp = read_response(&mut stream).await;
            assert_eq!(
                resp[0], 0x0A,
                "readonly write must be rejected: {q} (got 0x{:02X})",
                resp[0]
            );
            match Message::decode(&resp).unwrap() {
                Message::Error { message } => assert!(
                    message.contains("permission denied"),
                    "expected permission-denied error for {q}, got: {message}"
                ),
                other => panic!("expected Error for {q}, got {other:?}"),
            }
        }

        // The connection survives the rejections.
        stream.write_all(&encode_query("User")).await.unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x07, "connection must stay alive after denials");

        // And nothing was actually written/dropped.
        stream
            .write_all(&encode_query("count(User)"))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x08, "table must still exist");
        match Message::decode(&resp).unwrap() {
            Message::ResultScalar { value } => {
                assert_eq!(value, "1", "row count must be unchanged")
            }
            other => panic!("expected scalar, got {other:?}"),
        }
    }

    // Readwrite user keeps full write access.
    {
        let mut stream = TcpStream::connect(&addr).await.unwrap();
        stream
            .write_all(&encode_connect_user("testdb", "pw", "rw"))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x02, "readwrite user should connect");

        stream
            .write_all(&encode_query(r#"insert User { name := "Bob", age := 25 }"#))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x09, "readwrite insert should succeed");
    }

    handle.abort();
    std::fs::remove_dir_all(&data_dir).ok();
}
