//! Paired unindexed membership counts; fixture generation is not a write benchmark.
use powdb_query::{executor::Engine, result::QueryResult};
use powdb_storage::types::Value;
use serde_json::{json, Value as Json};
use std::{path::PathBuf, process::Command, time::Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const MARKER: &str = "unindexed-membership-fixture.json";

#[derive(Default)]
struct Options {
    rows: Option<usize>,
    repetitions: Option<usize>,
    round: usize,
    seed: Option<PathBuf>,
    measure: Option<PathBuf>,
    baseline: Option<PathBuf>,
    readonly: bool,
    not_in: bool,
}

impl Options {
    fn parse() -> Result<Self> {
        let mut o = Self::default();
        let mut args = std::env::args().skip(1);
        while let Some(key) = args.next() {
            match key.as_str() {
                "--readonly" => o.readonly = true,
                "--not-in" => o.not_in = true,
                "--rows" | "--repetitions" | "--round" | "--seed" | "--measure"
                | "--baseline-bin" => {
                    let value = args.next().ok_or("option requires a value")?;
                    match key.as_str() {
                        "--rows" => o.rows = Some(value.parse()?),
                        "--repetitions" => o.repetitions = Some(value.parse()?),
                        "--round" => o.round = value.parse()?,
                        "--seed" => o.seed = Some(value.into()),
                        "--measure" => o.measure = Some(value.into()),
                        "--baseline-bin" => o.baseline = Some(value.into()),
                        _ => unreachable!(),
                    }
                }
                _ => return Err(format!("unknown option: {key}").into()),
            }
        }
        let rows = o.rows.unwrap_or(1_000_000);
        let reps = o.repetitions.unwrap_or(5);
        if !(1..=2_000_000).contains(&rows) || !(1..=50).contains(&reps) || o.round >= 50 {
            return Err("rows/repetitions/round outside fixed bounds".into());
        }
        if o.seed.is_some() && o.measure.is_some() {
            return Err("seed and measure are mutually exclusive".into());
        }
        if o.measure.is_none() && (o.readonly || o.not_in || o.round != 0) {
            return Err("surface/case/round flags require --measure".into());
        }
        Ok(o)
    }
}

fn seed(path: &std::path::Path, rows: usize) -> Result<()> {
    if path.exists() {
        return Err("seed destination must not exist".into());
    }
    let mut engine = Engine::new(path)?;
    engine.execute_powql("type Event { required id: int, required bucket: int, required user: str, required body: str, required detail: str, required score: int, required created_at: datetime }")?;
    let body = "x".repeat(128);
    let detail = "y".repeat(128);
    // Offline bootstrap bypasses statement/WAL dispatch. Checkpoint completes
    // before measurement; none of the measured queries writes data.
    for id in 0..rows {
        engine
            .catalog_mut()
            .get_table_mut("Event")
            .ok_or("missing fixture table")?
            .insert(&vec![
                Value::Int(id as i64),
                Value::Int((id % 1000) as i64),
                Value::Str(format!("user_{}", id % 20_000)),
                Value::Str(body.clone()),
                Value::Str(detail.clone()),
                Value::Int((id % 100) as i64),
                Value::DateTime(1_700_000_000_000_000 + id as i64),
            ])?;
    }
    engine.catalog_mut().checkpoint()?;
    drop(engine);
    std::fs::write(
        path.join(MARKER),
        json!({"profile":"wide-unindexed-v1","rows":rows}).to_string(),
    )?;
    Ok(())
}

fn measure(o: &Options) -> Result<Json> {
    let path = o.measure.as_ref().ok_or("missing measure path")?;
    let rows = o.rows.unwrap_or(1_000_000);
    let marker: Json = serde_json::from_slice(&std::fs::read(path.join(MARKER))?)?;
    if marker["profile"] != "wide-unindexed-v1" || marker["rows"] != rows {
        return Err("fixture marker does not match requested profile/rows".into());
    }
    let start = Instant::now();
    let mut engine = if o.readonly {
        Engine::open_read_only(path)?
    } else {
        Engine::new(path)?
    };
    let open_ns = start.elapsed().as_nanos().to_string();
    let actual_rows = engine.execute_powql("count(Event)")?;
    if !matches!(actual_rows, QueryResult::Scalar(Value::Int(n)) if n == rows as i64) {
        return Err("fixture row count does not match --rows".into());
    }
    let first = (o.round * 37) % 980;
    let keys = (first..first + 20)
        .map(|k| k.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let operator = if o.not_in { "not in" } else { "in" };
    let query = format!("count(Event filter .bucket {operator} ({keys}))");
    let expected = (0..rows)
        .filter(|id| (first..first + 20).contains(&(id % 1000)) != o.not_in)
        .count();
    let plan = format!("{:?}", engine.execute_powql(&format!("explain {query}"))?);
    let start = Instant::now();
    let result = engine.execute_powql(&query);
    let elapsed_ns = start.elapsed().as_nanos() as f64;
    let outcome = match result {
        Ok(QueryResult::Scalar(Value::Int(n))) if n == expected as i64 => {
            json!({"ok":true,"count":n})
        }
        Ok(other) => {
            return Err(format!("oracle mismatch: expected {expected}, got {other:?}").into())
        }
        Err(error) => json!({"ok":false,"error":error.to_string()}),
    };
    Ok(
        json!({"rows":rows,"round":o.round,"readonly":o.readonly,"not_in":o.not_in,
        "keys_first":first,"keys_count":20,"expected":expected,"query":query,"plan":plan,
        "open_ns":open_ns,"elapsed_ns":elapsed_ns,"outcome":outcome,"wal_mode":"Full",
        "query_memory_limit":engine.query_memory_limit(),"fixture_profile":"wide-unindexed-v1"}),
    )
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn main() -> Result<()> {
    let o = Options::parse()?;
    let rows = o.rows.unwrap_or(1_000_000);
    if let Some(path) = &o.seed {
        seed(path, rows)?;
        return Ok(());
    }
    if o.measure.is_some() {
        println!("{}", measure(&o)?);
        return Ok(());
    }
    let repetitions = o.repetitions.unwrap_or(5);
    if repetitions < 5 {
        return Err("paired gate requires at least five repetitions".into());
    }
    let baseline = o
        .baseline
        .or_else(|| std::env::var_os("POWDB_MEMBERSHIP_BASELINE").map(PathBuf::from))
        .ok_or("explicit frozen baseline is required")?;
    let candidate = std::env::current_exe()?;
    if std::fs::read(&baseline)? == std::fs::read(&candidate)? {
        return Err("identical baseline/candidate binaries".into());
    }
    let fixture_root = tempfile::tempdir()?;
    let mut samples = Vec::new();
    let sizes = if rows == 100_000 {
        vec![rows]
    } else {
        vec![100_000, rows]
    };
    for size in sizes {
        let fixture = fixture_root.path().join(format!("rows-{size}"));
        seed(&fixture, size)?;
        for round in 0..repetitions {
            for readonly in [false, true] {
                for not_in in [false, true] {
                    for role in if round.is_multiple_of(2) {
                        ["baseline", "candidate"]
                    } else {
                        ["candidate", "baseline"]
                    } {
                        let binary = if role == "baseline" {
                            &baseline
                        } else {
                            &candidate
                        };
                        let mut command = Command::new(binary);
                        command.arg("--measure").arg(&fixture).args([
                            "--rows",
                            &size.to_string(),
                            "--round",
                            &round.to_string(),
                        ]);
                        if readonly {
                            command.arg("--readonly");
                        }
                        if not_in {
                            command.arg("--not-in");
                        }
                        let output = command.output()?;
                        if !output.status.success() {
                            return Err(format!(
                                "{role} measurement failed: {}",
                                String::from_utf8_lossy(&output.stderr)
                            )
                            .into());
                        }
                        let mut sample: Json = serde_json::from_slice(&output.stdout)?;
                        let first = (round * 37) % 980;
                        let expected = (0..size)
                            .filter(|id| (first..first + 20).contains(&(id % 1000)) != not_in)
                            .count();
                        if sample["rows"] != size
                            || sample["round"] != round
                            || sample["readonly"] != readonly
                            || sample["not_in"] != not_in
                            || sample["fixture_profile"] != "wide-unindexed-v1"
                            || sample["query_memory_limit"] != 256 * 1024 * 1024
                            || sample["wal_mode"] != "Full"
                            || sample["expected"] != expected
                            || sample["outcome"]["ok"].as_bool().is_none()
                            || !sample["elapsed_ns"]
                                .as_f64()
                                .is_some_and(|n| n.is_finite() && n > 0.0)
                            || (sample["outcome"]["ok"] == true
                                && sample["outcome"]["count"] != expected)
                        {
                            return Err(
                                format!("{role} returned an invalid measurement record").into()
                            );
                        }
                        sample["role"] = json!(role);
                        samples.push(sample);
                    }
                }
            }
        }
    }
    let mut pass = true;
    let mut summaries = Vec::new();
    for size in if rows == 100_000 {
        vec![rows]
    } else {
        vec![100_000, rows]
    } {
        for readonly in [false, true] {
            for not_in in [false, true] {
                let select = |role: &str| {
                    samples
                        .iter()
                        .filter(|s| {
                            s["rows"] == size
                                && s["readonly"] == readonly
                                && s["not_in"] == not_in
                                && s["role"] == role
                        })
                        .collect::<Vec<_>>()
                };
                let a = select("baseline");
                let b = select("candidate");
                let candidate_ok = b.iter().all(|s| s["outcome"]["ok"] == true);
                let control_ok = size != 100_000 || a.iter().all(|s| s["outcome"]["ok"] == true);
                let successes = a
                    .iter()
                    .zip(&b)
                    .filter(|(a, b)| a["outcome"]["ok"] == true && b["outcome"]["ok"] == true)
                    .collect::<Vec<_>>();
                let speedup = if successes.is_empty() {
                    None
                } else {
                    Some(
                        median(
                            successes
                                .iter()
                                .map(|(a, _)| a["elapsed_ns"].as_f64().unwrap_or(f64::INFINITY))
                                .collect(),
                        ) / median(
                            successes
                                .iter()
                                .map(|(_, b)| b["elapsed_ns"].as_f64().unwrap_or(f64::INFINITY))
                                .collect(),
                        ),
                    )
                };
                let timing_pass = speedup.is_none_or(|ratio| ratio >= 2.0);
                pass &= candidate_ok && timing_pass && control_ok;
                summaries.push(json!({"rows":size,"readonly":readonly,"not_in":not_in,"candidate_ok":candidate_ok,"control_ok":control_ok,"baseline_successes":a.iter().filter(|s|s["outcome"]["ok"]==true).count(),"successful_pairs":successes.len(),"speedup":speedup,"timing_pass":timing_pass}));
            }
        }
    }
    println!(
        "{}",
        json!({"schema":1,"pass":pass,"diagnostic_only":true,"durable_write_claim":false,"fixture_profile":"wide-unindexed-v1","summaries":summaries,"samples":samples})
    );
    if !pass {
        return Err("unindexed membership gate failed; inspect JSON outcomes".into());
    }
    Ok(())
}
