//! Process-level tests for `POWDB_DIRTY_PAGE_BUDGET`.
//!
//! The dirty-page budget bounds unflushed heap pages held across every table.
//! Inside an explicit transaction the buffer cannot be spilled without breaking
//! ROLLBACK, so exceeding the budget refuses the statement with
//! `StorageError::TransactionTooLarge`. That ceiling shipped hard-coded at
//! 256 MiB with no operator override; these tests boot the real binary and
//! assert the env override actually reaches the engine, rather than only
//! checking that the plumbing compiles.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{
    encode_connect, encode_query, read_response, spawn_server_bound, spawn_server_bound_env,
    wait_for_bind,
};
use powdb_server::protocol::Message;
use tokio::io::AsyncWriteExt;

/// Eight 4 KiB pages. Small enough that a few hundred padded rows blow past it,
/// large enough that the schema write itself fits.
const TINY_BUDGET_BYTES: &str = "32768";

/// Rows in the oversized transaction, each carrying a 400-byte pad. ~120 KiB of
/// heap, comfortably over `TINY_BUDGET_BYTES` and comfortably under the 256 MiB
/// default so the control case still commits.
const ROWS: usize = 300;

fn oversized_insert() -> String {
    let pad = "x".repeat(400);
    let rows: Vec<String> = (0..ROWS)
        .map(|i| format!("{{ id := {i}, pad := \"{pad}\" }}"))
        .collect();
    format!("insert Big {}", rows.join(", "))
}

/// Drive one connection through `type` / `begin` / oversized insert and return
/// the final response frame.
async fn run_oversized_transaction(port: u16) -> Vec<u8> {
    let mut stream = wait_for_bind(port, Duration::from_secs(20)).await;
    stream.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(
        read_response(&mut stream).await[0],
        0x02,
        "expected CONNECT_OK"
    );

    stream
        .write_all(&encode_query(
            "type Big { required id: int, required pad: str }",
        ))
        .await
        .unwrap();
    let r = read_response(&mut stream).await;
    assert!(r[0] == 0x09 || r[0] == 0x0B, "expected create-type ack");

    stream.write_all(&encode_query("begin")).await.unwrap();
    let r = read_response(&mut stream).await;
    assert!(r[0] == 0x09 || r[0] == 0x0B, "expected begin ack");

    stream
        .write_all(&encode_query(&oversized_insert()))
        .await
        .unwrap();
    read_response(&mut stream).await
}

#[tokio::test]
async fn env_override_lowers_the_dirty_page_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, port) = spawn_server_bound_env(
        tmp.path(),
        &[],
        &[("POWDB_DIRTY_PAGE_BUDGET", TINY_BUDGET_BYTES)],
    );

    let resp = run_oversized_transaction(port).await;
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(
        resp[0], 0x0A,
        "an oversized transaction must be refused under a lowered budget"
    );
    let message = match Message::decode(&resp).unwrap() {
        Message::Error { message } => message,
        other => panic!("expected an error frame, got {other:?}"),
    };
    assert!(
        message.contains("dirty-page budget"),
        "expected the transaction-too-large refusal, got: {message}"
    );
    assert!(
        message.contains(TINY_BUDGET_BYTES),
        "the refusal must report the overridden budget, not the 256 MiB default: {message}"
    );
}

#[tokio::test]
async fn the_same_transaction_fits_the_default_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, port) = spawn_server_bound(tmp.path(), &[]);

    let resp = run_oversized_transaction(port).await;
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(
        resp[0], 0x09,
        "the control transaction must succeed on the 256 MiB default, \
         otherwise the override test proves nothing"
    );
}
