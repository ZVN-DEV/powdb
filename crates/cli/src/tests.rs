use super::*;

#[test]
fn cli_connect_frame_states_this_build_capabilities() {
    let frame = client_connect_message("db".into(), Some("pw".into()), None).encode();
    match Message::decode(&frame).expect("connect frame must decode") {
        Message::ConnectWithHello {
            db_name,
            password,
            hello,
            ..
        } => {
            assert_eq!(db_name, "db");
            assert_eq!(password.as_deref().map(|p| p.as_str()), Some("pw"));
            assert_eq!(hello, ClientHello::current());
        }
        other => panic!("the CLI must send a hello-bearing CONNECT, got {other:?}"),
    }
}

#[test]
fn handshake_accepts_a_negotiating_server() {
    let hello = ServerHello {
        protocol: 2,
        min_protocol: 1,
        max_protocol: 2,
        catalog_version: CLIENT_CATALOG_VERSION,
        features: vec!["sql".into()],
    };
    let (version, negotiated) = classify_handshake_reply(Some(Message::ConnectOkWithHello {
        version: "0.22.0".into(),
        hello: hello.clone(),
    }))
    .expect("a same-version server must be accepted");
    assert_eq!(version, "0.22.0");
    assert_eq!(negotiated, hello);
}

#[test]
fn handshake_stays_backward_compatible_with_a_pre_negotiation_server() {
    // A v0.21.0 server answers with a bare version string. The CLI must
    // still connect, at protocol v1.
    let (version, hello) = classify_handshake_reply(Some(Message::ConnectOk {
        version: "0.21.0".into(),
    }))
    .expect("a pre-0.22.0 server must still be usable");
    assert_eq!(version, "0.21.0");
    assert_eq!(hello, ServerHello::legacy());
}

#[test]
fn handshake_refuses_a_server_whose_catalog_is_from_the_future() {
    let failure = classify_handshake_reply(Some(Message::ConnectOkWithHello {
        version: "9.9.9".into(),
        hello: ServerHello {
            protocol: 2,
            min_protocol: 1,
            max_protocol: 2,
            catalog_version: CLIENT_CATALOG_VERSION + 1,
            features: Vec::new(),
        },
    }))
    .expect_err("a newer catalog format must be refused at handshake time");
    assert!(matches!(failure, HandshakeFailure::Refused(_)));
    assert!(
        failure.message().contains("upgrade the client"),
        "{failure:?}"
    );
}

#[test]
fn handshake_classifies_refusals_apart_from_silence() {
    let refused = classify_handshake_reply(Some(Message::Error {
        message: "authentication failed".into(),
    }))
    .expect_err("an Error reply is a refusal");
    assert!(matches!(refused, HandshakeFailure::Refused(_)));
    assert!(refused.message().contains("authentication failed"));

    // A closed socket and a non-handshake frame both mean "no usable
    // reply", which is what earns the TLS hint.
    let closed = classify_handshake_reply(None).expect_err("a closed socket is not a handshake");
    assert!(matches!(closed, HandshakeFailure::NoReply(_)));
    let wrong_frame =
        classify_handshake_reply(Some(Message::Pong)).expect_err("a Pong is not a handshake reply");
    assert!(matches!(wrong_frame, HandshakeFailure::NoReply(_)));
}

#[test]
fn remote_null_sentinel_renders_as_null_like_embedded() {
    // The wire sends NULL as the bareword "null"; remote display must
    // match the embedded REPL's `NULL`.
    assert_eq!(render_remote_cell("null"), "NULL");
    assert_eq!(format_value(&Value::Empty), "NULL");
    // Ordinary values pass through untouched.
    assert_eq!(render_remote_cell("42"), "42");
    assert_eq!(render_remote_cell(""), "");
    assert_eq!(render_remote_cell("NULL"), "NULL");
}

#[test]
fn remote_db_default_matches_ts_client() {
    assert_eq!(DEFAULT_DB_NAME, "default");
}

