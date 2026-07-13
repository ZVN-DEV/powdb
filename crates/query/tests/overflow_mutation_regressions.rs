//! Regression tests for the v1-only-code-fed-v2-rows defect class (v0.11
//! overflow pages). Each test encodes a defect reproduced against the engine:
//!
//! - P0  : in-place UPDATE fast paths corrupt a spilled (v2) row.
//! - P0-2: updating a sibling / shrinking a var column of a spilled row panics.
//! - P0-3: index / point lookups return NULL for a spilled column.
//! - P0-4: values >= 64KB were silently truncated (u16 var-offset wrap) on read.
//! - P1  : fused delete/update predicates evaluated a spilled column as Empty.
//!
//! The unifying invariant: no read/mutation path may interpret a v2 row with
//! v1 layout math, and no value is ever silently truncated.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_engine(name: &str) -> (Engine, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "powdb_ovf_mut_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(&dir).unwrap();
    (engine, dir)
}

fn rows(res: QueryResult) -> Vec<Vec<Value>> {
    match res {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn one_row(res: QueryResult) -> Vec<Value> {
    let mut r = rows(res);
    assert_eq!(r.len(), 1, "expected exactly one row");
    r.pop().unwrap()
}

fn str_len(v: &Value) -> usize {
    match v {
        Value::Str(s) => s.len(),
        other => panic!("expected Str, got {other:?}"),
    }
}

// ── P0: update a fixed column on an already-spilled row ────────────────────

#[test]
fn p0_update_fixed_col_on_spilled_row_preserves_every_column() {
    let (mut engine, dir) = temp_engine("fixed");
    engine
        .execute_powql("type T { required id: int, hits: int, b: str }")
        .unwrap();
    let body = "p".repeat(20_000); // spills (b out of line, id+hits inline)
    engine
        .execute_powql(&format!(
            r#"insert T {{ id := 1, hits := 0, b := "{body}" }}"#
        ))
        .unwrap();

    // Reviewer repro: this corrupted id to 0x0500_0000_0000_0001 and left
    // hits = 0, because the byte-patch wrote at v1 offsets on a v2 row.
    engine
        .execute_powql("T filter .id = 1 update { hits := 5 }")
        .unwrap();

    let row = one_row(
        engine
            .execute_powql("T filter .id = 1 { .id, .hits, .b }")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Int(1), "id must be intact");
    assert_eq!(row[1], Value::Int(5), "hits must be updated");
    assert_eq!(
        str_len(&row[2]),
        20_000,
        "spilled body must survive byte-exact"
    );
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

// ── P0c: same via the FUSED path (Filter(SeqScan), compound int predicate) ──

#[test]
fn p0_fused_update_fixed_col_on_spilled_row() {
    let (mut engine, dir) = temp_engine("fused");
    engine
        .execute_powql("type T { required id: int, hits: int, b: str }")
        .unwrap();
    let body = "p".repeat(20_000);
    engine
        .execute_powql(&format!(
            r#"insert T {{ id := 1, hits := 0, b := "{body}" }}"#
        ))
        .unwrap();

    // A compound AND predicate does not fold to IndexScan/RangeScan, so the
    // plan is Update(Filter(SeqScan)) and the fused byte-patch path would fire.
    engine
        .execute_powql("T filter .id > 0 and .hits = 0 update { hits := 7 }")
        .unwrap();

    let row = one_row(
        engine
            .execute_powql("T filter .id = 1 { .id, .hits, .b }")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Int(1));
    assert_eq!(row[1], Value::Int(7));
    assert_eq!(str_len(&row[2]), 20_000);
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

// ── P0-2: update / shrink a SIBLING var column of a spilled row (panic bug) ──

#[test]
fn p0_2_update_sibling_var_col_of_spilled_row_no_panic() {
    let (mut engine, dir) = temp_engine("sibling");
    engine
        .execute_powql("type S { required k: int, other: str, b: str }")
        .unwrap();
    let body = "z".repeat(20_000);
    engine
        .execute_powql(&format!(
            r#"insert S {{ k := 1, other := "original_value", b := "{body}" }}"#
        ))
        .unwrap();

    // Reviewer repro: this panicked in patch_var_column_in_place (v1 offsets on
    // a v2 row). It must now update `other` and leave the spilled `b` intact.
    engine
        .execute_powql(r#"S filter .k = 1 update { other := "changed" }"#)
        .unwrap();

    let row = one_row(
        engine
            .execute_powql("S filter .k = 1 { .k, .other, .b }")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Int(1));
    assert_eq!(row[1], Value::Str("changed".into()));
    assert_eq!(
        str_len(&row[2]),
        20_000,
        "spilled sibling must be untouched"
    );
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn p0_2_shrink_spilled_var_col_to_inline() {
    let (mut engine, dir) = temp_engine("shrink");
    engine
        .execute_powql("type S { required k: int, b: str }")
        .unwrap();
    engine
        .execute_powql(&format!(
            r#"insert S {{ k := 1, b := "{}" }}"#,
            "z".repeat(20_000)
        ))
        .unwrap();

    // Spilled -> small (fits inline): must not panic and must read back exactly.
    engine
        .execute_powql(r#"S filter .k = 1 update { b := "tiny" }"#)
        .unwrap();

    let row = one_row(engine.execute_powql("S filter .k = 1 { .k, .b }").unwrap());
    assert_eq!(row[0], Value::Int(1));
    assert_eq!(row[1], Value::Str("tiny".into()));
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

// ── P0 durability: the corrected update survives a reopen / WAL replay ──────

#[test]
fn p0_updated_spilled_row_survives_reopen() {
    let (mut engine, dir) = temp_engine("durable");
    engine
        .execute_powql("type T { required id: int, hits: int, b: str }")
        .unwrap();
    engine
        .execute_powql(&format!(
            r#"insert T {{ id := 1, hits := 0, b := "{}" }}"#,
            "p".repeat(20_000)
        ))
        .unwrap();
    engine
        .execute_powql("T filter .id = 1 update { hits := 9 }")
        .unwrap();
    engine.execute_powql("count(T)").unwrap(); // group-commit the WAL
    drop(engine);

    let mut engine = Engine::new(&dir).expect("reopen after spilled update");
    let row = one_row(
        engine
            .execute_powql("T filter .id = 1 { .id, .hits, .b }")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Int(1));
    assert_eq!(row[1], Value::Int(9));
    assert_eq!(str_len(&row[2]), 20_000);
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

// ── P1: fused delete/update predicates evaluated a spilled column as Empty ──

#[test]
fn p1_is_null_delete_does_not_delete_spilled_nonnull_rows() {
    let (mut engine, dir) = temp_engine("isnull");
    engine
        .execute_powql("type D { required id: int, body: str }")
        .unwrap();
    engine
        .execute_powql(&format!(
            r#"insert D {{ id := 1, body := "{}" }}"#,
            "b".repeat(20_000)
        ))
        .unwrap(); // spilled, NON-null
    engine
        .execute_powql(r#"insert D { id := 2, body := "small" }"#)
        .unwrap(); // inline, non-null
    engine.execute_powql("insert D { id := 3 }").unwrap(); // body null

    // Reviewer repro: this DELETED id=1 (spilled body read as Empty). It must
    // delete ONLY id=3.
    engine
        .execute_powql("D filter .body is null delete")
        .unwrap();

    let mut ids: Vec<i64> = rows(engine.execute_powql("D { .id }").unwrap())
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(v) => v,
            _ => panic!(),
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2], "only the truly-null row (id=3) is deleted");
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn p1_is_not_null_matches_spilled_and_inline() {
    let (mut engine, dir) = temp_engine("notnull");
    engine
        .execute_powql("type D { required id: int, body: str }")
        .unwrap();
    engine
        .execute_powql(&format!(
            r#"insert D {{ id := 1, body := "{}" }}"#,
            "b".repeat(20_000)
        ))
        .unwrap();
    engine
        .execute_powql(r#"insert D { id := 2, body := "small" }"#)
        .unwrap();
    engine.execute_powql("insert D { id := 3 }").unwrap();

    let n = rows(
        engine
            .execute_powql("D filter .body is not null { .id }")
            .unwrap(),
    )
    .len();
    assert_eq!(n, 2, "both the spilled and inline non-null rows match");
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn p1_exact_equality_matches_the_spilled_row() {
    let (mut engine, dir) = temp_engine("eq");
    engine
        .execute_powql("type D { required id: int, body: str }")
        .unwrap();
    let big = "Z".repeat(5000); // spills, still exact-matchable
    engine
        .execute_powql(&format!(r#"insert D {{ id := 1, body := "{big}" }}"#))
        .unwrap();
    engine
        .execute_powql(r#"insert D { id := 2, body := "small" }"#)
        .unwrap();

    // select
    let sel = rows(
        engine
            .execute_powql(&format!(r#"D filter .body = "{big}" {{ .id }}"#))
            .unwrap(),
    );
    assert_eq!(
        sel.len(),
        1,
        "equality on a spilled value matches exactly one row"
    );
    assert_eq!(sel[0][0], Value::Int(1));

    // delete
    engine
        .execute_powql(&format!(r#"D filter .body = "{big}" delete"#))
        .unwrap();
    let remaining: Vec<i64> = rows(engine.execute_powql("D { .id }").unwrap())
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(v) => v,
            _ => panic!(),
        })
        .collect();
    assert_eq!(
        remaining,
        vec![2],
        "only the spilled row is deleted by exact match"
    );
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

// ── P0-3: point / index lookup returns the spilled value, not NULL ──────────

#[test]
fn p0_3_point_lookup_returns_spilled_value() {
    let (mut engine, dir) = temp_engine("point");
    engine
        .execute_powql("type P { unique auto id: int, v: str }")
        .unwrap();
    engine
        .execute_powql(&format!(r#"insert P {{ v := "{}" }}"#, "q".repeat(5000)))
        .unwrap();

    // Indexed point lookup (the IndexScan path that bypassed reassembly).
    let row = one_row(
        engine
            .execute_powql("P filter .id = 1 { .id, .v }")
            .unwrap(),
    );
    assert_eq!(row[0], Value::Int(1));
    assert_eq!(
        str_len(&row[1]),
        5000,
        "point lookup must reassemble the spilled value"
    );

    // And a full-row point lookup projection.
    let full = one_row(engine.execute_powql("P filter .id = 1").unwrap());
    assert_eq!(str_len(&full[1]), 5000);
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}

// ── P0-4: values >= 64KB round-trip byte-exact (no u16 wrap) ────────────────

#[test]
fn p0_4_large_values_roundtrip_byte_exact_across_read_paths() {
    let (mut engine, dir) = temp_engine("huge");
    engine
        .execute_powql("type Big { unique auto id: int, v: str }")
        .unwrap();

    // Straddle the u16 boundary and go well past it.
    let sizes = [65_535usize, 65_536, 65_537, 70_000, 100_000, 1_000_000];
    for (i, &n) in sizes.iter().enumerate() {
        let want = i as i64 + 1;
        engine
            .execute_powql(&format!(r#"insert Big {{ v := "{}" }}"#, "z".repeat(n)))
            .unwrap();

        // Point lookup (indexed IndexScan path).
        let row = one_row(
            engine
                .execute_powql(&format!("Big filter .id = {want} {{ .v }}"))
                .unwrap(),
        );
        assert_eq!(
            str_len(&row[0]),
            n,
            "point-lookup projection wrapped at size {n}"
        );

        // length() must agree with the returned byte count.
        let lrow = one_row(
            engine
                .execute_powql(&format!("Big filter .id = {want} {{ length(.v) }}"))
                .unwrap(),
        );
        assert_eq!(
            lrow[0],
            Value::Int(n as i64),
            "length() disagrees at size {n}"
        );
    }

    // Seq-scan projection over all rows must return every value at full length.
    let all = rows(engine.execute_powql("Big { .id, .v }").unwrap());
    assert_eq!(all.len(), sizes.len());
    for r in &all {
        let id = match r[0] {
            Value::Int(v) => v as usize,
            _ => panic!(),
        };
        assert_eq!(
            str_len(&r[1]),
            sizes[id - 1],
            "seqscan projection wrapped for id {id}"
        );
    }

    // Equality filter over a large (>= 64KB) value must match exactly.
    let big = "z".repeat(70_000);
    let hits = rows(
        engine
            .execute_powql(&format!(r#"Big filter .v = "{big}" {{ .id }}"#))
            .unwrap(),
    );
    assert_eq!(
        hits.len(),
        1,
        "equality on a 70KB value must match exactly one row"
    );
    assert_eq!(hits[0][0], Value::Int(4));
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
}
