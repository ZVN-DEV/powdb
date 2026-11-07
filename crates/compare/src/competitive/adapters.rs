use super::{ensure_empty, timed_point, Database, Result, Row};
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn open(engine: &str, directory: &Path, prepare: bool) -> Result<Box<dyn Database>> {
    let path = directory.join("engine");
    if prepare {
        ensure_empty(&path)?;
    }
    match engine {
        #[cfg(feature = "competitive-powdb")]
        "powdb-powql" | "powdb-sql" => Ok(Box::new(Powdb {
            engine: powdb_query::executor::Engine::new(&path).map_err(|e| e.to_string())?,
            sql: engine == "powdb-sql",
        })),
        #[cfg(feature = "competitive-sqlite")]
        "sqlite" => {
            let connection =
                rusqlite::Connection::open(path.join("data.sqlite3")).map_err(|e| e.to_string())?;
            connection
                .execute_batch(
                    "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA threads=1;",
                )
                .map_err(|e| e.to_string())?;
            Ok(Box::new(Sqlite(connection)))
        }
        #[cfg(feature = "competitive-duckdb")]
        "duckdb" => {
            let connection =
                duckdb::Connection::open(path.join("data.duckdb")).map_err(|e| e.to_string())?;
            connection.execute_batch("SET threads=1; SET enable_external_access=false; SET autoinstall_known_extensions=false; SET autoload_known_extensions=false;")
                .map_err(|e| e.to_string())?;
            Ok(Box::new(Duckdb(connection)))
        }
        #[cfg(feature = "competitive-postgres")]
        "postgres" => {
            let connection = postgres::Config::new()
                .host_path(directory.join("pg-socket"))
                .user("bench")
                .dbname("postgres")
                .options("-c max_parallel_workers_per_gather=0")
                .connect(postgres::NoTls)
                .map_err(postgres_error)?;
            Ok(Box::new(Postgres(connection)))
        }
        #[cfg(feature = "competitive-mysql")]
        "mysql" => {
            let options = mysql::OptsBuilder::new()
                .socket(Some(
                    directory
                        .join("mysql-socket/mysql.sock")
                        .to_string_lossy()
                        .into_owned(),
                ))
                .user(Some("root"))
                .db_name(Some("bench"));
            let connection = mysql::Conn::new(options).map_err(|e| e.to_string())?;
            Ok(Box::new(Mysql(connection)))
        }
        #[cfg(feature = "competitive-wire")]
        "powdb-server" | "powdb-server-sql" => Ok(Box::new(Wire::open(
            &directory.join("powdb-socket/powdb.sock"),
            engine == "powdb-server-sql",
        )?)),
        _ => Err(format!(
            "engine {engine} is not enabled in this worker build"
        )),
    }
}

#[cfg(feature = "competitive-powdb")]
struct Powdb {
    engine: powdb_query::executor::Engine,
    sql: bool,
}

