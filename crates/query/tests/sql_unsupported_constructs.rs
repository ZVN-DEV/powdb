//! Five common SQL constructs must fail *by name*, not with parser noise.
//!
//! `CASE WHEN`, `COALESCE`, `COUNT(DISTINCT ...)`, `CAST(x AS TYPE)` and window
//! `OVER` are all outside the SQL subset, and all five used to lower into
//! syntactically plausible but semantically wrong canonical PowQL
//! (`count(.DISTINCT, .k)`, `cast(.id, .AS, .INT)`, a bare `.CASE` field) that
//! then died in the PowQL re-parse with a low-level message:
//!
//! ```text
//! SELECT name, CASE WHEN 1=1 THEN 'hi' ELSE 'lo' END AS lbl FROM T
//!   -> expected from, got WHEN
//! SELECT COALESCE(k, 'none') FROM T          -> expected ')', got ','
//! SELECT count(DISTINCT k) FROM T            -> expected ')', got ','
//! SELECT CAST(id AS INT) FROM T              -> expected string literal for cast type
//! SELECT row_number() OVER (ORDER BY k) FROM T -> expected from, got OVER
//! ```
//!
//! None of those name SQL, name the feature, or hint at a workaround, so they
//! are indistinguishable from the user's own typo. The documented gaps (`SQL
//! BETWEEN is not supported yet in the SQL frontend`) fail perfectly, so the
//! fix is to make these five fail in that same shape.
//!
//! Each test also runs the PowQL spelling the error message points at, so the
//! suite proves the suggested workaround is real and keeps the message honest
//! if PowQL ever changes.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

/// `T { id, k }` with two rows, one of which has a missing `k`.
fn fixture(dir: &std::path::Path) -> Engine {
    let mut engine = Engine::new(dir).unwrap();
    engine
        .execute_powql("type T { required id: int, name: str, k: str }")
        .unwrap();
    engine
        .execute_powql(r#"insert T { id := 1, name := "a", k := "x" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert T { id := 2, name := "b" }"#)
        .unwrap();
    engine
}

fn rows(engine: &mut Engine, query: &str) -> Vec<Vec<Value>> {
    match engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("PowQL `{query}` must work, got: {e}"))
    {
        QueryResult::Rows { rows, .. } => rows,
        QueryResult::Scalar(v) => vec![vec![v]],
        other => panic!("PowQL `{query}`: expected rows, got {other:?}"),
    }
}

/// Every gap must fail the way the already-documented gaps fail: a terminal
/// error that says "not supported yet", names the SQL feature, and never
/// panics or hangs.
#[track_caller]
fn refused(engine: &mut Engine, sql: &str, must_mention: &[&str]) -> String {
    let err = engine
        .execute_sql(sql)
        .err()
        .unwrap_or_else(|| panic!("`{sql}` must be refused, not silently accepted"));
    let msg = err.to_string();
    assert!(
        msg.contains("not supported yet"),
        "`{sql}`: message must read as a subset gap, got: {msg}"
    );
    for needle in must_mention {
        assert!(
            msg.contains(needle),
            "`{sql}`: message must mention `{needle}`, got: {msg}"
        );
    }
    // The error must be terminal: the engine keeps answering afterwards rather
    // than being left mid-statement.
    assert!(
        engine.execute_sql("SELECT id FROM T ORDER BY id").is_ok(),
        "`{sql}`: engine must still serve queries after the refusal"
    );
    msg
}

