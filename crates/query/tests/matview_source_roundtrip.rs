//! A materialized view stores its defining query as reconstructed *text*
//! (`CreateViewExpr::query_text`), which the engine re-lexes on every refresh.
//! If that reconstruction is not the exact inverse of the lexer, the view runs
//! a DIFFERENT query than the one the user wrote: a silent wrong answer that no
//! error ever surfaces.
//!
//! The original defect: string literals were written back raw between two
//! quotes, so `"back\\slash"` re-lexed as `back` + a swallowed escape and the
//! view matched nothing, while `"he said \"hi\""` re-lexed into garbage and the
//! view was rejected outright.
//!
//! These tests pin the invariant end to end (create, refresh, reopen) and as a
//! property over generated queries: the stored source text must lex to exactly
//! the token stream the user's own query lexes to.

use powdb_query::ast::Statement;
use powdb_query::executor::Engine;
use powdb_query::lexer::lex;
use powdb_query::parser::parse;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_matview_roundtrip_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn exec(engine: &mut Engine, query: &str) -> QueryResult {
    engine
        .execute_powql(query)
        .unwrap_or_else(|e| panic!("failed to execute `{query}`: {e}"))
}

/// The single projected int column of every returned row, sorted.
fn col_i64(engine: &mut Engine, query: &str) -> Vec<i64> {
    match exec(engine, query) {
        QueryResult::Rows { rows, .. } => {
            let mut out: Vec<i64> = rows
                .iter()
                .map(|r| match r.as_slice() {
                    [Value::Int(n)] => *n,
                    other => panic!("expected a single int column, got {other:?}"),
                })
                .collect();
            out.sort_unstable();
            out
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The reconstructed source text a `materialize` statement would store.
fn stored_source(query: &str) -> String {
    match parse(&format!("materialize V_stored as {query}"))
        .unwrap_or_else(|e| panic!("failed to parse a materialize over `{query}`: {e}"))
    {
        Statement::CreateView(v) => v.query_text,
        other => panic!("expected CreateView, got {other:?}"),
    }
}

/// The core invariant: stored view source re-lexes to the user's own tokens.
fn assert_source_relexes_identically(query: &str) {
    let stored = stored_source(query);
    let want = lex(query).unwrap_or_else(|e| panic!("failed to lex `{query}`: {}", e.message));
    let got = lex(&stored)
        .unwrap_or_else(|e| panic!("stored source `{stored}` does not lex: {}", e.message));
    assert_eq!(
        got, want,
        "stored view source `{stored}` re-lexes differently from `{query}`"
    );
}

/// A backslash inside a string literal was written back raw, so the stored
/// source lost it and the view matched no rows while the identical direct
/// query matched one. Checked through create, refresh, and reopen: a view's
/// source text outlives the process, so a broken escape is a permanent wrong
/// answer, not a transient one.
#[test]
fn test_matview_backslash_literal_matches_direct_query() {
    let dir = temp_dir("backslash");
    std::fs::create_dir_all(&dir).unwrap();
    let mut engine = Engine::new(&dir).unwrap();

    exec(
        &mut engine,
        "type U { required id: int, required name: str }",
    );
    exec(
        &mut engine,
        r#"insert U { id := 1, name := "back\\slash" }"#,
    );
    exec(&mut engine, r#"insert U { id := 2, name := "plain" }"#);

    let direct = r#"U filter .name = "back\\slash" { .id }"#;
    assert_eq!(
        col_i64(&mut engine, direct),
        vec![1],
        "the direct query must match the backslash row"
    );

    exec(&mut engine, &format!("materialize W as {direct}"));
    assert_eq!(
        col_i64(&mut engine, "W"),
        vec![1],
        "the view must return exactly what its defining query returns"
    );

    exec(&mut engine, "refresh W");
    assert_eq!(
        col_i64(&mut engine, "W"),
        vec![1],
        "the view must survive an explicit refresh (source text is re-lexed)"
    );

    drop(engine);
    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(
        col_i64(&mut engine, "W"),
        vec![1],
        "the view must survive a reopen (source text is re-read from the catalog)"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// An escaped double quote inside a string literal was written back raw, which
/// closed the literal early and made the stored source unparseable: creating
/// the view failed outright with a trailing-token error.
#[test]
fn test_matview_escaped_quote_literal_matches_direct_query() {
    let dir = temp_dir("escaped_quote");
    std::fs::create_dir_all(&dir).unwrap();
    let mut engine = Engine::new(&dir).unwrap();

    exec(
        &mut engine,
        "type U { required id: int, required name: str }",
    );
    exec(
        &mut engine,
        r#"insert U { id := 1, name := "he said \"hi\"" }"#,
    );
    exec(&mut engine, r#"insert U { id := 2, name := "plain" }"#);

    let direct = r#"U filter .name = "he said \"hi\"" { .id }"#;
    assert_eq!(col_i64(&mut engine, direct), vec![1]);

    exec(&mut engine, &format!("materialize V as {direct}"));
    assert_eq!(col_i64(&mut engine, "V"), vec![1]);

    exec(&mut engine, "refresh V");
    assert_eq!(col_i64(&mut engine, "V"), vec![1]);

    drop(engine);
    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(col_i64(&mut engine, "V"), vec![1]);

    std::fs::remove_dir_all(&dir).ok();
}

/// A backtick-quoted identifier lets a reserved word be a table or column
/// name. Written back bare it re-lexes as the KEYWORD, so the stored source is
/// a different (usually unparseable) query.
#[test]
fn test_matview_over_reserved_word_table_name() {
    let dir = temp_dir("reserved_word");
    std::fs::create_dir_all(&dir).unwrap();
    let mut engine = Engine::new(&dir).unwrap();

    exec(
        &mut engine,
        "type `order` { required id: int, required qty: int }",
    );
    exec(&mut engine, "insert `order` { id := 1, qty := 5 }");
    exec(&mut engine, "insert `order` { id := 2, qty := 1 }");

    let direct = "`order` filter .qty > 3 { .id }";
    assert_eq!(col_i64(&mut engine, direct), vec![1]);

    exec(&mut engine, &format!("materialize BigOrders as {direct}"));
    assert_eq!(col_i64(&mut engine, "BigOrders"), vec![1]);

    exec(&mut engine, "refresh BigOrders");
    assert_eq!(col_i64(&mut engine, "BigOrders"), vec![1]);

    std::fs::remove_dir_all(&dir).ok();
}

/// Every literal shape whose reconstruction is not simply "print the payload":
/// escapes the lexer decodes, identifiers that only exist quoted, and floats
/// whose `Display` drops the decimal point (`3.0` -> `3`, which re-lexes as an
/// INTEGER token).
#[test]
fn test_stored_source_relexes_identically_for_awkward_literals() {
    let cases = [
        // String escapes the lexer decodes: \\ \" \n \t.
        r#"U filter .name = "back\\slash" { .id }"#,
        r#"U filter .name = "he said \"hi\"" { .id }"#,
        r#"U filter .name = "line\nbreak" { .id }"#,
        r#"U filter .name = "tab\there" { .id }"#,
        r#"U filter .name = "\\\"" { .id }"#,
        r#"U filter .name = "" { .id }"#,
        // A quote/hash/brace inside a literal must not leak into the grammar.
        r#"U filter .name = "} filter .x = 1 #" { .id }"#,
        // Backtick-quoted identifiers: reserved words, spaces, digits, symbols.
        "`order` filter .qty > 3 { .id }",
        "`true` { .id }",
        "`column name` { .id }",
        "U filter .`order` = 1 { .id }",
        "U filter .`field name` = 1 { .id }",
        "U { .`select` }",
        // Floats whose Display has no decimal point, and negative zero.
        "U filter .score = 3.0 { .id }",
        "U filter .score = -0.0 { .id }",
        "U filter .score = 1.5 { .id }",
        "U filter .score = -2.0 { .id }",
        // Ints, including the extremes and a negative next to a minus.
        "U filter .n = -9223372036854775808 { .id }",
        "U filter .n = 9223372036854775807 { .id }",
        "U filter .n - 1 = -5 { .id }",
    ];
    for case in cases {
        assert_source_relexes_identically(case);
    }
}

/// Property: for a generated query, the stored view source lexes to exactly
/// the token stream the query itself lexes to. Generated over the payloads
/// that actually differ between "source text" and "token payload" (escapes,
/// quoted identifiers, awkward numbers), because that is where a
/// reconstruction can silently disagree with the lexer.
mod prop {
    use super::assert_source_relexes_identically;
    use proptest::prelude::*;

    /// Characters that make a string literal interesting to re-escape.
    fn literal_payload() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just('\\'),
                Just('"'),
                Just('\n'),
                Just('\t'),
                Just('\r'),
                Just('#'),
                Just('`'),
                Just('$'),
                Just('}'),
                Just(' '),
                Just('a'),
                Just('9'),
                Just('é'),
            ],
            0..12usize,
        )
        .prop_map(|chars| {
            let mut out = String::new();
            for c in chars {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '"' => out.push_str("\\\""),
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    other => out.push(other),
                }
            }
            out
        })
    }

    /// Identifier bodies that only survive inside backticks.
    fn quoted_ident() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("order".to_string()),
            Just("filter".to_string()),
            Just("true".to_string()),
            Just("null".to_string()),
            Just("count".to_string()),
            Just("column name".to_string()),
            Just("1st".to_string()),
            Just("a-b".to_string()),
            Just("with#hash".to_string()),
            Just("with.dot".to_string()),
            Just("plain".to_string()),
        ]
    }

    proptest! {
        #[test]
        fn prop_string_literal_source_relexes_identically(payload in literal_payload()) {
            assert_source_relexes_identically(&format!(
                "U filter .name = \"{payload}\" {{ .id }}"
            ));
        }

        #[test]
        fn prop_quoted_identifier_source_relexes_identically(
            table in quoted_ident(),
            field in quoted_ident(),
        ) {
            assert_source_relexes_identically(&format!(
                "`{table}` filter .`{field}` = 1 {{ .`{field}` }}"
            ));
        }

        #[test]
        fn prop_numeric_literal_source_relexes_identically(
            int in any::<i64>(),
            float in prop_oneof![
                any::<i32>().prop_map(f64::from),
                (-1e18f64..1e18f64),
                Just(0.0f64),
                Just(-0.0f64),
            ],
        ) {
            assert_source_relexes_identically(&format!("U filter .n = {int} {{ .id }}"));
            // Rendered through Rust's own Display, which is what the lexer
            // must be able to read back.
            let rendered = if float.fract() == 0.0 {
                format!("{float:.1}")
            } else {
                format!("{float}")
            };
            assert_source_relexes_identically(&format!("U filter .s = {rendered} {{ .id }}"));
        }
    }
}

/// Fixing the reconstruction is not retroactive.
///
/// A database written by a release up to 0.21.0 can already hold a view source
/// that does not parse: the raw escaped quote below is the exact text those
/// releases wrote back for `.name = "he said \"hi\""`. Nothing rewrites a
/// source that is already on disk, so opening such a database with a fixed
/// binary still finds it.
///
/// What made that a SILENT wrong answer rather than an error is the second
/// half: dependency extraction returned "no dependencies" for a source it could
/// not parse, so no mutation ever marked the view dirty, no read ever refreshed
/// it, and every read served the rows the backing table happened to hold when
/// the source broke, permanently. The base table below gains a row that the
/// view's own query matches, and the view keeps answering without it.
///
/// The contract is correct-or-error: a view whose source cannot be read must
/// say so, naming itself, rather than answer.
mod stored_source_is_not_retroactively_fixed {
    use super::{col_i64, exec, temp_dir};
    use powdb_query::executor::Engine;
    use powdb_query::result::QueryError;
    use powdb_storage::view::{ViewDef, ViewRegistry};

    /// What a release up to 0.21.0 stored for a query containing an escaped
    /// double quote: the payload written back raw, which closes the literal
    /// early and leaves trailing tokens.
    const LEGACY_BROKEN_SOURCE: &str = r#"U filter .name = "he said "hi"" { .id }"#;

    /// Replace a registered view's stored source, exactly as a database written
    /// by an older release would already hold it: an unparseable query and the
    /// empty dependency list that release derived from it.
    fn plant_broken_source(dir: &std::path::Path, view: &str) {
        let mut registry = ViewRegistry::open(dir).expect("the view registry is readable");
        registry.unregister(view).expect("the view is registered");
        registry
            .register(ViewDef {
                name: view.to_string(),
                query: LEGACY_BROKEN_SOURCE.to_string(),
                depends_on: Vec::new(),
                dirty: false,
            })
            .expect("the view can be re-registered");
    }

    fn fixture(tag: &str) -> std::path::PathBuf {
        let dir = temp_dir(tag);
        std::fs::create_dir_all(&dir).expect("the fixture directory is creatable");
        let mut engine = Engine::new(&dir).expect("engine opens over a fresh temp dir");
        exec(
            &mut engine,
            "type U { required id: int, required name: str }",
        );
        exec(&mut engine, r#"insert U { id := 1, name := "a" }"#);
        exec(&mut engine, r#"insert U { id := 2, name := "b" }"#);
        exec(&mut engine, "materialize V as U filter .id > 0 { .id }");
        assert_eq!(
            col_i64(&mut engine, "V"),
            vec![1, 2],
            "the fixture view must be correct before its source is broken"
        );
        drop(engine);
        plant_broken_source(&dir, "V");
        dir
    }

    fn assert_names_the_view(error: &QueryError) {
        let text = error.to_string();
        assert!(
            matches!(error, QueryError::ViewError(_)),
            "a view whose source cannot be read must report a view error, got: {error:?}"
        );
        assert!(
            text.contains('V') && text.contains("materialize"),
            "the error must name the view and say how to repair it, got: {text}"
        );
    }

    /// The silent one: reading the view. The base table gains a row the view's
    /// query matches, and nothing marks the view dirty, so a read served the
    /// pre-breakage rows with no error.
    #[test]
    fn reading_a_view_with_an_unreadable_source_errors_instead_of_serving_stale_rows() {
        let dir = fixture("stale_read");
        let mut engine = Engine::new(&dir).expect("engine reopens the planted database");
        exec(&mut engine, r#"insert U { id := 3, name := "c" }"#);

        let error = engine
            .execute_powql("V")
            .expect_err("a view whose stored source cannot be parsed must not answer");
        assert_names_the_view(&error);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The same through a filter over the view, which reaches a different
    /// dirty-check site than the bare scan above.
    #[test]
    fn filtering_a_view_with_an_unreadable_source_errors() {
        let dir = fixture("stale_filter");
        let mut engine = Engine::new(&dir).expect("engine reopens the planted database");
        exec(&mut engine, r#"insert U { id := 3, name := "c" }"#);

        let error = engine
            .execute_powql("V filter .id > 0 { .id }")
            .expect_err("a view whose stored source cannot be parsed must not answer");
        assert_names_the_view(&error);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// And explicitly asking for the refresh reports the same thing rather than
    /// re-executing a source it could not read.
    #[test]
    fn refreshing_a_view_with_an_unreadable_source_errors() {
        let dir = fixture("stale_refresh");
        let mut engine = Engine::new(&dir).expect("engine reopens the planted database");

        let error = engine
            .execute_powql("refresh V")
            .expect_err("refreshing an unreadable source must not silently succeed");
        assert_names_the_view(&error);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The repair path has to work: dropping and re-creating the view over the
    /// same data must leave a view that answers correctly, so the error above
    /// is actionable rather than terminal.
    #[test]
    fn a_view_with_an_unreadable_source_can_be_dropped_and_recreated() {
        let dir = fixture("stale_repair");
        let mut engine = Engine::new(&dir).expect("engine reopens the planted database");
        exec(&mut engine, r#"insert U { id := 3, name := "c" }"#);

        exec(&mut engine, "drop view V");
        exec(&mut engine, "materialize V as U filter .id > 0 { .id }");
        assert_eq!(
            col_i64(&mut engine, "V"),
            vec![1, 2, 3],
            "the re-created view must answer its own query over the current rows"
        );

        exec(&mut engine, r#"insert U { id := 4, name := "d" }"#);
        assert_eq!(
            col_i64(&mut engine, "V"),
            vec![1, 2, 3, 4],
            "the re-created view must be dirtied by writes to its base table again"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