#[cfg(feature = "competitive-powdb")]
impl Database for Powdb {
    fn bound_points(&mut self, ids: &[i64]) -> Result<Value> {
        if self.sql {
            return Err("unsupported: SQL frontend has no parameterized prepare API".into());
        }
        let statement = self
            .engine
            .prepare("BenchRow filter .id = 0 { .id, .name, .amount }")
            .map_err(|e| e.to_string())?;
        let samples = ids
            .iter()
            .map(|id| {
                timed_point(|| {
                    let result = self
                        .engine
                        .execute_prepared(&statement, &[powdb_query::ast::Literal::Int(*id)])
                        .map_err(|e| e.to_string())?;
                    match result {
                        powdb_query::result::QueryResult::Rows { mut rows, .. }
                            if rows.len() == 1 =>
                        {
                            rows.remove(0).into_iter().map(powdb_cell).collect()
                        }
                        other => Err(format!(
                            "incorrect: bound point did not return one row: {other:?}"
                        )),
                    }
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(
            json!({"samples":samples,"producer":"materialized-prepared-point","binding":"PreparedQuery literal substitution"}),
        )
    }
    fn command(&mut self, query: &str) -> Result<()> {
        self.engine
            .execute_powql(query)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn visit(&mut self, query: &str, visitor: &mut dyn FnMut(Row)) -> Result<()> {
        use powdb_query::result::QueryResult;
        let result = if self.sql {
            self.engine.execute_sql(query)
        } else {
            self.engine.execute_powql(query)
        }
        .map_err(|e| e.to_string())?;
        match result {
            QueryResult::Rows { rows, .. } => {
                for row in rows {
                    visitor(row.into_iter().map(powdb_cell).collect::<Result<_>>()?);
                }
            }
            QueryResult::Scalar(value) => visitor(vec![powdb_cell(value)?]),
            other => return Err(format!("adapter: expected rows/scalar, got {other:?}")),
        }
        Ok(())
    }

    fn metadata(&mut self) -> Result<Value> {
        Ok(
            json!({"version":env!("CARGO_PKG_VERSION"),"durability":"Full","query_memory_limit_bytes":268435456,"threads_per_query":1,"surface":if self.sql {"sql"} else {"powql"}}),
        )
    }

    fn source_language(&self) -> &'static str {
        "powql"
    }
    fn producer(&self) -> &'static str {
        "materialized-engine-result"
    }
    fn checkpoint(&mut self) -> Result<()> {
        self.engine
            .catalog_mut()
            .checkpoint()
            .map_err(|e| e.to_string())
    }
}

#[cfg(any(feature = "competitive-powdb", feature = "competitive-wire"))]
fn powdb_cell(value: powdb_storage::types::Value) -> Result<Value> {
    use powdb_storage::types::Value as Cell;
    match value {
        Cell::Empty => Ok(Value::Null),
        Cell::Int(value) => Ok(json!(value)),
        Cell::Float(value) if value.is_finite() => Ok(json!(value)),
        Cell::Str(value) => Ok(json!(value)),
        Cell::Bool(value) => Ok(json!(value)),
        Cell::Bytes(value) => Ok(json!({"bytes":value})),
        other => Err(format!(
            "adapter: unhandled PowDB result type {:?}",
            other.type_id()
        )),
    }
}

#[cfg(feature = "competitive-wire")]
struct Wire {
    socket: std::os::unix::net::UnixStream,
    sql: bool,
    version: String,
}

#[cfg(feature = "competitive-wire")]
impl Wire {
    fn open(path: &Path, sql: bool) -> Result<Self> {
        use powdb_server::protocol::{ClientHello, Message};
        let socket = std::os::unix::net::UnixStream::connect(path).map_err(|e| e.to_string())?;
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(90)))
            .map_err(|e| e.to_string())?;
        socket
            .set_write_timeout(Some(std::time::Duration::from_secs(90)))
            .map_err(|e| e.to_string())?;
        let mut value = Self {
            socket,
            sql,
            version: String::new(),
        };
        let hello = value.exchange(Message::ConnectWithHello {
            db_name: "default".into(),
            password: None,
            username: None,
            hello: ClientHello::current(),
        })?;
        value.version = match hello {
            Message::ConnectOkWithHello { version, .. } => version,
            other => return Err(format!("adapter: unexpected hello: {other:?}")),
        };
        Ok(value)
    }

    fn exchange(
        &mut self,
        message: powdb_server::protocol::Message,
    ) -> Result<powdb_server::protocol::Message> {
        use powdb_server::protocol::Message;
        use std::io::{Read, Write};
        self.socket
            .write_all(&message.encode())
            .map_err(|e| e.to_string())?;
        let mut header = [0_u8; 6];
        self.socket
            .read_exact(&mut header)
            .map_err(|e| e.to_string())?;
        let length = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;
        if length > 256 * 1024 * 1024 {
            return Err("adapter: result frame exceeds collector cap".into());
        }
        let mut frame = Vec::with_capacity(6 + length);
        frame.extend_from_slice(&header);
        frame.resize(6 + length, 0);
        self.socket
            .read_exact(&mut frame[6..])
            .map_err(|e| e.to_string())?;
        Message::decode(&frame).map_err(|e| e.to_string())
    }
}

#[cfg(feature = "competitive-wire")]
impl Database for Wire {
    fn bound_points(&mut self, ids: &[i64]) -> Result<Value> {
        use powdb_server::protocol::{Message, WireParam};
        if self.sql {
            return Err("unsupported: SQL wire frontend has no parameterized message".into());
        }
        let samples = ids
            .iter()
            .map(|id| {
                timed_point(|| {
                    let result = self.exchange(Message::QueryWithParamsNative {
                        query: "BenchRow filter .id = $1 { .id, .name, .amount }".into(),
                        params: vec![WireParam::Int(*id)],
                    })?;
                    match result {
                        Message::ResultRowsNative { mut rows, .. } if rows.len() == 1 => {
                            rows.remove(0).into_iter().map(powdb_cell).collect()
                        }
                        other => Err(format!(
                            "incorrect: bound point did not return one row: {other:?}"
                        )),
                    }
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(
            json!({"samples":samples,"producer":"bound-native-wire-point","binding":"QueryWithParamsNative"}),
        )
    }
    fn command(&mut self, query: &str) -> Result<()> {
        use powdb_server::protocol::Message;
        match self.exchange(Message::QueryNative {
            query: query.into(),
        })? {
            Message::Error { message } | Message::ErrorWithClass { message, .. } => Err(message),
            Message::ResultOk { .. } | Message::ResultMessage { .. } => Ok(()),
            other => Err(format!("adapter: unexpected command response: {other:?}")),
        }
    }
    fn visit(&mut self, query: &str, visitor: &mut dyn FnMut(Row)) -> Result<()> {
        use powdb_server::protocol::Message;
        let request = if self.sql {
            Message::QuerySqlNative {
                query: query.into(),
            }
        } else {
            Message::QueryNative {
                query: query.into(),
            }
        };
        match self.exchange(request)? {
            Message::ResultRowsNative { rows, .. } => {
                for row in rows {
                    visitor(row.into_iter().map(powdb_cell).collect::<Result<_>>()?);
                }
            }
            Message::ResultScalarNative { value } => visitor(vec![powdb_cell(value)?]),
            Message::Error { message } | Message::ErrorWithClass { message, .. } => {
                return Err(message)
            }
            other => return Err(format!("adapter: unexpected query response: {other:?}")),
        }
        Ok(())
    }
    fn metadata(&mut self) -> Result<Value> {
        Ok(
            json!({"version":self.version,"transport":"private-unix-socket","native_typed_results":true,"surface":if self.sql {"sql"} else {"powql"},"durability":"Full"}),
        )
    }
    fn source_language(&self) -> &'static str {
        "powql"
    }
    fn producer(&self) -> &'static str {
        "materialized-native-wire-frame"
    }
    fn checkpoint(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(feature = "competitive-sqlite")]
struct Sqlite(rusqlite::Connection);

#[cfg(feature = "competitive-sqlite")]
impl Database for Sqlite {
    fn bound_points(&mut self, ids: &[i64]) -> Result<Value> {
        let mut statement = self
            .0
            .prepare("SELECT id,name,amount FROM BenchRow WHERE id=?1")
            .map_err(|e| e.to_string())?;
        let samples = ids
            .iter()
            .map(|id| {
                timed_point(|| {
                    statement
                        .query_row([id], |row| {
                            Ok(vec![
                                json!(row.get::<_, i64>(0)?),
                                json!(row.get::<_, String>(1)?),
                                json!(row.get::<_, i64>(2)?),
                            ])
                        })
                        .map_err(|e| format!("adapter: bound-point row mapping: {e}"))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(
            json!({"samples":samples,"producer":"materialized-prepared-point","binding":"prepared statement"}),
        )
    }
    fn command(&mut self, query: &str) -> Result<()> {
        self.0
            .execute_batch(&query.replace("id BIGINT PRIMARY KEY", "id INTEGER PRIMARY KEY"))
            .map_err(|e| e.to_string())
    }
    fn visit(&mut self, query: &str, visitor: &mut dyn FnMut(Row)) -> Result<()> {
        let mut statement = self.0.prepare(query).map_err(|e| e.to_string())?;
        let columns = statement.column_count();
        let mut rows = statement.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut values = Vec::with_capacity(columns);
            for column in 0..columns {
                use rusqlite::types::ValueRef;
                values.push(match row.get_ref(column).map_err(|e| e.to_string())? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(value) => json!(value),
                    ValueRef::Real(value) if value.is_finite() => json!(value),
                    ValueRef::Text(value) => {
                        json!(std::str::from_utf8(value).map_err(|e| e.to_string())?)
                    }
                    ValueRef::Blob(value) => json!({"bytes":value}),
                    other => return Err(format!("adapter: unhandled SQLite result {other:?}")),
                });
            }
            visitor(values);
        }
        Ok(())
    }
    fn metadata(&mut self) -> Result<Value> {
        let version: String = self
            .0
            .query_row("SELECT sqlite_version()", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let synchronous: i64 = self
            .0
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let journal: String = self
            .0
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let cache_pages: i64 = self
            .0
            .query_row("PRAGMA cache_size", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        Ok(
            json!({"version":version,"journal_mode":journal,"synchronous":synchronous,"cache_size":cache_pages,"surface":"sql"}),
        )
    }
    fn producer(&self) -> &'static str {
        "sqlite-step-cursor"
    }
    fn checkpoint(&mut self) -> Result<()> {
        self.command("PRAGMA wal_checkpoint(TRUNCATE)")
    }
}

#[cfg(feature = "competitive-duckdb")]
struct Duckdb(duckdb::Connection);

#[cfg(feature = "competitive-duckdb")]
impl Database for Duckdb {
    fn bound_points(&mut self, ids: &[i64]) -> Result<Value> {
        let mut statement = self
            .0
            .prepare("SELECT id,name,amount FROM BenchRow WHERE id=?")
            .map_err(|e| e.to_string())?;
        let samples = ids
            .iter()
            .map(|id| {
                timed_point(|| {
                    statement
                        .query_row([id], |row| {
                            Ok(vec![
                                json!(row.get::<_, i64>(0)?),
                                json!(row.get::<_, String>(1)?),
                                json!(row.get::<_, i64>(2)?),
                            ])
                        })
                        .map_err(|e| format!("adapter: bound-point row mapping: {e}"))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(
            json!({"samples":samples,"producer":"materialized-prepared-point","binding":"prepared statement"}),
        )
    }
    fn command(&mut self, query: &str) -> Result<()> {
        self.0.execute_batch(query).map_err(|e| e.to_string())
    }
    fn visit(&mut self, query: &str, visitor: &mut dyn FnMut(Row)) -> Result<()> {
        let mut statement = self.0.prepare(query).map_err(|e| e.to_string())?;
        let stream = statement.stream_arrow([]).map_err(|e| e.to_string())?;
        for batch in stream {
            let batch = batch.map_err(|e| e.to_string())?;
            for row in 0..batch.num_rows() {
                let values = batch
                    .columns()
                    .iter()
                    .map(|column| duckdb_arrow_cell(column.as_ref(), row))
                    .collect::<Result<_>>()?;
                visitor(values);
            }
        }
        Ok(())
    }
    fn metadata(&mut self) -> Result<Value> {
        let version: String = self
            .0
            .query_row("SELECT version()", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let memory: String = self
            .0
            .query_row("SELECT current_setting('memory_limit')", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        Ok(
            json!({"version":version,"threads":1,"memory_limit":memory,"external_access":false,"surface":"sql"}),
        )
    }
    fn producer(&self) -> &'static str {
        "duckdb-arrow-chunk-stream"
    }
    fn checkpoint(&mut self) -> Result<()> {
        self.command("CHECKPOINT")
    }
}

#[cfg(feature = "competitive-duckdb")]
fn duckdb_arrow_cell(array: &dyn duckdb::arrow::array::Array, row: usize) -> Result<Value> {
    use duckdb::arrow::{array::*, datatypes::DataType};
    if array.is_null(row) {
        return Ok(Value::Null);
    }
    macro_rules! cell {
        ($kind:ty) => {
            array
                .as_any()
                .downcast_ref::<$kind>()
                .ok_or("adapter: Arrow type mismatch")?
                .value(row)
        };
    }
    Ok(match array.data_type() {
        DataType::Null => Value::Null,
        DataType::Boolean => json!(cell!(BooleanArray)),
        DataType::Int8 => json!(cell!(Int8Array)),
        DataType::Int16 => json!(cell!(Int16Array)),
        DataType::Int32 => json!(cell!(Int32Array)),
        DataType::Int64 => json!(cell!(Int64Array)),
        DataType::UInt8 => json!(cell!(UInt8Array)),
        DataType::UInt16 => json!(cell!(UInt16Array)),
        DataType::UInt32 => json!(cell!(UInt32Array)),
        DataType::UInt64 => json!(cell!(UInt64Array)),
        DataType::Float32 => {
            let value = cell!(Float32Array);
            if !value.is_finite() {
                return Err("adapter: nonfinite float".into());
            }
            json!(value)
        }
        DataType::Float64 => {
            let value = cell!(Float64Array);
            if !value.is_finite() {
                return Err("adapter: nonfinite float".into());
            }
            json!(value)
        }
        DataType::Decimal128(_, 0) => {
            json!(i64::try_from(cell!(Decimal128Array)).map_err(|e| format!("adapter: {e}"))?)
        }
        DataType::Utf8 => json!(cell!(StringArray)),
        DataType::LargeUtf8 => json!(cell!(LargeStringArray)),
        DataType::Utf8View => json!(cell!(StringViewArray)),
        DataType::Binary => json!({"bytes":cell!(BinaryArray)}),
        DataType::LargeBinary => json!({"bytes":cell!(LargeBinaryArray)}),
        DataType::BinaryView => json!({"bytes":cell!(BinaryViewArray)}),
        other => return Err(format!("adapter: unhandled Arrow type {other:?}")),
    })
}

#[cfg(feature = "competitive-postgres")]
fn postgres_error(error: postgres::Error) -> String {
    if let Some(db) = error.as_db_error() {
        format!(
            "PostgreSQL {}: {}; detail: {}; hint: {}",
            db.code().code(),
            db.message(),
            db.detail().unwrap_or("none"),
            db.hint().unwrap_or("none")
        )
    } else {
        format!("PostgreSQL transport: {error:?}")
    }
}

#[cfg(feature = "competitive-postgres")]
struct Postgres(postgres::Client);

#[cfg(feature = "competitive-postgres")]
impl Database for Postgres {
    fn bound_points(&mut self, ids: &[i64]) -> Result<Value> {
        let statement = self
            .0
            .prepare("SELECT id,name,amount FROM BenchRow WHERE id=$1")
            .map_err(postgres_error)?;
        let samples = ids
            .iter()
            .map(|id| {
                timed_point(|| {
                    let row = self
                        .0
                        .query_one(&statement, &[id])
                        .map_err(postgres_error)?;
                    Ok(vec![
                        json!(row.try_get::<_, i64>(0).map_err(postgres_error)?),
                        json!(row.try_get::<_, String>(1).map_err(postgres_error)?),
                        json!(row.try_get::<_, i64>(2).map_err(postgres_error)?),
                    ])
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(
            json!({"samples":samples,"producer":"materialized-prepared-point","binding":"server prepared statement"}),
        )
    }
    fn command(&mut self, query: &str) -> Result<()> {
        self.0.batch_execute(query).map_err(|e| e.to_string())
    }
    fn visit(&mut self, query: &str, visitor: &mut dyn FnMut(Row)) -> Result<()> {
        // A portal fetches bounded batches. The transaction is deliberate and
        // counted, including server-side blocking work before the first row.
        let mut transaction = self.0.transaction().map_err(postgres_error)?;
        let portal = transaction.bind(query, &[]).map_err(postgres_error)?;
        loop {
            let rows = transaction
                .query_portal(&portal, 256)
                .map_err(postgres_error)?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                visitor(postgres_row(row)?);
            }
        }
        transaction.commit().map_err(|e| e.to_string())
    }
    fn visit_materialized(&mut self, query: &str, visitor: &mut dyn FnMut(Row)) -> Result<()> {
        // Use the normal complete-result API for materialization. Explicit
        // portal/transaction round trips belong only to the consumption probe.
        for row in self.0.query(query, &[]).map_err(postgres_error)? {
            visitor(postgres_row(row)?);
        }
        Ok(())
    }
    fn metadata(&mut self) -> Result<Value> {
        let mut settings = serde_json::Map::new();
        for name in [
            "server_version",
            "fsync",
            "synchronous_commit",
            "full_page_writes",
            "shared_buffers",
            "work_mem",
            "max_parallel_workers_per_gather",
        ] {
            let row = self
                .0
                .query_one(&format!("SHOW {name}"), &[])
                .map_err(postgres_error)?;
            settings.insert(name.into(), json!(row.get::<_, String>(0)));
        }
        Ok(Value::Object(settings))
    }
    fn producer(&self) -> &'static str {
        "postgres-portal-256"
    }
    fn materialized_producer(&self) -> &'static str {
        "postgres-query-rows"
    }
    fn checkpoint(&mut self) -> Result<()> {
        self.command("CHECKPOINT")
    }
}

#[cfg(feature = "competitive-postgres")]
fn postgres_row(row: postgres::Row) -> Result<Row> {
    use postgres::types::Type;
    let mut values = Vec::with_capacity(row.len());
    for (index, column) in row.columns().iter().enumerate() {
        let value = match *column.type_() {
            Type::INT8 => row
                .try_get::<_, Option<i64>>(index)
                .map(|v| v.map_or(Value::Null, |v| json!(v))),
            Type::INT4 => row
                .try_get::<_, Option<i32>>(index)
                .map(|v| v.map_or(Value::Null, |v| json!(v))),
            Type::INT2 => row
                .try_get::<_, Option<i16>>(index)
                .map(|v| v.map_or(Value::Null, |v| json!(v))),
            Type::FLOAT8 => row
                .try_get::<_, Option<f64>>(index)
                .map(|v| v.map_or(Value::Null, |v| json!(v))),
            Type::FLOAT4 => row
                .try_get::<_, Option<f32>>(index)
                .map(|v| v.map_or(Value::Null, |v| json!(v))),
            Type::BOOL => row
                .try_get::<_, Option<bool>>(index)
                .map(|v| v.map_or(Value::Null, |v| json!(v))),
            Type::TEXT | Type::VARCHAR | Type::BPCHAR => row
                .try_get::<_, Option<String>>(index)
                .map(|v| v.map_or(Value::Null, |v| json!(v))),
            _ => {
                return Err(format!(
                    "adapter: unhandled PostgreSQL type {}",
                    column.type_()
                ))
            }
        }
        .map_err(postgres_error)?;
        values.push(value);
    }
    Ok(values)
}

#[cfg(feature = "competitive-mysql")]
struct Mysql(mysql::Conn);

#[cfg(feature = "competitive-mysql")]
impl Database for Mysql {
    fn bound_points(&mut self, ids: &[i64]) -> Result<Value> {
        use mysql::prelude::Queryable;
        let statement = self
            .0
            .prep("SELECT id,name,amount FROM BenchRow WHERE id=?")
            .map_err(|e| e.to_string())?;
        let samples = ids
            .iter()
            .map(|id| {
                timed_point(|| {
                    let row: Option<(i64, String, i64)> = self
                        .0
                        .exec_first(&statement, (id,))
                        .map_err(|e| e.to_string())?;
                    let (id, name, amount) = row.ok_or("incorrect: missing bound point row")?;
                    Ok(vec![json!(id), json!(name), json!(amount)])
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.0.close(statement).map_err(|e| e.to_string())?;
        Ok(
            json!({"samples":samples,"producer":"materialized-prepared-point","binding":"server prepared statement"}),
        )
    }
    fn command(&mut self, query: &str) -> Result<()> {
        use mysql::prelude::Queryable;
        self.0.query_drop(query).map_err(|e| e.to_string())
    }
    fn visit(&mut self, query: &str, visitor: &mut dyn FnMut(Row)) -> Result<()> {
        use mysql::prelude::Queryable;
        let result = self.0.query_iter(query).map_err(|e| e.to_string())?;
        for row in result {
            let row = row.map_err(|e| e.to_string())?;
            let mut values = Vec::with_capacity(row.len());
            for index in 0..row.len() {
                let column = &row.columns_ref()[index];
                values.push(mysql_cell(
                    row.as_ref(index).ok_or("adapter: missing MySQL cell")?,
                    column.column_type(),
                    column.flags(),
                )?);
            }
            visitor(values);
        }
        Ok(())
    }
    fn metadata(&mut self) -> Result<Value> {
        use mysql::prelude::Queryable;
        let version: Option<String> = self
            .0
            .query_first("SELECT VERSION()")
            .map_err(|e| e.to_string())?;
        let flush: Option<u64> = self
            .0
            .query_first("SELECT @@innodb_flush_log_at_trx_commit")
            .map_err(|e| e.to_string())?;
        let sync_binlog: Option<u64> = self
            .0
            .query_first("SELECT @@sync_binlog")
            .map_err(|e| e.to_string())?;
        let buffer_pool: Option<u64> = self
            .0
            .query_first("SELECT @@innodb_buffer_pool_size")
            .map_err(|e| e.to_string())?;
        Ok(
            json!({"version":version,"innodb_flush_log_at_trx_commit":flush,"sync_binlog":sync_binlog,"innodb_buffer_pool_size":buffer_pool,"surface":"sql"}),
        )
    }
    fn producer(&self) -> &'static str {
        "mysql-query-iterator"
    }
    fn checkpoint(&mut self) -> Result<()> {
        self.command("FLUSH TABLES")
    }
}

#[cfg(feature = "competitive-mysql")]
fn mysql_cell(
    value: &mysql::Value,
    kind: mysql::consts::ColumnType,
    flags: mysql::consts::ColumnFlags,
) -> Result<Value> {
    use mysql::consts::{ColumnFlags, ColumnType};
    fn float(text: &str) -> Result<Value> {
        let value = text
            .parse::<f64>()
            .map_err(|e| format!("adapter: MySQL numeric conversion: {e}"))?;
        if !value.is_finite() {
            return Err("adapter: non-finite MySQL numeric value".into());
        }
        Ok(json!(value))
    }
    fn integer(text: &str, unsigned: bool) -> Result<Value> {
        if unsigned {
            text.parse::<u64>()
                .map(|v| json!(v))
                .map_err(|e| format!("adapter: MySQL unsigned integer: {e}"))
        } else {
            text.parse::<i64>()
                .map(|v| json!(v))
                .map_err(|e| format!("adapter: MySQL signed integer: {e}"))
        }
    }
    match value {
        mysql::Value::NULL => Ok(Value::Null),
        mysql::Value::Int(v) => Ok(json!(v)),
        mysql::Value::UInt(v) => Ok(json!(v)),
        mysql::Value::Float(v) if v.is_finite() => Ok(json!(v)),
        mysql::Value::Double(v) if v.is_finite() => Ok(json!(v)),
        mysql::Value::Bytes(bytes) => {
            let text =
                std::str::from_utf8(bytes).map_err(|e| format!("adapter: MySQL UTF-8: {e}"))?;
            let unsigned = flags.contains(ColumnFlags::UNSIGNED_FLAG);
            match kind {
                ColumnType::MYSQL_TYPE_TINY
                | ColumnType::MYSQL_TYPE_SHORT
                | ColumnType::MYSQL_TYPE_LONG
                | ColumnType::MYSQL_TYPE_LONGLONG
                | ColumnType::MYSQL_TYPE_INT24
                | ColumnType::MYSQL_TYPE_YEAR => integer(text, unsigned),
                ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE => float(text),
                ColumnType::MYSQL_TYPE_NEWDECIMAL | ColumnType::MYSQL_TYPE_DECIMAL => {
                    // SUM(BIGINT) is DECIMAL in MySQL's text protocol. Preserve
                    // exact integral results; never route an overflowing int
                    // through f64. This fixture's fractional decimal is AVG.
                    if let Some((whole, fraction)) = text.split_once('.') {
                        if fraction.bytes().all(|b| b == b'0') {
                            integer(whole, unsigned)
                        } else {
                            float(text)
                        }
                    } else {
                        integer(text, unsigned)
                    }
                }
                _ => Ok(json!(text)),
            }
        }
        other => Err(format!("adapter: unhandled MySQL result {other:?}")),
    }
}

#[cfg(all(test, feature = "competitive-mysql"))]
mod mysql_decoding_tests {
    use super::*;
    use mysql::consts::{ColumnFlags, ColumnType};
    #[test]
    fn text_and_binary_protocol_preserve_exact_integer_types() {
        for value in [i64::MIN, -483, 17, 9_007_199_254_740_993, i64::MAX] {
            let text = mysql::Value::Bytes(value.to_string().into_bytes());
            assert_eq!(
                mysql_cell(&text, ColumnType::MYSQL_TYPE_LONGLONG, ColumnFlags::empty()).unwrap(),
                json!(value)
            );
            assert_eq!(
                mysql_cell(
                    &mysql::Value::Int(value),
                    ColumnType::MYSQL_TYPE_LONGLONG,
                    ColumnFlags::empty()
                )
                .unwrap(),
                json!(value)
            );
        }
        assert_eq!(
            mysql_cell(
                &mysql::Value::Bytes(u64::MAX.to_string().into_bytes()),
                ColumnType::MYSQL_TYPE_LONGLONG,
                ColumnFlags::UNSIGNED_FLAG
            )
            .unwrap(),
            json!(u64::MAX)
        );
    }
    #[test]
    fn numeric_text_stays_text_and_null_stays_null() {
        assert_eq!(
            mysql_cell(
                &mysql::Value::Bytes(b"0017".to_vec()),
                ColumnType::MYSQL_TYPE_VAR_STRING,
                ColumnFlags::empty()
            )
            .unwrap(),
            json!("0017")
        );
        assert_eq!(
            mysql_cell(
                &mysql::Value::NULL,
                ColumnType::MYSQL_TYPE_LONGLONG,
                ColumnFlags::empty()
            )
            .unwrap(),
            Value::Null
        );
    }
    #[test]
    fn decimal_integrals_do_not_lose_precision() {
        assert_eq!(
            mysql_cell(
                &mysql::Value::Bytes(b"9007199254740993.0000".to_vec()),
                ColumnType::MYSQL_TYPE_NEWDECIMAL,
                ColumnFlags::empty()
            )
            .unwrap(),
            json!(9_007_199_254_740_993_i64)
        );
        assert_eq!(
            mysql_cell(
                &mysql::Value::Bytes(b"-0.5000".to_vec()),
                ColumnType::MYSQL_TYPE_NEWDECIMAL,
                ColumnFlags::empty()
            )
            .unwrap(),
            json!(-0.5)
        );
        assert!(mysql_cell(
            &mysql::Value::Bytes(b"9223372036854775808".to_vec()),
            ColumnType::MYSQL_TYPE_NEWDECIMAL,
            ColumnFlags::empty()
        )
        .is_err());
        assert!(mysql_cell(
            &mysql::Value::Bytes(b"NaN".to_vec()),
            ColumnType::MYSQL_TYPE_DOUBLE,
            ColumnFlags::empty()
        )
        .is_err());
    }
}
