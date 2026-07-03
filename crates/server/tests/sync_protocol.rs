use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use powdb_query::executor::Engine;
use powdb_server::handler::{handle_connection, new_tx_gate, ConnOpts};
use powdb_server::metrics::Metrics;
use powdb_server::protocol::{Message, WireSyncRepairAction};
use powdb_sync::{
    archive_wal_records_for_identity, read_identity, read_units_since, retained_segments_dir,
    write_identity_snapshot, write_segment_atomic, DatabaseIdentity, IdentitySnapshot,
    ReplicaCursor, RetainedSegment, RetainedUnit, RETAINED_SEGMENT_FORMAT_VERSION,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

fn sync_identity() -> DatabaseIdentity {
    DatabaseIdentity {
        database_id: *b"wire-sync-test!!",
        primary_generation: 1,
    }
}

fn retained_unit(lsn: u64) -> RetainedUnit {
    RetainedUnit {
        tx_id: 1,
        record_type: 4,
        lsn,
        data: lsn.to_le_bytes().to_vec(),
    }
}

fn write_sync_identity_and_tail(data_dir: &std::path::Path, through_lsn: u64) {
    let identity = sync_identity();
    write_identity_snapshot(data_dir, &IdentitySnapshot::from_identity(identity, 1)).unwrap();
    let units = (1..=through_lsn).map(retained_unit).collect();
    let segment = RetainedSegment::new(identity.segment_identity(), units).unwrap();
    write_segment_atomic(&retained_segments_dir(data_dir), &segment).unwrap();
}

fn write_sync_identity_only(data_dir: &std::path::Path) {
    let identity = sync_identity();
    write_identity_snapshot(data_dir, &IdentitySnapshot::from_identity(identity, 1)).unwrap();
}

fn archive_if_sync_enabled(
    data_dir: &Path,
    records: &[powdb_storage::wal::WalRecord],
) -> std::io::Result<()> {
    match read_identity(data_dir) {
        Ok(identity) => archive_wal_records_for_identity(data_dir, identity, records),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

async fn start_single_conn_server(
    engine: Arc<RwLock<Engine>>,
    expected_password: Option<&str>,
) -> SocketAddr {
    start_single_conn_server_with_metrics(
        engine,
        expected_password,
        Arc::new(powdb_server::metrics::Metrics::new()),
    )
    .await
}

async fn start_single_conn_server_with_metrics(
    engine: Arc<RwLock<Engine>>,
    expected_password: Option<&str>,
    metrics: Arc<Metrics>,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tx_gate = new_tx_gate();
    let expected_password = expected_password.map(|password| password.to_string());

    tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        let (_, mut shutdown_rx) = watch::channel(false);
        handle_connection(
            stream,
            ConnOpts {
                engine,
                tx_gate,
                expected_password: expected_password.map(zeroize::Zeroizing::new),
                users: Arc::new(powdb_auth::UserStore::new()),
                shutdown_rx: &mut shutdown_rx,
                idle_timeout: Duration::from_secs(30),
                query_timeout: Duration::from_secs(30),
                rate_limiter: None,
                peer_addr: Some(peer),
                metrics,
            },
        )
        .await;
    });

    addr
}

async fn start_multi_conn_server(
    engine: Arc<RwLock<Engine>>,
    expected_password: Option<&str>,
) -> SocketAddr {
    start_multi_conn_server_with_users(
        engine,
        expected_password,
        Arc::new(powdb_auth::UserStore::new()),
    )
    .await
}

async fn start_multi_conn_server_with_users(
    engine: Arc<RwLock<Engine>>,
    expected_password: Option<&str>,
    users: Arc<powdb_auth::UserStore>,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tx_gate = new_tx_gate();
    let expected_password = expected_password.map(|password| password.to_string());

    tokio::spawn(async move {
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let engine = engine.clone();
            let tx_gate = tx_gate.clone();
            let expected_password = expected_password.clone();
            let users = users.clone();
            let (_, mut shutdown_rx) = watch::channel(false);
            tokio::spawn(async move {
                handle_connection(
                    stream,
                    ConnOpts {
                        engine,
                        tx_gate,
                        expected_password: expected_password.map(zeroize::Zeroizing::new),
                        users,
                        shutdown_rx: &mut shutdown_rx,
                        idle_timeout: Duration::from_secs(30),
                        query_timeout: Duration::from_secs(30),
                        rate_limiter: None,
                        peer_addr: Some(peer),
                        metrics: Arc::new(powdb_server::metrics::Metrics::new()),
                    },
                )
                .await;
            });
        }
    });

    addr
}

