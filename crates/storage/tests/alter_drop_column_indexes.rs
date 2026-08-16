//! `alter ... drop <column>` must not abort the process, and must not leave a
//! data directory that no later process can open.
//!
//! The rewrite that follows a drop rebuilds every secondary index from the
//! rewritten heap, and it did so using the `col_idx` each index carried
//! *before* the drop. Rows are one column shorter afterwards and the surviving
//! columns shift down, so two things went wrong at once: the dropped column's
//! own index entry was never removed, and every index whose column sat after
//! the dropped one pointed one slot too far right. Both ended in
//! `row[col_idx]` indexing past the end.
//!
//! That is not a normal panic. The crate is built `panic = "abort"`, so it is a
//! SIGABRT, and it fires *before* the catalog is persisted while the
//! `DdlDropColumn` WAL record is already durable. Every subsequent open replays
//! the record and aborts in the same place, so a single `alter` permanently
//! bricks the directory: a supervised server restart-loops forever on data that
//! is otherwise intact.
//!
//! Each test here reopens the catalog in a separate step, because an in-process
//! assertion alone would not have caught the part that actually hurts.

use powdb_storage::catalog::Catalog;
use powdb_storage::types::{ColumnDef, Row, Schema, TypeId, Value};
use std::path::{Path, PathBuf};

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "powdb_alter_drop_idx_{name}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn col(name: &str, type_id: TypeId, position: u16) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        type_id,
        required: false,
        position,
    }
}

/// `id`, `label`, `note` — so a drop can be tested before, at, and after an
/// indexed column.
fn three_col_schema() -> Schema {
    Schema {
        table_name: "T".into(),
        columns: vec![
            col("id", TypeId::Int, 0),
            col("label", TypeId::Str, 1),
            col("note", TypeId::Str, 2),
        ],
    }
}

fn row3(id: i64, label: &str, note: &str) -> Row {
    vec![
        Value::Int(id),
        Value::Str(label.into()),
        Value::Str(note.into()),
    ]
}

/// Read a whole table back as rows, from a freshly opened catalog.
fn reopen_and_scan(dir: &Path, table: &str) -> Vec<Vec<Value>> {
    let cat = Catalog::open(dir).expect("reopen after drop must succeed");
    cat.scan(table)
        .expect("table must still be readable")
        .map(|(_, row)| row)
        .collect()
}

#[test]
fn dropping_an_indexed_column_leaves_the_directory_openable() {
    let dir = fresh_dir("drop_indexed");
    {
        let mut cat = Catalog::create(&dir).unwrap();
        cat.create_table(three_col_schema()).unwrap();
        cat.create_index_unique("T", "id", true).unwrap();
        cat.create_index_unique("T", "label", true).unwrap();
        for i in 0..20i64 {
            cat.insert("T", &row3(i, &format!("l{i}"), &format!("n{i}")))
                .unwrap();
        }
        // `label` is the *second* indexed column, so its own entry must go and
        // nothing must be left pointing at slot 1 of a now-2-wide row.
        cat.alter_table_drop_column("T", "label").unwrap();
    }

    let rows = reopen_and_scan(&dir, "T");
    assert_eq!(rows.len(), 20, "every row must survive the drop");
    for row in &rows {
        assert_eq!(row.len(), 2, "rows must be 2 columns wide after the drop");
    }

    // The surviving index must still answer, and answer correctly.
    let cat = Catalog::open(&dir).unwrap();
    for i in 0..20i64 {
        let hit = cat
            .index_lookup("T", "id", &Value::Int(i))
            .expect("id index must survive");
        assert!(hit.is_some(), "id={i} must still be findable via its index");
    }
    assert!(
        !cat.get_table("T").unwrap().has_index("label"),
        "the dropped column's index entry must be gone, not merely unused"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dropping_a_column_before_an_indexed_one_keeps_that_index_correct() {
    let dir = fresh_dir("drop_before_indexed");
    {
        let mut cat = Catalog::create(&dir).unwrap();
        cat.create_table(three_col_schema()).unwrap();
        // Only `note` (slot 2) is indexed. Dropping `label` (slot 1) shifts it
        // to slot 1, so an unremapped index reads the wrong column.
        cat.create_index_unique("T", "note", true).unwrap();
        for i in 0..20i64 {
            cat.insert("T", &row3(i, &format!("l{i}"), &format!("n{i}")))
                .unwrap();
        }
        cat.alter_table_drop_column("T", "label").unwrap();
    }

    let cat = Catalog::open(&dir).unwrap();
    for i in 0..20i64 {
        let key = Value::Str(format!("n{i}"));
        let hit = cat
            .index_lookup("T", "note", &key)
            .expect("note index must survive the shift");
        assert!(
            hit.is_some(),
            "note=n{i} must be findable; a stale col_idx would have indexed the wrong column"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dropping_the_only_column_leaves_the_directory_openable() {
    let dir = fresh_dir("drop_only_column");
    {
        let mut cat = Catalog::create(&dir).unwrap();
        cat.create_table(Schema {
            table_name: "T".into(),
            columns: vec![col("id", TypeId::Int, 0)],
        })
        .unwrap();
        cat.create_index_unique("T", "id", true).unwrap();
        for i in 0..5i64 {
            cat.insert("T", &vec![Value::Int(i)]).unwrap();
        }
        // Zero-width rows are the degenerate end of the same bug: the index
        // rebuild read row[0] of a row with no columns at all.
        cat.alter_table_drop_column("T", "id").unwrap();
    }

    let rows = reopen_and_scan(&dir, "T");
    assert_eq!(rows.len(), 5, "rows must survive even when nothing is left");
    for row in &rows {
        assert!(row.is_empty(), "rows must be zero columns wide");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dropping_an_indexed_column_removes_its_index_file() {
    let dir = fresh_dir("drop_removes_idx_file");
    let idx_path = dir.join("T_label.idx");
    {
        let mut cat = Catalog::create(&dir).unwrap();
        cat.create_table(three_col_schema()).unwrap();
        cat.create_index_unique("T", "label", true).unwrap();
        for i in 0..5i64 {
            cat.insert("T", &row3(i, &format!("l{i}"), &format!("n{i}")))
                .unwrap();
        }
        assert!(idx_path.exists(), "the index file must exist to begin with");
        cat.alter_table_drop_column("T", "label").unwrap();
    }

    assert!(
        !idx_path.exists(),
        "the dropped column's .idx file must be removed, not orphaned next to \
         a table that no longer has the column"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
