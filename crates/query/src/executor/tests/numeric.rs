use super::*;

// ── Mixed-type arithmetic (Int <-> Float) regression tests ─────────

/// Engine with a Product type containing price:float + stock:int.
/// Exercises mixed numeric promotion in `eval_binop`.
fn product_mix_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_product_mix_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql(
            "type Product { required name: str, required price: float, required stock: int }",
        )
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Apple",  price := 1.5, stock := 10 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Banana", price := 0.25, stock := 4 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Product { name := "Cherry", price := 2.0, stock := 3 }"#)
        .unwrap();
    engine
}

fn as_float(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn test_arith_float_times_int() {
    let mut engine = product_mix_engine();
    let result = engine
        .execute_powql("Product { .name, total: .price * .stock }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "total"]);
            let mut by_name: std::collections::HashMap<String, f64> =
                std::collections::HashMap::new();
            for row in &rows {
                let name = match &row[0] {
                    Value::Str(s) => s.clone(),
                    _ => panic!(),
                };
                by_name.insert(name, as_float(&row[1]));
            }
            assert!((by_name["Apple"] - 15.0).abs() < 1e-9);
            assert!((by_name["Banana"] - 1.0).abs() < 1e-9);
            assert!((by_name["Cherry"] - 6.0).abs() < 1e-9);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_arith_int_plus_float() {
    let mut engine = product_mix_engine();
    // stock:int + price:float → should promote to float
    let result = engine
        .execute_powql("Product { .name, bumped: .stock + .price }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            let mut by_name: std::collections::HashMap<String, f64> =
                std::collections::HashMap::new();
            for row in &rows {
                let name = match &row[0] {
                    Value::Str(s) => s.clone(),
                    _ => panic!(),
                };
                by_name.insert(name, as_float(&row[1]));
            }
            assert!((by_name["Apple"] - 11.5).abs() < 1e-9);
            assert!((by_name["Banana"] - 4.25).abs() < 1e-9);
            assert!((by_name["Cherry"] - 5.0).abs() < 1e-9);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_arith_float_div_int() {
    let mut engine = product_mix_engine();
    let result = engine
        .execute_powql("Product { .name, unit: .price / .stock }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            let mut by_name: std::collections::HashMap<String, f64> =
                std::collections::HashMap::new();
            for row in &rows {
                let name = match &row[0] {
                    Value::Str(s) => s.clone(),
                    _ => panic!(),
                };
                by_name.insert(name, as_float(&row[1]));
            }
            assert!((by_name["Apple"] - 0.15).abs() < 1e-9);
            assert!((by_name["Banana"] - 0.0625).abs() < 1e-9);
            assert!((by_name["Cherry"] - (2.0 / 3.0)).abs() < 1e-9);
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn test_arith_int_minus_float() {
    let mut engine = product_mix_engine();
    let result = engine
        .execute_powql("Product { .name, delta: .stock - .price }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            let mut by_name: std::collections::HashMap<String, f64> =
                std::collections::HashMap::new();
            for row in &rows {
                let name = match &row[0] {
                    Value::Str(s) => s.clone(),
                    _ => panic!(),
                };
                by_name.insert(name, as_float(&row[1]));
            }
            assert!((by_name["Apple"] - 8.5).abs() < 1e-9);
            assert!((by_name["Banana"] - 3.75).abs() < 1e-9);
            assert!((by_name["Cherry"] - 1.0).abs() < 1e-9);
        }
        _ => panic!("expected rows"),
    }
}

// Regression: sum() on a Float column must return the actual
// floating-point sum, not Int(0). The old slow-path loops filtered
// out Value::Float and only summed Ints, silently dropping every
// value in a Float column.
#[test]
fn test_sum_float_scalar() {
    let mut engine = product_mix_engine();
    let result = engine.execute_powql("sum(Product { .price })").unwrap();
    match result {
        QueryResult::Scalar(v) => {
            // 1.5 + 0.25 + 2.0 = 3.75
            assert!(
                (as_float(&v) - 3.75).abs() < 1e-9,
                "expected 3.75, got {v:?}"
            );
        }
        _ => panic!("expected scalar result, got {result:?}"),
    }
}

// Regression: sum() of a Float column inside a GROUP BY must work
// the same way. compute_group_aggregate had the identical Int-only
// bug as the scalar path.
#[test]
fn test_sum_float_group_by() {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("powdb_sum_float_gb_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Sale { required region: str, required amount: float }")
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { region := "E", amount := 1.5 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { region := "E", amount := 2.25 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { region := "W", amount := 4.0 }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert Sale { region := "W", amount := 0.5 }"#)
        .unwrap();

    let result = engine
        .execute_powql("Sale group .region { .region, total: sum(.amount) }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["region", "total"]);
            let mut by_region: std::collections::HashMap<String, f64> =
                std::collections::HashMap::new();
            for row in &rows {
                let region = match &row[0] {
                    Value::Str(s) => s.clone(),
                    _ => panic!(),
                };
                by_region.insert(region, as_float(&row[1]));
            }
            assert!(
                (by_region["E"] - 3.75).abs() < 1e-9,
                "E: {:?}",
                by_region.get("E")
            );
            assert!(
                (by_region["W"] - 4.5).abs() < 1e-9,
                "W: {:?}",
                by_region.get("W")
            );
        }
        _ => panic!("expected rows, got {result:?}"),
    }
}

// ─── Mission D10: Float fast-path parity ─────────────────────────────
//
// Prior to D10, three hot paths in the executor bailed on Float columns:
//   1. `agg_single_col_fast` — sum/avg/min/max/count fell through to the
//      generic row-decoding path (allocates Vec<Value> per row).
//   2. `project_filter_sort_limit_fast` — top-N by Float column fell
//      through the generic sort path.
//   3. `compile_predicate` / `build_int_leaf` — WHERE on Float columns
//      couldn't compile, so the whole filter walked Value::cmp.
//
// These tests exercise each Float fast path end-to-end, including NaN
// handling via `total_cmp` (which matches `Value::Ord` so semantics are
// identical between fast-path and generic-path reads).

/// Engine with a Price table: price:float, qty:int. Eight rows with a
/// deliberate spread of values, a NaN, a negative, -0.0, and a null.
/// The null exercises the bitmap-skip branch; NaN and -0.0 exercise
/// the `total_cmp` invariant.
fn float_fast_engine() -> Engine {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("powdb_float_fast_{}_{}", std::process::id(), id));
    let mut engine = Engine::new(&dir).unwrap();
    engine
        .execute_powql("type Price { required name: str, price: float, required qty: int }")
        .unwrap();
    // Insertion order deliberately scrambled so top-N doesn't trivially
    // match insertion order.
    let rows = [
        ("a", "price := 1.5", "qty := 1"),
        ("b", "price := 0.25", "qty := 2"),
        ("c", "price := 2.0", "qty := 3"),
        ("d", "price := -3.5", "qty := 4"),
        ("e", "price := 10.0", "qty := 5"),
        ("f", "price := 0.5", "qty := 6"),
        ("g", "price := 100.0", "qty := 7"),
        ("h", "price := -0.0", "qty := 8"),
    ];
    for (name, price, qty) in rows {
        engine
            .execute_powql(&format!(
                r#"insert Price {{ name := "{name}", {price}, {qty} }}"#
            ))
            .unwrap();
    }
    engine
}

#[test]
fn test_d10_agg_sum_float_fast_path() {
    let mut engine = float_fast_engine();
    let result = engine.execute_powql("sum(Price { .price })").unwrap();
    // 1.5 + 0.25 + 2.0 + -3.5 + 10.0 + 0.5 + 100.0 + -0.0 = 110.75
    match result {
        QueryResult::Scalar(v) => {
            assert!((as_float(&v) - 110.75).abs() < 1e-9, "got {v:?}");
        }
        _ => panic!("expected scalar, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_avg_float_fast_path() {
    let mut engine = float_fast_engine();
    let result = engine.execute_powql("avg(Price { .price })").unwrap();
    // 110.75 / 8 = 13.84375
    match result {
        QueryResult::Scalar(v) => {
            assert!((as_float(&v) - 13.84375).abs() < 1e-9, "got {v:?}");
        }
        _ => panic!("expected scalar, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_min_float_fast_path() {
    let mut engine = float_fast_engine();
    let result = engine.execute_powql("min(Price { .price })").unwrap();
    match result {
        QueryResult::Scalar(v) => {
            assert!((as_float(&v) - (-3.5)).abs() < 1e-9, "got {v:?}");
        }
        _ => panic!("expected scalar, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_max_float_fast_path() {
    let mut engine = float_fast_engine();
    let result = engine.execute_powql("max(Price { .price })").unwrap();
    match result {
        QueryResult::Scalar(v) => {
            assert!((as_float(&v) - 100.0).abs() < 1e-9, "got {v:?}");
        }
        _ => panic!("expected scalar, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_count_distinct_float_fast_path() {
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("count(distinct Price { .price })")
        .unwrap();
    // All 8 prices are distinct (+0.0 isn't present; -0.0 is, and
    // distinct from every other value). Hash via to_bits so -0.0 and
    // +0.0 would count separately — matches Value::Hash.
    match result {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 8, "got {n}"),
        _ => panic!("expected scalar int, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_float_with_compiled_where() {
    // Exercises `build_float_leaf` — WHERE .price > 1.0 must compile,
    // and the Float fast path must use it to short-circuit rows.
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("sum(Price filter .price > 1.0 { .price })")
        .unwrap();
    // Rows > 1.0: 1.5, 2.0, 10.0, 100.0 → sum = 113.5
    match result {
        QueryResult::Scalar(v) => {
            assert!((as_float(&v) - 113.5).abs() < 1e-9, "got {v:?}");
        }
        _ => panic!("expected scalar, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_float_with_compiled_where_int_literal() {
    // Novel cross-type: WHERE .price > 1 (Int literal on Float column)
    // must still compile via build_float_leaf — the Int literal is
    // promoted to f64 at compile time so the hot loop only sees f64.
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("sum(Price filter .price > 1 { .price })")
        .unwrap();
    match result {
        QueryResult::Scalar(v) => {
            assert!((as_float(&v) - 113.5).abs() < 1e-9, "got {v:?}");
        }
        _ => panic!("expected scalar, got {result:?}"),
    }
}

#[test]
fn test_d10_agg_float_with_reversed_literal() {
    // `100.0 > .price` (literal on LHS) must also compile. The
    // build_float_leaf flips the operator so the field is always LHS.
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("count(Price filter 1.0 < .price { .price })")
        .unwrap();
    // Rows where 1.0 < .price: 1.5, 2.0, 10.0, 100.0 → count = 4
    match result {
        QueryResult::Scalar(Value::Int(n)) => assert_eq!(n, 4, "got {n}"),
        _ => panic!("expected scalar int, got {result:?}"),
    }
}

#[test]
fn test_d10_sort_float_desc_limit_fast_path() {
    // Top-3 by price descending — exercises the Float branch of
    // project_filter_sort_limit_fast with the sortable-u64 transform.
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("Price order .price desc limit 3 { .name, .price }")
        .unwrap();
    match result {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["name", "price"]);
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Str("g".into())); // 100.0
            assert!((as_float(&rows[0][1]) - 100.0).abs() < 1e-9);
            assert_eq!(rows[1][0], Value::Str("e".into())); // 10.0
            assert!((as_float(&rows[1][1]) - 10.0).abs() < 1e-9);
            assert_eq!(rows[2][0], Value::Str("c".into())); // 2.0
            assert!((as_float(&rows[2][1]) - 2.0).abs() < 1e-9);
        }
        _ => panic!("expected rows, got {result:?}"),
    }
}

#[test]
fn test_d10_sort_float_asc_limit_fast_path() {
    // Bottom-3 by price — negative and -0.0 must order correctly.
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("Price order .price limit 3 { .name, .price }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0], Value::Str("d".into())); // -3.5
                                                            // -0.0 must come before +0.25 under total_cmp ordering.
            assert_eq!(rows[1][0], Value::Str("h".into())); // -0.0
            assert_eq!(rows[2][0], Value::Str("b".into())); // 0.25
        }
        _ => panic!("expected rows, got {result:?}"),
    }
}

#[test]
fn test_d10_sort_float_with_compiled_filter() {
    // Filter + sort + limit all on Float column — every fast path
    // fires on the same query.
    let mut engine = float_fast_engine();
    let result = engine
        .execute_powql("Price filter .price > 0.0 order .price desc limit 2 { .name }")
        .unwrap();
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0], Value::Str("g".into())); // 100.0
            assert_eq!(rows[1][0], Value::Str("e".into())); // 10.0
        }
        _ => panic!("expected rows, got {result:?}"),
    }
}

#[test]
fn test_f64_sortable_transform_monotonic() {
    // The sortable-u64 transform must preserve total_cmp ordering.
    // Regression guard against accidentally breaking the clever
    // sign-flip trick in `f64_bits_to_sortable_u64`.
    let samples: [f64; 11] = [
        f64::NEG_INFINITY,
        -1e100,
        -1.0,
        -f64::MIN_POSITIVE,
        -0.0,
        0.0,
        f64::MIN_POSITIVE,
        1.0,
        1e100,
        f64::INFINITY,
        f64::NAN, // total_cmp says NaN > +∞
    ];
    let mut sorted = samples;
    sorted.sort_by(|a, b| a.total_cmp(b));

    let as_sortable: Vec<u64> = sorted
        .iter()
        .map(|f| f64_bits_to_sortable_u64(f.to_bits()))
        .collect();

    // Each u64 must be strictly greater than its predecessor, because
    // `total_cmp` places every sample at a distinct total-order slot.
    for pair in as_sortable.windows(2) {
        assert!(
            pair[0] < pair[1],
            "sortable u64 not monotonic: {:#x} >= {:#x}",
            pair[0],
            pair[1]
        );
    }
}
