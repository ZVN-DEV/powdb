//! What a failure looks like on the wire: the egress allowlist, the class
//! byte, and the messages that must reach a client verbatim.

use super::*;

// ---- Error sanitization allowlist ----

#[test]
fn unique_violation_error_surfaces_to_remote_clients() {
    // The storage layer reports the actionable message; the server must
    // not replace it with the generic "query execution error".
    assert_eq!(
        sanitize_error("unique constraint violation on User.email"),
        "unique constraint violation on User.email"
    );
}

#[test]
fn internal_errors_stay_generic() {
    assert_eq!(
        sanitize_error("some internal io panic detail"),
        "query execution error"
    );
}

#[test]
fn cancellation_errors_surface_to_remote_clients() {
    // A cancelled/timed-out query must reach the client with its real
    // message (both are derived from the configured timeout or a client
    // disconnect and leak no internal state) rather than the generic mask.
    for msg in [
        &QueryError::Timeout { timeout_ms: 2000 }.to_string(),
        &QueryError::Cancelled.to_string(),
    ] {
        assert_eq!(sanitize_error(msg), *msg, "should pass through verbatim");
    }
    // Sanity-check the exact wording the executor emits.
    assert_eq!(
        QueryError::Timeout { timeout_ms: 2000 }.to_string(),
        "query timeout after 2000ms"
    );
    assert_eq!(
        QueryError::Cancelled.to_string(),
        "query cancelled by client disconnect"
    );
}

// ---- Entity-link diagnostics reach remote clients ----

/// Build a schema with a link, ready for the failure cases below.
fn linked_engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    for ddl in [
        "type User { required id: int, name: str }",
        "type Order { required id: int, user_id: int, total: int }",
        "link Order.user -> User on user_id = id",
    ] {
        engine.execute_powql(ddl).unwrap();
    }
    (dir, engine)
}

/// Every way a link statement or a link projection can be refused, executed
/// for real and then run through the sanitizer that guards the wire.
///
/// The sanitizer is an allowlist: a message family with no prefix in it is
/// replaced by "query execution error" on the way out. That is what
/// happened to the whole entity-link feature. An embedded caller saw
/// `link 'author' not found on owner type 'Post'` and a remote client saw
/// nothing it could act on, so the same typo was diagnosable in one
/// deployment shape and not the other.
///
/// The failures are enumerated by EXECUTING them rather than by quoting
/// strings, so rewording a message keeps it covered and only a genuinely
/// new failure is uncovered.
#[test]
fn every_link_diagnostic_survives_the_wire_sanitizer() {
    let (_dir, mut engine) = linked_engine();
    let refusals = [
        // Catalog-side refusals of the link DDL itself.
        "link Order.other -> User on nope = id",
        "link Order.other -> User on user_id = nope",
        "link Order.user_id -> User on user_id = id",
        "link Order.user -> User on user_id = id",
        // Planner and executor refusals of a link PROJECTION.
        "Order as o { o.nosuchlink.name }",
        "Order as o { wrongalias.user.name }",
        "count(Order as o { o.user.name })",
    ];
    let mut masked = Vec::new();
    for statement in refusals {
        let err = engine
            .execute_powql(statement)
            .expect_err(&format!("`{statement}` must be refused"));
        let message = err.to_string();
        if sanitize_error(&message) != message {
            masked.push(format!("  {statement}\n    -> {message}"));
        }
    }
    assert!(
        masked.is_empty(),
        "these link diagnostics are masked to \"query execution error\" on their way to a \
         remote client, so only embedded callers can see what went wrong. Add a prefix to \
         SAFE_ERROR_PREFIXES for each:\n{}",
        masked.join("\n")
    );
}