#[test]
fn sql_case_when_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = fixture(dir.path());

    // Was: "expected from, got WHEN".
    for sql in [
        "SELECT name, CASE WHEN 1=1 THEN 'hi' ELSE 'lo' END AS lbl FROM T",
        "SELECT name FROM T WHERE CASE WHEN id = 1 THEN true ELSE false END",
        "SELECT CASE id WHEN 1 THEN 'one' END FROM T",
    ] {
        refused(&mut engine, sql, &["CASE/WHEN", "SQL frontend"]);
    }

    // The workaround the message points at.
    assert_eq!(
        rows(
            &mut engine,
            r#"T { .name, lbl: case when .id = 1 then "hi" else "lo" end }"#
        ),
        vec![
            vec![Value::Str("a".into()), Value::Str("hi".into())],
            vec![Value::Str("b".into()), Value::Str("lo".into())],
        ]
    );

    // A quoted identifier is never a keyword, so a column named `case` is
    // still reachable and must not be swallowed by the refusal.
    let dir2 = tempfile::tempdir().unwrap();
    let mut engine2 = Engine::new(dir2.path()).unwrap();
    engine2
        .execute_powql("type C { required id: int, `case`: str }")
        .unwrap();
    engine2
        .execute_powql(r#"insert C { id := 1, `case` := "kept" }"#)
        .unwrap();
    match engine2.execute_sql(r#"SELECT "case" FROM C"#).unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Str("kept".into())]]),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn sql_coalesce_is_refused_and_points_at_the_powql_operator() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = fixture(dir.path());

    // Was: "expected ')', got ','".
    for sql in [
        "SELECT COALESCE(k, 'none') AS co FROM T",
        "SELECT id FROM T WHERE COALESCE(k, 'none') = 'none'",
        "SELECT COALESCE(k, name, 'none') FROM T",
    ] {
        refused(&mut engine, sql, &["COALESCE", "??"]);
    }

    // `??` is the working PowQL spelling the message advertises.
    assert_eq!(
        rows(&mut engine, r#"T { co: .k ?? "none" }"#),
        vec![
            vec![Value::Str("x".into())],
            vec![Value::Str("none".into())],
        ]
    );
}

#[test]
fn sql_count_distinct_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = fixture(dir.path());

    // Was: "expected ')', got ','".
    let msg = refused(
        &mut engine,
        "SELECT count(DISTINCT k) FROM T",
        &["COUNT(DISTINCT ...)", "SQL frontend"],
    );
    assert!(
        msg.contains("count(distinct T { .col })"),
        "COUNT(DISTINCT) must advertise the PowQL spelling, got: {msg}"
    );
    refused(
        &mut engine,
        "SELECT id, count(DISTINCT k) FROM T GROUP BY id",
        &["COUNT(DISTINCT ...)"],
    );

    // Other aggregates get the same shape of error, without claiming a PowQL
    // workaround that does not exist (PowQL only has count(distinct ...)).
    let sum_msg = refused(
        &mut engine,
        "SELECT SUM(DISTINCT id) FROM T",
        &["SUM(DISTINCT"],
    );
    assert!(
        !sum_msg.contains("PowQL spells"),
        "SUM(DISTINCT) must not advertise a PowQL spelling that does not exist: {sum_msg}"
    );

    // The workaround, and plain COUNT still works.
    assert_eq!(
        rows(&mut engine, "count(distinct T { .k })"),
        vec![vec![Value::Int(1)]]
    );
    match engine.execute_sql("SELECT count(k) FROM T").unwrap() {
        QueryResult::Scalar(v) => assert_eq!(v, Value::Int(1)),
        other => panic!("expected a scalar count, got {other:?}"),
    }
}

#[test]
fn sql_cast_as_type_is_refused_and_points_at_the_two_argument_form() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = fixture(dir.path());

    // Was: "expected string literal for cast type, got field '.AS'".
    for sql in [
        "SELECT CAST(id AS INT) FROM T",
        "SELECT CAST(id AS TEXT) AS s FROM T",
        "SELECT name FROM T WHERE CAST(id AS INT) = 1",
    ] {
        refused(&mut engine, sql, &["CAST(x AS TYPE)", "cast(x, 'int')"]);
    }

    // The two-argument form the message advertises really is accepted in SQL
    // mode, with single quotes for the type string.
    match engine
        .execute_sql("SELECT cast(id, 'str') AS s FROM T")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => assert_eq!(
            rows,
            vec![vec![Value::Str("1".into())], vec![Value::Str("2".into())],]
        ),
        other => panic!("expected rows, got {other:?}"),
    }
    assert_eq!(
        rows(&mut engine, r#"T { s: cast(.id, "str") }"#),
        vec![vec![Value::Str("1".into())], vec![Value::Str("2".into())],]
    );
}

#[test]
fn sql_window_functions_are_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = fixture(dir.path());

    // Was: "expected from, got OVER".
    for sql in [
        "SELECT k, row_number() OVER (ORDER BY k) AS rn FROM T",
        "SELECT sum(id) OVER (PARTITION BY k) FROM T",
        "SELECT rank() OVER () FROM T",
    ] {
        refused(
            &mut engine,
            sql,
            &["window functions (OVER)", "SQL frontend"],
        );
    }

    // PowQL has window functions, exactly as the message claims.
    assert_eq!(
        rows(&mut engine, "T { rn: row_number() over (order .id) }"),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );
}

