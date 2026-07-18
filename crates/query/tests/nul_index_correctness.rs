//! End-to-end correctness of non-unique column indexes over string values that
//! contain an embedded NUL byte (v0.16 Workstream 1).
//!
//! Before v0.16, a non-unique index encoded a `Str` value as raw bytes plus a
//! single 0x00 terminator, then the 8-byte rid. A value like "A\0" produced
//! composite keys that interleaved with "A"'s keys, so indexed equality could
//! return rows belonging to a different value. The property under test: an
//! indexed equality filter returns exactly the same rows as the plain
//! sequential scan, for every string value including embedded-NUL ones.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|error| panic!("failed `{}`: {error}", query.escape_debug()))
}

fn sorted_ids(result: QueryResult) -> Vec<i64> {
    let QueryResult::Rows { rows, columns } = result else {
        panic!("expected rows");
    };
    let id_col = columns
        .iter()
        .position(|c| c == "id")
        .expect("id column present");
    let mut ids: Vec<i64> = rows
        .into_iter()
        .map(|row| match &row[id_col] {
            Value::Int(id) => *id,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// Seed a table of (id, name) rows. The lexer copies string-literal bytes
/// verbatim, so a real NUL character embedded in the query text lands in the
/// stored value; PowQL has no `\0` escape, so this is how NUL gets in.
fn seed(engine: &mut Engine) {
    exec(engine, "type Doc { required id: int, name: str }");
    let rows: &[(i64, &str)] = &[
        (1, "A"),
        (2, "A\u{0}"),
        (3, "A"),
        (4, "A\u{0}\u{0}"),
        (5, "AB"),
        (6, "A\u{0}B"),
        (7, "B"),
        (8, ""),
    ];
    for (id, name) in rows {
        exec(
            engine,
            &format!(r#"insert Doc {{ id := {id}, name := "{name}" }}"#),
        );
    }
}

/// Indexed equality must agree with the sequential-scan answer for every
/// distinct value, embedded NUL included.
#[test]
fn indexed_equality_matches_seqscan_for_nul_strings() {
    let probes: &[&str] = &["A", "A\u{0}", "A\u{0}\u{0}", "AB", "A\u{0}B", "B", ""];

    // Sequential-scan ground truth (no index yet).
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed(&mut engine);
    let ground_truth: Vec<Vec<i64>> = probes
        .iter()
        .map(|name| {
            sorted_ids(exec(
                &mut engine,
                &format!(r#"Doc filter .name = "{name}" {{ .id }}"#),
            ))
        })
        .collect();

    // "A" must not include the "A\0" rows (ids 2, 4, 6) under the seq scan.
    assert_eq!(ground_truth[0], vec![1, 3], "seqscan sanity for \"A\"");

    // Now add the non-unique index and re-run: results must be identical.
    exec(&mut engine, "alter Doc add index .name");
    for (name, expected) in probes.iter().zip(&ground_truth) {
        let got = sorted_ids(exec(
            &mut engine,
            &format!(r#"Doc filter .name = "{name}" {{ .id }}"#),
        ));
        assert_eq!(
            &got,
            expected,
            "indexed equality diverged from seqscan for {:?}",
            name.escape_debug().to_string()
        );
    }
}

/// Index-driven delete and update must touch only the matching value's rows.
/// Deleting "A\0" must leave every "A" row intact, and updating an "A" row must
/// not disturb the "A\0" rows.
#[test]
fn indexed_mutations_respect_nul_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed(&mut engine);
    exec(&mut engine, "alter Doc add index .name");

    // Delete every "A\0" row (ids 2, 4 have name "A\0" / "A\0\0"; only id 2 is
    // exactly "A\0"). Deleting exactly "A\0" must remove only id 2.
    exec(&mut engine, "Doc filter .name = \"A\u{0}\" delete");
    assert_eq!(
        sorted_ids(exec(&mut engine, r#"Doc filter .name = "A" { .id }"#)),
        vec![1, 3],
        "\"A\" rows survive the \"A\\0\" delete"
    );
    assert_eq!(
        sorted_ids(exec(&mut engine, "Doc filter .name = \"A\u{0}\" { .id }")),
        Vec::<i64>::new(),
        "the \"A\\0\" row is gone"
    );
    assert_eq!(
        sorted_ids(exec(
            &mut engine,
            "Doc filter .name = \"A\u{0}\u{0}\" { .id }"
        )),
        vec![4],
        "\"A\\0\\0\" is a different value and untouched"
    );

    // Update an "A" row's name to "Z"; the "A\0\0" row must be undisturbed.
    exec(&mut engine, r#"Doc filter .id = 1 update { name := "Z" }"#);
    assert_eq!(
        sorted_ids(exec(&mut engine, r#"Doc filter .name = "A" { .id }"#)),
        vec![3],
        "only id 1 left \"A\""
    );
    assert_eq!(
        sorted_ids(exec(&mut engine, r#"Doc filter .name = "Z" { .id }"#)),
        vec![1],
    );
    assert_eq!(
        sorted_ids(exec(
            &mut engine,
            "Doc filter .name = \"A\u{0}\u{0}\" { .id }"
        )),
        vec![4],
        "\"A\\0\\0\" still intact after the update"
    );
}
