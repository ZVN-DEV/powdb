//! "Did you mean" suggestions must describe the token that actually failed.
//!
//! The parser's trailing-token error used to compute its suggestion from the
//! FIRST token of the statement instead of the offending one. Since
//! `edit_distance("user", "upsert") == 2`, every query starting with the
//! table name `User` (the table in nearly every README and docs example)
//! answered "did you mean `upsert`?" no matter what actually broke.

use powdb_query::parser::parse;

fn error_message(query: &str) -> String {
    parse(query)
        .map(|stmt| panic!("expected `{query}` to fail parsing, got: {stmt:?}"))
        .unwrap_err()
        .to_string()
}

fn assert_suggests(query: &str, expected: &str) {
    let msg = error_message(query);
    assert!(
        msg.contains(&format!("did you mean `{expected}`?")),
        "`{query}` should suggest `{expected}`, got: {msg}"
    );
}

fn assert_no_suggestion(query: &str) {
    let msg = error_message(query);
    assert!(
        !msg.contains("did you mean"),
        "`{query}` resembles no keyword and should suggest nothing, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Pipeline-stage typos: the suggestion must come from the offending token
// ---------------------------------------------------------------------------

#[test]
fn mistyped_filter_suggests_filter_not_upsert() {
    assert_suggests("User filtr .age > 25", "filter");
}

#[test]
fn mistyped_sort_direction_suggests_desc() {
    assert_suggests("User order .age dsc", "desc");
}

#[test]
fn mistyped_group_suggests_group() {
    assert_suggests("User grup .city", "group");
}

#[test]
fn mistyped_order_suggests_order() {
    assert_suggests("User ordr .name", "order");
}

#[test]
fn mistyped_limit_suggests_limit() {
    assert_suggests("User limt 10", "limit");
}

// ---------------------------------------------------------------------------
// A wrong suggestion is worse than none
// ---------------------------------------------------------------------------

#[test]
fn gibberish_after_user_suggests_nothing() {
    assert_no_suggestion("User xyzzy");
}

#[test]
fn gibberish_after_other_table_suggests_nothing() {
    assert_no_suggestion("Team xyzzy");
}

// ---------------------------------------------------------------------------
// Not special-cased to the `User` table
// ---------------------------------------------------------------------------

#[test]
fn mistyped_filter_on_other_table_suggests_filter() {
    assert_suggests("Team filtr .id = 1", "filter");
}

#[test]
fn mistyped_filter_on_long_table_name_suggests_filter() {
    assert_suggests("OrderLineItem filtr .qty > 0", "filter");
}

// ---------------------------------------------------------------------------
// Statement-level typos keep working (guards parser.rs's own
// `typoed_statement_keyword_gets_suggestion`)
// ---------------------------------------------------------------------------

#[test]
fn mistyped_statement_keyword_still_suggests_statement_keyword() {
    assert_suggests("updat User set age = 1", "update");
}

#[test]
fn mistyped_insert_statement_still_suggests_insert() {
    assert_suggests("insrt User { name: \"a\" }", "insert");
}