/// The five messages, byte-exact. `docs/SQL.md` documents this set, so a
/// reword here is a docs change too.
#[test]
fn the_five_messages_are_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = fixture(dir.path());

    for (sql, expected) in [
        (
            "SELECT CASE WHEN id = 1 THEN 'hi' ELSE 'lo' END FROM T",
            "SQL CASE/WHEN is not supported yet in the SQL frontend; PowQL has it: \
             case when <cond> then <value> else <value> end",
        ),
        (
            "SELECT COALESCE(k, 'none') FROM T",
            "SQL COALESCE is not supported yet in the SQL frontend; PowQL spells it \
             with the ?? operator: .a ?? .b",
        ),
        (
            "SELECT count(DISTINCT k) FROM T",
            "SQL COUNT(DISTINCT ...) is not supported yet in the SQL frontend; \
             PowQL spells it count(distinct T { .col })",
        ),
        (
            "SELECT CAST(id AS INT) FROM T",
            "SQL CAST(x AS TYPE) is not supported yet in the SQL frontend; PowDB \
             spells a cast cast(x, 'int') with the target type as a string argument",
        ),
        (
            "SELECT row_number() OVER (ORDER BY id) FROM T",
            "SQL window functions (OVER) are not supported yet in the SQL frontend; \
             PowQL has them: row_number() over (order .col)",
        ),
    ] {
        assert_eq!(
            engine.execute_sql(sql).unwrap_err().to_string(),
            expected,
            "`{sql}`"
        );
    }
}

/// The gap that already failed correctly must keep failing correctly: this is
/// the shape all five above were made to match.
#[test]
fn the_documented_gaps_are_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = fixture(dir.path());

    refused(
        &mut engine,
        "SELECT k FROM T WHERE id BETWEEN 1 AND 2",
        &["BETWEEN"],
    );
    refused(
        &mut engine,
        "SELECT k FROM T WHERE id IN (1, 2)",
        &["IN lists"],
    );
}

/// The refusals are narrow: they must not reject supported SQL that merely
/// mentions a similar-looking name.
#[test]
fn supported_sql_is_untouched_by_the_new_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = fixture(dir.path());

    for sql in [
        "SELECT upper(name) AS u FROM T",
        "SELECT name AS n FROM T ORDER BY id",
        "SELECT count(*) FROM T",
        "SELECT count(k) FROM T",
        "SELECT id FROM T WHERE name LIKE 'a%'",
        "SELECT id FROM T WHERE k IS NULL",
        "SELECT T.id FROM T AS T WHERE T.id = 1",
    ] {
        assert!(
            engine.execute_sql(sql).is_ok(),
            "`{sql}` is inside the supported subset and must still run"
        );
    }
}

/// `docs/SQL.md` promises that an unsupported construct "returns an explicit
/// unsupported-feature parse error that names the construct". The table-level
/// constraint check originally tested only `primary`/`foreign`/`constraint`, so
/// `UNIQUE (a)` and `CHECK (a > 0)` fell through to the generic `expected type
/// name, got (`, which reads like a typo rather than a documented gap.
#[test]
fn every_table_constraint_spelling_gets_the_documented_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = fixture(dir.path());

    for sql in [
        "CREATE TABLE C1 (a INTEGER, PRIMARY KEY (a))",
        "CREATE TABLE C2 (a INTEGER, FOREIGN KEY (a) REFERENCES T(id))",
        "CREATE TABLE C3 (a INTEGER, CONSTRAINT x UNIQUE (a))",
        "CREATE TABLE C4 (a INTEGER, UNIQUE (a))",
        "CREATE TABLE C5 (a INTEGER, CHECK (a > 0))",
    ] {
        let err = engine
            .execute_sql(sql)
            .expect_err("`{sql}` must be refused")
            .to_string();
        assert!(
            err.contains("SQL table constraints are not supported"),
            "`{sql}` must name the construct, got: {err}"
        );
    }

    // The refusal must stay narrow: UNIQUE and CHECK are *column* constraints
    // too, and those follow the type rather than sitting where a column name is
    // expected. Only the column form is supported, and it must keep working.
    for sql in [
        "CREATE TABLE OK1 (a INTEGER UNIQUE, b TEXT)",
        "CREATE TABLE OK2 (a INTEGER NOT NULL, b TEXT)",
    ] {
        assert!(
            engine.execute_sql(sql).is_ok(),
            "`{sql}` is a column constraint and must still run"
        );
    }
}
