#![no_main]
use libfuzzer_sys::fuzz_target;
use powdb_query::executor::Engine;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

// Fuzz the WHOLE query path, not just the front end.
//
// `fuzz_lexer`, `fuzz_parser` and `fuzz_sql` stop at the AST. Everything that
// has actually shipped a P0 in this engine lives past that line: the planner
// picking an index, the executor's compiled-predicate fast paths, the heap
// row decoder, the B-tree probe. `fuzz.yml`'s path filter did not even list
// those files, so a change to `planner.rs` or `executor/` never triggered a
// fuzz run. This target closes that gap: arbitrary PowQL, executed against a
// real on-disk engine with a real schema and real rows.
//
// Invariant: `execute_powql` returns `Ok` or a clean `Err` for every input.
// A panic (an abort under the shipped release profile) is the failure.
// Wrong ANSWERS are not in scope here; the property suites in
// `crates/query/tests` own that.

/// One engine reused across cases: opening a fresh data dir per input costs
/// more than the query being fuzzed, which starves coverage. State
/// accumulates on purpose (a fuzzed `insert` is visible to a later fuzzed
/// `filter`), and the dir is rebuilt periodically so a run cannot grow
/// without bound.
static ENGINE: OnceLock<Mutex<Option<Engine>>> = OnceLock::new();
static CASES: AtomicU64 = AtomicU64::new(0);

/// Rebuild the data dir every this many inputs. Chosen so a 5-minute nightly
/// run rebuilds a handful of times rather than filling the runner's disk with
/// fuzzed inserts.
const REBUILD_EVERY: u64 = 4096;

fn data_dir() -> PathBuf {
    std::env::temp_dir().join(format!("powdb_fuzz_execute_{}", std::process::id()))
}

/// Seed a data dir with a schema broad enough that the planner and executor
/// have real choices to make: an indexed unique int, a nullable string, a
/// float, a JSON column, a second table, and a declared entity link.
fn build_engine() -> Engine {
    let dir = data_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fuzz data dir");
    let mut engine = Engine::new(&dir).expect("open fuzz engine");
    let setup = [
        "type User { required unique id: int, name: str, score: float, tags: json }",
        "type Note { required unique id: int, user_id: int, body: str }",
        "link Note.user -> User on user_id = id",
        "insert User { id := 1, name := \"alice\", score := 1.5, tags := \"[1,2]\" }",
        "insert User { id := 2, name := \"bob\", score := -0.5 }",
        "insert User { id := 3, score := 12.0, tags := \"{\\\"k\\\":true}\" }",
        "insert Note { id := 1, user_id := 1, body := \"first\" }",
        "insert Note { id := 2, user_id := 2, body := \"second\" }",
        "insert Note { id := 3, user_id := 99, body := \"orphan\" }",
    ];
    for stmt in setup {
        engine
            .execute_powql(stmt)
            .unwrap_or_else(|e| panic!("fuzz fixture setup failed on {stmt:?}: {e:?}"));
    }
    engine
}

fuzz_target!(|data: &[u8]| {
    let Ok(query) = std::str::from_utf8(data) else {
        return;
    };

    let slot = ENGINE.get_or_init(|| Mutex::new(None));
    // The engine is only touched here; a poisoned lock means a previous case
    // already panicked and libFuzzer has reported it.
    let mut guard = match slot.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };

    if CASES
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(REBUILD_EVERY)
    {
        // Drop the old engine (releasing its data-dir lock) before rebuilding.
        *guard = None;
        *guard = Some(build_engine());
    }
    let engine = guard.as_mut().expect("engine present after rebuild");

    // Ok and Err are both acceptable outcomes. Only a panic/abort fails.
    let _ = engine.execute_powql(query);
});
