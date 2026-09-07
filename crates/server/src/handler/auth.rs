//! Connect-handshake authentication: the credential decision, the per-IP
//! failure rate limiter, the connection's [`Principal`], and the RBAC check
//! every statement passes before it reaches the engine.

use powdb_auth::{Permission, Role, UserStore};
use powdb_query::executor::is_read_only_statement;
use powdb_query::result::QueryError;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tracing::{info, warn};

/// What the handshake authenticates against, kept in step with `auth.json`.
///
/// `powdb-cli useradd` / `passwd` / `userdel` write that file directly. The
/// server used to load it once at startup, so against a running server a
/// rotated password kept working and a deleted user kept logging in until
/// somebody restarted the process, with nothing anywhere saying so. The file
/// is re-read when its length or modification time changes, checked once per
/// authentication attempt (which the per-peer failure limiter already bounds)
/// and on SIGHUP.
pub struct UserDirectory {
    /// The data directory `auth.json` lives in. `None` for a store that is
    /// handed over directly and never reloads (tests, embedded callers).
    data_dir: Option<PathBuf>,
    state: Mutex<DirectoryState>,
}

struct DirectoryState {
    users: Arc<UserStore>,
    stamp: Option<FileStamp>,
}

/// What "the file changed" means here: a different length or a different
/// modification time. Cheap enough to check on every authentication attempt.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStamp {
    fn of(path: &Path) -> Option<FileStamp> {
        let meta = std::fs::metadata(path).ok()?;
        Some(FileStamp {
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

impl UserDirectory {
    /// Load `dir/auth.json` and watch it for later changes.
    pub fn load(dir: &Path) -> std::io::Result<Self> {
        let users = UserStore::load(dir)?;
        Ok(UserDirectory {
            data_dir: Some(dir.to_path_buf()),
            state: Mutex::new(DirectoryState {
                users: Arc::new(users),
                stamp: FileStamp::of(&dir.join("auth.json")),
            }),
        })
    }

    /// A directory that serves exactly this store and never reloads.
    pub fn fixed(users: UserStore) -> Self {
        UserDirectory {
            data_dir: None,
            state: Mutex::new(DirectoryState {
                users: Arc::new(users),
                stamp: None,
            }),
        }
    }

    /// An empty non-reloading directory: no named users, so the handshake
    /// falls back to the shared-password (or open) path.
    pub fn empty() -> Self {
        Self::fixed(UserStore::new())
    }

    /// The users to authenticate against right now, re-reading `auth.json`
    /// first if it changed since the last look.
    pub fn current(&self) -> Arc<UserStore> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(dir) = self.data_dir.as_ref() else {
            return Arc::clone(&state.users);
        };
        let stamp = FileStamp::of(&dir.join("auth.json"));
        if stamp == state.stamp {
            return Arc::clone(&state.users);
        }
        match UserStore::load(dir) {
            Ok(users) => {
                info!(users = users.len(), "reloaded auth.json");
                state.users = Arc::new(users);
                state.stamp = stamp;
            }
            Err(e) => {
                // Keep serving the store we have: a half-written file must not
                // lock every user out. The stamp is left alone so the next
                // attempt retries.
                warn!(error = %e, "auth.json changed but could not be re-read; keeping the loaded users");
            }
        }
        Arc::clone(&state.users)
    }

    /// Re-read `auth.json` unconditionally, returning the user count.
    pub fn reload(&self) -> std::io::Result<usize> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(dir) = self.data_dir.as_ref() else {
            return Ok(state.users.len());
        };
        let users = UserStore::load(dir)?;
        let count = users.len();
        state.users = Arc::new(users);
        state.stamp = FileStamp::of(&dir.join("auth.json"));
        Ok(count)
    }
}

/// Who a failed authentication attempt is counted against.
///
/// The bucket is keyed by (peer, username) with the peer alone kept as an
/// outer bound at a higher threshold. Keying by peer alone meant five wrong
/// guesses at ANY username locked the legitimate user out of that address for
/// a minute, which is a denial of service a single bad script can cause; and a
/// Unix-socket peer has no address at all, so it was never throttled.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum AuthBucket {
    /// One username as attempted from one peer.
    PeerUser {
        peer: AuthPeer,
        user: Option<String>,
    },
    /// Everything that peer attempts, whatever username it names.
    Peer(AuthPeer),
}

