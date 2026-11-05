//! Test-only physical work counters; no instrumentation is shipped by default.

use std::cell::Cell;

/// Physical execution work recorded by the instrumented scan paths.
///
/// Counts are inclusive of nested captures, local to the executing thread,
/// and cover visits rather than output cardinality. They do not estimate I/O,
/// bytes decoded, cache hits or wall-clock time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryWork {
    /// Rows visited by instrumented sequential/raw scan paths.
    pub seq_rows: u64,
    /// Equality index probes made by instrumented execution paths.
    pub index_probes: u64,
    /// Heap row fetch attempts made by instrumented execution paths.
    pub heap_fetches: u64,
}

impl QueryWork {
    fn plus(self, other: Self) -> Self {
        Self {
            seq_rows: self.seq_rows.saturating_add(other.seq_rows),
            index_probes: self.index_probes.saturating_add(other.index_probes),
            heap_fetches: self.heap_fetches.saturating_add(other.heap_fetches),
        }
    }
}

thread_local! {
    static CURRENT: Cell<Option<QueryWork>> = const { Cell::new(None) };
}

struct CaptureGuard(Option<QueryWork>);

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| {
            let inner = current.get().unwrap_or_default();
            current.set(self.0.map(|outer| outer.plus(inner)));
        });
    }
}

/// Capture physical work on this thread without changing query behavior.
///
/// Nested captures contribute to their parent's count. The previous capture
/// is restored on errors and unwinding, so concurrent test/query threads never
/// share counters. Available only with the non-shipping `testing` feature.
pub fn capture_query_work<T>(run: impl FnOnce() -> T) -> (T, QueryWork) {
    let guard = CaptureGuard(CURRENT.with(|current| current.replace(Some(QueryWork::default()))));
    let result = run();
    let work = CURRENT.with(|current| current.get().unwrap_or_default());
    drop(guard);
    (result, work)
}

fn record(update: impl FnOnce(&mut QueryWork)) {
    CURRENT.with(|current| {
        if let Some(mut work) = current.get() {
            update(&mut work);
            current.set(Some(work));
        }
    });
}

pub(super) fn record_seq_row() {
    record(|work| work.seq_rows = work.seq_rows.saturating_add(1));
}

pub(super) fn record_index_probe() {
    record(|work| work.index_probes = work.index_probes.saturating_add(1));
}

pub(super) fn record_heap_fetch() {
    record(|work| work.heap_fetches = work.heap_fetches.saturating_add(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_are_nested_inclusive_and_restore_after_unwind() {
        let (_, outer) = capture_query_work(|| {
            record_seq_row();
            let (_, inner) = capture_query_work(record_index_probe);
            assert_eq!(inner.index_probes, 1);
            let failed = std::panic::catch_unwind(|| {
                capture_query_work(|| {
                    record_heap_fetch();
                    panic!("fixture panic");
                });
            });
            assert!(failed.is_err());
        });
        assert_eq!(
            outer,
            QueryWork {
                seq_rows: 1,
                index_probes: 1,
                heap_fetches: 1
            }
        );
        assert_eq!(capture_query_work(|| {}).1, QueryWork::default());
    }

    #[test]
    fn captures_are_thread_local() {
        let (_, outer) = capture_query_work(|| {
            record_seq_row();
            let other = std::thread::spawn(|| capture_query_work(record_index_probe).1)
                .join()
                .unwrap();
            assert_eq!(other.index_probes, 1);
            assert_eq!(other.seq_rows, 0);
        });
        assert_eq!(outer.seq_rows, 1);
        assert_eq!(outer.index_probes, 0);
    }
}
