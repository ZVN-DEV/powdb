//! docs/metrics.md claims to list every metric family the `/metrics`
//! endpoint exposes. This gate makes the claim true by construction: the
//! documented family list must equal the families a live registry renders.
//!
//! Same pattern as errors_doc_sync.rs (docs/errors.md vs ErrorClass) and
//! powql_doc_sync.rs (keyword list vs lexer table): the doc is data, the
//! code is authority, CI holds them equal.

use powdb_server::metrics::Metrics;

#[test]
fn the_documented_metric_families_equal_the_rendered_ones() {
    let tmp = tempfile::tempdir().unwrap();
    // `with_data_dir` turns on the size gauges, so the render below carries
    // every family the endpoint can expose.
    let rendered = Metrics::new().with_data_dir(tmp.path()).render();

    let mut exposed: Vec<String> = rendered
        .lines()
        .filter_map(|line| line.strip_prefix("# TYPE "))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(str::to_string)
        .collect();
    exposed.sort_unstable();
    assert!(
        !exposed.is_empty(),
        "render() must expose at least one metric family"
    );

    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/metrics.md"
    ))
    .expect("docs/metrics.md must exist in the workspace");
    let anchor = "## Metric reference";
    let start = doc
        .find(anchor)
        .expect("metric-reference section must exist");
    // No dedup: a family documented twice is also a doc bug and must fail.
    let mut documented: Vec<String> = doc[start..]
        .lines()
        .filter(|line| line.starts_with("| `powdb_"))
        .filter_map(|line| line.split('`').nth(1))
        .map(str::to_string)
        .collect();
    documented.sort_unstable();

    let missing: Vec<_> = exposed.iter().filter(|f| !documented.contains(f)).collect();
    let extra: Vec<_> = documented.iter().filter(|f| !exposed.contains(f)).collect();
    assert_eq!(
        documented, exposed,
        "docs/metrics.md disagrees with Metrics::render() \
         (undocumented families: {missing:?}; documented but never rendered: {extra:?})"
    );
}
