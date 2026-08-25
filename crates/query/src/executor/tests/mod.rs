//! Executor unit tests, sharded by theme. Every shard starts with
//! `use super::*;`, so the shared imports, the temp-dir counter, and the
//! `test_engine` fixture below are visible everywhere; shard-local fixtures
//! live next to the tests that use them.

use super::compiled::f64_bits_to_sortable_u64;
use super::Engine;
// The shards reach these through `super::`, exactly as the single file
// reached the executor module before it was sharded.
use super::{mem_budget, plan_exec, WalSyncMode, MAX_NESTED_LOOP_PAIRS};
use crate::ast::{BinOp, Expr, JoinKind, Literal};
use crate::result::QueryError;
use crate::result::QueryResult;
use powdb_storage::types::*;
use std::sync::atomic::{AtomicU32, Ordering};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

fn test_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_exec_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type User { required name: str, required email: str, age: int }")
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Alice", email := "alice@ex.com", age := 30 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Bob", email := "bob@ex.com", age := 25 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert User { name := "Charlie", email := "charlie@ex.com", age := 35 }"#)
        .unwrap();
    engine
}

mod basics;
mod cancellation;
mod explain_subqueries_functions;
mod fast_paths;
mod index_selection;
mod joins;
mod numeric;
mod parser_ddl_regressions;
mod prepared;
mod sql_features;
mod transactions_and_indexes;
mod update_coercion;
mod views_and_sets;
mod windows_unique_params;
