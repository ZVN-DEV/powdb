//! A storage refusal must reach the query layer with its kind intact.
//!
//! Storage refusals used to arrive as `QueryError::StorageError(String)`: the
//! variant was rendered to text at the crate boundary and thrown away. The
//! server then had to recover the kind by searching that text for known
//! phrases in order to pick the wire error class a driver branches on, which
//! made a security-relevant wire field depend on message wording.
//!
//! These tests pin the two halves of the fix: the kind survives the
//! `io::Error` the engine raises it through, and the rendered message does not
//! change while it does. The second half is what makes the first safe to land:
//! `SAFE_ERROR_PREFIXES` in the server and several integration suites match on
//! these strings verbatim.

use powdb_query::executor::Engine;
use powdb_query::result::QueryError;
use powdb_storage::error::{StorageError, StorageErrorKind};

/// The refusals a client is expected to act on, in the shape the storage
/// engine actually raises them: a typed error inside an `io::Error`.
fn typed_refusals() -> Vec<(StorageError, StorageErrorKind)> {
    vec![
        (
            StorageError::DdlInTransaction { verb: "drop" },
            StorageErrorKind::DdlInTransaction,
        ),
        (
            StorageError::TransactionTooLarge {
                pages: 65_536,
                limit_bytes: 268_435_456,
            },
            StorageErrorKind::TransactionTooLarge,
        ),
        (
            StorageError::UniqueConstraintViolation {
                table: "User".into(),
                column: "email".into(),
            },
            StorageErrorKind::UniqueConstraintViolation,
        ),
        (
            StorageError::UniqueExpressionIndexViolation {
                table: "Doc".into(),
                expression: ".data->code".into(),
            },
            StorageErrorKind::UniqueExpressionIndexViolation,
        ),
    ]
}

#[test]
fn from_storage_io_keeps_the_kind_of_a_typed_refusal() {
    for (error, expected) in typed_refusals() {
        let wrapped = std::io::Error::new(std::io::ErrorKind::InvalidInput, error);
        match QueryError::from_storage_io(wrapped) {
            QueryError::Storage { kind, .. } => assert_eq!(
                kind, expected,
                "the refusal reached the query layer under the wrong kind"
            ),
            other => panic!("expected a typed storage error, got {other:?}"),
        }
    }
}

/// The whole change is only safe because it is Display-invisible: the message
/// a client reads must be byte-identical to what the untyped variant produced.
#[test]
fn typing_a_refusal_does_not_change_one_byte_of_its_message() {
    for (error, _) in typed_refusals() {
        let wrapped = std::io::Error::new(std::io::ErrorKind::InvalidInput, error);
        let legacy = QueryError::StorageError(wrapped.to_string()).to_string();
        let typed = QueryError::from_storage_io(wrapped).to_string();
        assert_eq!(
            typed, legacy,
            "the typed variant must render exactly what the untyped one did"
        );
    }
}

/// A plain I/O failure carries no kind to recover, so it must fall back to the
/// untyped variant rather than being mislabelled as some storage refusal.
#[test]
fn a_plain_io_failure_falls_back_to_the_untyped_variant() {
    let bare = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
    let rendered = bare.to_string();
    match QueryError::from_storage_io(bare) {
        QueryError::StorageError(message) => assert_eq!(message, rendered),
        other => panic!("expected the untyped fallback, got {other:?}"),
    }
}

/// End to end through the real executor: a duplicate key on a unique column
/// must arrive as a typed refusal, not as rendered text. Without this the
/// typed variant could exist with no producer, and the server's type-driven
/// classification would quietly never fire.
#[test]
fn a_duplicate_unique_key_reaches_the_caller_typed() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type Acct { unique email: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert Acct { email := "a@example.com" }"#)
        .unwrap();

    let error = engine
        .execute_powql(r#"insert Acct { email := "a@example.com" }"#)
        .expect_err("a duplicate value in a unique column must be refused");

    match &error {
        QueryError::Storage { kind, message } => {
            assert_eq!(*kind, StorageErrorKind::UniqueConstraintViolation);
            assert_eq!(message, "unique constraint violation on Acct.email");
        }
        other => panic!("expected a typed storage refusal, got {other:?}"),
    }
}

/// The same for a unique expression index, whose refusal names an expression
/// rather than a column and therefore never matched the column-level phrase
/// the server used to search for.
#[test]
fn a_duplicate_expression_index_key_reaches_the_caller_typed() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type Doc { required id: int, data: json }")
        .unwrap();
    engine
        .execute_powql(r#"insert Doc { id := 1, data := "{\"code\":\"a\"}" }"#)
        .unwrap();
    engine
        .execute_powql("alter Doc add unique (.data->code)")
        .unwrap();

    let error = engine
        .execute_powql(r#"insert Doc { id := 2, data := "{\"code\":\"a\"}" }"#)
        .expect_err("a duplicate expression-index key must be refused");

    match &error {
        QueryError::Storage { kind, message } => {
            assert_eq!(*kind, StorageErrorKind::UniqueExpressionIndexViolation);
            // The parenthesised part is the index's canonical text, an
            // internal encoding of the indexed expression, so only the stable
            // prefix is pinned here.
            assert!(
                message.starts_with("unique expression index violation on Doc ("),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected a typed storage refusal, got {other:?}"),
    }
}

/// DDL inside an explicit transaction is refused by the catalog, several
/// layers below the executor, so this proves the kind survives the whole
/// `io::Result` chain rather than only the one boundary above.
#[test]
fn ddl_refused_inside_a_transaction_reaches_the_caller_typed() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type Doomed { required id: int }")
        .unwrap();
    engine.execute_powql("begin").unwrap();

    let error = engine
        .execute_powql("drop Doomed")
        .expect_err("DDL inside an explicit transaction must be refused");

    match &error {
        QueryError::Storage { kind, message } => {
            assert_eq!(*kind, StorageErrorKind::DdlInTransaction);
            assert!(
                message.contains("DDL is not transactional in PowDB"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected a typed storage refusal, got {other:?}"),
    }
}
