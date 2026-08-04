//! The two PowDB legs of the oracle: PowQL and PowDB-SQL.
//!
//! Both run against the *same* [`Engine`] instance, so they share a catalog
//! and a plan cache. That is deliberate: a plan cached by one dialect and
//! reused by the other is a real deployment shape and a real bug surface.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use powdb_storage::wal::WalSyncMode;
use tempfile::TempDir;

use crate::fixture::{FixRow, Fixture, Load};
use crate::model::{Lit, OVal, Outcome, ResultSet, COLUMNS, SCALAR_COLUMN, TABLE};
use crate::normalize::powdb_value;

pub struct Powdb {
    engine: Engine,
    _tmp: TempDir,
}

impl Powdb {
    /// Build a fresh engine in a fresh temp directory and load `fixture`.
    pub fn open(fixture: &Fixture) -> Result<Self, String> {
        let tmp = TempDir::new().map_err(|e| format!("tempdir: {e}"))?;
        let mut engine = Engine::new(tmp.path()).map_err(|e| format!("engine: {e}"))?;
        // The oracle compares query *answers*, not durability. SQLite's
        // in-memory database does no fsync either, so turning WAL sync off
        // keeps fixture loading from dominating the run without changing a
        // single result. Crash behaviour is covered by the durability suite.
        engine.catalog_mut().set_wal_sync_mode(WalSyncMode::Off);

        let cols = COLUMNS
            .iter()
            .map(|c| {
                let req = if c.required { "required " } else { "" };
                format!("{req}{}: {}", c.name, c.ty.powql())
            })
            .collect::<Vec<_>>()
            .join(", ");
        engine
            .execute_powql(&format!("type {TABLE} {{ {cols} }}"))
            .map_err(|e| format!("create type: {e}"))?;

        match fixture.load {
            Load::Powql => {
                for row in &fixture.rows {
                    let stmt = insert_powql(row)?;
                    engine
                        .execute_powql(&stmt)
                        .map_err(|e| format!("insert failed ({stmt}): {e}"))?;
                }
            }
            Load::StorageApi => {
                let table = engine
                    .catalog_mut()
                    .get_table_mut(TABLE)
                    .ok_or_else(|| format!("table {TABLE} missing after create"))?;
                for row in &fixture.rows {
                    let values = row
                        .iter()
                        .map(lit_to_value)
                        .collect::<Result<Vec<_>, _>>()?;
                    table
                        .insert(&values)
                        .map_err(|e| format!("storage insert failed: {e}"))?;
                }
            }
        }

        for col in fixture.indexes {
            engine
                .execute_powql(&format!("alter {TABLE} add index .{col}"))
                .map_err(|e| format!("create index on {col}: {e}"))?;
        }

        Ok(Powdb { engine, _tmp: tmp })
    }

    pub fn powql(&mut self, query: &str) -> Outcome {
        to_outcome(self.engine.execute_powql(query).map_err(|e| e.to_string()))
    }

    pub fn sql(&mut self, query: &str) -> Outcome {
        to_outcome(self.engine.execute_sql(query).map_err(|e| e.to_string()))
    }
}

fn to_outcome(result: Result<QueryResult, String>) -> Outcome {
    match result {
        Err(e) => Outcome::Error(e),
        Ok(QueryResult::Rows { columns, rows }) => Outcome::Rows(ResultSet {
            columns,
            rows: rows
                .iter()
                .map(|r| r.iter().map(powdb_value).collect::<Vec<OVal>>())
                .collect(),
        }),
        // A PowQL scalar carries no column name; see `normalize::scalarize`.
        Ok(QueryResult::Scalar(v)) => Outcome::Rows(ResultSet {
            columns: vec![SCALAR_COLUMN.to_string()],
            rows: vec![vec![powdb_value(&v)]],
        }),
        Ok(other) => Outcome::Error(format!(
            "expected a row or scalar result, got {}",
            match other {
                QueryResult::Modified(n) => format!("Modified({n})"),
                QueryResult::Created(t) => format!("Created({t})"),
                QueryResult::Executed { message } => format!("Executed({message})"),
                QueryResult::Rows { .. } | QueryResult::Scalar(_) => unreachable!(),
            }
        )),
    }
}

