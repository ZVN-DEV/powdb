//! The auth rate limiter's memory, driven through the real pre-auth path.
//!
//! Every key in the limiter is derived from a CONNECT frame sent by a peer
//! that has not authenticated. The username is the attacker-chosen half, and
//! nothing under the 4 KB pre-auth frame limit bounded it: a failed handshake
//! pinned a fresh copy of the whole name for a 60-second window, outliving the
//! connection that sent it. These tests hold the limiter to a bound rather
//! than to a reading of the source.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use powdb_auth::UserStore;
use powdb_server::handler::{
    new_rate_limiter, AuthRateLimiter, UserDirectory, MAX_AUTH_BUCKETS, MAX_AUTH_BUCKET_USER_BYTES,
};
use powdb_server::protocol::Message;

/// How the spawned server authenticates: the two shapes with different
/// bucketing, not two arbitrary configurations.
enum AuthMode {
    /// A user store, so the handshake takes the multi-user branch and the
    /// username really is part of the limiter key.
    OneUser,
    /// The shared-password (single-node) default, where the username is never
    /// looked at.
    SharedPassword,
}

async fn spawn_server(
    data_dir: std::path::PathBuf,
    limiter: AuthRateLimiter,
    mode: AuthMode,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (users, expected_password) = match mode {
        AuthMode::OneUser => {
            let mut users = UserStore::new();
            users
                .create_user("alice", "correct-horse", "admin")
                .unwrap();
            (users, None)
        }
        AuthMode::SharedPassword => (
            UserStore::new(),
            Some(zeroize::Zeroizing::new("shared-secret".to_string())),
        ),
    };
    let users = Arc::new(UserDirectory::fixed(users));
    tokio::spawn(async move {
        let engine = powdb_query::executor::Engine::new(&data_dir).unwrap();
        let engine = Arc::new(std::sync::RwLock::new(engine));
        let tx_gate = powdb_server::handler::new_tx_gate();
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            let eng = engine.clone();
            let tx_gate = tx_gate.clone();
            let users = Arc::clone(&users);
            let limiter = Arc::clone(&limiter);
            let expected_password = expected_password.clone();
            let (_, mut rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                powdb_server::handler::handle_connection(
                    stream,
                    powdb_server::handler::ConnOpts {
                        tx_wait_timeout: Duration::from_secs(5),
                        db_name: None,
                        engine: eng,
                        tx_gate,
                        expected_password,
                        users,
                        shutdown_rx: &mut rx,
                        idle_timeout: Duration::from_secs(10),
                        preauth_deadline: powdb_server::handler::DEFAULT_PREAUTH_DEADLINE,
                        query_timeout: Duration::from_secs(10),
                        rate_limiter: Some(&limiter),
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

/// One failed CONNECT naming `username`, carrying `password`.
async fn failed_connect_with(addr: &str, username: &str, password: Option<&str>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let frame = Message::Connect {
        db_name: "default".into(),
        password: password.map(|p| zeroize::Zeroizing::new(p.to_string())),
        username: Some(username.to_string()),
    }
    .encode();
    stream.write_all(&frame).await.unwrap();
    // Read the refusal (or the connection close) so the server has finished
    // recording the failure before the next attempt starts.
    let mut header = [0u8; 6];
    if stream.read_exact(&mut header).await.is_ok() {
        let payload_len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; payload_len];
        let _ = stream.read_exact(&mut payload).await;
    }
}

/// A failed CONNECT with no password at all: the multi-user branch rejects it
/// before any password hashing runs, so a spray costs the test nothing.
async fn failed_connect(addr: &str, username: &str) {
    failed_connect_with(addr, username, None).await;
}

/// A username that fills most of the pre-auth frame: the largest a peer can
/// actually send, and therefore the size the limiter has to survive.
fn long_username(n: usize) -> String {
    format!("{n:08}{}", "a".repeat(3800))
}

#[tokio::test]
async fn a_spray_of_long_usernames_cannot_pin_unbounded_limiter_memory() {
    let dir = tempfile::tempdir().unwrap();
    let limiter = new_rate_limiter();
    let addr = spawn_server(
        dir.path().to_path_buf(),
        Arc::clone(&limiter),
        AuthMode::OneUser,
    )
    .await;

    // Enough attempts to reach the peer-wide bound, each naming a username no
    // other attempt uses, which is what defeats the per-pair counter.
    let attempts = 60;
    for n in 0..attempts {
        failed_connect(&addr, &long_username(n)).await;
    }

    let table = limiter.lock().unwrap();
    assert!(
        table.len() <= MAX_AUTH_BUCKETS,
        "the limiter held {} buckets, above its {MAX_AUTH_BUCKETS} cap",
        table.len()
    );
    let ceiling = table.len() * MAX_AUTH_BUCKET_USER_BYTES;
    assert!(
        table.retained_user_bytes() <= ceiling,
        "the limiter retained {} bytes of peer-supplied username across {} buckets; \
         a key holds at most {MAX_AUTH_BUCKET_USER_BYTES} bytes, so the ceiling is {ceiling}",
        table.retained_user_bytes(),
        table.len()
    );
    // The spray must still have been counted: a bound that works by not
    // limiting anything is no bound at all.
    assert!(
        !table.is_empty(),
        "the failures were never recorded, so this test proves nothing"
    );
}

/// On a server with no user store, varying the username must not buy more
/// password guesses.
///
/// `authenticate_connect` never looks at the username there, so keying the
/// counter on it let one address make 50 guesses a minute instead of 5: a
/// tenfold loosening of the brute-force defence for exactly the deployment
/// shape that has a single shared password and no user store.
#[tokio::test]
async fn a_varying_username_buys_no_extra_shared_password_guesses() {
    let dir = tempfile::tempdir().unwrap();
    let limiter = new_rate_limiter();
    let addr = spawn_server(
        dir.path().to_path_buf(),
        Arc::clone(&limiter),
        AuthMode::SharedPassword,
    )
    .await;

    // Five wrong guesses, each naming a username nothing has used before.
    for n in 0..5 {
        failed_connect_with(&addr, &format!("throwaway-{n}"), Some("wrong")).await;
    }

    // The sixth is refused as rate limited, even though it names yet another
    // fresh username and carries the CORRECT password.
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    let frame = Message::Connect {
        db_name: "default".into(),
        password: Some(zeroize::Zeroizing::new("shared-secret".to_string())),
        username: Some("throwaway-5".to_string()),
    }
    .encode();
    stream.write_all(&frame).await.unwrap();
    let mut header = [0u8; 6];
    stream.read_exact(&mut header).await.unwrap();
    let payload_len = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).await.unwrap();
    let mut frame = header.to_vec();
    frame.extend_from_slice(&payload);
    match Message::decode(&frame).expect("the reply must decode") {
        Message::ErrorWithClass { message, .. } | Message::Error { message } => assert!(
            message.contains("too many auth failures"),
            "expected a rate-limit refusal, got {message:?}"
        ),
        other => panic!("a sixth guess from one address must be rate limited, got {other:?}"),
    }

    // And nothing peer-supplied was retained as a key.
    let table = limiter.lock().unwrap();
    assert_eq!(
        table.retained_user_bytes(),
        0,
        "a server with no user store must not key its limiter on a name it never reads"
    );
}