#[test]
fn comment_only_input_is_not_sent_to_the_engine() {
    // Every documented example opens with a comment. Sending one to the
    // engine lexes to zero tokens and reports "expected statement, got end
    // of input", so pasting the README produced one error per comment line.
    assert!(is_effectively_blank(""));
    assert!(is_effectively_blank("   \n\t "));
    assert!(is_effectively_blank("# just a comment"));
    assert!(is_effectively_blank("  # indented\n# and another\n"));

    // Real statements must still reach the engine, including one that
    // merely *contains* a comment, and one whose string literal contains a
    // '#' that is not a comment at all.
    assert!(!is_effectively_blank("User { .name }"));
    assert!(!is_effectively_blank("# leading\nUser { .name }"));
    assert!(!is_effectively_blank("User filter .s = \"#\" { .name }"));

    // A lex error is a real problem with a real statement: let the engine
    // report it rather than silently swallowing the line.
    assert!(!is_effectively_blank("User filter .s = \"unterminated"));
}

#[test]
fn typo_detection_separates_subcommands_from_data_dirs() {
    // The five slips actually observed, each one edit from a real command.
    assert_eq!(nearest_subcommand("usrs"), Some("users"));
    assert_eq!(nearest_subcommand("backupp"), Some("backup"));
    assert_eq!(nearest_subcommand("sync-statuss"), Some("sync-status"));
    assert_eq!(nearest_subcommand("useradds"), Some("useradd"));
    assert_eq!(nearest_subcommand("restor"), Some("restore"));

    // Anything path-shaped is a data directory, never a typo, even when it
    // is spelled exactly like a subcommand.
    assert_eq!(nearest_subcommand("./users"), None);
    assert_eq!(nearest_subcommand("../backup"), None);
    assert_eq!(nearest_subcommand("~/backup"), None);
    assert_eq!(nearest_subcommand("/var/lib/backup"), None);

    // Ordinary data-dir names are far enough from every subcommand.
    assert_eq!(nearest_subcommand("mydata"), None);
    assert_eq!(nearest_subcommand("powdb_data"), None);
    assert_eq!(nearest_subcommand("db"), None);
}

#[test]
fn edit_distance_counts_single_edits() {
    assert_eq!(edit_distance("", ""), 0);
    assert_eq!(edit_distance("users", "users"), 0);
    assert_eq!(edit_distance("usrs", "users"), 1); // insertion
    assert_eq!(edit_distance("backupp", "backup"), 1); // deletion
    assert_eq!(edit_distance("restire", "restore"), 1); // substitution
    assert_eq!(edit_distance("", "sweep"), 5);
}

#[test]
fn typed_results_are_requested_only_for_json_on_a_capable_server() {
    let mut modern = ServerHello::legacy();
    modern.features = vec![FEATURE_NATIVE_TYPED.to_string()];
    let old = ServerHello::legacy();

    // json is the mode that promises types, so it is the one that asks.
    assert!(negotiate_typed_json(OutputMode::Json, &modern, &mut false));
    // table and csv render the server's display strings and must not move.
    assert!(!negotiate_typed_json(
        OutputMode::Table,
        &modern,
        &mut false
    ));
    assert!(!negotiate_typed_json(OutputMode::Csv, &modern, &mut false));
    // A server without the feature falls back rather than sending a frame
    // it cannot answer.
    assert!(!negotiate_typed_json(OutputMode::Json, &old, &mut false));

    // The fallback note is emitted once per session, not once per query.
    let mut warned = false;
    negotiate_typed_json(OutputMode::Json, &old, &mut warned);
    assert!(warned, "first untyped json query must set the warned flag");
    negotiate_typed_json(OutputMode::Json, &old, &mut warned);
    assert!(warned);
}

