//! Frame admission and the read/write escalation the query frontends share.

use super::*;

#[test]
fn admission_classification_has_query_shape_parity_and_fails_closed() {
    assert_eq!(
        classify_query_admission("User filter .id = 1"),
        AdmissionMode::Reader
    );
    assert_eq!(
        classify_sql_admission("SELECT * FROM User WHERE id = 1"),
        AdmissionMode::Reader
    );
    assert_eq!(
        classify_params_admission("User filter .id = $1", &[WireParam::Int(1)]),
        AdmissionMode::Reader
    );

    assert_eq!(
        classify_query_admission("insert User { id := 1 }"),
        AdmissionMode::Writer
    );
    assert_eq!(
        classify_sql_admission("INSERT INTO User (id) VALUES (1)"),
        AdmissionMode::Writer
    );
    assert_eq!(
        classify_params_admission("insert User { id := $1 }", &[WireParam::Int(1)]),
        AdmissionMode::Writer
    );
    assert_eq!(
        classify_query_admission("this is not valid PowQL"),
        AdmissionMode::Writer,
        "uncertain statements must never enter through reader admission"
    );
}

#[tokio::test]
async fn unparsable_frame_is_rejected_without_acquiring_any_permit() {
    let (_dir, engine) = one_row_engine();
    // A single-permit gate whose only permit is already held: any
    // acquisition at all, reader or writer, would have to wait.
    let gate = new_tx_gate_with_permits(1);
    let metrics = Arc::new(Metrics::new());
    let held_reader = acquire_autocommit_permit(
        &gate,
        AdmissionMode::Reader,
        Duration::from_secs(1),
        &metrics,
    )
    .await
    .expect("held reader admission");
    assert_eq!(gate.available_permits(), 0);

    let (_client, server) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(server);
    let mut wire_read_buffer = Vec::new();
    let mut pending_messages = InFlightReadAhead::default();
    let mut tx_permit = None;
    let (message, ticket, termination) = tokio::time::timeout(
        Duration::from_millis(250),
        execute_wire_query(
            QueryContext {
                engine,
                tx_gate: gate.clone(),
                tx_permit: &mut tx_permit,
                principal: None,
                result_mode: WireResultMode::Native,
                query_timeout: Duration::from_secs(2),
                tx_wait_timeout: Duration::from_secs(10),
                metrics: &metrics,
                stream: FrameStream {
                    reader: &mut reader,
                    buffered: &mut wire_read_buffer,
                    pending: &mut pending_messages,
                },
            },
            "this is not valid PowQL".into(),
        ),
    )
    .await
    .expect("an unparsable frame must never wait on the transaction gate");

    match message {
        Message::ErrorWithClass { class, .. } => assert_eq!(class, ErrorClass::Parse),
        other => panic!("expected a typed parse error, got {other:?}"),
    }
    assert!(ticket.is_none());
    assert!(termination.is_none());
    assert!(tx_permit.is_none());
    assert_eq!(
        gate.available_permits(),
        0,
        "a statement that executes nothing must acquire nothing"
    );
    drop(held_reader);
    assert_eq!(gate.available_permits(), 1);
    // The rejected frame is still a failed statement from the client's view.
    assert!(metrics
        .render()
        .contains("powdb_queries_total{result=\"error\"} 1"));
}

fn dirty_view_engine() -> (tempfile::TempDir, Arc<RwLock<Engine>>) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type Source { required id: int }")
        .unwrap();
    engine.execute_powql("insert Source { id := 1 }").unwrap();
    engine
        .execute_powql("materialize Snapshot as Source")
        .unwrap();
    engine.execute_powql("insert Source { id := 2 }").unwrap();
    (dir, Arc::new(RwLock::new(engine)))
}

#[test]
fn dirty_view_requests_explicit_escalation_on_every_frontend() {
    let (_dir, engine) = dirty_view_engine();

    assert!(matches!(
        dispatch_query(&engine, "Snapshot", None, false).0,
        Err(QueryError::ReadonlyNeedsWrite)
    ));
    assert!(matches!(
        dispatch_sql_query(&engine, "SELECT * FROM Snapshot", None, false).0,
        Err(QueryError::ReadonlyNeedsWrite)
    ));
    assert!(matches!(
        dispatch_query_with_params(
            &engine,
            "Snapshot filter .id = $1",
            &[WireParam::Int(1)],
            None,
            false,
        )
        .0,
        Err(QueryError::ReadonlyNeedsWrite)
    ));
}

