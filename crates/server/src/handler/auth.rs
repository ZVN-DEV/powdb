//! Connect-handshake authentication: the credential decision, the per-IP
//! failure rate limiter, the connection's [`Principal`], and the RBAC check
//! every statement passes before it reaches the engine.

use powdb_auth::{Permission, Role, UserStore};
use powdb_query::executor::is_read_only_statement;
use powdb_query::result::QueryError;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Tracks per-IP authentication failure counts for rate limiting.
pub type AuthRateLimiter = Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>;

/// Maximum number of auth failures per IP within the rate-limit window.
const MAX_AUTH_FAILURES: u32 = 5;

/// Window during which auth failures are counted (60 seconds).
const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// Create a new shared rate limiter.
pub fn new_rate_limiter() -> AuthRateLimiter {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Check whether an IP is rate-limited and record a failure if requested.
/// Returns `true` if the IP should be rejected.
pub(super) fn is_rate_limited(limiter: &AuthRateLimiter, ip: IpAddr) -> bool {
    let mut map = limiter.lock().unwrap_or_else(|e| e.into_inner());
    // Clean up stale entries while we have the lock.
    let now = Instant::now();
    map.retain(|_, (_, ts)| now.duration_since(*ts) < AUTH_FAILURE_WINDOW);

    if let Some((count, _)) = map.get(&ip) {
        *count >= MAX_AUTH_FAILURES
    } else {
        false
    }
}

/// Record an auth failure for the given IP.
pub(super) fn record_auth_failure(limiter: &AuthRateLimiter, ip: IpAddr) {
    let mut map = limiter.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let entry = map.entry(ip).or_insert((0, now));
    // Reset counter if the window has elapsed.
    if now.duration_since(entry.1) >= AUTH_FAILURE_WINDOW {
        *entry = (1, now);
    } else {
        entry.0 += 1;
    }
}

/// Clear the failure counter on successful auth.
pub(super) fn clear_auth_failures(limiter: &AuthRateLimiter, ip: IpAddr) {
    let mut map = limiter.lock().unwrap_or_else(|e| e.into_inner());
    map.remove(&ip);
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
/// altering, or dropping a type or view. `explain <ddl>` is classified by its
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