/// The same guarantee, asserted where it is actually delivered: the frame
/// `execute_wire_query` hands back. Testing `sanitize_error` alone would
/// pass even if the wire path stopped calling it.
#[tokio::test]
async fn a_link_error_reaches_the_wire_with_its_real_message() {
    let (_dir, engine) = linked_engine();
    let engine = Arc::new(RwLock::new(engine));
    let gate = new_tx_gate_with_permits(1);
    let metrics = Arc::new(Metrics::new());
    let (_client, server) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(server);
    let mut wire_read_buffer = Vec::new();
    let mut pending_messages = InFlightReadAhead::default();
    let mut tx_permit = None;

    let (message, _, _) = execute_wire_query(
        QueryContext {
            engine,
            tx_gate: gate,
            tx_permit: &mut tx_permit,
            principal: None,
            result_mode: WireResultMode::Native,
            query_timeout: Duration::from_secs(2),
            tx_wait_timeout: Duration::from_secs(2),
            metrics: &metrics,
            stream: FrameStream {
                reader: &mut reader,
                buffered: &mut wire_read_buffer,
                pending: &mut pending_messages,
            },
        },
        "Order as o { o.nosuchlink.name }".into(),
    )
    .await;

    match message {
        Message::ErrorWithClass { message, .. } => {
            assert!(
                message.contains("nosuchlink"),
                "the client was told nothing about its own typo: {message}"
            );
            assert_ne!(message, "query execution error");
        }
        other => panic!("expected a typed error frame, got {other:?}"),
    }
}

// ---- JSON (v0.12): canonical-text wire rendering + parse-error passthrough ----

