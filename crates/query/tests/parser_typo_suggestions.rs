//! "Did you mean" suggestions must describe the token that actually failed.
//!
//! The parser's trailing-token error used to compute its suggestion from the
//! FIRST token of the statement instead of the offending one. Since
//! `edit_distance("user", "upsert") == 2`, every query starting with the
//! table name `User` (the table in nearly every README and docs example)
//! answered "did you mean `upsert`?" no matter what actually broke.
//!
//! Reading the right token was only half of it. The bound was loose enough
//! that ordinary English table names drew keyword advice: `Comment` came back
//! as `commit`, `Owner` as `order`, `Watch` as `match`. A wrong suggestion is
//! worse than none, so these tests pin both halves: the real typos we do want
//! to help, and the ordinary nouns we must leave alone.

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

// ---------------------------------------------------------------------------
// Ordinary identifiers must not draw keyword suggestions
//
// The most common way to reach this error is pasting two statements separated
// by a newline instead of a `;`, so the second table name lands in the
// trailing-token path. Table names are ordinary English nouns, and a loose
// bound turns them into nonsense advice.
// ---------------------------------------------------------------------------

#[test]
fn common_table_name_comment_suggests_nothing() {
    assert_no_suggestion("User Comment");
}

#[test]
fn common_table_name_owner_suggests_nothing() {
    assert_no_suggestion("User Owner");
}

#[test]
fn common_table_name_alert_suggests_nothing() {
    assert_no_suggestion("User Alert");
}

#[test]
fn common_table_name_rating_suggests_nothing() {
    assert_no_suggestion("User Rating");
}

#[test]
fn common_table_name_banner_suggests_nothing() {
    assert_no_suggestion("User Banner");
}

#[test]
fn common_table_name_offer_suggests_nothing() {
    assert_no_suggestion("User Offer");
}

#[test]
fn common_table_name_watch_suggests_nothing() {
    assert_no_suggestion("User Watch");
}

#[test]
fn two_statements_split_by_a_newline_suggest_nothing() {
    assert_no_suggestion("User filter .age > 18\nComment filter .id = 1");
}

// ---------------------------------------------------------------------------
// A word that already IS the keyword is not a typo
// ---------------------------------------------------------------------------

#[test]
fn quoted_keyword_does_not_suggest_itself() {
    assert_no_suggestion("User `filter`");
}

#[test]
fn quoted_statement_keyword_does_not_suggest_itself() {
    assert_no_suggestion("User `commit`");
}

// ---------------------------------------------------------------------------
// The first-token fallback names the word it read
//
// It used to report "number 42; did you mean `commit`?", naming one token and
// suggesting for a different one, with nothing in the message to connect them.
// ---------------------------------------------------------------------------

#[test]
fn first_token_fallback_on_ordinary_table_name_suggests_nothing() {
    assert_no_suggestion("Comment 42");
}

#[test]
fn first_token_fallback_names_the_word_it_suggests_for() {
    let msg = error_message("updat 42");
    assert!(
        msg.contains("did you mean `update`?"),
        "`updat 42` should suggest `update`, got: {msg}"
    );
    assert!(
        msg.contains("`updat`"),
        "the suggestion is for the first token, so the message must name it, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Long keywords keep a distance-2 bound
//
// Both of these are adjacent transpositions, the most common real typo, and
// both sit at edit distance 2. They fail under a flat bound of 1, which is
// what pins the long-word branch of the scaling.
// ---------------------------------------------------------------------------

#[test]
fn transposed_returning_still_suggests_returning() {
    assert_suggests("User retruning .id", "returning");
}

#[test]
fn transposed_distinct_still_suggests_distinct() {
    assert_suggests("User distnict .id", "distinct");
}
