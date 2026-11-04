//! Paired, read-only query recovery evaluator; not a durable-write benchmark.
use powdb_query::{executor::Engine, result::QueryResult};
use powdb_storage::types::Value;
use serde_json::{json, Value as Json};
use std::{path::Path, process::Command, time::Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const GROUP: &str = "Comment group 1 { cnt: count(.id), total: sum(.score), mean: avg(.rating), latest: max(.created_at) }";

fn arg(args: &[String], key: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == key).map(|w| w[1].clone())
}

fn seed(path: &Path, rows: usize) -> Result<()> {
    let mut engine = Engine::new(path)?;
    engine.execute_powql("type Comment { required id: int, required note_id: str, required user_id: str, required body: str, required score: int, required rating: float, required created_at: datetime }")?;
    let body = "x".repeat(128);
    // Offline fixture construction, followed by checkpoint. This bypasses
    // statement/WAL dispatch and makes no write-throughput/durability claim.
    for id in 0..rows {
        engine
            .catalog_mut()
            .get_table_mut("Comment")
            .ok_or("missing table")?
            .insert(&vec![
                Value::Int(id as i64),
                Value::Str(format!("note_{}", id % 150_000)),
                Value::Str(format!("user_{}", id % 20_000)),
                Value::Str(body.clone()),
                Value::Int((id % 100) as i64),
                Value::Float((id % 20) as f64 / 2.0),
                Value::DateTime(1_700_000_000_000_000 + id as i64),
            ])?;
    }
    engine
        .catalog_mut()
        .create_index_unique("Comment", "id", true)?;
    engine
        .catalog_mut()
        .create_index_unique("Comment", "note_id", false)?;
    engine.catalog_mut().checkpoint()?;
    Ok(())
}

fn ids(result: QueryResult) -> Result<Vec<i64>> {
    let QueryResult::Rows { rows, .. } = result else {
        return Err("expected rows".into());
    };
    let mut out = rows
        .into_iter()
        .map(|r| match r.first() {
            Some(Value::Int(n)) => Ok(*n),
            _ => Err("noninteger id".into()),
        })
        .collect::<Result<Vec<_>>>()?;
    out.sort_unstable();
    Ok(out)
}