/// The peer half of a bucket key. A Unix-socket peer has no address, so every
/// local connection shares one bucket rather than none.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum AuthPeer {
    Ip(IpAddr),
    /// Any connection over the Unix domain socket.
    UnixSocket,
}

impl AuthPeer {
    /// The peer key for a connection, whether or not it has an address.
    pub(super) fn of(peer_addr: Option<std::net::SocketAddr>) -> AuthPeer {
        match peer_addr {
            Some(addr) => AuthPeer::Ip(addr.ip()),
            None => AuthPeer::UnixSocket,
        }
    }
}

/// Tracks authentication failure counts per bucket.
pub type AuthRateLimiter = Arc<Mutex<AuthFailureTable>>;

/// Failures at ONE username from one peer before that pair is locked out.
const MAX_AUTH_FAILURES: u32 = 5;

/// Failures across ALL usernames from one peer before the whole peer is locked
/// out. Higher than the per-user threshold on purpose: it exists to bound a
/// sweep across many usernames, not to let one wrong username lock out
/// another.
pub(super) const MAX_PEER_AUTH_FAILURES: u32 = 50;

/// Window during which auth failures are counted (60 seconds).
const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// The longest username retained inside a rate-limiter key.
///
/// The username on a CONNECT frame is chosen by a peer that has not
/// authenticated yet, and nothing under the 4 KB pre-auth frame limit bounded
/// it. Every failed handshake used to pin a fresh copy of the whole thing for
/// the length of the window, outliving the connection that sent it. Keys are
/// truncated instead: two names sharing a 64-byte prefix share one bucket,
/// which can only make the limiter stricter, never more permissive. Real
/// usernames are far shorter than this.
pub const MAX_AUTH_BUCKET_USER_BYTES: usize = 64;

/// The most failure buckets held at once, across every peer.
///
/// One peer is bounded by [`MAX_PEER_AUTH_FAILURES`], but the number of
/// distinct peers is not: a single IPv6 /64 supplies more source addresses
/// than this table could ever hold, and each of their entries outlives the
/// connection that created it. At capacity the table evicts, so its memory is
/// bounded by this constant whatever an unauthenticated peer does.
pub const MAX_AUTH_BUCKETS: usize = 4096;

/// How many buckets survive an eviction. The headroom means a full table sorts
/// itself once per thousand inserts rather than on every one.
const AUTH_BUCKETS_AFTER_EVICTION: usize = MAX_AUTH_BUCKETS * 3 / 4;

/// How often the expiry sweep runs.
///
/// The sweep is an O(n) scan under the limiter's mutex. Running it on every
/// CONNECT made each legitimate handshake pay for the size of a table an
/// attacker had inflated; once per second bounds that cost without letting an
/// expired entry linger meaningfully longer than it used to.
const AUTH_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// How long a locked-out peer or user must wait, for the client-facing
/// message. The window is fixed, so this is the whole of it.
pub(super) fn auth_retry_after_secs() -> u64 {
    AUTH_FAILURE_WINDOW.as_secs()
}

/// One bucket's failure count and the window it is being counted in.
#[derive(Clone, Copy, Debug)]
struct FailureWindow {
    count: u32,
    started: Instant,
}

/// Authentication failure counts, bounded in both key size and entry count.
///
/// Every key here is derived from bytes an unauthenticated peer sent, so the
/// table treats its own size as part of the attack surface: keys are
/// truncated ([`MAX_AUTH_BUCKET_USER_BYTES`]), the entry count is capped
/// ([`MAX_AUTH_BUCKETS`]), and eviction is deterministic.
#[derive(Debug)]
pub struct AuthFailureTable {
    buckets: HashMap<AuthBucket, FailureWindow>,
    last_sweep: Instant,
}

