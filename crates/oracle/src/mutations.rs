//! Mutation adjudication: the oracle's blind spot until now.
//!
//! Every shape in [`crate::shapes`] is a *read*. Writes exist there only to
//! seed a fixture, never as the thing under test, so the oracle has always
//! compared what the two engines *say* and never what they *did*. That is
//! exactly where the damage has been: the two worst defects of the last two
//! releases were a relocating UPDATE that WAL replay duplicated, and a
//! DISTINCT/LIMIT ordering bug. The first was invisible here by construction.
//!
//! # How a mutation is adjudicated
//!
//! A read case can share one loaded fixture across every case, because a read
//! leaves no trace. A mutation cannot. So each case gets its own freshly
//! loaded engine per leg (`Powdb::open` already builds a new temp dir, and
//! SQLite is in-memory, so this is cheap), then:
//!
//! 1. apply the mutation,
//! 2. read the **entire table back** with [`dump_powql`] / [`dump_sqlite`],
//! 3. compare that resulting state, not the mutation's own return value.
//!
//! Comparing the final state rather than a reported row count is deliberate.
//! "2 rows affected" agreeing on both sides proves nothing about *which* two
//! rows, and a row count is precisely what a duplicate-row bug still gets
//! right.

use crate::compare::diff;
use crate::engines::powdb::Powdb;
use crate::engines::sqlite::Sqlite;
use crate::fixture::Fixture;
use crate::model::{ColType, Kind, Lit, COLUMNS, LIKE_COLUMN, TABLE};
use crate::rng::Rng;

/// One mutation, spelled three ways, plus the state read that adjudicates it.
pub struct MutationCase {
    pub shape: &'static str,
    /// The mutation as native PowQL.
    pub powql: String,
    /// The same mutation through PowDB's SQL frontend.
    pub powdb_sql: String,
    /// The same mutation for SQLite, the reference.
    pub sqlite_sql: String,
    /// Bound parameters for the SQLite leg, for the same reason the read
    /// shapes bind: SQLite must hold the generator's exact intended value so a
    /// broken PowQL literal cannot be cancelled out by an identical mistake.
    pub sqlite_params: Vec<Lit>,
    /// False when SQLite cannot be the authority for this mutation.
    pub sqlite_comparable: bool,
}

