use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

mod adapters;

type Row = Vec<Value>;
type Result<T> = std::result::Result<T, String>;

trait Database {
    fn command(&mut self, query: &str) -> Result<()>;
    fn visit(&mut self, query: &str, visitor: &mut dyn FnMut(Row)) -> Result<()>;
    fn visit_materialized(&mut self, query: &str, visitor: &mut dyn FnMut(Row)) -> Result<()> {
        self.visit(query, visitor)
    }
    fn metadata(&mut self) -> Result<Value>;
    fn source_language(&self) -> &'static str {
        "sql"
    }
    fn producer(&self) -> &'static str;
    fn checkpoint(&mut self) -> Result<()>;
    fn bound_points(&mut self, _ids: &[i64]) -> Result<Value> {
        Err("adapter: bound-point experiment is not implemented for this adapter".into())
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Delivery {
    Materialized,
    Consume,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    #[serde(default)]
    queries: Vec<String>,
    delivery: Delivery,
    #[serde(default)]
    bound_ids: Option<Vec<i64>>,
}

fn timed_point(operation: impl FnOnce() -> Result<Row>) -> Result<Value> {
    let start = Instant::now();
    let row = operation()?;
    let elapsed = start.elapsed().as_nanos();
    let mut digest = blake3::Hasher::new();
    digest_row(&mut digest, &row);
    Ok(json!({"elapsed_ns":elapsed,"first_row_ns":elapsed,"rows":1,
        "digest":digest.finalize().to_hex().to_string(),"preview":[row]}))
}

fn digest_row(hasher: &mut blake3::Hasher, row: &Row) {
    // Serialize complete typed values. Delimit rows, including empty rows, so
    // concatenations cannot collide merely because their boundaries differ.
    let encoded = serde_json::to_vec(row).expect("JSON values always serialize");
    hasher.update(&(encoded.len() as u64).to_le_bytes());
    hasher.update(&encoded);
}

fn emit(value: &Value) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, value).map_err(|e| e.to_string())?;
    writeln!(output).map_err(|e| e.to_string())?;
    output.flush().map_err(|e| e.to_string())
}

fn measure(database: &mut dyn Database, request: Request) -> Value {
    if let Some(ids) = request.bound_ids {
        if ids.is_empty()
            || ids.len() > 10000
            || !matches!(request.delivery, Delivery::Materialized)
        {
            return json!({"error":"bound points require 1..10000 ids and materialized delivery"});
        }
        return database
            .bound_points(&ids)
            .unwrap_or_else(|error| json!({"error":error}));
    }
    if request.queries.is_empty() || request.queries.len() > 10_000 {
        return json!({"error":"request requires 1..10000 queries"});
    }
    let mut samples = Vec::with_capacity(request.queries.len());
    for query in &request.queries {
        let mut hasher = blake3::Hasher::new();
        let mut count = 0_usize;
        let mut first_row_ns = None;
        let start = Instant::now();
        let (outcome, elapsed_ns, preview) = match request.delivery {
            Delivery::Materialized => {
                let mut rows = Vec::new();
                let result = database.visit_materialized(query, &mut |row| rows.push(row));
                // Full application result materialization belongs inside
                // timing for EVERY engine in this delivery cohort.
                let elapsed = start.elapsed().as_nanos();
                if !rows.is_empty() {
                    first_row_ns = Some(elapsed);
                }
                count = rows.len();
                for row in &rows {
                    digest_row(&mut hasher, row);
                }
                let preview: Vec<_> = rows.into_iter().take(3).collect();
                (result, elapsed, preview)
            }
            Delivery::Consume => {
                let mut preview = Vec::new();
                let result = database.visit(query, &mut |row| {
                    if first_row_ns.is_none() {
                        first_row_ns = Some(start.elapsed().as_nanos());
                    }
                    count += 1;
                    // Streaming consumption includes a bounded checksum sink.
                    // It never retains the full result in the controller.
                    digest_row(&mut hasher, &row);
                    if preview.len() < 3 {
                        preview.push(row);
                    }
                });
                (result, start.elapsed().as_nanos(), preview)
            }
        };
        match outcome {
            Ok(()) => samples.push(json!({
                "elapsed_ns":elapsed_ns,
                "first_row_ns":first_row_ns,
                "rows":count,
                "digest":hasher.finalize().to_hex().to_string(),
                "preview":preview
            })),
            Err(error) => {
                return json!({"error":error,"completed_samples":samples,"partial_rows":count,"partial_first_row_ns":first_row_ns,"failed_elapsed_ns":elapsed_ns})
            }
        }
    }
    json!({"samples":samples,"producer":database.producer()})
}

