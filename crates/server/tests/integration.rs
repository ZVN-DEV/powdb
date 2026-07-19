mod common;

use common::{encode_connect, encode_connect_user, read_response, unique_temp_dir, InprocServer};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use common::encode_query;

#[tokio::test]
async fn test_full_lifecycle() {
    let data_dir = unique_temp_dir("integ");
    std::fs::create_dir_all(&data_dir).unwrap();

    let engine = powdb_query::executor::Engine::new(&data_dir).unwrap();
    let engine = Arc::new(RwLock::new(engine));
    let (addr, handle) = InprocServer::default().start(engine).await;

    // Connect
    let mut stream = TcpStream::connect(addr).await.unwrap();
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

/// Pipelined connect: a client may write CONNECT and one or more QUERY frames
/// back-to-back in a SINGLE TCP write, without waiting for CONNECT_OK in
/// between. The server reads frames sequentially off one buffered reader, so
/// it must answer every frame, in order. Eager clients rely on this wire
/// contract to shave a full round trip off fresh connections; if this test
/// breaks, pipelined connects break with it.
#[tokio::test]
async fn test_connect_and_queries_pipelined_in_single_write() {
    let data_dir = unique_temp_dir("pipelined");
    std::fs::create_dir_all(&data_dir).unwrap();

    let engine = powdb_query::executor::Engine::new(&data_dir).unwrap();
    let engine = Arc::new(RwLock::new(engine));
    let (addr, handle) = InprocServer::default().start(engine).await;

    // CONNECT + DDL QUERY in one write: the handshake reply must come first,
    // then the query result.
    {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut burst = encode_connect("testdb");
        burst.extend_from_slice(&encode_query("type Pipelined { required name: str }"));
        stream.write_all(&burst).await.unwrap();

        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x02, "first reply must be CONNECT_OK");
        let resp = read_response(&mut stream).await;
        assert!(
            resp[0] == 0x09 || resp[0] == 0x0B,
            "second reply must answer the pipelined DDL, got 0x{:02X}",
            resp[0]
        );
    }

    // A fresh connection pipelining CONNECT + three queries in one write gets
    // four replies, in request order.
    {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut burst = encode_connect("testdb");
        burst.extend_from_slice(&encode_query(r#"insert Pipelined { name := "Alice" }"#));
        burst.extend_from_slice(&encode_query(r#"insert Pipelined { name := "Bob" }"#));
        burst.extend_from_slice(&encode_query("count(Pipelined)"));
        stream.write_all(&burst).await.unwrap();

        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x02, "first reply must be CONNECT_OK");
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x09, "first insert must return RESULT_OK");
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x09, "second insert must return RESULT_OK");
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x08, "count must return RESULT_SCALAR");
        match powdb_server::protocol::Message::decode(&resp).unwrap() {
            powdb_server::protocol::Message::ResultScalar { value } => {
                assert_eq!(value, "2", "both pipelined inserts must have applied")
            }
            other => panic!("expected count scalar, got {other:?}"),
        }
    }

    handle.abort();
    std::fs::remove_dir_all(&data_dir).ok();
}