async fn connect(addr: SocketAddr, password: Option<&str>) -> TcpStream {
    connect_as(addr, None, password).await
}

async fn connect_as(addr: SocketAddr, username: Option<&str>, password: Option<&str>) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    Message::Connect {
        db_name: "default".into(),
        password: password.map(|password| zeroize::Zeroizing::new(password.to_string())),
        username: username.map(str::to_string),
    }
    .write_to(&mut stream)
    .await
    .unwrap();
    assert!(matches!(
        Message::read_from(&mut stream).await.unwrap().unwrap(),
        Message::ConnectOk { .. }
    ));
    stream
}

async fn query(stream: &mut TcpStream, query: &str) -> Message {
    Message::Query {
        query: query.into(),
    }
    .write_to(stream)
    .await
    .unwrap();
    Message::read_from(stream).await.unwrap().unwrap()
}

#[tokio::test]
async fn open_server_rejects_sync_metadata_access() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(RwLock::new(Engine::new(dir.path()).unwrap()));
    let addr = start_single_conn_server(engine, None).await;
    let mut stream = connect(addr, None).await;

    Message::SyncStatus {
        replica_id: "replica-a".into(),
    }
    .write_to(&mut stream)
    .await
    .unwrap();

    match Message::read_from(&mut stream).await.unwrap().unwrap() {
        Message::Error { message } => assert!(message.contains("requires authentication")),
        other => panic!("expected auth error, got {other:?}"),
    }
}