impl Default for AuthFailureTable {
    fn default() -> Self {
        AuthFailureTable {
            buckets: HashMap::new(),
            last_sweep: Instant::now(),
        }
    }
}

impl AuthFailureTable {
    /// How many buckets are currently held.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether no failure is currently being counted.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Bytes of peer-supplied username retained across every key.
    ///
    /// Exposed so the limiter's memory bound can be asserted by a test that
    /// drives the real handshake, rather than argued from the source.
    pub fn retained_user_bytes(&self) -> usize {
        self.buckets
            .keys()
            .map(|bucket| match bucket {
                AuthBucket::PeerUser {
                    user: Some(user), ..
                } => user.len(),
                _ => 0,
            })
            .sum()
    }

    /// Drop every bucket whose window has elapsed.
    fn sweep(&mut self, now: Instant) {
        self.buckets
            .retain(|_, window| now.duration_since(window.started) < AUTH_FAILURE_WINDOW);
        self.last_sweep = now;
    }

    fn sweep_if_due(&mut self, now: Instant) {
        if now.duration_since(self.last_sweep) >= AUTH_SWEEP_INTERVAL {
            self.sweep(now);
        }
    }

    /// Failures counted against `key` inside the current window. An entry
    /// whose window has elapsed reads as zero whether or not it has been
    /// swept yet.
    fn count(&self, key: &AuthBucket, now: Instant) -> u32 {
        match self.buckets.get(key) {
            Some(window) if now.duration_since(window.started) < AUTH_FAILURE_WINDOW => {
                window.count
            }
            _ => 0,
        }
    }

    /// Count one failure against `key`.
    fn record(&mut self, key: AuthBucket, now: Instant) {
        if let Some(window) = self.buckets.get_mut(&key) {
            if now.duration_since(window.started) >= AUTH_FAILURE_WINDOW {
                *window = FailureWindow {
                    count: 1,
                    started: now,
                };
            } else {
                window.count = window.count.saturating_add(1);
            }
            return;
        }
        self.make_room(now);
        self.buckets.insert(
            key,
            FailureWindow {
                count: 1,
                started: now,
            },
        );
    }

    fn forget(&mut self, key: &AuthBucket) {
        self.buckets.remove(key);
    }

    /// Make room for one more bucket.
    ///
    /// Expired entries go first. If the table is still full, the buckets
    /// furthest from locking anybody out are dropped: lowest failure count,
    /// then oldest window, then key order. The ordering is total, so the same
    /// table always evicts the same entries, and a spray of one-failure
    /// buckets can never displace a peer that is close to its bound.
    fn make_room(&mut self, now: Instant) {
        if self.buckets.len() < MAX_AUTH_BUCKETS {
            return;
        }
        self.sweep(now);
        if self.buckets.len() < MAX_AUTH_BUCKETS {
            return;
        }
        let mut ranked: Vec<(u32, Instant, AuthBucket)> = self
            .buckets
            .iter()
            .map(|(key, window)| (window.count, window.started, key.clone()))
            .collect();
        ranked.sort_unstable();
        let evict = self
            .buckets
            .len()
            .saturating_sub(AUTH_BUCKETS_AFTER_EVICTION);
        for (_, _, key) in ranked.into_iter().take(evict) {
            self.buckets.remove(&key);
        }
    }
}

/// Create a new shared rate limiter.
pub fn new_rate_limiter() -> AuthRateLimiter {
    Arc::new(Mutex::new(AuthFailureTable::default()))
}

/// Whether this (peer, user) pair, or the peer as a whole, has failed too many
/// times inside the window.
pub(super) fn is_rate_limited(
    limiter: &AuthRateLimiter,
    peer: &AuthPeer,
    user: Option<&str>,
) -> bool {
    let mut table = limiter.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    table.sweep_if_due(now);
    table.count(&pair_bucket(peer, user), now) >= MAX_AUTH_FAILURES
        || table.count(&AuthBucket::Peer(peer.clone()), now) >= MAX_PEER_AUTH_FAILURES
}

