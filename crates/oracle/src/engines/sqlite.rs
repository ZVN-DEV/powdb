//! The SQLite leg of the oracle.
//!
//! SQLite is the reference. It receives the same fixture body as PowDB, but
//! through bound parameters rather than literal text, so its copy of the data
//! is exactly what the generator intended and cannot be corrupted by a
//! literal-syntax mistake shared with the engine under test.

use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;

use crate::fixture::Fixture;
use crate::model::{Kind, Lit, OVal, Outcome, ResultSet, COLUMNS, TABLE};
use crate::normalize::sqlite_value;

pub struct Sqlite {
    conn: Connection,
}

impl Sqlite {
    pub fn open(fixture: &Fixture) -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| format!("open: {e}"))?;
        let cols = COLUMNS
            .iter()
            .map(|c| {
                let notnull = if c.required { " NOT NULL" } else { "" };
                format!("{} {}{notnull}", c.name, c.ty.sqlite())
            })
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute_batch(&format!("CREATE TABLE {TABLE} ({cols})"))
            .map_err(|e| format!("create table: {e}"))?;

        let names = COLUMNS
            .iter()
            .map(|c| c.name)
            .collect::<Vec<_>>()
            .join(", ");
        let holes = (1..=COLUMNS.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        {
            let mut stmt = conn
                .prepare(&format!("INSERT INTO {TABLE} ({names}) VALUES ({holes})"))
                .map_err(|e| format!("prepare insert: {e}"))?;
            for row in &fixture.rows {
                let params: Vec<SqlValue> = row.iter().map(Lit::sqlite_param).collect();
                stmt.execute(rusqlite::params_from_iter(params.iter()))
                    .map_err(|e| format!("insert: {e}"))?;
            }
        }

        for col in fixture.indexes {
            conn.execute_batch(&format!("CREATE INDEX idx_{col} ON {TABLE} ({col})"))
                .map_err(|e| format!("create index on {col}: {e}"))?;
        }

        Ok(Sqlite { conn })
    }

    /// Run a statement that returns no rows (the reference leg of a mutation).
    ///
    /// Separate from [`Self::query`] because a mutation has no output columns,
    /// so there are no `kinds` to declare and nothing to normalize. The error
    /// is returned rather than swallowed: SQLite refusing a mutation is
    /// information about the mutation, not a harness failure.
    pub fn execute(&self, sql: &str, params: &[Lit]) -> Result<usize, String> {
        let bound: Vec<SqlValue> = params.iter().map(Lit::sqlite_param).collect();
        self.conn
            .execute(sql, rusqlite::params_from_iter(bound.iter()))
            .map_err(|e| e.to_string())
    }

    pub fn query(&self, sql: &str, params: &[Lit], kinds: &[Kind]) -> Outcome {
        match self.try_query(sql, params, kinds) {
            Ok(rs) => Outcome::Rows(rs),
            Err(e) => Outcome::Error(e),
        }
    }

    fn try_query(&self, sql: &str, params: &[Lit], kinds: &[Kind]) -> Result<ResultSet, String> {
        let mut stmt = self.conn.prepare(sql).map_err(|e| e.to_string())?;
        let columns: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        if columns.len() != kinds.len() {
            return Err(format!(
                "oracle bug: {} output columns but {} declared kinds for `{sql}`",
                columns.len(),
                kinds.len()
            ));
        }
        let bound: Vec<SqlValue> = params.iter().map(Lit::sqlite_param).collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(bound.iter()))
            .map_err(|e| e.to_string())?;

        let mut out: Vec<Vec<OVal>> = Vec::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut values = Vec::with_capacity(kinds.len());
            for (idx, kind) in kinds.iter().enumerate() {
                let raw = row.get_ref(idx).map_err(|e| e.to_string())?;
                values.push(sqlite_value(raw, *kind)?);
            }
            out.push(values);
        }
        Ok(ResultSet { columns, rows: out })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Load;
    use crate::model::ColType;

    /// One row with a distinctive value in every column, built by column name
    /// so that adding a column to the schema cannot silently drop it from this
    /// round-trip test.
    fn unit_row() -> Vec<Lit> {
        COLUMNS
            .iter()
            .map(|c| match (c.name, c.ty) {
                ("id", _) => Lit::Int(1),
                (_, ColType::Int) => Lit::Int(-5),
                (_, ColType::Float) => Lit::Float(1.5),
                (_, ColType::Bool) => Lit::Bool(true),
                // Embedded NUL: SQLite's own `length()` truncates here, so a
                // sloppy read path would lose the tail.
                (_, ColType::Str) => Lit::Str("a\0b".into()),
                (_, ColType::DateTime) => Lit::DateTime(-62_135_596_800_000_000),
                (_, ColType::Uuid) => Lit::Uuid([3u8; 16]),
                (_, ColType::Bytes) => Lit::Bytes(vec![0, 255]),
                (_, ColType::Json) => Lit::Json("{\"a\":1}".into()),
            })
            .collect()
    }

    fn one_row_fixture() -> Fixture {
        Fixture {
            name: "unit",
            rows: vec![unit_row()],
            indexes: &[],
            load: Load::Powql,
            sqlite_representable: true,
        }
    }

    /// Every value must survive the bind/read round trip byte-exactly,
    /// including the embedded NUL that SQLite's own `length()` truncates at.
    #[test]
    fn every_column_round_trips_through_sqlite() {
        let db = Sqlite::open(&one_row_fixture()).expect("open");
        let kinds: Vec<Kind> = COLUMNS.iter().map(|c| Kind::Col(c.ty)).collect();
        let got = db.query(&format!("SELECT * FROM {TABLE}"), &[], &kinds);
        let rs = match got {
            Outcome::Rows(rs) => rs,
            Outcome::Error(e) => panic!("query failed: {e}"),
        };
        assert_eq!(rs.rows.len(), 1);
        let expected: Vec<OVal> = COLUMNS
            .iter()
            .map(|c| match (c.name, c.ty) {
                ("id", _) => OVal::Int(1),
                (_, ColType::Int) => OVal::Int(-5),
                (_, ColType::Float) => OVal::Float(1.5),
                (_, ColType::Bool) => OVal::Bool(true),
                (_, ColType::Str) => OVal::Str("a\0b".into()),
                (_, ColType::DateTime) => OVal::DateTime(-62_135_596_800_000_000),
                (_, ColType::Uuid) => OVal::Uuid([3u8; 16]),
                (_, ColType::Bytes) => OVal::Bytes(vec![0, 255]),
                (_, ColType::Json) => OVal::Json("{\"a\":1}".into()),
            })
            .collect();
        assert_eq!(rs.rows[0], expected);
    }

    #[test]
    fn a_kind_count_mismatch_is_an_error_not_a_silent_truncation() {
        let db = Sqlite::open(&one_row_fixture()).expect("open");
        let got = db.query(
            &format!("SELECT id, i FROM {TABLE}"),
            &[],
            &[Kind::Col(ColType::Int)],
        );
        match got {
            Outcome::Error(e) => assert!(e.contains("declared kinds"), "unexpected: {e}"),
            Outcome::Rows(_) => panic!("expected an oracle-bug error"),
        }
    }

    #[test]
    fn a_bad_query_is_an_error_outcome_not_a_panic() {
        let db = Sqlite::open(&one_row_fixture()).expect("open");
        match db.query("SELECT nope FROM T", &[], &[Kind::Expr]) {
            Outcome::Error(_) => {}
            Outcome::Rows(_) => panic!("expected an error"),
        }
    }
}