/// Read the whole table back in a total order, as PowQL.
///
/// Ordered by `id`, which is the required dense key, so there is no NULL
/// ordering disagreement to normalize away and the comparison can be
/// sequence-sensitive.
pub fn dump_powql() -> String {
    let proj = COLUMNS
        .iter()
        .map(|c| format!(".{}", c.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{TABLE} order .id {{ {proj} }}")
}

/// The same read for SQLite.
pub fn dump_sqlite() -> String {
    let cols = COLUMNS
        .iter()
        .map(|c| c.name)
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT {cols} FROM {TABLE} ORDER BY id")
}

/// One [`Kind`] per dumped column, driving SQLite normalization.
pub fn dump_kinds() -> Vec<Kind> {
    COLUMNS.iter().map(|c| Kind::Col(c.ty)).collect()
}

/// A deterministic value for `ty`, drawn from `rng`.
///
/// Deliberately small and boring: the point of a mutation shape is the write
/// path, not literal parsing, which the read shapes already cover exhaustively.
fn value_for(ty: ColType, rng: &mut Rng) -> Lit {
    match ty {
        ColType::Int => Lit::Int(rng.below(200) as i64 - 100),
        ColType::Float => Lit::Float((rng.below(2000) as f64 - 1000.0) / 8.0),
        ColType::Bool => Lit::Bool(rng.chance(1, 2)),
        ColType::Str => Lit::Str(format!("m{}", rng.below(1000))),
        ColType::DateTime => Lit::DateTime(rng.below(1_000_000) as i64),
        ColType::Uuid => {
            let mut b = [0u8; 16];
            for byte in b.iter_mut() {
                *byte = rng.below(256) as u8;
            }
            Lit::Uuid(b)
        }
        ColType::Bytes => Lit::Bytes(vec![rng.below(256) as u8; 1 + rng.below(4)]),
        ColType::Json => Lit::Json(format!(r#"{{"k":{}}}"#, rng.below(100))),
    }
}

/// Generate the mutation cases for one run.
///
/// `budget` caps the total, matching [`crate::shapes::generate`] so a run stays
/// bounded.
pub fn generate(seed: u64, per_shape: usize, budget: usize) -> Vec<MutationCase> {
    let mut rng = Rng::new(seed ^ 0x6D75_7461_7469_6F6E); // "mutation"
    let mut out = Vec::new();

    for _ in 0..per_shape {
        // Every non-required column, set to a fresh value on a single row.
        // The row is addressed by the unique key so the target is unambiguous.
        for col in COLUMNS.iter().filter(|c| !c.required) {
            let v = value_for(col.ty, &mut rng);
            let (Some(pq), Some(sq)) = (v.powql(), v.powdb_sql()) else {
                continue;
            };
            let id = rng.below(8) as i64;
            out.push(MutationCase {
                shape: "update_one_by_key",
                powql: format!(
                    "{TABLE} filter .id = {id} update {{ {} := {pq} }}",
                    col.name
                ),
                powdb_sql: format!("UPDATE {TABLE} SET {} = {sq} WHERE id = {id}", col.name),
                sqlite_sql: format!("UPDATE {TABLE} SET {} = ? WHERE id = ?", col.name),
                sqlite_params: vec![v, Lit::Int(id)],
                sqlite_comparable: true,
            });
        }

        // Set a nullable column to NULL. `Empty` is reachable everywhere in
        // this schema, and clearing a value is a different heap path from
        // overwriting one.
        for col in COLUMNS.iter().filter(|c| !c.required) {
            let id = rng.below(8) as i64;
            out.push(MutationCase {
                shape: "update_to_null",
                powql: format!(
                    "{TABLE} filter .id = {id} update {{ {} := null }}",
                    col.name
                ),
                powdb_sql: format!("UPDATE {TABLE} SET {} = NULL WHERE id = {id}", col.name),
                sqlite_sql: format!("UPDATE {TABLE} SET {} = NULL WHERE id = ?", col.name),
                sqlite_params: vec![Lit::Int(id)],
                sqlite_comparable: true,
            });
        }

        // A row grown far past its original size, which is the case that makes
        // UPDATE non-idempotent: the heap cannot fit it in place, so it
        // relocates via delete plus insert with a fresh RowId. This is the
        // exact shape of the v0.23.0 WAL-replay duplication defect, and the
        // shape no read query can ever reach.
        {
            let id = rng.below(8) as i64;
            let big = "g".repeat(600 + rng.below(2400));
            let v = Lit::Str(big);
            if let (Some(pq), Some(sq)) = (v.powql(), v.powdb_sql()) {
                out.push(MutationCase {
                    shape: "update_grow_row",
                    powql: format!("{TABLE} filter .id = {id} update {{ {LIKE_COLUMN} := {pq} }}"),
                    powdb_sql: format!("UPDATE {TABLE} SET {LIKE_COLUMN} = {sq} WHERE id = {id}"),
                    sqlite_sql: format!("UPDATE {TABLE} SET {LIKE_COLUMN} = ? WHERE id = ?"),
                    sqlite_params: vec![v, Lit::Int(id)],
                    sqlite_comparable: true,
                });
            }
        }

        // A set-valued update: every row matching a range predicate. Exercises
        // the fused scan-plus-update path rather than a keyed point update.
        {
            let bound = rng.below(200) as i64 - 100;
            let v = value_for(ColType::Int, &mut rng);
            if let (Some(pq), Some(sq)) = (v.powql(), v.powdb_sql()) {
                out.push(MutationCase {
                    shape: "update_filtered_range",
                    powql: format!("{TABLE} filter .i > {bound} update {{ i := {pq} }}"),
                    powdb_sql: format!("UPDATE {TABLE} SET i = {sq} WHERE i > {bound}"),
                    sqlite_sql: format!("UPDATE {TABLE} SET i = ? WHERE i > ?"),
                    sqlite_params: vec![v, Lit::Int(bound)],
                    sqlite_comparable: true,
                });
            }
        }

        // Deletes: one keyed, one set-valued, one total.
        {
            let id = rng.below(8) as i64;
            out.push(MutationCase {
                shape: "delete_one_by_key",
                powql: format!("{TABLE} filter .id = {id} delete"),
                powdb_sql: format!("DELETE FROM {TABLE} WHERE id = {id}"),
                sqlite_sql: format!("DELETE FROM {TABLE} WHERE id = ?"),
                sqlite_params: vec![Lit::Int(id)],
                sqlite_comparable: true,
            });

            let bound = rng.below(200) as i64 - 100;
            out.push(MutationCase {
                shape: "delete_filtered_range",
                powql: format!("{TABLE} filter .i > {bound} delete"),
                powdb_sql: format!("DELETE FROM {TABLE} WHERE i > {bound}"),
                sqlite_sql: format!("DELETE FROM {TABLE} WHERE i > ?"),
                sqlite_params: vec![Lit::Int(bound)],
                sqlite_comparable: true,
            });

            out.push(MutationCase {
                shape: "delete_all",
                powql: format!("{TABLE} delete"),
                powdb_sql: format!("DELETE FROM {TABLE}"),
                sqlite_sql: format!("DELETE FROM {TABLE}"),
                sqlite_params: vec![],
                sqlite_comparable: true,
            });
        }

        if out.len() >= budget {
            break;
        }
    }

    out.truncate(budget);
    out
}

/// A disagreement about the state a mutation left behind.
pub struct StateDivergence {
    pub fixture: &'static str,
    pub shape: &'static str,
    pub pair: &'static str,
    pub mutation: String,
    pub detail: String,
}

/// Apply `case` three ways against fresh copies of `fixture` and compare the
/// table state each one leaves behind.
///
/// Returns every disagreement found. An error from the mutation itself is a
/// legitimate outcome and is compared like any other, so "PowQL rejected it,
/// SQL accepted it" is caught rather than skipped.
pub fn adjudicate(fixture: &Fixture, case: &MutationCase) -> Result<Vec<StateDivergence>, String> {
    let dump_pq = dump_powql();
    let mut found = Vec::new();

    // Leg 1: mutate through PowQL, then read the table back.
    //
    // The mutation's own return value is deliberately NOT compared. The engine
    // adapter models reads, so it reports a `Modified(n)` result as an error
    // ("expected a row or scalar result"), which makes every mutation look
    // identically failed and any comparison of that value vacuous. The state
    // read below is the authority, and it subsumes the case anyway: a dialect
    // that rejected the mutation leaves the table unchanged, which is a state
    // divergence from one that applied it.
    let mut a = Powdb::open(fixture)?;
    let _ = a.powql(&case.powql);
    let a_state = a.powql(&dump_pq);

    // Leg 2: the same mutation through the SQL frontend, on a fresh copy.
    let mut b = Powdb::open(fixture)?;
    let _ = b.sql(&case.powdb_sql);
    let b_state = b.powql(&dump_pq);

    if let Some(detail) = diff(&a_state, &b_state, true) {
        found.push(StateDivergence {
            fixture: fixture.name,
            shape: case.shape,
            pair: "powql-vs-powdb-sql",
            mutation: case.powql.clone(),
            detail,
        });
    }

    if fixture.sqlite_representable && case.sqlite_comparable {
        let sqlite = Sqlite::open(fixture)?;
        // An error here is the reference refusing the mutation, which is
        // information, not a harness failure.
        if sqlite
            .execute(&case.sqlite_sql, &case.sqlite_params)
            .is_ok()
        {
            let c_state = sqlite.query(&dump_sqlite(), &[], &dump_kinds());
            if let Some(detail) = diff(&a_state, &c_state, true) {
                found.push(StateDivergence {
                    fixture: fixture.name,
                    shape: case.shape,
                    pair: "powdb-vs-sqlite",
                    mutation: case.powql.clone(),
                    detail,
                });
            }
        }
    }

    Ok(found)
}