/// The username as it is stored inside a key: at most
/// [`MAX_AUTH_BUCKET_USER_BYTES`], cut on a character boundary so the key
/// stays a valid `String`.
fn bucket_user_key(user: &str) -> String {
    let mut end = MAX_AUTH_BUCKET_USER_BYTES.min(user.len());
    while end > 0 && !user.is_char_boundary(end) {
        end -= 1;
    }
    user[..end].to_string()
}

fn pair_bucket(peer: &AuthPeer, user: Option<&str>) -> AuthBucket {
    AuthBucket::PeerUser {
        peer: peer.clone(),
        user: user.map(bucket_user_key),
    }
}

/// Record an auth failure against both the pair and the peer bucket.
pub(super) fn record_auth_failure(limiter: &AuthRateLimiter, peer: &AuthPeer, user: Option<&str>) {
    let mut table = limiter.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    table.sweep_if_due(now);
    for key in [pair_bucket(peer, user), AuthBucket::Peer(peer.clone())] {
        table.record(key, now);
    }
}

/// Clear the failure counters a successful authentication settles: this pair,
/// and the peer bound it contributed to.
pub(super) fn clear_auth_failures(limiter: &AuthRateLimiter, peer: &AuthPeer, user: Option<&str>) {
    let mut table = limiter.lock().unwrap_or_else(|e| e.into_inner());
    table.forget(&pair_bucket(peer, user));
    table.forget(&AuthBucket::Peer(peer.clone()));
}

/// Constant-time password comparison. Hashes both inputs to fixed-size
/// SHA-256 digests so neither length nor content leaks through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    let ha = Sha256::digest(a);
    let hb = Sha256::digest(b);
    let mut diff = 0u8;
    for (x, y) in ha.iter().zip(hb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// An authenticated connection's identity. Bound at connect time and consulted
/// on every query by `dispatch_query` to enforce the user's role: a
/// `readonly` principal may only execute read statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
    pub role: String,
}

/// Whether a parsed statement is data-definition (schema) work: creating,
/// altering, or dropping a type, link or view. `explain <ddl>` is classified by its
/// inner statement so `explain drop User` needs the same permission as
/// `drop User`. Mutations that change *rows* (insert/update/delete/upsert/
/// refresh) and transaction control are NOT DDL — they fall under `Write`.
fn is_ddl_statement(stmt: &powdb_query::ast::Statement) -> bool {
    use powdb_query::ast::Statement;
    let inner = match stmt {
        Statement::Explain(inner) => inner.as_ref(),
        other => other,
    };
    matches!(
        inner,
        Statement::CreateType(_)
            | Statement::CreateLink(_)
            | Statement::DropLink(_)
            | Statement::AlterTable(_)
            | Statement::DropTable(_)
            | Statement::CreateView(_)
            | Statement::DropView(_)
    )
}

/// The capability a parsed statement requires under the RBAC lattice
/// (`crates/auth/src/role.rs`). Reads need [`Permission::Read`]; schema
/// definition needs [`Permission::Ddl`]; every other mutation needs
/// [`Permission::Write`]. [`Permission::Admin`] is reserved for user/role
/// management, which is CLI-only today and never reaches this wire path.
fn required_permission(stmt: &powdb_query::ast::Statement) -> Permission {
    if is_read_only_statement(stmt) {
        Permission::Read
    } else if is_ddl_statement(stmt) {
        Permission::Ddl
    } else {
        Permission::Write
    }
}