#[test]
fn query_frames_follow_dialect_and_typing() {
    assert!(matches!(
        query_message("q".into(), Dialect::Powql, false),
        Message::Query { .. }
    ));
    assert!(matches!(
        query_message("q".into(), Dialect::Powql, true),
        Message::QueryNative { .. }
    ));
    assert!(matches!(
        query_message("q".into(), Dialect::Sql, false),
        Message::QuerySql { .. }
    ));
    assert!(matches!(
        query_message("q".into(), Dialect::Sql, true),
        Message::QuerySqlNative { .. }
    ));
}

#[test]
fn typed_rows_render_the_same_json_on_both_transports() {
    // Same columns, same values: the embedded renderer and the remote
    // typed renderer share `rows_to_json`, so this is a structural check
    // that neither transport can drift into stringly-typed output.
    let columns = vec!["i".to_string(), "f".to_string(), "b".to_string()];
    let rows = vec![vec![Value::Int(1), Value::Float(1.5), Value::Bool(true)]];
    assert_eq!(
        rows_to_json(&columns, &rows),
        r#"{"columns":["i","f","b"],"rows":[[1,1.5,true]]}"#
    );

    // i64 values past 2^53 keep every digit: the CLI writes the decimal
    // form directly and never routes an int through an f64.
    let big = vec![vec![Value::Int(9007199254740993)]];
    assert_eq!(
        rows_to_json(&["big".to_string()], &big),
        r#"{"columns":["big"],"rows":[[9007199254740993]]}"#
    );

    // NULL is JSON null, not the string "NULL".
    let nulls = vec![vec![Value::Empty]];
    assert_eq!(
        rows_to_json(&["n".to_string()], &nulls),
        r#"{"columns":["n"],"rows":[[null]]}"#
    );
}

#[test]
fn tls_config_env_vars_imply_tls() {
    // The regression: setting only a CA left the session in cleartext,
    // silently, while crates/cli/README.md said the variable implied --tls.
    assert!(tls_enabled_from_env(None, Some("/ca.pem"), None));
    assert!(tls_enabled_from_env(None, None, Some("db.internal")));
    assert!(tls_enabled_from_env(Some("0"), Some("/ca.pem"), None));

    // And the plain switch still works on its own, in both directions.
    assert!(tls_enabled_from_env(Some("1"), None, None));
    assert!(!tls_enabled_from_env(None, None, None));
    assert!(!tls_enabled_from_env(Some("0"), None, None));
}

#[test]
fn tls_enabled_env_grammar_matches_server() {
    // Same truthy grammar as the server's POWDB_REQUIRE_TLS.
    assert!(parse_tls_enabled(Some("1")));
    assert!(parse_tls_enabled(Some("true")));
    assert!(parse_tls_enabled(Some("TRUE")));
    assert!(parse_tls_enabled(Some(" yes ")));
    assert!(parse_tls_enabled(Some("on")));
    assert!(!parse_tls_enabled(Some("0")));
    assert!(!parse_tls_enabled(Some("")));
    assert!(!parse_tls_enabled(None));
}

#[test]
fn tls_server_name_derivation() {
    use tokio_rustls::rustls::pki_types::ServerName;
    // Hostname from host:port.
    assert_eq!(
        resolve_tls_server_name("db.example.com:5433", None).unwrap(),
        ServerName::try_from("db.example.com").unwrap()
    );
    // IP literal becomes an IP-address name (valid for IP-SAN certs).
    assert_eq!(
        resolve_tls_server_name("127.0.0.1:5433", None).unwrap(),
        ServerName::try_from("127.0.0.1").unwrap()
    );
    // Bracketed IPv6 literal is unwrapped.
    assert_eq!(
        resolve_tls_server_name("[::1]:5433", None).unwrap(),
        ServerName::try_from("::1").unwrap()
    );
    // Explicit override wins over the address's host part.
    assert_eq!(
        resolve_tls_server_name("127.0.0.1:5433", Some("db.example.com")).unwrap(),
        ServerName::try_from("db.example.com").unwrap()
    );
    // Garbage overrides fail with a clear error.
    assert!(resolve_tls_server_name("127.0.0.1:5433", Some("bad name!")).is_err());
}

