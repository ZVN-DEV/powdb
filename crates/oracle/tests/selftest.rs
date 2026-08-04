//! Proof that the oracle can fail.
//!
//! A differential harness that always agrees is worthless and looks identical
//! to one that works. These tests plant a known difference between the two
//! stores and require the machinery to find it end to end: real PowDB engine,
//! real SQLite connection, real query, real comparison. Each one is paired
//! with a control where the two stores agree, so "always reports a
//! divergence" fails too.

use powdb_oracle::compare::diff;
use powdb_oracle::engines::powdb::Powdb;
use powdb_oracle::engines::sqlite::Sqlite;
use powdb_oracle::fixture::{FixRow, Fixture, Load};
use powdb_oracle::model::{column, ColType, Kind, Lit, COLUMNS, LIKE_COLUMN, TABLE};

fn at(name: &str) -> usize {
    COLUMNS
        .iter()
        .position(|c| c.name == name)
        .expect("known column")
}

fn row(id: i64, i: Option<i64>, s: Option<&str>) -> FixRow {
    let mut r: FixRow = COLUMNS.iter().map(|_| Lit::Null).collect();
    r[at("id")] = Lit::Int(id);
    if let Some(v) = i {
        r[at("i")] = Lit::Int(v);
    }
    if let Some(v) = s {
        r[at(LIKE_COLUMN)] = Lit::Str(v.to_string());
    }
    r
}

fn fixture(rows: Vec<FixRow>) -> Fixture {
    Fixture {
        name: "selftest",
        rows,
        indexes: &[],
        load: Load::Powql,
        sqlite_representable: true,
    }
}

fn star_kinds() -> Vec<Kind> {
    COLUMNS.iter().map(|c| Kind::Col(c.ty)).collect()
}

/// Run `SELECT *` against a PowDB loaded with `left` and a SQLite loaded with
/// `right`, and return the reported difference.
fn compare_bodies(left: Vec<FixRow>, right: Vec<FixRow>) -> Option<String> {
    let mut powdb = Powdb::open(&fixture(left)).expect("powdb");
    let sqlite = Sqlite::open(&fixture(right)).expect("sqlite");
    let a = powdb.powql(&format!("{TABLE} order .id asc"));
    let b = sqlite.query(
        &format!("SELECT * FROM {TABLE} ORDER BY id ASC"),
        &[],
        &star_kinds(),
    );
    diff(&a, &b, true)
}

#[test]
fn identical_bodies_agree() {
    let body = vec![row(1, Some(5), Some("a")), row(2, None, Some("b"))];
    assert_eq!(
        compare_bodies(body.clone(), body),
        None,
        "the control must agree, otherwise every other assertion here is noise"
    );
}

#[test]
fn a_missing_row_is_caught() {
    let full = vec![row(1, Some(5), Some("a")), row(2, Some(6), Some("b"))];
    let short = vec![row(1, Some(5), Some("a"))];
    assert!(compare_bodies(short, full).is_some());
}

#[test]
fn a_single_wrong_value_is_caught() {
    let a = vec![row(1, Some(5), Some("a"))];
    let b = vec![row(1, Some(6), Some("a"))];
    assert!(compare_bodies(a, b).is_some());
}

/// The case a row-count comparison passes and a full-result comparison must
/// not: same number of rows, different contents.
#[test]
fn same_row_count_with_different_rows_is_caught() {
    let a = vec![row(1, Some(5), Some("a")), row(2, Some(6), Some("b"))];
    let b = vec![row(1, Some(5), Some("a")), row(2, Some(6), Some("B"))];
    let detail = compare_bodies(a, b).expect("must be caught");
    assert!(detail.contains("column `s`"), "unexpected detail: {detail}");
}

#[test]
fn a_null_where_a_value_belongs_is_caught() {
    let a = vec![row(1, Some(5), None)];
    let b = vec![row(1, Some(5), Some(""))];
    assert!(
        compare_bodies(a, b).is_some(),
        "NULL must not pass for the empty string"
    );
}