/// Enforce the principal's role against a parsed statement using the full
/// permission lattice. Reads are always permitted (any authenticated role can
/// read — unknown role names still read but fail closed on any mutation).
/// Mutations require the specific capability the statement maps to: row
/// mutations need `Write`, schema changes need `Ddl`. Unknown role names
/// resolve to no builtin and therefore grant nothing beyond reads.
///
/// Classification uses the parsed AST via
/// [`powdb_query::executor::is_read_only_statement`] — the exact same
/// classifier the RwLock read/write split relies on — so the permission
/// boundary and the concurrency boundary can never disagree.
pub(super) fn check_statement_permitted(
    principal: Option<&Principal>,
    stmt: &powdb_query::ast::Statement,
) -> Result<(), QueryError> {
    let Some(p) = principal else {
        // No per-user identity (shared-password or open mode): full access,
        // byte-identical to the pre-RBAC behavior.
        return Ok(());
    };
    // Reads are permitted for every authenticated principal (preserves the
    // pre-lattice contract that any connected role may run read-only queries).
    if is_read_only_statement(stmt) {
        return Ok(());
    }
    let needed = required_permission(stmt);
    if Role::builtin(&p.role).is_some_and(|r| r.allows(needed)) {
        return Ok(());
    }
    let kind = if needed == Permission::Ddl {
        "schema-definition"
    } else {
        "write"
    };
    Err(QueryError::Execution(format!(
        "permission denied: role '{}' cannot execute {kind} statements",
        p.role
    )))
}

/// Result of the connect-time authentication decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutcome {
    /// Authenticated. `principal` is `Some` when a named user authenticated via
    /// the UserStore, and `None` for the legacy shared-password / open paths
    /// where there is no per-user identity.
    Authenticated { principal: Option<Principal> },
    /// Rejected. The caller sends a generic "authentication failed" error and
    /// records a rate-limit failure — it must not reveal which check failed.
    Rejected,
}

/// Pure, exhaustively-testable authentication decision for a CONNECT handshake.
///
/// Policy:
/// - If `users` has at least one user, multi-user auth is in force: a
///   `username` is required and `users.authenticate(username, password)` must
///   succeed. Unknown user, wrong password, or a missing username all reject
///   with an indistinguishable `Rejected` (no user-vs-password leak).
/// - If `users` is empty, fall back verbatim to the legacy behavior: when
///   `expected_password` is `Some`, the candidate must match it (constant time);
///   when `None`, no auth is required (open). The `username` is ignored here so
///   that a new client talking to a shared-password server still connects.
pub fn authenticate_connect(
    users: &UserStore,
    expected_password: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> AuthOutcome {
    if !users.is_empty() {
        // Multi-user mode: a username is mandatory.
        let Some(name) = username else {
            return AuthOutcome::Rejected;
        };
        let Some(candidate) = password else {
            return AuthOutcome::Rejected;
        };
        match users.authenticate(name, candidate) {
            Some(user) => AuthOutcome::Authenticated {
                principal: Some(Principal {
                    name: user.name.clone(),
                    role: user.role.clone(),
                }),
            },
            None => AuthOutcome::Rejected,
        }
    } else {
        // Legacy shared-password fallback (byte-identical to prior behavior).
        match expected_password {
            Some(expected) => {
                if password.is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes())) {
                    AuthOutcome::Authenticated { principal: None }
                } else {
                    AuthOutcome::Rejected
                }
            }
            None => AuthOutcome::Authenticated { principal: None },
        }
    }
}

/// The sentinel database name clients send when the user selected none. Both
/// the CLI and the TS client default to this, so it means "no specific
/// database" and is always accepted — even when the server is pinned to a name.
pub(super) const DEFAULT_DB_NAME: &str = "default";

/// Decide whether a CONNECT's requested `db_name` is served by this process.
///
/// One server process serves exactly one global database. When it is pinned to
/// a name (`configured = Some`), a request that *explicitly* names a different
/// database is rejected so a client can never silently read/write the wrong
/// store. An empty name or the client default sentinel (`"default"`) means "no
/// specific database selected" and is always accepted. When unpinned (`None`)
/// every name is accepted (0.9.x back-compat); the caller warns on a non-default
/// name so the silent-mismatch footgun is at least visible in the logs.
pub(super) fn check_db_name(configured: Option<&str>, requested: &str) -> Result<(), String> {
    if requested.is_empty() || requested == DEFAULT_DB_NAME {
        return Ok(());
    }
    match configured {
        None => Ok(()),
        Some(name) if requested == name => Ok(()),
        Some(name) => Err(format!(
            "unknown database '{requested}'; this server serves '{name}'"
        )),
    }
}