/// Build `insert T { .. }`, omitting null columns (PowQL has no null literal;
/// an omitted nullable column *is* null).
fn insert_powql(row: &FixRow) -> Result<String, String> {
    let mut parts = Vec::new();
    for (col, lit) in COLUMNS.iter().zip(row) {
        if lit.is_null() {
            if col.required {
                return Err(format!(
                    "required column {} is null in a fixture row",
                    col.name
                ));
            }
            continue;
        }
        let text = lit
            .powql()
            .ok_or_else(|| format!("no PowQL literal for {lit:?} in column {}", col.name))?;
        parts.push(format!("{} := {text}", col.name));
    }
    Ok(format!("insert {TABLE} {{ {} }}", parts.join(", ")))
}

/// Storage-level conversion, used only by [`Load::StorageApi`] fixtures.
fn lit_to_value(lit: &Lit) -> Result<Value, String> {
    Ok(match lit {
        Lit::Null => Value::Empty,
        Lit::Int(v) => Value::Int(*v),
        Lit::Float(v) => Value::Float(*v),
        Lit::Bool(v) => Value::Bool(*v),
        Lit::Str(s) => Value::Str(s.clone()),
        Lit::DateTime(v) => Value::DateTime(*v),
        Lit::Uuid(u) => Value::Uuid(*u),
        Lit::Bytes(b) => Value::Bytes(b.clone()),
        Lit::Json(text) => Value::Json(
            powdb_storage::pj1::parse_json_text(text)
                .map_err(|e| format!("canonicalise {text}: {e:?}"))?
                .into_boxed_slice(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColType;

    /// Built by column *name* rather than as a positional literal list: a
    /// hand-written list shorter than the schema would be silently truncated by
    /// the `zip` in `insert_powql`, and the assertion would then pass while
    /// testing fewer columns than it claims to.
    #[test]
    fn insert_statement_omits_nulls_and_keeps_the_key() {
        let mut row: FixRow = COLUMNS.iter().map(|_| Lit::Null).collect();
        let put = |row: &mut FixRow, name: &str, v: Lit| {
            let idx = COLUMNS
                .iter()
                .position(|c| c.name == name)
                .expect("known column");
            row[idx] = v;
        };
        put(&mut row, "id", Lit::Int(7));
        put(&mut row, "f", Lit::Float(1.5));
        put(&mut row, "s", Lit::Str("x".into()));
        assert_eq!(row.len(), COLUMNS.len());

        let stmt = insert_powql(&row).expect("statement");
        assert_eq!(stmt, "insert T { id := 7, f := 1.5, s := \"x\" }");
    }

    #[test]
    fn insert_statement_refuses_a_null_key() {
        let row: FixRow = COLUMNS.iter().map(|_| Lit::Null).collect();
        assert!(insert_powql(&row).is_err());
    }

    #[test]
    fn storage_conversion_carries_non_finite_floats() {
        let v = lit_to_value(&Lit::Float(f64::NAN)).expect("nan value");
        match v {
            Value::Float(f) => assert!(f.is_nan()),
            other => panic!("expected float, got {other:?}"),
        }
    }

    #[test]
    fn schema_declares_every_column_type() {
        // A column type that never reaches the DDL is a coverage hole, so the
        // declared list is checked against the type enum here.
        let ddl: Vec<&str> = COLUMNS.iter().map(|c| c.ty.powql()).collect();
        for ty in [
            ColType::Int,
            ColType::Float,
            ColType::Bool,
            ColType::Str,
            ColType::DateTime,
            ColType::Uuid,
            ColType::Bytes,
            ColType::Json,
        ] {
            assert!(
                ddl.contains(&ty.powql()),
                "type {} is not in the fixture schema",
                ty.powql()
            );
        }
    }
}
