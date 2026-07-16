//! End-to-end contract tests for Lane A conjunction index selection.
//!
//! These tests use only the public PowQL execution surface. The core property
//! is that a conjunction filter returns exactly the same rows whether or not
//! the driving index exists: adding an index is a pure performance change, so
//! the index-driven path (with a compiled residual recheck) and the plain
//! sequential-scan path must agree for every query shape.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|error| panic!("failed `{query}`: {error}"))
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

fn explain_text(engine: &mut Engine, query: &str) -> String {
    let QueryResult::Rows { rows, .. } = exec(engine, query) else {
        panic!("expected EXPLAIN rows");
    };
    rows.into_iter()
        .filter_map(|row| match row.into_iter().next() {
            Some(Value::Str(line)) => Some(line),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn insert_doc(engine: &mut Engine, id: i64, model_id: i64, published: bool, data: &str) {
    let escaped = data.replace('\\', "\\\\").replace('"', "\\\"");
    exec(
        engine,
        &format!(
            r#"insert Doc {{ id := {id}, model_id := {model_id}, is_published := {published}, data := "{escaped}" }}"#
        ),
    );
}

/// A CMS-shaped table: scalar columns plus a nested JSON document. Seeded with
/// present, missing, and JSON-null values on the indexed path.
fn seed_docs(engine: &mut Engine) {
    exec(
        engine,
        "type Doc { required id: int, model_id: int, is_published: bool, data: json }",
    );
    insert_doc(engine, 1, 1, true, r#"{"ns":{"value":"x"}}"#);
    insert_doc(engine, 2, 1, false, r#"{"ns":{"value":"y"}}"#);
    insert_doc(engine, 3, 2, true, r#"{"ns":{"value":"x"}}"#);
    insert_doc(engine, 4, 1, true, r#"{"ns":{"value":"x"}}"#);
    insert_doc(engine, 5, 1, true, r#"{"ns":{}}"#); // missing value
    insert_doc(engine, 6, 1, true, r#"{"ns":{"value":null}}"#); // JSON null
    insert_doc(engine, 7, 2, false, r#"{"ns":{"value":"z"}}"#);
    insert_doc(engine, 8, 1, true, r#"{"ns":{"value":"x"}}"#);
}

/// Every conjunction shape returns the identical row set before and after the
/// indexes exist. This is the property-style check that the index-driven path
/// (and its residual recheck) matches the sequential-scan path exactly.
#[test]
fn conjunction_results_match_with_and_without_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_docs(&mut engine);

    let queries = [
        // eq + eq (column + column)
        r#"Doc filter .model_id = 1 and .is_published = true { .id }"#,
        // eq + range (column + column)
        r#"Doc filter .model_id = 1 and .id > 3 { .id }"#,
        // path + column mix
        r#"Doc filter .data->ns->value = "x" and .model_id = 1 { .id }"#,
        // three conjuncts across path and columns
        r#"Doc filter .model_id = 1 and .is_published = true and .data->ns->value = "x" { .id }"#,
        // JSON null / missing key on the driving path
        r#"Doc filter .data->ns->value = null and .model_id = 1 { .id }"#,
        // path-driven with a range residual
        r#"Doc filter .data->ns->value = "x" and .id > 2 { .id }"#,
    ];

    // Capture the sequential-scan answers first (no indexes exist yet).
    let unindexed: Vec<Vec<i64>> = queries
        .iter()
        .map(|q| sorted_ids(exec(&mut engine, q)))
        .collect();

    exec(&mut engine, "alter Doc add index .model_id");
    exec(&mut engine, "alter Doc add index (.data->ns->value)");

    for (query, expected) in queries.iter().zip(unindexed) {
        assert_eq!(
            sorted_ids(exec(&mut engine, query)),
            expected,
            "index-driven result diverged from the sequential scan for `{query}`"
        );
    }
}

/// EXPLAIN on the driving three-conjunct shape must show the index scan under a
/// residual Filter, not a SeqScan, once the expression index exists.
#[test]
fn explain_shows_index_scan_with_residual_filter_for_conjunction() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_docs(&mut engine);

    let query =
        r#"explain Doc filter .model_id = 1 and .is_published = true and .data->ns->value = "x""#;

    // Before any index: a plain sequential scan.
    let before = explain_text(&mut engine, query);
    assert!(before.contains("SeqScan"), "{before}");
    assert!(!before.contains("ExprIndexScan"), "{before}");

    exec(&mut engine, "alter Doc add index (.data->ns->value)");

    // After: the expression index drives the scan, the rest is a residual Filter.
    let after = explain_text(&mut engine, query);
    assert!(after.contains("ExprIndexScan"), "{after}");
    assert!(after.contains("Filter"), "{after}");
    assert!(!after.contains("SeqScan"), "{after}");
    assert!(
        after.contains("path=v1:.data->\"ns\"->\"value\""),
        "residual explain should name the driving path: {after}"
    );
}

/// When a column and an expression index both apply, v0.15 ranks them by
/// estimated rows rather than conjunct order. On the `seed_docs` shape the path
/// `.data->ns->value` (6 entries over 3 distinct, est 2) is more selective than
/// `.model_id` (8 entries over 2 distinct, est 4), so the path drives whichever
/// conjunct comes first textually. Adding the path index must still never
/// change results.
#[test]
fn column_index_and_path_index_ranked_by_selectivity() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_docs(&mut engine);
    exec(&mut engine, "alter Doc add index .model_id");
    exec(&mut engine, "alter Doc add index (.data->ns->value)");

    // model_id is textually first, but the path is more selective and drives.
    let model_first = explain_text(
        &mut engine,
        r#"explain Doc filter .model_id = 1 and .data->ns->value = "x""#,
    );
    assert!(model_first.contains("ExprIndexScan"), "{model_first}");
    assert!(
        !model_first.contains("IndexScan table=Doc column=model_id"),
        "the less selective column index must not drive: {model_first}"
    );

    // Path is textually first here too, and still drives (order-independent).
    let path_first = explain_text(
        &mut engine,
        r#"explain Doc filter .data->ns->value = "x" and .model_id = 1"#,
    );
    assert!(path_first.contains("ExprIndexScan"), "{path_first}");

    // Parity: the driver choice never changes the row set.
    assert_eq!(
        sorted_ids(exec(
            &mut engine,
            r#"Doc filter .model_id = 1 and .data->ns->value = "x" { .id }"#,
        )),
        vec![1, 4, 8],
    );
}

// ── C1: cross-type (int literal vs indexed float column) parity ──────
//
// The reference `Filter(SeqScan)` path coerces an int literal to f64 when the
// column is a float (the compiled float leaf does `v as f64`), so `.f = 1`
// matches a stored `1.0`. A plain-column index stores the value under its
// declared type with a type tag, so a conjunction that drives the scan from a
// raw `Int(1)` key would miss every `Float(1.0)` row. These tests pin the
// invariant that driving from the indexed float conjunct returns exactly the
// sequential-scan answer, for select / update / delete and the range tier.

/// `F { id, f: float, b: int }`. Rows 1,2,4 store `f = 1.0`; 3,5 store `2.0`.
/// `b` mixes 1 and 2 so a residual `.b = 1` conjunct is selective.
fn seed_floats(engine: &mut Engine, with_index: bool) {
    exec(engine, "type F { required id: int, f: float, b: int }");
    exec(engine, "insert F { id := 1, f := 1.0, b := 1 }");
    exec(engine, "insert F { id := 2, f := 1.0, b := 2 }");
    exec(engine, "insert F { id := 3, f := 2.0, b := 1 }");
    exec(engine, "insert F { id := 4, f := 1.0, b := 1 }");
    exec(engine, "insert F { id := 5, f := 2.0, b := 2 }");
    if with_index {
        // Non-unique secondary indexes: the type-tagged composite key path is
        // exactly where a raw int key misses the float-typed stored keys.
        exec(engine, "alter F add index .f");
        exec(engine, "alter F add index .b");
    }
}

fn fresh_floats(with_index: bool) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_floats(&mut engine, with_index);
    (dir, engine)
}

#[test]
fn int_literal_eq_on_indexed_float_column_matches_seqscan() {
    let query = "F filter .f = 1 and .b = 1 { .id }";

    let (_d1, mut unindexed) = fresh_floats(false);
    let expected = sorted_ids(exec(&mut unindexed, query));
    assert_eq!(expected, vec![1, 4], "reference seqscan answer");

    let (_d2, mut indexed) = fresh_floats(true);
    assert_eq!(
        sorted_ids(exec(&mut indexed, query)),
        expected,
        "int-literal eq on an indexed float column diverged from the sequential scan"
    );
}

#[test]
fn int_literal_range_on_indexed_float_column_matches_seqscan() {
    // The float range `.f >= 1 and .f <= 1` drives (both bounds are int
    // literals on the float column); `.b >= 1` is the residual.
    let query = "F filter .f >= 1 and .f <= 1 and .b >= 1 { .id }";

    let (_d1, mut unindexed) = fresh_floats(false);
    let expected = sorted_ids(exec(&mut unindexed, query));
    assert_eq!(expected, vec![1, 2, 4], "reference seqscan answer");

    let (_d2, mut indexed) = fresh_floats(true);
    assert_eq!(
        sorted_ids(exec(&mut indexed, query)),
        expected,
        "int-literal range on an indexed float column diverged from the sequential scan"
    );
}

#[test]
fn int_literal_update_on_indexed_float_column_matches_seqscan() {
    let update = "F filter .f = 1 and .b = 1 update { b := 9 }";
    let probe = "F filter .b = 9 { .id }";

    let (_d1, mut unindexed) = fresh_floats(false);
    exec(&mut unindexed, update);
    let expected = sorted_ids(exec(&mut unindexed, probe));
    assert_eq!(
        expected,
        vec![1, 4],
        "reference update touched rows 1 and 4"
    );

    let (_d2, mut indexed) = fresh_floats(true);
    exec(&mut indexed, update);
    assert_eq!(
        sorted_ids(exec(&mut indexed, probe)),
        expected,
        "int-literal update over an indexed float conjunction touched the wrong rows"
    );
}

#[test]
fn int_literal_delete_on_indexed_float_column_matches_seqscan() {
    let delete = "F filter .f = 1 and .b = 1 delete";
    let probe = "F { .id }";

    let (_d1, mut unindexed) = fresh_floats(false);
    exec(&mut unindexed, delete);
    let expected = sorted_ids(exec(&mut unindexed, probe));
    assert_eq!(
        expected,
        vec![2, 3, 5],
        "reference delete removed rows 1 and 4"
    );

    let (_d2, mut indexed) = fresh_floats(true);
    exec(&mut indexed, delete);
    assert_eq!(
        sorted_ids(exec(&mut indexed, probe)),
        expected,
        "int-literal delete over an indexed float conjunction removed the wrong rows"
    );
}

// ── C2: index-driven conjunction mutations match the sequential scan ──
//
// A conjunction update/delete whose discovery scan lowered to
// `Filter(<index scan>)` (or a bare index scan) now collects its rids from the
// index and rechecks the residual, instead of the O(N*M) generic value
// rematch. These pin that the row set it touches equals the unindexed scan's
// for the range and expression-index driving shapes.

#[test]
fn range_driven_conjunction_mutation_matches_seqscan() {
    // `.f >= 1 and .f <= 1` (int bounds on the float column) drives the scan;
    // `.id >= 4` is the residual. Only row 4 (f = 1.0, id >= 4) qualifies.
    let update = "F filter .f >= 1 and .f <= 1 and .id >= 4 update { b := 7 }";
    let probe = "F filter .b = 7 { .id }";

    let (_d1, mut unindexed) = fresh_floats(false);
    exec(&mut unindexed, update);
    let expected = sorted_ids(exec(&mut unindexed, probe));
    assert_eq!(expected, vec![4], "reference range-driven update");

    let (_d2, mut indexed) = fresh_floats(true);
    exec(&mut indexed, update);
    assert_eq!(
        sorted_ids(exec(&mut indexed, probe)),
        expected,
        "range-driven conjunction update over an index touched the wrong rows"
    );

    // Delete side: remove f = 1.0 rows with id <= 2 (rows 1, 2), leaving the
    // rest untouched.
    let delete = "F filter .f >= 1 and .f <= 1 and .id <= 2 delete";
    let (_d3, mut unindexed) = fresh_floats(false);
    exec(&mut unindexed, delete);
    let expected = sorted_ids(exec(&mut unindexed, "F { .id }"));
    assert_eq!(expected, vec![3, 4, 5], "reference range-driven delete");

    let (_d4, mut indexed) = fresh_floats(true);
    exec(&mut indexed, delete);
    assert_eq!(
        sorted_ids(exec(&mut indexed, "F { .id }")),
        expected,
        "range-driven conjunction delete over an index removed the wrong rows"
    );
}

#[test]
fn path_driven_conjunction_mutation_matches_seqscan() {
    let seed = |engine: &mut Engine, with_index: bool| {
        exec(engine, "type J { required id: int, tag: int, data: json }");
        exec(
            engine,
            r#"insert J { id := 1, tag := 0, data := "{\"score\": 2}" }"#,
        );
        exec(
            engine,
            r#"insert J { id := 2, tag := 0, data := "{\"score\": 3}" }"#,
        );
        exec(
            engine,
            r#"insert J { id := 3, tag := 0, data := "{\"score\": 2}" }"#,
        );
        exec(
            engine,
            r#"insert J { id := 4, tag := 0, data := "{\"score\": 2}" }"#,
        );
        if with_index {
            exec(engine, "alter J add index (.data->score)");
        }
    };

    // `.data->score = 2` drives (expression index); `.id >= 3` is the residual.
    // Rows 3 and 4 qualify.
    let update = "J filter .data->score = 2 and .id >= 3 update { tag := 5 }";
    let probe = "J filter .tag = 5 { .id }";

    let d1 = tempfile::tempdir().unwrap();
    let mut unindexed = Engine::new(d1.path()).unwrap();
    seed(&mut unindexed, false);
    exec(&mut unindexed, update);
    let expected = sorted_ids(exec(&mut unindexed, probe));
    assert_eq!(expected, vec![3, 4], "reference path-driven update");

    let d2 = tempfile::tempdir().unwrap();
    let mut indexed = Engine::new(d2.path()).unwrap();
    seed(&mut indexed, true);
    exec(&mut indexed, update);
    assert_eq!(
        sorted_ids(exec(&mut indexed, probe)),
        expected,
        "path-driven conjunction update over an expression index touched the wrong rows"
    );

    // Delete side: remove score = 2 rows with id <= 3 (rows 1, 3).
    let delete = "J filter .data->score = 2 and .id <= 3 delete";
    let d3 = tempfile::tempdir().unwrap();
    let mut unindexed = Engine::new(d3.path()).unwrap();
    seed(&mut unindexed, false);
    exec(&mut unindexed, delete);
    let expected = sorted_ids(exec(&mut unindexed, "J { .id }"));
    assert_eq!(expected, vec![2, 4], "reference path-driven delete");

    let d4 = tempfile::tempdir().unwrap();
    let mut indexed = Engine::new(d4.path()).unwrap();
    seed(&mut indexed, true);
    exec(&mut indexed, delete);
    assert_eq!(
        sorted_ids(exec(&mut indexed, "J { .id }")),
        expected,
        "path-driven conjunction delete over an expression index removed the wrong rows"
    );
}

/// JSON-path expression indexes resolve scalars through `BTree::lookup_all`,
/// which compares raw `Value`s, so the path tier already agrees with the
/// sequential scan (a stored float only matches a float-typed path predicate,
/// exactly as the seqscan evaluates it). The C1 fix touches only plain-column
/// index keys; this pins that the path tier stays byte-for-byte in parity for
/// a conjunction that drives from the path.
#[test]
fn eq_on_indexed_json_path_matches_seqscan() {
    let queries = [
        // int path value, int literal: driven by the path index.
        r#"S filter .data->score = 2 and .id > 0 { .id }"#,
        // float path value, float literal: same-typed match on the path.
        r#"S filter .data->ratio = 1.5 and .id > 0 { .id }"#,
    ];

    let seed = |engine: &mut Engine, with_index: bool| {
        exec(engine, "type S { required id: int, data: json }");
        exec(
            engine,
            r#"insert S { id := 1, data := "{\"score\": 2, \"ratio\": 1.5}" }"#,
        );
        exec(
            engine,
            r#"insert S { id := 2, data := "{\"score\": 3, \"ratio\": 2.5}" }"#,
        );
        exec(
            engine,
            r#"insert S { id := 3, data := "{\"score\": 2, \"ratio\": 1.5}" }"#,
        );
        if with_index {
            exec(engine, "alter S add index (.data->score)");
            exec(engine, "alter S add index (.data->ratio)");
        }
    };

    let d1 = tempfile::tempdir().unwrap();
    let mut unindexed = Engine::new(d1.path()).unwrap();
    seed(&mut unindexed, false);
    let expected: Vec<_> = queries
        .iter()
        .map(|q| sorted_ids(exec(&mut unindexed, q)))
        .collect();
    assert_eq!(expected[0], vec![1, 3], "int path reference answer");
    assert_eq!(expected[1], vec![1, 3], "float path reference answer");

    let d2 = tempfile::tempdir().unwrap();
    let mut indexed = Engine::new(d2.path()).unwrap();
    seed(&mut indexed, true);
    for (query, want) in queries.iter().zip(&expected) {
        assert_eq!(
            &sorted_ids(exec(&mut indexed, query)),
            want,
            "json-path conjunction diverged from the sequential scan for `{query}`"
        );
    }
}

// ── v0.15: per-index statistics rank conjunction drivers by selectivity ──
//
// The chooser now ranks candidates by estimated rows per key (from coarse
// per-index counters) instead of tier-then-conjunct-order. These tests pin the
// S4 shape: a large CMS-shaped table whose textually-first indexed conjunct is
// unselective while a selective index exists on another conjunct.

/// 200-row CMS-shaped table. Indexed attributes span the full selectivity range:
/// `is_published` is 50/50 (est ~100), `model_id` has 50 distinct values
/// (est ~4), the JSON path `ns.value` has 2 distinct values (est ~100), and the
/// JSON path `ns.slug` is unique per row (est 1). `value` keys off `id % 3` so
/// it is not perfectly correlated with `model_id`'s groups (every model_id group
/// still contains mixed values), keeping conjunction results non-trivial.
fn seed_cms(engine: &mut Engine, with_index: bool) {
    exec(
        engine,
        "type CmsDoc { required id: int, model_id: int, is_published: bool, data: json }",
    );
    for id in 1..=200i64 {
        let model_id = id % 50;
        let published = id % 2 == 0;
        let value = if id % 3 == 0 { "a" } else { "b" };
        let data = format!(r#"{{"ns":{{"value":"{value}","slug":"s{id}"}}}}"#);
        let escaped = data.replace('\\', "\\\\").replace('"', "\\\"");
        exec(
            engine,
            &format!(
                r#"insert CmsDoc {{ id := {id}, model_id := {model_id}, is_published := {published}, data := "{escaped}" }}"#
            ),
        );
    }
    if with_index {
        exec(engine, "alter CmsDoc add index .is_published");
        exec(engine, "alter CmsDoc add index .model_id");
        exec(engine, "alter CmsDoc add index (.data->ns->value)");
        exec(engine, "alter CmsDoc add index (.data->ns->slug)");
    }
}

/// Shape inversion: the selective driver is chosen even when the unselective
/// conjunct comes first textually, in both directions (a selective column index
/// beating an unselective JSON-path index, and a selective JSON-path index
/// beating an unselective boolean column index).
#[test]
fn selective_index_drives_regardless_of_conjunct_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_cms(&mut engine, true);

    // Unselective JSON path first, selective column second: the column drives.
    let path_first = explain_text(
        &mut engine,
        r#"explain CmsDoc filter .data->ns->value = "a" and .model_id = 5"#,
    );
    assert!(
        path_first.contains("IndexScan table=CmsDoc column=model_id"),
        "selective column index should drive: {path_first}"
    );
    assert!(
        !path_first.contains("ExprIndexScan"),
        "unselective path index must not drive: {path_first}"
    );
    // Stats tokens are recomputed from the catalog: 200 entries / 50 distinct.
    assert!(
        path_first.contains("est_rows=4")
            && path_first.contains("entries=200")
            && path_first.contains("distinct=50"),
        "explain should annotate the driver's stats: {path_first}"
    );

    // Unselective boolean first, selective JSON path second: the path drives.
    let bool_first = explain_text(
        &mut engine,
        r#"explain CmsDoc filter .is_published = true and .data->ns->slug = "s6""#,
    );
    assert!(
        bool_first.contains("ExprIndexScan"),
        "selective path index should drive: {bool_first}"
    );
    assert!(
        !bool_first.contains("column=is_published"),
        "unselective boolean index must not drive: {bool_first}"
    );
    // Unique-per-row slug: 200 entries / 200 distinct, est 1.
    assert!(
        bool_first.contains("est_rows=1")
            && bool_first.contains("entries=200")
            && bool_first.contains("distinct=200"),
        "explain should annotate the driver's stats: {bool_first}"
    );
}

/// Parity: the skewed conjunctions return the identical row set with and without
/// the indexes, for selects and for update / delete mutations. A wrong estimate
/// is only ever a performance bug because the residual rechecks every row.
#[test]
fn skewed_conjunction_parity_indexed_vs_unindexed() {
    let selects = [
        // selective column driver, unselective path residual
        r#"CmsDoc filter .data->ns->value = "a" and .model_id = 5 { .id }"#,
        // selective path driver, unselective boolean residual
        r#"CmsDoc filter .is_published = true and .data->ns->slug = "s6" { .id }"#,
        // selective column driver, unselective boolean residual
        r#"CmsDoc filter .is_published = false and .model_id = 7 { .id }"#,
    ];

    let d1 = tempfile::tempdir().unwrap();
    let mut unindexed = Engine::new(d1.path()).unwrap();
    seed_cms(&mut unindexed, false);
    let expected: Vec<_> = selects
        .iter()
        .map(|q| sorted_ids(exec(&mut unindexed, q)))
        .collect();

    let d2 = tempfile::tempdir().unwrap();
    let mut indexed = Engine::new(d2.path()).unwrap();
    seed_cms(&mut indexed, true);
    for (query, want) in selects.iter().zip(&expected) {
        assert_eq!(
            &sorted_ids(exec(&mut indexed, query)),
            want,
            "skewed conjunction diverged from the sequential scan for `{query}`"
        );
    }

    // Update mutation driven by the selective path: touch exactly slug s10.
    let update = r#"CmsDoc filter .is_published = true and .data->ns->slug = "s10" update { model_id := 999 }"#;
    let probe = "CmsDoc filter .model_id = 999 { .id }";
    let d3 = tempfile::tempdir().unwrap();
    let mut unindexed = Engine::new(d3.path()).unwrap();
    seed_cms(&mut unindexed, false);
    exec(&mut unindexed, update);
    let want_update = sorted_ids(exec(&mut unindexed, probe));
    assert_eq!(want_update, vec![10], "reference update touched slug s10");
    let d4 = tempfile::tempdir().unwrap();
    let mut indexed = Engine::new(d4.path()).unwrap();
    seed_cms(&mut indexed, true);
    exec(&mut indexed, update);
    assert_eq!(
        sorted_ids(exec(&mut indexed, probe)),
        want_update,
        "index-driven skewed update touched the wrong rows"
    );

    // Delete mutation driven by the selective column: remove model_id = 5 rows
    // that also carry value "a" (a residual recheck on the path).
    let delete = r#"CmsDoc filter .data->ns->value = "a" and .model_id = 5 delete"#;
    let d5 = tempfile::tempdir().unwrap();
    let mut unindexed = Engine::new(d5.path()).unwrap();
    seed_cms(&mut unindexed, false);
    exec(&mut unindexed, delete);
    let want_delete = sorted_ids(exec(&mut unindexed, "CmsDoc { .id }"));
    let d6 = tempfile::tempdir().unwrap();
    let mut indexed = Engine::new(d6.path()).unwrap();
    seed_cms(&mut indexed, true);
    exec(&mut indexed, delete);
    assert_eq!(
        sorted_ids(exec(&mut indexed, "CmsDoc { .id }")),
        want_delete,
        "index-driven skewed delete removed the wrong rows"
    );
}

/// Stability: the plan cache stores pre-lowering plans, so an identical query
/// re-lowers against current stats on every execution. Creating a more selective
/// index between two runs of the same query changes the driver on the cache-hit
/// path.
#[test]
fn plan_cache_hit_relowers_with_current_stats() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    seed_cms(&mut engine, false);
    // Only the unselective boolean is indexed at first, so it is the sole eq
    // candidate and drives.
    exec(&mut engine, "alter CmsDoc add index .is_published");

    let query = r#"explain CmsDoc filter .is_published = true and .model_id = 5"#;
    let before = explain_text(&mut engine, query);
    assert!(
        before.contains("IndexScan table=CmsDoc column=is_published"),
        "boolean index should drive while it is the only candidate: {before}"
    );

    // Add the far more selective column index. The cached (pre-lowering) plan is
    // re-lowered on the next execution and now picks the selective driver.
    exec(&mut engine, "alter CmsDoc add index .model_id");
    let after = explain_text(&mut engine, query);
    assert!(
        after.contains("IndexScan table=CmsDoc column=model_id"),
        "the newly-selective column index should drive after re-lowering: {after}"
    );
    assert!(
        !after.contains("column=is_published"),
        "the unselective boolean index must no longer drive: {after}"
    );
}
