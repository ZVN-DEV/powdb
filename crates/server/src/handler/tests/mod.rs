//! Unit tests for the connection handler, split by the concern each one
//! pins. They reach across the whole `handler` tree on purpose: what they
//! check is how the modules behave together on one connection.

mod auth;
mod errors;
mod gate;
mod query;
mod sync;
mod transaction;
mod wire;

use crate::protocol::{WireParam, WireSyncRepairAction};
use powdb_query::parser;
use powdb_query::result::{QueryError, QueryResult};
use powdb_storage::error::StorageError;
use powdb_storage::types::Value;
use powdb_storage::wal::WalRecordType;
use powdb_sync::{
    retained_segments_dir, write_identity_snapshot, write_segment_atomic, DatabaseIdentity,
    IdentitySnapshot, ReplicaCursor, RetainedSegment, RetainedUnit,
    RETAINED_SEGMENT_FORMAT_VERSION,
};
use std::sync::Mutex;

// The whole handler tree, re-exported down into every test file below: a
// connection is served by all of these together, so the tests that pin its
// behavior reach into all of them.
use super::auth::*;
use super::classify::*;
use super::query::*;
use super::sync::*;
use super::transaction::*;
use super::wire::*;
use super::*;

/// A one-row engine, shared by every file that needs a statement to actually
/// execute rather than merely be admitted.
fn one_row_engine() -> (tempfile::TempDir, Arc<RwLock<Engine>>) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type User { required id: int }")
        .unwrap();
    engine.execute_powql("insert User { id := 1 }").unwrap();
    (dir, Arc::new(RwLock::new(engine)))
}

/// A principal in `role`, for the RBAC boundary and the gate matrix.
fn principal(role: &str) -> Option<Principal> {
    Some(Principal {
        name: "u".into(),
        role: role.into(),
    })
}

/// The identity the sync frontend's tests and the gate matrix run as.
fn admin_principal() -> Principal {
    Principal {
        name: "admin".into(),
        role: "admin".into(),
    }
}
