//! Regression tests for AST chains built ITERATIVELY by the parsers.
//!
//! `MAX_NESTING_DEPTH` used to be checked only on recursive descent (parens,
//! subqueries, unary prefixes). Binary and arithmetic chains are parsed in
//! `while` loops that never touched the counter, so a ~50KB query such as
//! `User filter .a = 1 and .a = 1 and ...` (repeated ~5000 times) parsed into
//! a 5000-deep left-leaning `Expr::BinaryOp` tree. The parser survived; the
//! recursive planner walk then overflowed the 2MB tokio worker stack and, with
//! `panic = "abort"`, took the whole server process down. `--query-timeout`
//! could not help: the crash happened during parse/plan, before any scan.
//!
//! Every test here asserts a GRACEFUL typed error, not merely that the process
//! survived. The known-bad size (5000) and ten times that (50000) are both
//! covered, for `and`, `or`, arithmetic chains, and the `having` accumulator,
//! on both the PowQL and the SQL frontend.

use powdb_query::executor::Engine;
use powdb_query::parser::{parse, ParseError};
use powdb_query::sql::parse_sql;

/// The chain length that aborted the release server before this fix.
const KNOWN_BAD: usize = 5_000;
/// Ten times the known-bad length: the guard must not be size-dependent.
const TEN_X: usize = 50_000;

/// `<head>` followed by `<term>` repeated `n` times, e.g.
/// `User filter .a = 1 and .a = 1 ...`.
fn chain(head: &str, term: &str, n: usize) -> String {
    let mut query = String::with_capacity(head.len() + term.len() * n);
    query.push_str(head);
    for _ in 0..n {
        query.push_str(term);
    }
    query
}

fn expect_err<T: std::fmt::Debug>(result: Result<T, ParseError>, what: &str) -> ParseError {
    match result {
        Err(err) => err,
        Ok(ok) => panic!("{what}: expected a parse error, got {ok:?}"),
    }
}

fn assert_depth_error<T: std::fmt::Debug>(result: Result<T, ParseError>, what: &str) {
    let err = expect_err(result, what);
    assert!(
        err.message().contains("nesting depth"),
        "{what}: expected a nesting-depth error, got: {}",
        err.message()
    );
}

#[test]
fn powql_and_chain_is_rejected() {
    for n in [KNOWN_BAD, TEN_X] {
        let query = chain("User filter .a = 1", " and .a = 1", n);
        assert_depth_error(parse(&query), &format!("PowQL and chain n={n}"));
    }
}

#[test]
fn powql_or_chain_is_rejected() {
    for n in [KNOWN_BAD, TEN_X] {
        let query = chain("User filter .a = 1", " or .a = 1", n);
        assert_depth_error(parse(&query), &format!("PowQL or chain n={n}"));
    }
}

#[test]
fn powql_additive_chain_is_rejected() {
    for n in [KNOWN_BAD, TEN_X] {
        let query = chain("User filter .a", " + 1", n);
        assert_depth_error(parse(&query), &format!("PowQL additive chain n={n}"));
    }
}

#[test]
fn powql_multiplicative_chain_is_rejected() {
    for n in [KNOWN_BAD, TEN_X] {
        let query = chain("User filter .a", " * 2", n);
        assert_depth_error(parse(&query), &format!("PowQL multiplicative chain n={n}"));
    }
}

#[test]
fn powql_having_chain_is_rejected() {
    for n in [KNOWN_BAD, TEN_X] {
        let query = chain("User group .a having .a = 1", " having .a = 1", n);
        assert_depth_error(parse(&query), &format!("PowQL having chain n={n}"));
    }
}

#[test]
fn sql_and_chain_is_rejected() {
    for n in [KNOWN_BAD, TEN_X] {
        let query = chain("SELECT a FROM User WHERE a = 1", " AND a = 1", n);
        assert_depth_error(parse_sql(&query), &format!("SQL and chain n={n}"));
    }
}

#[test]
fn sql_additive_chain_is_rejected() {
    for n in [KNOWN_BAD, TEN_X] {
        let query = chain("SELECT a FROM User WHERE a = 1", " + 1", n);
        assert_depth_error(parse_sql(&query), &format!("SQL additive chain n={n}"));
    }
}

/// End to end through the engine: parse, plan, and execute must all decline
/// cleanly rather than aborting the process.
#[test]
fn engine_rejects_chain_without_crashing() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type User { required a: int }")
        .unwrap();
    engine.execute_powql("insert User { a := 1 }").unwrap();

    let powql = chain("User filter .a = 1", " and .a = 1", KNOWN_BAD);
    assert!(
        engine.execute_powql(&powql).is_err(),
        "deep PowQL and chain must be refused"
    );
    let sql = chain("SELECT a FROM User WHERE a = 1", " AND a = 1", KNOWN_BAD);
    assert!(
        engine.execute_sql(&sql).is_err(),
        "deep SQL and chain must be refused"
    );
}

/// The guard must not break ordinary queries: a chain comfortably inside the
/// documented limit still parses and executes on both frontends.
#[test]
fn moderate_chain_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type User { required a: int }")
        .unwrap();
    engine.execute_powql("insert User { a := 1 }").unwrap();

    let powql = chain("User filter .a = 1", " and .a = 1", 40);
    engine
        .execute_powql(&powql)
        .expect("a 40-term and chain must still be accepted");
    let sql = chain("SELECT a FROM User WHERE a = 1", " AND a = 1", 40);
    engine
        .execute_sql(&sql)
        .expect("a 40-term SQL and chain must still be accepted");
}

/// `Expr` drops recursively, so teardown is a second overflow site: the fix
/// relies on the construction cap being low enough that no ACCEPTED tree can
/// ever be deep enough to overflow on drop. Build the deepest shape the parser
/// still accepts (parenthesis nesting and a chain draw on the same budget),
/// run it, and drop it.
#[test]
fn deepest_accepted_tree_executes_and_drops() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql("type User { required a: int }")
        .unwrap();
    engine.execute_powql("insert User { a := 1 }").unwrap();

    let inner = chain(".a = 1", " and .a = 1", 8);
    let query = format!("User filter {}{inner}{}", "(".repeat(25), ")".repeat(25));
    let statement = parse(&query).expect("deeply parenthesized chain must parse");
    drop(statement);
    engine
        .execute_powql(&query)
        .expect("deeply parenthesized chain must execute");
}