#[test]
fn sync_aware_engine_archives_writes_on_drop_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let remote_lsn;
    {
        let mut engine = Engine::with_memory_limit_and_wal_archive(
            dir.path(),
            256 * 1024 * 1024,
            archive_if_sync_enabled,
        )
        .unwrap();
        engine
            .execute_powql("type SyncT { required id: int, v: str }")
            .unwrap();
        engine
            .execute_powql(r#"insert SyncT { id := 1, v := "one" }"#)
            .unwrap();
        write_sync_identity_only(dir.path());
        engine
            .execute_powql(r#"insert SyncT { id := 2, v := "two" }"#)
            .unwrap();
        remote_lsn = engine.catalog().max_lsn();
        assert!(remote_lsn > 0);
    }

    let units = read_units_since(
        &retained_segments_dir(dir.path()),
        sync_identity().segment_identity(),
        0,
        4096,
    )
    .unwrap();
    assert!(units.last().is_some_and(|unit| unit.lsn >= remote_lsn));

    let engine = Engine::with_memory_limit_and_wal_archive(
        dir.path(),
        256 * 1024 * 1024,
        archive_if_sync_enabled,
    )
    .unwrap();
    let count = engine.execute_powql_readonly("count(SyncT)").unwrap();
    assert!(
        matches!(
            count,
            powdb_query::result::QueryResult::Scalar(powdb_storage::types::Value::Int(2))
        ),
        "sync-aware reopen must preserve committed rows, got {count:?}"
    );
}

#[tokio::test]
async fn named_user_tcp_sync_frames_enforce_roles() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type SyncT { required id: int, v: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert SyncT { id := 1, v := "sensitive_row_payload" }"#)
        .unwrap();
    let remote_lsn = engine.catalog().max_lsn();
    assert!(remote_lsn > 0);
    write_sync_identity_and_tail(dir.path(), remote_lsn);
    powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0)).unwrap();

    let mut users = powdb_auth::UserStore::new();
    users
        .create_user("writer", "writer-pw", "readwrite")
        .unwrap();
    users.create_user("admin", "admin-pw", "admin").unwrap();
    users
        .create_user("reader", "reader-pw", "readonly")
        .unwrap();

    let engine = Arc::new(RwLock::new(engine));
    let addr = start_multi_conn_server_with_users(engine, None, Arc::new(users)).await;

    let mut writer = connect_as(addr, Some("writer"), Some("writer-pw")).await;
    Message::SyncStatus {
        replica_id: "replica-a".into(),
    }
    .write_to(&mut writer)
    .await
    .unwrap();
    match Message::read_from(&mut writer).await.unwrap().unwrap() {
        Message::SyncStatusResult { status } => {
            assert_eq!(status.remote_lsn, remote_lsn);
            assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
        }
        other => panic!("readwrite user should be allowed to inspect sync status, got {other:?}"),
    }

    let mut admin = connect_as(addr, Some("admin"), Some("admin-pw")).await;
    Message::SyncStatus {
        replica_id: "replica-a".into(),
    }
    .write_to(&mut admin)
    .await
    .unwrap();
    assert!(matches!(
        Message::read_from(&mut admin).await.unwrap().unwrap(),
        Message::SyncStatusResult { .. }
    ));

    let mut reader = connect_as(addr, Some("reader"), Some("reader-pw")).await;
    Message::SyncStatus {
        replica_id: "replica-a".into(),
    }
    .write_to(&mut reader)
    .await
    .unwrap();
    match Message::read_from(&mut reader).await.unwrap().unwrap() {
        Message::Error { message } => assert!(message.contains("permission denied")),
        other => panic!("readonly user should be denied sync status, got {other:?}"),
    }
}

#[tokio::test]
async fn sync_frames_respect_open_transaction_gate() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::with_memory_limit_and_wal_archive(
        dir.path(),
        256 * 1024 * 1024,
        archive_if_sync_enabled,
    )
    .unwrap();
    engine
        .execute_powql("type SyncT { required id: int, v: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert SyncT { id := 1, v := "one" }"#)
        .unwrap();
    let committed_lsn = engine.catalog().max_lsn();
    assert!(committed_lsn > 0);
    write_sync_identity_only(dir.path());
    powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0)).unwrap();

    let engine = Arc::new(RwLock::new(engine));
    let addr = start_multi_conn_server(engine, Some("secret")).await;
    let mut tx_client = connect(addr, Some("secret")).await;

    assert!(matches!(
        query(&mut tx_client, "begin").await,
        Message::ResultMessage { .. }
    ));
    assert!(matches!(
        query(
            &mut tx_client,
            r#"insert SyncT { id := 2, v := "uncommitted" }"#
        )
        .await,
        Message::ResultOk { .. }
    ));

    Message::SyncStatus {
        replica_id: "replica-a".into(),
    }
    .write_to(&mut tx_client)
    .await
    .unwrap();
    match Message::read_from(&mut tx_client).await.unwrap().unwrap() {
        Message::Error { message } => assert!(message.contains("active transaction")),
        other => panic!("sync status inside a transaction must fail, got {other:?}"),
    }

    Message::SyncAck {
        replica_id: "replica-a".into(),
        applied_lsn: 0,
        remote_lsn: committed_lsn,
    }
    .write_to(&mut tx_client)
    .await
    .unwrap();
    match Message::read_from(&mut tx_client).await.unwrap().unwrap() {
        Message::Error { message } => assert!(message.contains("active transaction")),
        other => panic!("sync ack inside a transaction must fail, got {other:?}"),
    }

    let mut sync_client = connect(addr, Some("secret")).await;
    Message::SyncStatus {
        replica_id: "replica-a".into(),
    }
    .write_to(&mut sync_client)
    .await
    .unwrap();
    let blocked = tokio::time::timeout(
        Duration::from_millis(200),
        Message::read_from(&mut sync_client),
    )
    .await;
    assert!(
        blocked.is_err(),
        "sync status on another connection must wait while BEGIN is open"
    );

    let rollback = query(&mut tx_client, "rollback").await;
    assert!(
        matches!(
            rollback,
            Message::ResultMessage { .. } | Message::ResultOk { .. }
        ),
        "rollback should succeed after sync-frame rejections, got {rollback:?}"
    );

    let status = tokio::time::timeout(Duration::from_secs(5), Message::read_from(&mut sync_client))
        .await
        .expect("sync status should resume after rollback")
        .unwrap()
        .unwrap();
    match status {
        Message::SyncStatusResult { status } => {
            assert_eq!(status.last_applied_lsn, Some(0));
            assert_eq!(status.servable_lsn, Some(committed_lsn));
            assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
        }
        other => panic!("expected sync status after rollback, got {other:?}"),
    }

    let cursors = powdb_sync::read_replica_cursors(dir.path()).unwrap();
    assert_eq!(cursors[0].applied_lsn, 0);
}

