//! What a failure looks like on the wire: which messages reach a client
//! verbatim, and the class byte that travels with them.

use super::*;

// ---- What crosses the wire verbatim ----

#[test]
fn unique_violation_error_surfaces_to_remote_clients() {
    // The storage layer reports the actionable message; the server must
    // not replace it with the generic "query execution error".
    let typed = QueryError::from(StorageError::UniqueConstraintViolation {
        table: "User".into(),
        column: "email".into(),
    });
    assert_eq!(
        client_facing_message(&typed),
        "unique constraint violation on User.email"
    );
}

#[test]
fn internal_errors_stay_generic() {
    assert_eq!(
        client_facing_message(&QueryError::StorageError(
            "some internal io panic detail".into()
        )),
        "query execution error"
    );
}

/// The read-only retry sentinel is a marker string, not a message. It is
/// intercepted before Display on every live path, and if one ever misses it
/// the client must still not be handed the marker.
#[test]
fn the_readonly_retry_sentinel_never_crosses_the_wire() {
    assert_eq!(
        client_facing_message(&QueryError::ReadonlyNeedsWrite),
        "query execution error"
    );
}

#[test]
fn cancellation_errors_surface_to_remote_clients() {
    // A cancelled/timed-out query must reach the client with its real
    // message (both are derived from the configured timeout or a client
    // disconnect and leak no internal state) rather than the generic mask.
    for err in [
        QueryError::Timeout { timeout_ms: 2000 },
        QueryError::Cancelled,
    ] {
        assert_eq!(
            client_facing_message(&err),
            err.to_string(),
            "should pass through verbatim"
        );
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

/// Every `StorageErrorKind` gets a class chosen deliberately, and no kind
/// lands on `Internal` by accident.
///
/// `class_for_storage_kind` matches exhaustively, so a new kind fails to
/// compile there — but a compiling default of `Internal` ("the server broke,
/// nothing you can do") is exactly the wrong answer for a refusal the caller
/// caused, and that is what `RowTooLarge`, `ValueTooLarge` and
/// `InvalidIdentifier` used to get. This table is the second half of the gate:
/// the kinds are listed here by hand, so a new one has to be classified in
/// both places.
#[test]
fn every_storage_kind_has_a_deliberate_wire_class() {
    use powdb_storage::error::StorageErrorKind::*;
    let expected = [
        (Io, ErrorClass::Internal),
        (CorruptData, ErrorClass::Internal),
        (CorruptCrc, ErrorClass::Internal),
        (WalReplay, ErrorClass::Internal),
        (CatalogCorrupt, ErrorClass::Internal),
        (PageCorrupt, ErrorClass::Internal),
        (OverflowCorrupt, ErrorClass::Internal),
        (InvalidIdentifier, ErrorClass::Execution),
        (DdlInTransaction, ErrorClass::Execution),
        (RowTooLarge, ErrorClass::LimitExceeded),
        (ValueTooLarge, ErrorClass::LimitExceeded),
        (TransactionTooLarge, ErrorClass::LimitExceeded),
        (UniqueConstraintViolation, ErrorClass::ConstraintViolation),
        (
            UniqueExpressionIndexViolation,
            ErrorClass::ConstraintViolation,
        ),
    ];
    for (kind, class) in expected {
        assert_eq!(
            class_for_storage_kind(kind),
            class,
            "{kind:?} is classified as {:?}, not {class:?}",
            class_for_storage_kind(kind)
        );
    }
    // A count check, so adding a kind without adding it here fails rather
    // than silently leaving the new kind untested.
    assert_eq!(
        expected.len(),
        14,
        "a StorageErrorKind was added or removed; classify it above"
    );
}

// ---- SQL frontend walls reach remote clients ----

/// Every documented "unsupported SQL" wall (the table in docs/SQL.md),
/// executed for real and then run through the egress rendering that guards
/// the wire. The SQL frontend's whole design is that a subset gap is a typed
/// error naming the working alternative, never a silent wrong answer, but a
/// remote SQL user once saw "query execution error" while an embedded caller
/// saw the real diagnostic (docs/SQL.md even documented this gap and told
/// users to prototype embedded). Same enumerate-by-executing shape as the
/// link test below: rewording a message keeps it covered.
#[test]
fn every_documented_sql_wall_survives_the_wire_sanitizer() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type User { required id: int, name: str, total: int }")
        .unwrap();
    let walls = [
        "SELECT CASE WHEN total > 1 THEN 1 ELSE 2 END FROM User",
        "SELECT COALESCE(name, 'x') FROM User",
        "SELECT COUNT(DISTINCT name) FROM User",
        "SELECT CAST(total AS INT) FROM User",
        "SELECT row_number() OVER (ORDER BY id) FROM User",
        "SELECT * FROM User WHERE total IN (1, 2)",
        "SELECT * FROM User WHERE EXISTS (SELECT id FROM User)",
        "SELECT * FROM User WHERE total = (SELECT total FROM User)",
        "SELECT * FROM User WHERE total BETWEEN 1 AND 2",
        "CREATE TABLE t (a INT, UNIQUE (a))",
        "INSERT INTO User (id, name, total) VALUES (1, 'a', 2) RETURNING id, name",
    ];
    let mut masked = Vec::new();
    for statement in walls {
        let err = engine
            .execute_sql(statement)
            .expect_err(&format!("`{statement}` must be refused"));
        let message = err.to_string();
        if client_facing_message(&err) != message {
            masked.push(format!("  {statement}\n    -> {message}"));
        }
    }
    assert!(
        masked.is_empty(),
        "these SQL-subset diagnostics are masked to \"query execution error\" on their way \
         to a remote client, so only embedded callers can see what went wrong:\n{}",
        masked.join("\n")
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
/// for real and then run through the egress rendering that guards the wire.
///
/// Egress used to be a message allowlist: a family with no prefix in it was
/// replaced by "query execution error" on the way out. That is what happened
/// to the whole entity-link feature. An embedded caller saw
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
        if client_facing_message(&err) != message {
            masked.push(format!("  {statement}\n    -> {message}"));
        }
    }
    assert!(
        masked.is_empty(),
        "these link diagnostics are masked to \"query execution error\" on their way to a \
         remote client, so only embedded callers can see what went wrong:\n{}",
        masked.join("\n")
    );
}

/// The same guarantee, asserted where it is actually delivered: the frame
/// `execute_wire_query` hands back. Testing `client_facing_message` alone
/// would pass even if the wire path stopped calling it.
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
    // Display is "type mismatch: <detail>" (crates/query/src/result.rs), and
    // that variant crosses verbatim so the actionable detail reaches the
    // client. The raw storage-layer phrasing ("invalid JSON: ...") reaches
    // the wire through the untyped-storage fallback. Internal PJ1 corruption
    // ("malformed PJ1: ...") is deliberately masked: it leaks storage
    // internals and never occurs on the client-driven insert path.
    for err in [
        QueryError::TypeError("invalid JSON: unexpected character 'x' at position 3".into()),
        QueryError::StorageError("invalid JSON: nesting exceeds depth cap 128".into()),
    ] {
        assert_eq!(
            client_facing_message(&err),
            err.to_string(),
            "should pass through verbatim"
        );
    }
    assert_eq!(
        client_facing_message(&QueryError::StorageError("malformed PJ1: truncated".into())),
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
    // they must reach the client verbatim, not be masked to the generic
    // message. Every one is checked through its typed variant, so a reworded
    // message stays covered.
    for err in [
        QueryError::SortLimitExceeded,
        QueryError::JoinLimitExceeded,
        QueryError::MemoryLimitExceeded {
            limit_bytes: 50,
            requested_bytes: 100,
        },
        QueryError::NestedLoopPairLimitExceeded {
            left_rows: 4000,
            right_rows: 4000,
            limit: 100,
        },
    ] {
        assert_eq!(
            client_facing_message(&err),
            err.to_string(),
            "should pass through verbatim"
        );
        assert_eq!(classify_query_error(&err), ErrorClass::LimitExceeded);
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
    // redaction or the client is told to act on nothing.
    assert!(
        client_facing_message(&err).contains("DDL is not transactional"),
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

    // The same refusal through the `From<StorageError> for io::Error`
    // conversion (the path the P0 scan fixes use): the kind must survive
    // that boundary too, now that the conversion carries the typed error
    // as the source instead of stringifying it. This is what made the
    // substring fallback deletable.
    let via_from: std::io::Error = StorageError::TransactionTooLarge {
        pages: 65_536,
        limit_bytes: 268_435_456,
    }
    .into();
    let converted = QueryError::from_storage_io(via_from);
    assert_eq!(
        classify_query_error(&converted),
        ErrorClass::LimitExceeded,
        "the From<StorageError> conversion must keep the kind classifiable"
    );
    assert_eq!(
        typed.to_string(),
        converted.to_string(),
        "typing the refusal must not change one byte of what the client reads"
    );
    assert_eq!(
        client_facing_message(&typed),
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

    // Through the `From<StorageError> for io::Error` conversion as well:
    // the kind survives, so no producer is left whose refusal could reach
    // classification as text (the substring fallback is deleted).
    let via_from: std::io::Error = StorageError::UniqueConstraintViolation {
        table: "User".into(),
        column: "email".into(),
    }
    .into();
    let converted = QueryError::from_storage_io(via_from);
    assert_eq!(
        classify_query_error(&converted),
        ErrorClass::ConstraintViolation,
        "the From<StorageError> conversion must keep the kind classifiable"
    );
    assert_eq!(typed.to_string(), converted.to_string());
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
        client_facing_message(&err),
        "query execution error",
        "an internal I/O detail must never cross the wire verbatim"
    );
}