fn measure(path: &Path, rows: usize, round: usize, readonly: bool) -> Result<Json> {
    let opened = Instant::now();
    let mut engine = if readonly {
        Engine::open_read_only(path)?
    } else {
        Engine::new(path)?
    };
    let open_ns = opened.elapsed().as_nanos().to_string();
    let first = (round * 37) % 149_980;
    let keys = (first..first + 20)
        .map(|k| format!("\"note_{k}\""))
        .collect::<Vec<_>>()
        .join(",");
    let query = format!("Comment filter .note_id in ({keys}) {{ .id }}");
    let plan = format!("{:?}", engine.execute_powql(&format!("explain {query}"))?);
    let expected = (0..rows)
        .filter(|id| (first..first + 20).contains(&(id % 150_000)))
        .map(|id| id as i64)
        .collect::<Vec<_>>();
    let start = Instant::now();
    let result = engine.execute_powql(&query)?;
    let in_ns = start.elapsed().as_nanos() as f64;
    if ids(result)? != expected {
        return Err("IN result mismatch".into());
    }
    let start = Instant::now();
    let aggregate = engine.execute_powql(GROUP);
    let aggregate_ns = start.elapsed().as_nanos() as f64;
    let aggregate = match aggregate {
        Ok(QueryResult::Rows {
            columns,
            rows: data,
        }) => {
            if data.len() != 1 {
                return Err("aggregate row count mismatch".into());
            }
            let cell = |name: &str| -> Result<&Value> {
                Ok(&data[0][columns
                    .iter()
                    .position(|c| c == name)
                    .ok_or("aggregate column missing")?])
            };
            let sum = (0..rows).map(|i| (i % 100) as i64).sum::<i64>();
            let avg = (0..rows).map(|i| (i % 20) as f64 / 2.0).sum::<f64>() / rows as f64;
            if cell("cnt")? != &Value::Int(rows as i64)
                || cell("total")? != &Value::Int(sum)
                || cell("mean")? != &Value::Float(avg)
                || cell("latest")? != &Value::DateTime(1_700_000_000_000_000 + rows as i64 - 1)
            {
                return Err("aggregate value mismatch".into());
            }
            json!({"ok":true})
        }
        Ok(other) => return Err(format!("unexpected aggregate: {other:?}").into()),
        Err(e) => json!({"ok":false,"error":e.to_string()}),
    };
    Ok(
        json!({"rows":rows,"round":round,"readonly":readonly,"wal_mode":"Full","query_memory_limit":engine.query_memory_limit(),"open_ns":open_ns,"in_ns":in_ns,"matches":expected.len(),"plan":plan,"aggregate_ns":aggregate_ns,"aggregate":aggregate,"correctness":true}),
    )
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    xs[xs.len() / 2]
}

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let rows = arg(&args, "--rows")
        .unwrap_or("1000000".into())
        .parse::<usize>()?;
    let repetitions = arg(&args, "--repetitions")
        .unwrap_or("5".into())
        .parse::<usize>()?;
    if rows == 0 || rows > 2_000_000 || repetitions == 0 || repetitions > 50 {
        return Err("invalid fixture/sample bound".into());
    }
    if let Some(path) = arg(&args, "--measure") {
        let round = arg(&args, "--round").unwrap_or("0".into()).parse()?;
        println!(
            "{}",
            measure(
                Path::new(&path),
                rows,
                round,
                args.iter().any(|a| a == "--readonly")
            )?
        );
        return Ok(());
    }
    if let Some(path) = arg(&args, "--seed") {
        if Path::new(&path).exists() {
            return Err("seed destination must not exist".into());
        }
        seed(Path::new(&path), rows)?;
        return Ok(());
    }
    let baseline = arg(&args, "--baseline-bin")
        .or_else(|| std::env::var("POWDB_QUERY_RECOVERY_BASELINE").ok())
        .ok_or("explicit frozen baseline binary is required")?;
    if repetitions < 5 {
        return Err("paired evaluator needs at least five rounds".into());
    }
    let candidate = std::env::current_exe()?;
    if std::fs::read(&baseline)? == std::fs::read(&candidate)? {
        return Err("identical baseline/candidate binaries".into());
    }
    let dir = tempfile::tempdir()?;
    let fixture = dir.path().join("fixture");
    seed(&fixture, rows)?;
    let mut samples = Vec::new();
    for round in 0..repetitions {
        for readonly in [false, true] {
            for role in if round.is_multiple_of(2) {
                ["baseline", "candidate"]
            } else {
                ["candidate", "baseline"]
            } {
                let bin = if role == "baseline" {
                    Path::new(&baseline)
                } else {
                    &candidate
                };
                let mut cmd = Command::new(bin);
                cmd.args([
                    "--measure",
                    fixture.to_str().ok_or("nonUTF8 path")?,
                    "--rows",
                    &rows.to_string(),
                    "--round",
                    &round.to_string(),
                ]);
                if readonly {
                    cmd.arg("--readonly");
                }
                let out = cmd.output()?;
                if !out.status.success() {
                    return Err(
                        format!("{role} failed: {}", String::from_utf8_lossy(&out.stderr)).into(),
                    );
                }
                let mut sample: Json = serde_json::from_slice(&out.stdout)?;
                sample["role"] = json!(role);
                samples.push(sample);
            }
        }
    }
    let mut pass = true;
    let mut summaries = Vec::new();
    for readonly in [false, true] {
        let values = |role: &str| {
            samples
                .iter()
                .filter(|s| s["role"] == role && s["readonly"] == readonly)
                .map(|s| s["in_ns"].as_f64().unwrap_or(f64::INFINITY))
                .collect()
        };
        let a = median(values("baseline"));
        let b = median(values("candidate"));
        let aggregate_pass = samples
            .iter()
            .filter(|s| s["role"] == "candidate" && s["readonly"] == readonly)
            .all(|s| s["aggregate"]["ok"] == true);
        let access_pass = samples
            .iter()
            .filter(|s| s["role"] == "candidate" && s["readonly"] == readonly)
            .all(|s| !s["plan"].as_str().unwrap_or("").contains("SeqScan"));
        pass &= a / b >= 10.0 && aggregate_pass && access_pass;
        summaries.push(json!({"readonly":readonly,"baseline_in_ns":a,"candidate_in_ns":b,"speedup":a/b,"aggregate_pass":aggregate_pass,"access_pass":access_pass}));
    }
    println!(
        "{}",
        json!({"schema":1,"diagnostic_only":true,"durable_write_claim":false,"pass":pass,"summaries":summaries,"samples":samples})
    );
    if !pass {
        return Err("query recovery gate failed; inspect JSON outcomes".into());
    }
    Ok(())
}