#[tokio::test]
async fn authenticated_sync_status_pull_ack_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type SyncT { required id: int, v: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert SyncT { id := 1, v := "one" }"#)
        .unwrap();
    let remote_lsn = engine.catalog().max_lsn();
    assert!(remote_lsn > 0);
    write_sync_identity_and_tail(dir.path(), remote_lsn);
    powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active("replica-a", 0)).unwrap();

    let engine = Arc::new(RwLock::new(engine));
    let addr = start_single_conn_server(engine, Some("secret")).await;
    let mut stream = connect(addr, Some("secret")).await;

    Message::SyncStatus {
        replica_id: "replica-a".into(),
    }
    .write_to(&mut stream)
    .await
    .unwrap();
    let status = match Message::read_from(&mut stream).await.unwrap().unwrap() {
        Message::SyncStatusResult { status } => status,
        other => panic!("expected sync status, got {other:?}"),
    };
    assert_eq!(status.remote_lsn, remote_lsn);
    assert_eq!(status.last_applied_lsn, Some(0));
    assert_eq!(status.repair_action, WireSyncRepairAction::Pull);

    let identity = sync_identity().segment_identity();
    Message::SyncPull {
        replica_id: "replica-a".into(),
        since_lsn: 0,
        max_units: 64,
        max_bytes: 1024 * 1024,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
    }
    .write_to(&mut stream)
    .await
    .unwrap();
    let units = match Message::read_from(&mut stream).await.unwrap().unwrap() {
        Message::SyncPullResult {
            status,
            units,
            has_more,
        } => {
            assert_eq!(status.repair_action, WireSyncRepairAction::Pull);
            assert!(!has_more);
            units
        }
        other => panic!("expected sync pull, got {other:?}"),
    };
    assert_eq!(units.len() as u64, remote_lsn);
    assert_eq!(units.last().unwrap().lsn, remote_lsn);

    Message::SyncAck {
        replica_id: "replica-a".into(),
        applied_lsn: remote_lsn,
        remote_lsn,
    }
    .write_to(&mut stream)
    .await
    .unwrap();
    match Message::read_from(&mut stream).await.unwrap().unwrap() {
        Message::SyncAckResult {
            previous_applied_lsn,
            applied_lsn,
            remote_lsn: ack_remote_lsn,
            advanced,
            status,
        } => {
            assert_eq!(previous_applied_lsn, 0);
            assert_eq!(applied_lsn, remote_lsn);
            assert_eq!(ack_remote_lsn, remote_lsn);
            assert!(advanced);
            assert_eq!(status.repair_action, WireSyncRepairAction::None);
            assert!(!status.stale);
        }
        other => panic!("expected sync ack, got {other:?}"),
    }
}