/// Every column type must be able to carry a difference through the whole
/// pipeline. A type whose difference is invisible is a hole in the oracle.
#[test]
fn a_difference_in_every_column_type_is_caught() {
    fn value(ty: ColType, second: bool) -> Lit {
        match (ty, second) {
            (ColType::Int, false) => Lit::Int(0),
            (ColType::Int, true) => Lit::Int(1),
            (ColType::Float, false) => Lit::Float(0.0),
            // Deliberately not -0.0: SQLite stores a REAL whose value is an
            // exact integer through an integer serial type, so a bound -0.0
            // comes back as +0.0 and SQLite genuinely holds the same value as
            // the other side. That is the reason -0.0 lives in the
            // PowDB-only `nonfinite` fixture instead.
            (ColType::Float, true) => Lit::Float(1.0),
            (ColType::Bool, false) => Lit::Bool(false),
            (ColType::Bool, true) => Lit::Bool(true),
            (ColType::Str, false) => Lit::Str("x".into()),
            (ColType::Str, true) => Lit::Str("y".into()),
            (ColType::DateTime, false) => Lit::DateTime(0),
            (ColType::DateTime, true) => Lit::DateTime(1),
            (ColType::Uuid, false) => Lit::Uuid([0u8; 16]),
            // Differs only in the last byte, which a truncated key comparison
            // would miss.
            (ColType::Uuid, true) => {
                let mut u = [0u8; 16];
                u[15] = 1;
                Lit::Uuid(u)
            }
            (ColType::Bytes, false) => Lit::Bytes(vec![1, 2]),
            (ColType::Bytes, true) => Lit::Bytes(vec![1, 2, 3]),
            (ColType::Json, false) => Lit::Json("{\"a\":1}".into()),
            (ColType::Json, true) => Lit::Json("{\"a\":2}".into()),
        }
    }
    let base: FixRow = COLUMNS
        .iter()
        .map(|c| {
            if c.name == "id" {
                Lit::Int(1)
            } else {
                value(c.ty, false)
            }
        })
        .collect();

    for (idx, col) in COLUMNS.iter().enumerate() {
        let mut other = base.clone();
        other[idx] = if col.name == "id" {
            Lit::Int(2)
        } else {
            value(col.ty, true)
        };
        assert!(
            compare_bodies(vec![base.clone()], vec![other]).is_some(),
            "a difference in column `{}` ({}) was invisible to the oracle",
            col.name,
            col.ty.powql()
        );
    }
    // And the lookup helper the fixtures rely on really resolves.
    assert!(column("id").is_some());
    assert!(column("not_a_column").is_none());
}

/// A difference that only shows up beyond the first row must still be caught:
/// a comparison that only looked at row 0 would pass this and must not.
#[test]
fn a_difference_deep_in_the_result_is_caught() {
    let mut a: Vec<FixRow> = (1..=40).map(|id| row(id, Some(id), Some("s"))).collect();
    let b = a.clone();
    a[37] = row(38, Some(999), Some("s"));
    assert!(compare_bodies(a, b).is_some());
}

/// The two PowDB frontends are compared to each other as well. This proves
/// that leg is live rather than silently comparing a result with itself.
#[test]
fn the_powql_versus_powdb_sql_leg_is_live() {
    let mut powdb = Powdb::open(&fixture(vec![row(1, Some(5), Some("a"))])).expect("powdb");
    let same = diff(
        &powdb.powql(&format!("{TABLE} {{ .id }}")),
        &powdb.sql(&format!("SELECT id FROM {TABLE}")),
        false,
    );
    assert_eq!(same, None, "equivalent queries must agree");

    // Deliberately inequivalent queries: if this leg were comparing a result
    // with itself, this would also come back equal.
    let differ = diff(
        &powdb.powql(&format!("{TABLE} {{ .id }}")),
        &powdb.sql(&format!("SELECT id FROM {TABLE} WHERE id > 100")),
        false,
    );
    assert!(differ.is_some());
}
