//! Process-level durability test: SIGKILL the real `powdb-server` binary in
//! the middle of a sustained insert workload, restart it on the same data
//! dir, and assert every acknowledged insert survived.
//!
//! Unlike the in-process durability suite (which simulates crashes with
//! `std::mem::forget`), this drives the shipped binary and a real `kill -9`:
//! no Drop impls run, no graceful drain, no checkpoint. Recovery is pure WAL
//! replay on the next start.
//!
//! Sync mode: the server is started with `POWDB_SYNC_MODE=full` (also the
//! default). Full mode is the one whose durability contract promises that an
//! acknowledged write survives ANY crash, including power loss: the WAL
//! fsync completes before the RESULT_OK frame is sent. `normal` only bounds
//! loss for OS crashes (a process kill loses nothing, but the contract is
//! weaker), and `off` promises nothing, so `full` is the mode under test.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{
    encode_connect, encode_query, free_port, read_response, send_sigkill, spawn_server_bin_env,
    wait_for_bind, wait_with_timeout,
};
use powdb_server::protocol::Message;
use tokio::io::AsyncWriteExt;

/// Inserts before the kill. Small enough to stay well under a minute even
/// with a full fsync per acknowledged statement.
const PRE_KILL_ACKS: usize = 25;
/// Upper bound on the whole insert loop, so a server that never dies (or a
/// test bug) cannot spin forever.
const MAX_INSERTS: usize = 200;

#[tokio::test]
async fn sigkill_mid_write_preserves_every_acknowledged_insert() {
    let tmp = tempfile::tempdir().unwrap();
    let port = free_port();

    // Boot the real binary in full-durability mode (explicit, see header).
    let mut child = spawn_server_bin_env(port, tmp.path(), &[], &[("POWDB_SYNC_MODE", "full")]);
    let mut stream = wait_for_bind(port, Duration::from_secs(20)).await;

    stream.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(
        read_response(&mut stream).await[0],
        0x02,
        "expected CONNECT_OK"
    );
    stream
        .write_all(&encode_query("type Ledger { required id: int }"))
        .await
        .unwrap();
    let r = read_response(&mut stream).await;
    assert!(r[0] == 0x09 || r[0] == 0x0B, "expected create-type ack");

    // Sustained insert loop over the wire. Every RESULT_OK we fully read is
    // an acknowledged (fsynced) insert. The SIGKILL lands between the write
    // and the read of insert `PRE_KILL_ACKS`, so the process dies with that
    // statement genuinely in flight; later writes race the dying socket.
    let mut acked: Vec<i64> = Vec::new();
    for i in 0..MAX_INSERTS as i64 {
        let frame = encode_query(&format!("insert Ledger {{ id := {i} }}"));
        if stream.write_all(&frame).await.is_err() {
            break; // server is gone
        }
        if i == PRE_KILL_ACKS as i64 {
            send_sigkill(&child);
        }
        // Bounded read: after the kill the socket returns EOF/reset instead
        // of an ack; a hang here would mean the test lost its liveness.
        let resp = tokio::time::timeout(Duration::from_secs(10), async {
            let mut header = [0u8; 6];
            use tokio::io::AsyncReadExt;
            stream.read_exact(&mut header).await?;
            let payload_len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
            let mut payload = vec![0u8; payload_len];
            if payload_len > 0 {
                stream.read_exact(&mut payload).await?;
            }
            Ok::<u8, std::io::Error>(header[0])
        })
        .await;
        match resp {
            Ok(Ok(0x09)) => acked.push(i),
            Ok(Ok(other)) => panic!("unexpected reply 0x{other:02X} for insert {i}"),
            Ok(Err(_)) | Err(_) => break, // killed mid-flight
        }
    }
    assert!(
        acked.len() >= PRE_KILL_ACKS,
        "expected at least {PRE_KILL_ACKS} acknowledged inserts before the kill, got {}",
        acked.len()
    );

    // The process must be dead (killed by signal, so not success()).
    let status = wait_with_timeout(&mut child, Duration::from_secs(10));
    assert!(!status.success(), "kill -9 must terminate the server");
    drop(stream);

    // Restart on the same data dir: the server must come up cleanly (WAL
    // replay) and serve every acknowledged row.
    let mut child2 = spawn_server_bin_env(port, tmp.path(), &[], &[("POWDB_SYNC_MODE", "full")]);
    let mut s2 = wait_for_bind(port, Duration::from_secs(20)).await;
    s2.write_all(&encode_connect("testdb")).await.unwrap();
    assert_eq!(
        read_response(&mut s2).await[0],
        0x02,
        "expected CONNECT_OK after restart (clean WAL replay)"
    );

    s2.write_all(&encode_query("Ledger")).await.unwrap();
    let resp = read_response(&mut s2).await;
    assert_eq!(resp[0], 0x07, "expected RESULT_ROWS after restart");
    let recovered: Vec<i64> = match Message::decode(&resp).unwrap() {
        Message::ResultRows { columns, rows } => {
            let id_col = columns
                .iter()
                .position(|c| c == "id")
                .expect("Ledger must expose the id column");
            rows.iter()
                .map(|row| row[id_col].parse::<i64>().expect("integer id"))
                .collect()
        }
        other => panic!("expected ResultRows, got {other:?}"),
    };

    // Every acknowledged insert must be present. Unacknowledged in-flight
    // inserts MAY have committed (ack lost in the kill window); that is
    // allowed, silent loss of an acked row is not.
    for id in &acked {
        assert!(
            recovered.contains(id),
            "acknowledged insert id={id} lost after kill -9 + restart \
             (acked {} rows, recovered {:?})",
            acked.len(),
            recovered
        );
    }

    // The restarted server keeps working: one more durable write round-trip.
    s2.write_all(&encode_query("insert Ledger { id := 100000 }"))
        .await
        .unwrap();
    assert_eq!(
        read_response(&mut s2).await[0],
        0x09,
        "restarted server must accept new writes"
    );

    let _ = child2.kill();
    let _ = child2.wait();
}