#[tokio::test]
async fn sync_frames_record_prometheus_metrics_without_replica_labels() {
    let dir = tempfile::tempdir().unwrap();
    let replica_id = "customer-prod-replica-a";
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type SyncT { required id: int, v: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert SyncT { id := 1, v := "one" }"#)
        .unwrap();
    let remote_lsn = engine.catalog().max_lsn();
    write_sync_identity_and_tail(dir.path(), remote_lsn);
    powdb_sync::upsert_replica_cursor(dir.path(), ReplicaCursor::active(replica_id, 0)).unwrap();

    let metrics = Arc::new(Metrics::new());
    let engine = Arc::new(RwLock::new(engine));
    let addr = start_single_conn_server_with_metrics(engine, Some("secret"), metrics.clone()).await;
    let mut stream = connect(addr, Some("secret")).await;

    Message::SyncStatus {
        replica_id: replica_id.into(),
    }
    .write_to(&mut stream)
    .await
    .unwrap();
    assert!(matches!(
        Message::read_from(&mut stream).await.unwrap().unwrap(),
        Message::SyncStatusResult { .. }
    ));

    let identity = sync_identity().segment_identity();
    Message::SyncPull {
        replica_id: replica_id.into(),
        since_lsn: 0,
        max_units: 64,
        max_bytes: 1024 * 1024,
        database_id: identity.database_id,
        primary_generation: identity.primary_generation,
        wal_format_version: identity.wal_format_version,
        catalog_version: identity.catalog_version,
        segment_format_version: RETAINED_SEGMENT_FORMAT_VERSION,
    }
    .write_to(&mut stream)
    .await
    .unwrap();
    let pulled_units = match Message::read_from(&mut stream).await.unwrap().unwrap() {
        Message::SyncPullResult { units, .. } => units,
        other => panic!("expected sync pull, got {other:?}"),
    };
    assert!(!pulled_units.is_empty());
    let pulled_bytes = pulled_units.iter().fold(0u64, |total, unit| {
        total + 8 + 1 + 8 + 4 + unit.data.len() as u64
    });

    Message::SyncAck {
        replica_id: replica_id.into(),
        applied_lsn: remote_lsn,
        remote_lsn,
    }
    .write_to(&mut stream)
    .await
    .unwrap();
    assert!(matches!(
        Message::read_from(&mut stream).await.unwrap().unwrap(),
        Message::SyncAckResult { advanced: true, .. }
    ));

    let rendered = metrics.render();
    assert!(rendered.contains("powdb_sync_operations_total{operation=\"status\",result=\"ok\"} 1"));
    assert!(rendered.contains("powdb_sync_operations_total{operation=\"pull\",result=\"ok\"} 1"));
    assert!(rendered.contains("powdb_sync_operations_total{operation=\"ack\",result=\"ok\"} 1"));
    assert!(rendered.contains(
        "powdb_sync_repair_actions_total{operation=\"status\",repair_action=\"pull\"} 1"
    ));
    assert!(rendered
        .contains("powdb_sync_repair_actions_total{operation=\"ack\",repair_action=\"none\"} 1"));
    assert!(rendered.contains("powdb_sync_ack_advanced_total 1"));
    assert!(rendered.contains(&format!(
        "powdb_sync_pull_units_total {}",
        pulled_units.len()
    )));
    assert!(rendered.contains(&format!("powdb_sync_pull_bytes_total {pulled_bytes}")));
    let data_path = dir.path().display().to_string();
    for forbidden in [
        replica_id,
        "replica_id",
        "database_id",
        "wire-sync-test!!",
        "secret",
        "SyncT",
        "sensitive_row_payload",
        data_path.as_str(),
    ] {
        assert!(
            !rendered.contains(forbidden),
            "metrics must not expose sensitive sync details ({forbidden}): {rendered}"
        );
    }
}
