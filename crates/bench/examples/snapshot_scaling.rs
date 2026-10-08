//! Quiet, bounded scaling diagnostic; separate from the unchanged release gate.
use powdb_query::ast::Literal;
use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;
use powdb_storage::wal::WalSyncMode;
use std::hint::black_box;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    assert_eq!(
        args.len(),
        3,
        "usage: snapshot_scaling <rows> <operations> <insert|filter_same|filter_changed>"
    );
    let rows: usize = args[0].parse().expect("rows");
    let operations: usize = args[1].parse().expect("operations");
    let kind = &args[2];
    assert!(rows > 0 && operations > 0 && rows <= 10_000_000 && operations <= 10_000_000);
    assert!(matches!(
        kind.as_str(),
        "insert" | "filter_same" | "filter_changed"
    ));
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.set_wal_sync_mode(WalSyncMode::Off);
    engine.execute_powql("type User { required id: int, required name: str, required age: int, required status: str, required email: str, required created_at: int }").unwrap();
    for id in 0..rows {
        engine
            .catalog_mut()
            .get_table_mut("User")
            .unwrap()
            .insert(&vec![
                Value::Int(id as i64),
                Value::Str(format!("user_{id}")),
                Value::Int(18 + (id % 60) as i64),
                Value::Str("active".into()),
                Value::Str(format!("user_{id}@example.com")),
                Value::Int(1_700_000_000 + id as i64),
            ])
            .unwrap();
    }
    engine
        .catalog_mut()
        .create_index_unique("User", "id", true)
        .unwrap();
    let mut literals = vec![
        Literal::Int(rows as i64),
        Literal::String("new".into()),
        Literal::Int(30),
        Literal::String("active".into()),
        Literal::String("new@example.com".into()),
        Literal::Int(1_700_000_000),
    ];
    let insert = engine.prepare(r#"insert User { id := 0, name := "", age := 0, status := "", email := "", created_at := 0 }"#).unwrap();
    let senior = engine
        .prepare(r#"User filter .age > 50 update { status := "senior" }"#)
        .unwrap();
    let active = engine
        .prepare(r#"User filter .age > 50 update { status := "active" }"#)
        .unwrap();
    // One common untimed warmup; changing-value and same-value variants are explicit.
    if kind != "insert" {
        engine
            .execute_prepared(
                &senior,
                &[Literal::Int(50), Literal::String("senior".into())],
            )
            .unwrap();
    }
    let started = Instant::now();
    for op in 0..operations {
        match kind.as_str() {
            "insert" => {
                literals[0] = Literal::Int((rows + op) as i64);
                black_box(engine.execute_prepared(&insert, &literals).unwrap());
            }
            "filter_same" => {
                black_box(
                    engine
                        .execute_powql(r#"User filter .age > 50 update { status := "senior" }"#)
                        .unwrap(),
                );
            }
            "filter_changed" => {
                let query = if op.is_multiple_of(2) {
                    &active
                } else {
                    &senior
                };
                let status = if op.is_multiple_of(2) {
                    "active"
                } else {
                    "senior"
                };
                black_box(
                    engine
                        .execute_prepared(
                            query,
                            &[Literal::Int(50), Literal::String(status.into())],
                        )
                        .unwrap(),
                );
            }
            _ => unreachable!(),
        }
    }
    let elapsed_ns = started.elapsed().as_nanos();
    let expected = rows + if kind == "insert" { operations } else { 0 };
    match engine.execute_powql("count(User)").unwrap() {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, expected as i64),
        other => panic!("unexpected count: {other:?}"),
    }
    if kind != "insert" {
        let status = if kind == "filter_same" || operations.is_multiple_of(2) {
            "senior"
        } else {
            "active"
        };
        let q = format!(r#"User filter .age > 50 and .status != "{status}" {{ .id }}"#);
        assert_eq!(engine.execute_powql(&q).unwrap().row_count(), 0);
    } else {
        for id in [rows, rows + operations - 1] {
            assert_eq!(
                engine
                    .execute_powql(&format!("User filter .id = {id}"))
                    .unwrap()
                    .row_count(),
                1
            );
        }
    }
    println!(
        "{}",
        serde_json::json!({"schema":1,"kind":kind,"rows":rows,"operations":operations,
        "wal_mode":"Off","elapsed_ns":elapsed_ns.to_string(),"mean_ns_per_op":elapsed_ns as f64/operations as f64,
        "correctness":true,"durable_claim":false})
    );
}