#[test]
fn tls_connector_ca_error_paths() {
    // Missing CA file. (TlsConnector is not Debug, so match manually
    // instead of unwrap_err.)
    let Err(err) = build_tls_connector(Some("/nonexistent/ca.pem")) else {
        panic!("missing CA file must fail");
    };
    assert!(err.contains("failed to open TLS CA file"), "{err}");
    // A file with no certificates in it.
    let dir = std::env::temp_dir().join(format!("powdb_cli_tls_unit_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let empty = dir.join("empty.pem");
    std::fs::write(&empty, b"not a certificate").unwrap();
    let Err(err) = build_tls_connector(Some(empty.to_str().unwrap())) else {
        panic!("certificate-free CA file must fail");
    };
    assert!(err.contains("no certificates found"), "{err}");
    // No CA path: the built-in webpki root store builds fine.
    assert!(build_tls_connector(None).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn completion_keywords_include_current_lexer_surface() {
    for required in [
        "upsert",
        "conflict",
        "row_number",
        "dense_rank",
        "over",
        "partition",
        "date_add",
        "date_diff",
        "unique",
        "materialized",
        "explain",
    ] {
        assert!(
            POWQL_KEYWORDS.contains(&required),
            "lexer keyword missing from CLI completion source: {required}"
        );
    }
    assert!(CLI_COMMANDS.contains(&"exec"));
}

#[test]
fn cancel_line_grammar() {
    assert!(is_cancel_line(".cancel"));
    assert!(is_cancel_line("  .cancel  "));
    assert!(is_cancel_line("\\c"));
    assert!(!is_cancel_line(".cancelled"));
    assert!(!is_cancel_line("count(User) # .cancel"));
    assert!(is_cancel_line(".cancel"));
    assert!(META_COMMANDS.contains(&".cancel"));
    assert!(META_COMMANDS.contains(&".sql"));
    assert!(META_COMMANDS.contains(&".mode"));
}

#[test]
fn shared_meta_switches_dialect_and_mode() {
    let mut state = ReplState {
        timing: false,
        dialect: Dialect::Powql,
        output: OutputMode::Table,
    };
    assert!(matches!(
        handle_shared_meta(".sql", &mut state),
        MetaOutcome::Handled
    ));
    assert_eq!(state.dialect, Dialect::Sql);
    assert_eq!(state.dialect.prompt(), "sql> ");
    assert!(matches!(
        handle_shared_meta(".powql", &mut state),
        MetaOutcome::Handled
    ));
    assert_eq!(state.dialect, Dialect::Powql);

    // `.sql <STMT>` is a one-off: it must not change the mode.
    match handle_shared_meta(".sql SELECT 1", &mut state) {
        MetaOutcome::RunSql(stmt) => assert_eq!(stmt, "SELECT 1"),
        _ => panic!("`.sql <STMT>` must run one statement as SQL"),
    }
    assert_eq!(state.dialect, Dialect::Powql);

    assert!(matches!(
        handle_shared_meta(".mode json", &mut state),
        MetaOutcome::Handled
    ));
    assert_eq!(state.output, OutputMode::Json);
    // An unknown mode is reported and leaves the current mode alone.
    assert!(matches!(
        handle_shared_meta(".mode yaml", &mut state),
        MetaOutcome::Handled
    ));
    assert_eq!(state.output, OutputMode::Json);

    assert!(matches!(
        handle_shared_meta(".timing", &mut state),
        MetaOutcome::Handled
    ));
    assert!(state.timing);
    assert!(matches!(
        handle_shared_meta(".quit", &mut state),
        MetaOutcome::Quit
    ));
    // Mode-specific commands fall through to the caller.
    assert!(matches!(
        handle_shared_meta(".tables", &mut state),
        MetaOutcome::Unhandled
    ));
}

#[test]
fn output_mode_parsing() {
    assert_eq!(parse_output_mode("table"), Some(OutputMode::Table));
    assert_eq!(parse_output_mode(" JSON "), Some(OutputMode::Json));
    assert_eq!(parse_output_mode("csv"), Some(OutputMode::Csv));
    assert_eq!(parse_output_mode("yaml"), None);
}

#[test]
fn separator_hint_only_fires_on_the_real_mistake() {
    // Several newline-separated statements with no `;`: the exact shape a
    // user gets by copying a REPL session into a file.
    let hint = missing_separator_hint("insert U { a := 1 }\ninsert U { a := 2 }\n", 1)
        .expect("hint expected");
    assert!(hint.contains("separated by `;`"), "{hint}");
    assert!(hint.contains("2 non-empty lines"), "{hint}");

    // A single statement spanning lines is legitimate: no hint.
    assert!(missing_separator_hint("User\n", 1).is_none());
    // `;`-separated input that failed for some other reason: no hint.
    assert!(missing_separator_hint("insert U { a := 1 };\nbogus\n", 2).is_none());
    // Comments and blank lines do not count toward the line total.
    assert!(missing_separator_hint("# note\n\ncount(User)\n", 1).is_none());
}

#[test]
fn json_and_csv_rendering() {
    assert_eq!(json_string("a\"b\\c"), r#""a\"b\\c""#);
    assert_eq!(json_string("line\n\ttab"), r#""line\n\ttab""#);
    assert_eq!(json_string("\u{1}"), "\"\\u0001\"");

    assert_eq!(value_to_json(&Value::Int(-7)), "-7");
    assert_eq!(value_to_json(&Value::Bool(true)), "true");
    assert_eq!(value_to_json(&Value::Empty), "null");
    assert_eq!(value_to_json(&Value::Str("x\"y".into())), r#""x\"y""#);
    // A string that happens to be `null` stays a JSON string.
    assert_eq!(value_to_json(&Value::Str("null".into())), r#""null""#);
    // Remote cells are stringly typed; only the NULL sentinel is JSON null.
    assert_eq!(remote_cell_to_json("null"), "null");
    assert_eq!(remote_cell_to_json("42"), r#""42""#);

    assert_eq!(csv_field("plain"), "plain");
    assert_eq!(csv_field("a,b"), "\"a,b\"");
    assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    assert_eq!(csv_field("two\nlines"), "\"two\nlines\"");
}

#[test]
fn eof_warning_only_for_unterminated_input() {
    // Purely a shape check: the helper must ignore an empty or
    // whitespace-only buffer, which is the normal end of a session.
    warn_unterminated_at_eof("");
    warn_unterminated_at_eof("   \n");

    let mut buffer = String::from("count(User\n");
    cancel_buffer(&mut buffer);
    assert!(buffer.is_empty());
    cancel_buffer(&mut buffer);
    assert!(buffer.is_empty());
}

#[test]
fn continuation_tracking() {
    assert!(needs_continuation("type User {"));
    assert!(needs_continuation("type User {\n  required name: str,"));
    assert!(!needs_continuation("type User { required name: str }"));
    // Brace inside a string literal must not count.
    assert!(!needs_continuation(r#"insert U { s := "}" }"#));
    assert!(needs_continuation(r#"insert U { s := "}" "#));
    // Parens.
    assert!(needs_continuation("count(User filter ("));
    assert!(!needs_continuation("count(User)"));
    // Nested.
    assert!(needs_continuation("insert U { a := (1 + "));
    // Over-closed input is NOT a continuation — let the parser error.
    assert!(!needs_continuation("User }"));
    // Escaped quote inside a string must not end the string.
    assert!(needs_continuation(r#"insert U { s := "a\" "#));
    assert!(!needs_continuation(r#"insert U { s := "a\"b" }"#));
}