fn fixture(database: &mut dyn Database, rows: usize) -> Result<()> {
    let powql = database.source_language() == "powql";
    if powql {
        database.command("type BenchRow { required unique id: int, required grp: int, required bucket: int, required amount: int, required score: int, required name: str, optional_value: int, required payload: str }")?;
        database.command("type BenchGroup { required unique id: int, required label: str }")?;
    } else {
        // SQLite's native INTEGER PRIMARY KEY is a signed 64-bit row key.
        // Other adapters use BIGINT, retaining the same logical domain.
        database.command("CREATE TABLE BenchRow (id BIGINT PRIMARY KEY, grp BIGINT NOT NULL, bucket BIGINT NOT NULL, amount BIGINT NOT NULL, score BIGINT NOT NULL, name TEXT NOT NULL, optional_value BIGINT, payload TEXT NOT NULL)")?;
        database.command("CREATE TABLE BenchGroup (id BIGINT PRIMARY KEY, label TEXT NOT NULL)")?;
    }
    database.command(if powql { "begin" } else { "BEGIN" })?;
    for chunk in (0..rows).step_by(256) {
        let mut records = Vec::new();
        for id in chunk..(chunk + 256).min(rows) {
            let optional = if id % 7 == 0 {
                "null".to_string()
            } else {
                (id % 97).to_string()
            };
            records.push(if powql {
                format!("{{ id := {id}, grp := {}, bucket := {}, amount := {}, score := {}, name := \"user_{id}\", optional_value := {optional}, payload := \"{}\" }}",id%64,id%1000,id as i64%1000-500,(id*37)%1000,"x".repeat(128))
            } else {
                format!("({id},{},{},{},{},'user_{id}',{optional},'{}')",id%64,id%1000,id as i64%1000-500,(id*37)%1000,"x".repeat(128))
            });
        }
        let prefix = if powql {
            "insert BenchRow "
        } else {
            "INSERT INTO BenchRow VALUES "
        };
        database.command(&format!("{prefix}{}", records.join(",")))?;
    }
    for id in 0..64 {
        database.command(&if powql {
            format!("insert BenchGroup {{ id := {id}, label := \"group_{id}\" }}")
        } else {
            format!("INSERT INTO BenchGroup VALUES ({id},'group_{id}')")
        })?;
    }
    database.command(if powql { "commit" } else { "COMMIT" })?;
    database.command(if powql {
        "alter BenchRow add index .bucket"
    } else {
        "CREATE INDEX bench_bucket ON BenchRow(bucket)"
    })?;
    database.checkpoint()
}

fn options() -> Result<(String, PathBuf, usize, bool)> {
    let mut engine = None;
    let mut directory = None;
    let mut rows = None;
    let mut prepare = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--engine" => engine = Some(value),
            "--data-dir" => directory = Some(PathBuf::from(value)),
            "--rows" => rows = Some(value.parse::<usize>().map_err(|e| e.to_string())?),
            "--phase" => {
                prepare = Some(match value.as_str() {
                    "prepare" => true,
                    "query" => false,
                    _ => return Err("--phase must be prepare or query".into()),
                })
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    let directory = directory.ok_or("--data-dir is required")?;
    if !directory.is_absolute() || !directory.join(".competitive-disposable").is_file() {
        return Err("refusing non-disposable or reused fixture directory".into());
    }
    let rows = rows.ok_or("--rows is required")?;
    if !(1_000..=1_000_000).contains(&rows) {
        return Err("--rows must be within 1000..1000000".into());
    }
    let prepare = prepare.ok_or("--phase is required")?;
    if directory.join("engine").exists() == prepare {
        return Err("fixture phase does not match engine directory state".into());
    }
    Ok((
        engine.ok_or("--engine is required")?,
        directory,
        rows,
        prepare,
    ))
}

pub fn run() -> Result<()> {
    let (engine, directory, rows, prepare) = options()?;
    let mut database = adapters::open(&engine, &directory, prepare)?;
    if prepare {
        fixture(database.as_mut(), rows)?;
    }
    let metadata = database.metadata()?;
    emit(
        &json!({"ready":true,"protocol_schema":1,"engine":engine,"rows":rows,"pid":std::process::id(),"metadata":metadata}),
    )?;
    for line in io::stdin().lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let request = serde_json::from_str::<Request>(&line).map_err(|e| e.to_string())?;
        emit(&measure(database.as_mut(), request))?;
    }
    Ok(())
}

fn ensure_empty(path: &Path) -> Result<()> {
    std::fs::create_dir(path).map_err(|e| format!("fresh engine directory: {e}"))
}