#[tokio::test]
async fn explicit_transaction_blocks_other_connections_until_closed() {
    use powdb_server::protocol::Message;

    let data_dir = unique_temp_dir("tx_gate");
    std::fs::create_dir_all(&data_dir).unwrap();

    let engine = powdb_query::executor::Engine::new(&data_dir).unwrap();
    let engine = Arc::new(RwLock::new(engine));
    let (addr, handle) = InprocServer::default().start(engine).await;

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

/// A connection that drops while holding an open transaction must (1) roll back
/// its uncommitted writes and (2) release the TxGate so the next connection can
/// transact normally. Guards the connection-teardown path in `handle_connection`
/// (the buggy form released the permit before awaiting the rollback, which could
/// roll back a *different* connection's freshly-begun transaction).
#[tokio::test]
async fn dropped_connection_mid_transaction_rolls_back_and_frees_gate() {
    use powdb_server::protocol::Message;

    let data_dir = unique_temp_dir("tx_drop");
    std::fs::create_dir_all(&data_dir).unwrap();

    let engine = powdb_query::executor::Engine::new(&data_dir).unwrap();
    let engine = Arc::new(RwLock::new(engine));
    let (addr, handle) = InprocServer::default().start(engine).await;

    // Connection A: open a transaction, insert an uncommitted row, then drop the
    // socket WITHOUT committing or rolling back.
    let mut a = TcpStream::connect(addr).await.unwrap();
    a.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(read_response(&mut a).await[0], 0x02);
    a.write_all(&encode_query("type Item { required name: str }"))
        .await
        .unwrap();
    let resp = read_response(&mut a).await;
    assert!(resp[0] == 0x09 || resp[0] == 0x0B);
    a.write_all(&encode_query("begin")).await.unwrap();
    assert_eq!(read_response(&mut a).await[0], 0x0B);
    a.write_all(&encode_query(r#"insert Item { name := "uncommitted" }"#))
        .await
        .unwrap();
    assert_eq!(read_response(&mut a).await[0], 0x09);
    drop(a); // abrupt disconnect mid-transaction

    // Connection B: the gate must be free (this would hang/timeout otherwise),
    // and A's uncommitted row must have been rolled back on teardown.
    let mut b = TcpStream::connect(addr).await.unwrap();
    b.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(read_response(&mut b).await[0], 0x02);

    b.write_all(&encode_query("count(Item)")).await.unwrap();
    let resp = tokio::time::timeout(Duration::from_secs(5), read_response(&mut b))
        .await
        .expect("gate must be released after A drops mid-transaction");
    assert_eq!(resp[0], 0x08);
    match Message::decode(&resp).unwrap() {
        Message::ResultScalar { value } => {
            assert_eq!(value, "0", "A's uncommitted insert must be rolled back")
        }
        other => panic!("expected count scalar, got {other:?}"),
    }

    // B can then run its own transaction normally and durably commit.
    b.write_all(&encode_query("begin")).await.unwrap();
    assert_eq!(read_response(&mut b).await[0], 0x0B);
    b.write_all(&encode_query(r#"insert Item { name := "committed" }"#))
        .await
        .unwrap();
    assert_eq!(read_response(&mut b).await[0], 0x09);
    b.write_all(&encode_query("commit")).await.unwrap();
    assert_eq!(read_response(&mut b).await[0], 0x0B);
    b.write_all(&encode_query("count(Item)")).await.unwrap();
    let resp = read_response(&mut b).await;
    match Message::decode(&resp).unwrap() {
        Message::ResultScalar { value } => {
            assert_eq!(value, "1", "B's committed row must persist")
        }
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

    let data_dir = unique_temp_dir("userauth");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Seed a user store with one user.
    let mut store = UserStore::new();
    store.create_user("alice", "pw", "readwrite").unwrap();
    let users = Arc::new(store);

    let engine = powdb_query::executor::Engine::new(&data_dir).unwrap();
    let engine = Arc::new(RwLock::new(engine));
    let (addr, handle) = InprocServer {
        users,
        ..Default::default()
    }
    .start(engine)
    .await;

    // Valid user → CONNECT_OK.
    {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(&encode_connect_user("testdb", "pw", "alice"))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x02, "valid user should get CONNECT_OK");
    }

    // Wrong password → ERROR.
    {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(&encode_connect_user("testdb", "wrong", "alice"))
            .await
            .unwrap();
        let resp = read_response(&mut stream).await;
        assert_eq!(resp[0], 0x0A, "wrong password should get ERROR");
    }

    // Unknown user → ERROR.
    {
        let mut stream = TcpStream::connect(addr).await.unwrap();
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
        let mut stream = TcpStream::connect(addr).await.unwrap();
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
    let data_dir = unique_temp_dir("sharedpw");
    std::fs::create_dir_all(&data_dir).unwrap();

    let engine = powdb_query::executor::Engine::new(&data_dir).unwrap();
    let engine = Arc::new(RwLock::new(engine));
    let (addr, handle) = InprocServer {
        expected_password: Some("sekret".into()),
        ..Default::default() // empty UserStore → fallback
    }
    .start(engine)
    .await;

    // Right shared password, no username → CONNECT_OK.
    {
        let mut stream = TcpStream::connect(addr).await.unwrap();
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
        let mut stream = TcpStream::connect(addr).await.unwrap();
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
/// clean "permission denied" error, and the connection stays alive.
/// `readwrite` and `admin` users keep full query access.
#[tokio::test]
async fn test_readonly_role_enforced_over_tcp() {
    use powdb_auth::UserStore;
    use powdb_server::protocol::Message;

    let data_dir = unique_temp_dir("rbac");
    std::fs::create_dir_all(&data_dir).unwrap();

    let mut store = UserStore::new();
    store.create_user("root", "pw", "admin").unwrap();
    store.create_user("rw", "pw", "readwrite").unwrap();
    store.create_user("ro", "pw", "readonly").unwrap();
    let users = Arc::new(store);

    let engine = powdb_query::executor::Engine::new(&data_dir).unwrap();
    let engine = Arc::new(RwLock::new(engine));
    let (addr, handle) = InprocServer {
        users,
        ..Default::default()
    }
    .start(engine)
    .await;

    // Admin seeds the schema + one row.
    {
        let mut stream = TcpStream::connect(addr).await.unwrap();
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
        let mut stream = TcpStream::connect(addr).await.unwrap();
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
        let mut stream = TcpStream::connect(addr).await.unwrap();
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
