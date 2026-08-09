//! Seeded data generation.
//!
//! A fixture is a named table body plus the index set to build over it. The
//! same fixture body is loaded into PowDB and into SQLite from the *same*
//! [`Lit`] values, so any disagreement between the two stores is itself a
//! finding rather than a setup artefact.

use crate::model::{ColType, Column, Lit, COLUMNS, LIKE_COLUMN, LIKE_META_COLUMN};
use crate::rng::Rng;

/// One fixture row: one [`Lit`] per column of [`COLUMNS`], in schema order.
pub type FixRow = Vec<Lit>;

/// How a fixture's rows reach PowDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    /// Through `insert T { .. }`, so the PowQL literal path is under test too.
    Powql,
    /// Through the storage-level `Table::insert` API.
    ///
    /// Used only for values PowQL has no literal for at all (NaN, the
    /// infinities). Such fixtures are PowDB-only: SQLite cannot hold NaN
    /// either (it silently stores NULL), so there is no third opinion to have.
    StorageApi,
}

/// A named, fully determined table body.
pub struct Fixture {
    pub name: &'static str,
    pub rows: Vec<FixRow>,
    /// Columns to build a B+tree index over. Index scans and sequential scans
    /// must return the same rows; running the identical body twice, once
    /// indexed, is how the oracle sees it if they do not.
    pub indexes: &'static [&'static str],
    pub load: Load,
    /// False when SQLite cannot hold the body faithfully, in which case the
    /// case runs two-way (PowQL vs PowDB-SQL) instead of three-way. Recorded
    /// and reported, never quietly skipped.
    pub sqlite_representable: bool,
}

fn null_row(id: i64) -> FixRow {
    let mut row: FixRow = COLUMNS.iter().map(|_| Lit::Null).collect();
    row[0] = Lit::Int(id);
    row
}

fn set(row: &mut FixRow, name: &str, v: Lit) {
    let idx = COLUMNS
        .iter()
        .position(|c| c.name == name)
        .unwrap_or_else(|| panic!("unknown fixture column {name}"));
    row[idx] = v;
}

// ---------------------------------------------------------------------------
// Boundary pools. Every entry is a value that has historically broken a
// database somewhere: the extremes of the domain, the identity elements, and
// the encodings that need more than one byte.
// ---------------------------------------------------------------------------

fn int_pool() -> Vec<i64> {
    vec![
        0,
        1,
        -1,
        2,
        -2,
        42,
        i64::MIN,
        i64::MAX,
        i64::MIN + 1,
        i64::MAX - 1,
        // Either side of the f64 exact-integer boundary, where a careless
        // int->float promotion in a comparison loses the low bit.
        9_007_199_254_740_992,
        9_007_199_254_740_993,
        -9_007_199_254_740_993,
        4_294_967_296,
        -4_294_967_296,
    ]
}

fn float_pool() -> Vec<f64> {
    vec![
        0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        std::f64::consts::PI,
        f64::MAX,
        f64::MIN,
        f64::MIN_POSITIVE,
        f64::EPSILON,
        -f64::EPSILON,
        1e15,
        -1e15,
        // Exactly representable, and the value an int/float comparison bug
        // most often gets wrong.
        9_007_199_254_740_992.0,
    ]
}