#[tokio::test]
async fn dirty_view_upgrade_waits_for_held_reader_then_records_once() {
    let (_dir, engine) = dirty_view_engine();
    let gate = new_tx_gate_with_permits(2);
    let metrics = Arc::new(Metrics::new());
    let held_reader = acquire_autocommit_permit(
        &gate,
        AdmissionMode::Reader,
        Duration::from_secs(1),
        &metrics,
    )
    .await
    .expect("held reader admission");

    // Keep the peer open so the query monitor waits instead of treating
    // EOF as a client disconnect while the admission upgrade is blocked.
    let (_client, server) = tokio::io::duplex(1024);
    let task_gate = gate.clone();
    let task_metrics = metrics.clone();
    let mut task = tokio::spawn(async move {
        let mut reader = BufReader::new(server);
        let mut wire_read_buffer = Vec::new();
        let mut pending_messages = InFlightReadAhead::default();
        let mut tx_permit = None;
        execute_wire_query(
            QueryContext {
                engine,
                tx_gate: task_gate,
                tx_permit: &mut tx_permit,
                principal: None,
                result_mode: WireResultMode::Native,
                query_timeout: Duration::from_secs(2),
                tx_wait_timeout: Duration::from_secs(1),
                metrics: &task_metrics,
                stream: FrameStream {
                    reader: &mut reader,
                    buffered: &mut wire_read_buffer,
                    pending: &mut pending_messages,
                },
            },
            "Snapshot".into(),
        )
        .await
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut task)
            .await
            .is_err(),
        "dirty-view retry must wait for exclusive admission while another reader is held"
    );
    drop(held_reader);

    let (message, ticket, termination) = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("upgrade must finish after the held reader releases")
        .expect("query task");
    assert!(matches!(message, Message::ResultRowsNative { .. }));
    assert!(termination.is_none());

    let (ticket, metric) = ticket.expect("view refresh must defer its WAL metric");
    drop(ticket);
    metrics.record_query(metric.start.elapsed(), metric.outcome);

    let rendered = metrics.render();
    assert!(rendered.contains("powdb_queries_total{result=\"ok\"} 1"));
    assert!(rendered.contains("powdb_queries_total{result=\"error\"} 0"));
}

#[tokio::test]
async fn timed_out_readonly_escalation_is_not_retried_or_reported_as_generic_error() {
    let (_dir, engine) = dirty_view_engine();
    let metrics = Arc::new(Metrics::new());
    // Keep the peer open so the socket monitor does not turn this into a
    // disconnect before the deadline fires.
    let (_client, server) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(server);
    let mut wire_read_buffer = Vec::new();
    let mut pending_messages = InFlightReadAhead::default();
    let query_timeout = Duration::from_millis(20);
    let query_deadline = Instant::now() + query_timeout;

    let (message, ticket, termination, retry) = run_blocking_query(
        BlockingQuery {
            engine,
            principal: None,
            result_mode: WireResultMode::Native,
            query_timeout,
            query_deadline,
            metrics: &metrics,
            stream: FrameStream {
                reader: &mut reader,
                buffered: &mut wire_read_buffer,
                pending: &mut pending_messages,
            },
        },
        (),
        |_engine, (), _principal| {
            // Ignore the token deliberately: this reproduces the race in
            // which the async deadline wins but the joined task's final
            // result is the internal dirty-view escalation sentinel.
            std::thread::sleep(Duration::from_millis(50));
            (Err(QueryError::ReadonlyNeedsWrite), None)
        },
    )
    .await;

    assert!(ticket.is_none());
    assert!(termination.is_none());
    assert!(!retry, "a timed-out query must never be resurrected");
    match message {
        Message::ErrorWithClass { message, class } => {
            assert_eq!(class, ErrorClass::Timeout);
            assert!(
                message.contains("query timeout after 20ms"),
                "timeout must remain client-visible, got {message}"
            );
        }
        other => panic!("expected timeout error, got {other:?}"),
    }
    let rendered = metrics.render();
    assert!(rendered.contains("powdb_query_timeouts_total 1"));
    assert!(rendered.contains("powdb_queries_total{result=\"error\"} 1"));
}