#[test]
fn json_cell_renders_canonical_text_on_wire() {
    // A Json value flows through the same string-cell path as every other
    // value (value_to_display -> Value::to_wire_string). PJ1 is canonical,
    // so keys come back sorted bytewise regardless of input order and the
    // client receives parseable JSON text with no protocol change.
    let pj1 = powdb_storage::pj1::parse_json_text(r#"{"b":2,"a":1,"nested":{"z":true}}"#)
        .expect("valid JSON");
    let result = QueryResult::Rows {
        columns: vec!["doc".into()],
        rows: vec![vec![Value::Json(pj1.into())]],
    };
    match query_result_to_message(result, WireResultMode::LegacyText).expect("encodes") {
        Message::ResultRows { columns, rows } => {
            assert_eq!(columns, vec!["doc"]);
            assert_eq!(
                rows,
                vec![vec![r#"{"a":1,"b":2,"nested":{"z":true}}"#.to_string()]]
            );
        }
        other => panic!("expected ResultRows, got {other:?}"),
    }
}

#[test]
fn json_parse_error_surfaces_to_remote_clients() {
    // Lane B rejects invalid JSON on insert as QueryError::TypeError, whose
    // Display is "type mismatch: <detail>" (crates/query/src/result.rs).
    // That prefix is allowlisted, so the actionable detail reaches the
    // client instead of the generic "query execution error". The raw
    // storage-layer phrasing ("invalid JSON: ...") is also allowlisted as
    // defense-in-depth. Internal PJ1 corruption ("malformed PJ1: ...") is
    // deliberately NOT allowlisted: it leaks storage internals and never
    // occurs on the client-driven insert path.
    for msg in [
        "type mismatch: invalid JSON: unexpected character 'x' at position 3",
        "invalid JSON: nesting exceeds depth cap 128",
    ] {
        assert_eq!(sanitize_error(msg), msg, "should pass through verbatim");
    }
    assert_eq!(
        sanitize_error("malformed PJ1: truncated"),
        "query execution error",
        "internal storage corruption must stay masked"
    );
}

// `describe <Type>` renders a json column's type as the bareword "json"
// over the wire. introspect_describe emits type_id_to_name(TypeId::Json) =
// "json" (crates/query/src/executor/compiled.rs) as a Str cell, which flows
// through value_to_display unchanged; Lane B's DDL keyword makes `type Doc
// { body: json }` accepted, so this runs end to end (v0.12, Lane D).
#[test]
fn describe_shows_json_type_over_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type Doc { required id: int, body: json }")
        .expect("json column DDL should be accepted once Lane B lands");
    let result = engine.execute_powql("describe Doc").expect("describe runs");
    let msg = query_result_to_message(result, WireResultMode::LegacyText).expect("encodes");
    match msg {
        Message::ResultRows { columns, rows } => {
            assert_eq!(columns[1], "type");
            // The `body` column's type cell must be the bareword "json".
            let body = rows
                .iter()
                .find(|r| r[0] == "body")
                .expect("body column present");
            assert_eq!(body[1], "json");
        }
        other => panic!("expected ResultRows, got {other:?}"),
    }
}

#[test]
fn resource_limit_errors_surface_actionable_hints() {
    // These carry user-actionable guidance and leak no internal state, so
    // they must reach the client verbatim — not be masked to the generic
    // message. The exact strings come from QueryError's Display impl
    // (crates/query/src/result.rs).
    for msg in [
        "sort input exceeds row limit — add a LIMIT clause",
        "join result exceeds row limit",
        "query exceeded memory budget: requested 100 bytes, limit 50 bytes",
        "result too large: encoded response exceeds 1024 bytes; add a limit or narrower projection",
    ] {
        assert_eq!(sanitize_error(msg), msg, "should pass through verbatim");
    }
}

#[test]
fn oversized_result_is_rejected_before_wire_encoding() {
    let long = "x".repeat(MAX_RESPONSE_PAYLOAD_SIZE);
    let result = QueryResult::Rows {
        columns: vec!["payload".into()],
        rows: vec![vec![Value::Str(long)]],
    };
    let err = query_result_to_message(result, WireResultMode::LegacyText).unwrap_err();
    assert!(
        err.to_string().starts_with("result too large"),
        "unexpected error: {err}"
    );
}

// ---- Wire classes for the typed storage refusals ----

#[test]
fn ddl_inside_a_transaction_reaches_clients_as_a_client_error() {
    let (_dir, engine) = one_row_engine();
    let (begin, _) = dispatch_query(&engine, "begin", None, true);
    begin.expect("begin");

    let (result, _) = dispatch_query(&engine, "drop User", None, true);
    let err = result.expect_err("DDL inside an explicit transaction must be refused");
    assert_eq!(
        classify_query_error(&err),
        ErrorClass::Execution,
        "refusing DDL because the connection is mid-transaction is the client's mistake; \
         ErrorClass::Internal tells the driver it hit a server bug it cannot act on"
    );
    // The class is only half of it: the guidance has to survive egress
    // sanitization or the client is told to act on nothing.
    assert!(
        sanitize_error(&err.to_string()).contains("DDL is not transactional"),
        "guidance was masked: {err}"
    );
}

#[test]
fn transaction_over_the_dirty_page_budget_reaches_clients_as_a_limit() {
    // The refusal the heap raises (crates/storage/src/heap.rs), in both
    // shapes it can reach the server in.
    let raised = || {
        std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            StorageError::TransactionTooLarge {
                pages: 65_536,
                limit_bytes: 268_435_456,
            },
        )
    };

    // The path the executor takes today: the kind survives, and the class
    // comes from it.
    let typed = QueryError::from_storage_io(raised());
    assert!(
        matches!(typed, QueryError::Storage { .. }),
        "the executor must deliver this refusal typed, or the type-driven \
         classification below never fires in production"
    );
    assert_eq!(
        classify_query_error(&typed),
        ErrorClass::LimitExceeded,
        "a transaction refused by the dirty-page budget is a resource limit, the same \
         class MemoryLimitExceeded already carries"
    );

    // The legacy shape, for any path that still renders the refusal to
    // text before the server sees it. It must classify identically.
    let legacy = QueryError::StorageError(raised().to_string());
    assert_eq!(
        classify_query_error(&legacy),
        ErrorClass::LimitExceeded,
        "the legacy text fallback must agree with the typed path"
    );
    assert_eq!(
        typed.to_string(),
        legacy.to_string(),
        "typing the refusal must not change one byte of what the client reads"
    );
    assert_eq!(
        sanitize_error(&typed.to_string()),
        typed.to_string(),
        "the budget message names the limit and the remedy; it must cross verbatim"
    );
}

#[test]
fn a_unique_violation_reaches_clients_as_a_constraint_violation() {
    let raised = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            StorageError::UniqueConstraintViolation {
                table: "User".into(),
                column: "email".into(),
            },
        )
    };

    let typed = QueryError::from_storage_io(raised());
    assert_eq!(
        classify_query_error(&typed),
        ErrorClass::ConstraintViolation,
        "a duplicate key is the caller's data problem, not a server fault"
    );

    let legacy = QueryError::StorageError(raised().to_string());
    assert_eq!(
        classify_query_error(&legacy),
        ErrorClass::ConstraintViolation,
        "the legacy text fallback must agree with the typed path"
    );
    assert_eq!(typed.to_string(), legacy.to_string());
}

/// A storage failure with no kind to recover must not be dressed up as one
/// of the refusals a client can act on.
#[test]
fn a_plain_io_failure_still_reaches_clients_as_internal() {
    let bare = std::io::Error::other("disk went away");
    let err = QueryError::from_storage_io(bare);
    assert!(matches!(err, QueryError::StorageError(_)));
    assert_eq!(classify_query_error(&err), ErrorClass::Internal);
    assert_eq!(
        sanitize_error(&err.to_string()),
        "query execution error",
        "an internal I/O detail must never cross the wire verbatim"
    );
}
