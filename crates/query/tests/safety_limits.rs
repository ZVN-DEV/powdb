//! Safety-limit tests: verify that the query engine rejects pathological
//! inputs with clear error messages instead of consuming unbounded memory.

use powdb_query::lexer::lex;

// ---------------------------------------------------------------------------
// TASK-22: String literal length limit
// ---------------------------------------------------------------------------

#[test]
fn string_literal_exceeding_16mb_returns_lex_error() {
    // Build a string that exceeds the 16 MB limit (16 * 1024 * 1024 + 1 bytes
    // of content, plus the enclosing quotes).
    let content_len = 16 * 1024 * 1024 + 1;
    let mut query = String::with_capacity(content_len + 2);
    query.push('"');
    for _ in 0..content_len {
        query.push('x');
    }
    query.push('"');

    let err = lex(&query).expect_err("must reject oversized string literal");
    assert!(
        err.message.contains("exceeds maximum size"),
        "unexpected error message: {}",
        err.message
    );
    assert!(
        err.message.contains("16MB"),
        "error should mention the limit in MB: {}",
        err.message
    );
}

#[test]
fn string_literal_at_16mb_is_accepted() {
    // Exactly 16 MB should be fine (the limit is >16MB, not >=).
    let content_len = 16 * 1024 * 1024;
    let mut query = String::with_capacity(content_len + 2);
    query.push('"');
    for _ in 0..content_len {
        query.push('a');
    }
    query.push('"');

    let tokens = lex(&query).expect("16MB string should be accepted");
    assert_eq!(tokens.len(), 2); // StringLit + Eof
}

// ---------------------------------------------------------------------------
// TASK-23: Human-readable error messages
// ---------------------------------------------------------------------------

#[test]
fn parse_error_shows_human_readable_token_names() {
    // A query like `42` should produce an error mentioning "number" not "IntLit(42)".
    let err = powdb_query::parser::parse("42").expect_err("should fail to parse bare number");
    let msg = err.to_string();
    assert!(
        !msg.contains("IntLit"),
        "error should not contain raw Debug format 'IntLit': {msg}"
    );
    assert!(
        msg.contains("number"),
        "error should contain human-readable 'number': {msg}"
    );
}

#[test]
fn parse_error_for_string_in_type_position() {
    // `insert "hello" { }` — string where type name expected.
    let err = powdb_query::parser::parse(r#"insert "hello" { }"#)
        .expect_err("should fail to parse string as type name");
    let msg = err.to_string();
    assert!(
        !msg.contains("StringLit"),
        "error should not contain raw Debug format 'StringLit': {msg}"
    );
    assert!(
        msg.contains("string"),
        "error should contain human-readable 'string': {msg}"
    );
}

#[test]
fn display_name_covers_common_tokens() {
    use powdb_query::token::Token;

    assert_eq!(Token::IntLit(42).display_name(), "number 42");
    assert_eq!(Token::FloatLit(2.72).display_name(), "decimal number 2.72");
    assert_eq!(
        Token::StringLit("hi".into()).display_name(),
        "string \"hi\""
    );
    assert_eq!(
        Token::Ident("User".into()).display_name(),
        "identifier 'User'"
    );
    assert_eq!(Token::Filter.display_name(), "'filter'");
    assert_eq!(Token::Comma.display_name(), "','");
    assert_eq!(Token::Eof.display_name(), "end of input");
    assert_eq!(Token::LParen.display_name(), "'('");
    assert_eq!(Token::RParen.display_name(), "')'");
}
