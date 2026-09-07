//! Q17: a parameter that the query never references is an error.
//!
//! Extra parameters were dropped in silence. A driver that binds one array per
//! call and edits the query text, or a caller that reorders `$1` and `$2` and
//! deletes one, got no signal at all: the query ran against whatever subset it
//! happened to mention, which is the same failure mode as an off-by-one in a
//! positional API, with nothing in the result to show it.

use powdb_query::ast::ParamValue;
use powdb_query::executor::Engine;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type T { required unique id: int, n: int }")
        .unwrap();
    engine
        .execute_powql("insert T { id := 1, n := 5 }")
        .unwrap();
    (dir, engine)
}

#[test]
fn an_unused_trailing_parameter_is_an_error() {
    let (_dir, mut engine) = engine();
    let message = engine
        .execute_powql_with_params(
            "T filter .n = $1 { .id }",
            &[ParamValue::Int(5), ParamValue::Int(9)],
        )
        .map(|ok| panic!("the second parameter is unused, got {ok:?}"))
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("2 parameters supplied") && message.contains("$1"),
        "got {message}"
    );
}

#[test]
fn a_parameter_for_a_query_with_none_is_an_error() {
    let (_dir, mut engine) = engine();
    let message = engine
        .execute_powql_with_params("T { .id }", &[ParamValue::Int(5)])
        .map(|ok| panic!("the parameter is unused, got {ok:?}"))
        .unwrap_err()
        .to_string();
    assert!(message.contains("references none"), "got {message}");
}

#[test]
fn a_gap_in_the_placeholders_is_an_error() {
    let (_dir, mut engine) = engine();
    let message = engine
        .execute_powql_with_params(
            "T filter .n = $2 { .id }",
            &[ParamValue::Int(9), ParamValue::Int(5)],
        )
        .map(|ok| panic!("the first parameter is unused, got {ok:?}"))
        .unwrap_err()
        .to_string();
    assert!(message.contains("$1"), "got {message}");
}

#[test]
fn too_few_parameters_still_reports_the_missing_one() {
    let (_dir, mut engine) = engine();
    let message = engine
        .execute_powql_with_params("T filter .n = $2 { .id }", &[ParamValue::Int(5)])
        .unwrap_err()
        .to_string();
    assert!(message.contains("$2"), "got {message}");
}

#[test]
fn every_parameter_used_still_runs() {
    let (_dir, mut engine) = engine();
    engine
        .execute_powql_with_params(
            "T filter .n = $1 and .id = $2 { .id }",
            &[ParamValue::Int(5), ParamValue::Int(1)],
        )
        .unwrap();
    // The same placeholder twice counts as used once.
    engine
        .execute_powql_with_params(
            "T filter .n = $1 or .id = $1 { .id }",
            &[ParamValue::Int(1)],
        )
        .unwrap();
    engine.execute_powql_with_params("T { .id }", &[]).unwrap();
}

#[test]
fn the_read_only_path_refuses_it_too() {
    let (_dir, engine) = engine();
    assert!(engine
        .execute_powql_readonly_with_params("T { .id }", &[ParamValue::Int(5)])
        .is_err());
}
