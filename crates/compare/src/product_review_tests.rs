use super::*;

#[test]
fn lookup_workload_is_deterministic_bounded_and_varied() {
    for workload in [0, 1, 12] {
        let ids: Vec<_> = (0..READ_OPS)
            .map(|i| lookup_id(i, 100_000, workload))
            .collect();
        assert!(ids.iter().all(|id| (0..100_000).contains(id)));
        assert_eq!(ids[100], lookup_id(100, 100_000, workload));
        let distinct: std::collections::HashSet<_> = ids.iter().collect();
        assert!(distinct.len() > READ_OPS * 9 / 10);
        let write_ids = &ids[..WRITE_OPS];
        assert!(write_ids.iter().any(|id| *id < 1_000));
        assert!(write_ids.iter().any(|id| *id >= 99_000));
    }
    assert_eq!(lookup_id(123, 1, 0), 0);
}

#[test]
fn timing_loop_supplies_each_iteration() {
    let mut seen = Vec::new();
    time_iter(4, |iteration| seen.push(iteration));
    assert_eq!(seen, [0, 1, 2, 3]);
}

#[test]
fn primary_key_workload_changes_values_across_the_fixture() {
    for mut engine in [
        Box::new(PowdbEngine::new()) as Box<dyn BenchEngine>,
        Box::new(SqliteEngine::new()) as Box<dyn BenchEngine>,
    ] {
        let n_rows = 17;
        bench_engine(engine.as_mut(), n_rows, &["update_by_pk"]);
        // Every measured operation must change a value, including key repeats.
        let mut expected: Vec<i64> = (0..n_rows).map(|i| gen_row(i).2).collect();
        for iteration in 0..WRITE_OPS {
            expected[lookup_id(iteration, n_rows, 12) as usize] = 100 + iteration as i64;
        }
        assert!(expected.iter().all(|age| *age >= 100));
        assert_eq!(
            engine.agg_sum(),
            expected.iter().sum::<i64>(),
            "{}",
            engine.name()
        );
    }
}

#[test]
fn comparison_adapters_agree_with_fixture_and_mutations() {
    for mut engine in [
        Box::new(PowdbEngine::new()) as Box<dyn BenchEngine>,
        Box::new(SqliteEngine::new()) as Box<dyn BenchEngine>,
    ] {
        engine.setup(121);
        let fixture: Vec<_> = (0..121).map(gen_row).collect();
        for id in [0, 1, 59, 60, 120] {
            let expected = Some(format!("user_{id}"));
            assert_eq!(engine.point_lookup_indexed(id), expected);
            assert_eq!(engine.point_lookup_nonindexed(1_700_000_000 + id), expected);
        }
        assert_eq!(engine.point_lookup_indexed(-1), None);
        assert_eq!(engine.point_lookup_indexed(121), None);
        assert_eq!(engine.agg_sum(), fixture.iter().map(|r| r.2).sum::<i64>());
        assert_eq!(engine.agg_min(), 1_700_000_000);
        assert_eq!(engine.agg_max(), 77);
        let matching: Vec<_> = fixture.iter().filter(|r| r.2 > 30).collect();
        assert_eq!(engine.scan_filter_count(30), matching.len());
        let average = matching.iter().map(|r| r.2 as f64).sum::<f64>() / matching.len() as f64;
        assert!((engine.agg_avg(30) - average).abs() < 1e-10);
        assert_eq!(
            engine.scan_filter_project_top100(30),
            matching
                .iter()
                .take(100)
                .map(|r| (r.1.clone(), r.4.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            engine.scan_filter_sort_limit10(30),
            matching
                .iter()
                .rev()
                .take(10)
                .map(|r| (r.1.clone(), r.5))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            engine.multi_col_and_filter(30, "active"),
            matching
                .iter()
                .filter(|r| r.3 == "active")
                .map(|r| (r.1.clone(), r.2))
                .collect::<Vec<_>>()
        );
        assert_eq!(engine.update_by_pk(0, 100), 1);
        assert_eq!(engine.update_by_pk(-1, 100), 0);
        assert_eq!(engine.scan_filter_count(99), 1);
        assert_eq!(engine.delete_by_filter(20), 4);
        assert_eq!(engine.point_lookup_indexed(1), None);
        assert_eq!(engine.point_lookup_indexed(0), Some("user_0".to_owned()));
    }
}