/// Strings every engine can compare in full, including with LIKE. No embedded
/// NUL and no `%` or `_`; see [`crate::model::COLUMNS`] for why each of those
/// is excluded from this column and where it lives instead.
fn str_pool() -> Vec<String> {
    let mut v: Vec<String> = [
        "",
        " ",
        "  x",
        "a",
        "A",
        "z",
        "Z",
        "abc",
        "ABC",
        "back\\slash",
        "quote'and\"quote",
        "line\nbreak",
        "tab\there",
        // Multi-byte UTF-8 at three widths, plus a surrogate-pair emoji.
        "\u{e9}",
        "\u{65e5}\u{672c}\u{8a9e}",
        "\u{1f600}",
        // Combining sequence: two different byte strings that many systems
        // render identically. They must NOT compare equal.
        "e\u{301}",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    v.push("L".repeat(300));
    v
}

/// Strings containing literal LIKE metacharacters. Column `sm` carries these
/// and only the `like_metachar_in_text` shape queries it.
fn meta_str_pool() -> Vec<String> {
    [
        "%", "_", "%%", "__", "%a", "a%", "a%c", "a_c", "_x", "50% off", "a", "abc", "",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// The full string pool, including embedded NULs.
///
/// Column `sx` carries these. They are the exact shape that produced wrong
/// index rows before BTREE v3, so they must stay in the fixture; they are kept
/// out of the LIKE column because SQLite's LIKE stops at the NUL and so cannot
/// adjudicate them. See [`crate::model::COLUMNS`].
fn exotic_str_pool() -> Vec<String> {
    let mut v = str_pool();
    v.extend(meta_str_pool());
    v.push("a\0b".to_string());
    v.push("a\0c".to_string());
    v.push("\0".to_string());
    v.push("\0leading".to_string());
    v
}

fn datetime_pool() -> Vec<i64> {
    vec![
        0,
        1,
        -1,
        // 2023-11-14T22:13:20Z
        1_700_000_000_000_000,
        // 9999-12-31T23:59:59Z
        253_402_300_799_000_000,
        // 0001-01-01T00:00:00Z
        -62_135_596_800_000_000,
        i64::MIN,
        i64::MAX,
    ]
}

fn uuid_pool() -> Vec<[u8; 16]> {
    vec![
        [0u8; 16],
        [0xffu8; 16],
        [
            0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
            0x00, 0x00,
        ],
        // Differs from the previous one only in the last byte, so a truncated
        // key comparison collapses them.
        [
            0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
            0x00, 0x01,
        ],
        [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ],
    ]
}

fn bytes_pool() -> Vec<Vec<u8>> {
    vec![
        vec![],
        vec![0x00],
        vec![0xff],
        vec![0x00, 0x00],
        vec![0x00, 0xff, 0x10],
        // Prefix pair: a length-blind comparison ties these.
        vec![0x01, 0x02],
        vec![0x01, 0x02, 0x03],
        vec![0xaa; 300],
    ]
}

/// Canonical JSON documents: keys sorted bytewise, no insignificant
/// whitespace. See `normalize.rs` rule 4 for why the generator, not the
/// normalizer, is responsible for canonical form.
///
/// The keys `a`, `b` and `z` always hold a *non-boolean* scalar (or are
/// absent). Two constraints are behind that:
///
/// - a sub-document under an extracted key would come back as PJ1 canonical
///   text on one side and SQLite's own JSON rendering on the other;
/// - SQLite's `json_extract` returns the integers 1/0 for JSON `true`/`false`
///   because SQLite has no boolean storage class, while PowDB's `->` returns a
///   real `bool`. Leaving booleans under an extracted key would force a
///   ledger entry over the whole json-path shape, and that entry would then
///   also excuse a genuine path-extraction bug. Booleans stay in the pool at
///   top level and under `k`, where they are exercised by whole-column
///   projection, grouping and distinct; boolean *path extraction* against
///   SQLite is a declared coverage gap.
fn json_pool() -> Vec<String> {
    [
        "null",
        "true",
        "false",
        "0",
        "-1",
        "42",
        "1.5",
        "\"\"",
        "\"s\"",
        "[]",
        "{}",
        "[1,2,3]",
        "{\"a\":1}",
        "{\"a\":0}",
        "{\"a\":-9007199254740993}",
        "{\"a\":1,\"b\":\"x\"}",
        "{\"a\":1.5,\"b\":\"\"}",
        "{\"a\":2,\"z\":\"\u{e9}\"}",
        "{\"a\":null}",
        "{\"b\":\"x\"}",
        "{\"k\":{\"n\":1}}",
        "{\"k\":[1,2]}",
        "{\"k\":true}",
        "{\"k\":false}",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// A random non-null literal for a column, drawn from that column's pool.
///
/// Column-directed rather than type-directed, because the two `str` columns
/// draw from different pools (see [`exotic_str_pool`]).
fn pooled(rng: &mut Rng, col: &Column) -> Lit {
    match col.ty {
        ColType::Int => Lit::Int(*rng.pick(&int_pool())),
        ColType::Float => Lit::Float(*rng.pick(&float_pool())),
        ColType::Bool => Lit::Bool(rng.chance(1, 2)),
        ColType::Str if col.name == LIKE_COLUMN => Lit::Str(rng.pick(&str_pool()).clone()),
        ColType::Str if col.name == LIKE_META_COLUMN => {
            Lit::Str(rng.pick(&meta_str_pool()).clone())
        }
        ColType::Str => Lit::Str(rng.pick(&exotic_str_pool()).clone()),
        ColType::DateTime => Lit::DateTime(*rng.pick(&datetime_pool())),
        ColType::Uuid => Lit::Uuid(*rng.pick(&uuid_pool())),
        ColType::Bytes => Lit::Bytes(rng.pick(&bytes_pool()).clone()),
        ColType::Json => Lit::Json(rng.pick(&json_pool()).clone()),
    }
}

/// A predicate literal for a column. Same pools as the data, so a generated
/// comparison actually has a chance of matching rows instead of always
/// returning the empty set.
pub fn predicate_literal(rng: &mut Rng, col: &Column) -> Lit {
    pooled(rng, col)
}

/// The body shared by the indexed and non-indexed boundary fixtures.
///
/// Built in two halves: an exhaustive half that pins *every* pool value into
/// at least one row (so boundary coverage does not depend on the RNG), and a
/// random half that mixes columns so multi-column predicates have something to
/// bite on.
fn boundary_rows() -> Vec<FixRow> {
    let mut rng = Rng::new(0x0B0D_0A12_5041_5442);
    let mut rows: Vec<FixRow> = Vec::new();
    let mut next_id: i64 = 1;

    let push_pinned = |rows: &mut Vec<FixRow>, next_id: &mut i64, name: &str, v: Lit| {
        let mut row = null_row(*next_id);
        *next_id += 1;
        set(&mut row, name, v);
        rows.push(row);
    };

    for v in int_pool() {
        push_pinned(&mut rows, &mut next_id, "i", Lit::Int(v));
    }
    for v in float_pool() {
        push_pinned(&mut rows, &mut next_id, "f", Lit::Float(v));
    }
    for v in [true, false] {
        push_pinned(&mut rows, &mut next_id, "b", Lit::Bool(v));
    }
    for v in str_pool() {
        push_pinned(&mut rows, &mut next_id, "s", Lit::Str(v));
    }
    for v in meta_str_pool() {
        push_pinned(&mut rows, &mut next_id, "sm", Lit::Str(v));
    }
    for v in exotic_str_pool() {
        push_pinned(&mut rows, &mut next_id, "sx", Lit::Str(v));
    }
    for v in datetime_pool() {
        push_pinned(&mut rows, &mut next_id, "d", Lit::DateTime(v));
    }
    for v in uuid_pool() {
        push_pinned(&mut rows, &mut next_id, "u", Lit::Uuid(v));
    }
    for v in bytes_pool() {
        push_pinned(&mut rows, &mut next_id, "y", Lit::Bytes(v));
    }
    for v in json_pool() {
        push_pinned(&mut rows, &mut next_id, "j", Lit::Json(v));
    }
    // An all-null row (every nullable column unset) and a duplicate of it, so
    // NULL grouping and NULL distinct have more than one member.
    rows.push(null_row(next_id));
    next_id += 1;
    rows.push(null_row(next_id));
    next_id += 1;

    for _ in 0..60 {
        let mut row = null_row(next_id);
        next_id += 1;
        for col in COLUMNS.iter().skip(1) {
            // One column in four is left null, so every predicate meets a mix
            // of present and missing values.
            if !rng.chance(1, 4) {
                let v = pooled(&mut rng, col);
                set(&mut row, col.name, v);
            }
        }
        rows.push(row);
    }
    rows
}

fn single_row() -> Vec<FixRow> {
    let mut rng = Rng::new(0x5349_4E47_4C45_0001);
    let mut row = null_row(1);
    for col in COLUMNS.iter().skip(1) {
        let v = pooled(&mut rng, col);
        set(&mut row, col.name, v);
    }
    vec![row]
}

fn all_null_column_rows() -> Vec<FixRow> {
    let mut rng = Rng::new(0x414C_4C4E_554C_4C00);
    (1..=8i64)
        .map(|id| {
            let mut row = null_row(id);
            for col in COLUMNS.iter().skip(1) {
                // `i` is held null in every row: the all-NULL-column case.
                if col.name == "i" {
                    continue;
                }
                let v = pooled(&mut rng, col);
                set(&mut row, col.name, v);
            }
            row
        })
        .collect()
}

/// Rows that PowQL cannot express as literals and SQLite cannot store.
///
/// `f64::NAN` becomes NULL when bound into a SQLite REAL column, and SQLite
/// normalises `-0.0` to `+0.0` on the way into the record, so SQLite has no
/// opinion to offer on either. These rows exist to keep NaN, the infinities
/// and negative zero inside the PowQL-vs-PowDB-SQL comparison, which is the
/// part of the oracle that still applies.
fn nonfinite_rows() -> Vec<FixRow> {
    let vals = [
        f64::NAN,
        -f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.0,
        0.0,
        1.0,
        -1.0,
    ];
    let mut rows = Vec::new();
    for (idx, v) in vals.iter().enumerate() {
        let id = (idx as i64) + 1;
        let mut row = null_row(id);
        set(&mut row, "f", Lit::Float(*v));
        set(&mut row, "i", Lit::Int(id));
        rows.push(row);
    }
    // One row with `f` missing, so NULL and NaN are distinguishable.
    rows.push(null_row((vals.len() as i64) + 1));
    rows
}

/// Every fixture the oracle runs, in a fixed order.
pub fn all() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "empty",
            rows: Vec::new(),
            indexes: &[],
            load: Load::Powql,
            sqlite_representable: true,
        },
        Fixture {
            name: "single",
            rows: single_row(),
            indexes: &[],
            load: Load::Powql,
            sqlite_representable: true,
        },
        Fixture {
            name: "all_null_i",
            rows: all_null_column_rows(),
            indexes: &[],
            load: Load::Powql,
            sqlite_representable: true,
        },
        Fixture {
            name: "boundary",
            rows: boundary_rows(),
            indexes: &[],
            load: Load::Powql,
            sqlite_representable: true,
        },
        Fixture {
            name: "boundary_indexed",
            rows: boundary_rows(),
            indexes: &["i", "s", "d", "u", "y"],
            load: Load::Powql,
            sqlite_representable: true,
        },
        Fixture {
            name: "nonfinite",
            rows: nonfinite_rows(),
            indexes: &[],
            load: Load::StorageApi,
            sqlite_representable: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pool value must have a lossless PowQL spelling. A `None` here
    /// would mean a boundary value silently never reaches PowDB, which is the
    /// exact "test that cannot fail" failure mode this crate exists to stop.
    #[test]
    fn every_powql_loaded_value_has_a_literal() {
        for fixture in all() {
            if fixture.load != Load::Powql {
                continue;
            }
            for row in &fixture.rows {
                for (col, lit) in COLUMNS.iter().zip(row) {
                    if lit.is_null() {
                        continue;
                    }
                    assert!(
                        lit.powql().is_some(),
                        "fixture {} column {} has no PowQL literal for {lit:?}",
                        fixture.name,
                        col.name
                    );
                    assert!(
                        lit.powdb_sql().is_some(),
                        "fixture {} column {} has no PowDB-SQL literal for {lit:?}",
                        fixture.name,
                        col.name
                    );
                }
            }
        }
    }

    #[test]
    fn every_column_type_is_pinned_by_the_boundary_fixture() {
        let rows = boundary_rows();
        for (idx, col) in COLUMNS.iter().enumerate() {
            assert!(
                rows.iter().any(|r| !r[idx].is_null()),
                "no boundary row sets column {}",
                col.name
            );
        }
        // And every individual pool value appears at least once.
        let present: Vec<&Lit> = rows.iter().flat_map(|r| r.iter()).collect();
        for v in int_pool() {
            assert!(
                present.contains(&&Lit::Int(v)),
                "int pool value {v} missing"
            );
        }
        for v in str_pool() {
            assert!(
                present.contains(&&Lit::Str(v.clone())),
                "str pool value {v:?} missing"
            );
        }
        for v in datetime_pool() {
            assert!(
                present.contains(&&Lit::DateTime(v)),
                "datetime pool value {v} missing"
            );
        }
        for v in json_pool() {
            assert!(
                present.contains(&&Lit::Json(v.clone())),
                "json pool value {v} missing"
            );
        }
    }

    #[test]
    fn ids_are_dense_and_unique() {
        for fixture in all() {
            let mut ids: Vec<i64> = fixture
                .rows
                .iter()
                .map(|r| match r[0] {
                    Lit::Int(n) => n,
                    ref other => panic!("fixture {} id column is {other:?}", fixture.name),
                })
                .collect();
            let count = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(
                ids.len(),
                count,
                "fixture {} has duplicate ids",
                fixture.name
            );
        }
    }

    /// `id` must ascend with row position in every fixture.
    ///
    /// The rows reach PowDB in this order, so this is also the heap's scan
    /// order, and PowDB breaks a tie in `ORDER BY` by scan order (the generic
    /// sort is stable; the top-N heap carries a `seq` counter to reproduce it).
    /// The `order_desc_limit` / `order_asc_limit` shapes state that rule to
    /// SQLite as `id ASC`, which is only a true statement of PowDB's behaviour
    /// while this holds. `ids_are_dense_and_unique` does not cover it: a
    /// fixture could shuffle its rows and stay unique, and those shapes would
    /// then start reporting the reference as wrong.
    #[test]
    fn ids_ascend_with_row_order() {
        for fixture in all() {
            let mut previous: Option<i64> = None;
            for (position, row) in fixture.rows.iter().enumerate() {
                let Lit::Int(id) = row[0] else {
                    panic!("fixture {} id column is not an int", fixture.name)
                };
                if let Some(previous) = previous {
                    assert!(
                        id > previous,
                        "fixture {} row {position} has id {id} after id {previous}; \
                         the ordering shapes state PowDB's scan-order tiebreak as `id ASC`",
                        fixture.name
                    );
                }
                previous = Some(id);
            }
        }
    }

    #[test]
    fn fixture_generation_is_deterministic() {
        let a = boundary_rows();
        let b = boundary_rows();
        assert_eq!(a, b);
    }

    #[test]
    fn nonfinite_fixture_is_marked_powdb_only() {
        let f = all();
        let nf = f
            .iter()
            .find(|f| f.name == "nonfinite")
            .expect("nonfinite fixture");
        assert!(!nf.sqlite_representable);
        assert_eq!(nf.load, Load::StorageApi);
        // And it really does carry values PowQL cannot spell, otherwise the
        // storage-API path would be dead weight pretending to add coverage.
        assert!(nf
            .rows
            .iter()
            .flat_map(|r| r.iter())
            .any(|l| matches!(l, Lit::Float(v) if !v.is_finite())));
    }
}
