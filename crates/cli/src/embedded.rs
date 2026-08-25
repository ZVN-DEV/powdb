//! Embedded (in-process engine) execution: one-shot and the REPL loop.

use super::*;

// ─── One-shot execution (embedded) ──────────────────────────────────────────

pub(crate) fn exec_embedded(data_dir: &str, query: &str, session: SessionOpts) -> i32 {
    let mut engine = match Engine::new_with_wal_archive(
        Path::new(data_dir),
        archive_wal_records_if_sync_enabled,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error: failed to initialize engine: {e}");
            return 1;
        }
    };
    // Statement-aware splitting (#150): a `;` inside a string literal or a
    // `#` comment is not a boundary, so text-heavy rows load intact.
    let statements = split_statements_in(query, session.dialect);
    for stmt in &statements {
        // A segment that is only comments and whitespace is not a statement.
        // The engine lexes it to zero tokens and reports "expected statement,
        // got end of input", so a dump that merely *ended* with a comment line
        // exited 1 after committing every write — enough to abort a `set -e`
        // deploy script that had in fact succeeded.
        if is_effectively_blank_in(stmt, session.dialect) {
            continue;
        }
        if stmt.starts_with('.') {
            let cmd = stmt.split_whitespace().next().unwrap_or(stmt);
            eprintln!(
                "Error: '{}' is a REPL-only command \u{2014} start the interactive REPL without -c to use it",
                cmd
            );
            return 1;
        }
        let executed = match session.dialect {
            Dialect::Powql => engine.execute_powql(stmt),
            Dialect::Sql => engine.execute_sql(stmt),
        };
        match executed {
            Ok(result) => {
                print_local_result(&result, session.output);
            }
            Err(e) => {
                eprintln!("Error: {e}");
                if let Some(hint) = missing_separator_hint(query, statements.len()) {
                    eprintln!("{hint}");
                }
                return 1;
            }
        }
    }
    0
}

/// Explain the single most common `--exec-file` mistake instead of leaving the
/// user with a bare parser error.
///
/// `--exec` and `--exec-file` separate statements with `;`; a newline only
/// continues the current statement (PowQL pipelines legitimately span lines, so
/// newlines cannot be treated as separators without breaking them). A file of
/// newline-separated statements therefore parses as ONE statement and fails
/// with an opaque "unexpected trailing token" error. Detect that exact shape,
/// i.e. several non-empty lines and not a single `;`, and say so.
pub(crate) fn missing_separator_hint(source: &str, statement_count: usize) -> Option<String> {
    if statement_count != 1 || source.contains(';') {
        return None;
    }
    let lines = source
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .count();
    if lines < 2 {
        return None;
    }
    Some(format!(
        "note: this input has {lines} non-empty lines and no `;`, so it was parsed as ONE statement.\n\
         note: statements are separated by `;`, not by newlines (a newline continues a statement,\n\
         \x20     which is what lets a PowQL pipeline span several lines). End each statement with `;`."
    ))
}

// ─── Embedded mode ──────────────────────────────────────────────────────────

pub(crate) fn run_embedded(data_dir: &str, session: SessionOpts) {
    eprintln!("PowDB v{} — embedded mode", env!("CARGO_PKG_VERSION"));
    eprintln!("Data directory: {data_dir}");
    eprintln!("Type PowQL queries. Use Ctrl-D to exit. Type .help for commands.\n");

    let mut engine = match Engine::new_with_wal_archive(
        Path::new(data_dir),
        archive_wal_records_if_sync_enabled,
    ) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("Error: failed to initialize engine: {e}");
            std::process::exit(1);
        }
    };

    let mut rl = match Editor::new() {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("Error: failed to initialize readline: {e}");
            std::process::exit(1);
        }
    };
    rl.set_helper(Some(PowqlHelper));

    let hist = history_path();
    rl.load_history(&hist).ok();

    let mut state = ReplState {
        timing: false,
        dialect: session.dialect,
        output: session.output,
    };
    let mut buffer = String::new();
    let interactive = std::io::IsTerminal::is_terminal(&io::stdin());
    let mut continuation_noted = false;
    // A `.sql <STMT>` meta-command runs one statement outside the current
    // dialect; `None` means "use `state.dialect`".
    let mut one_off_sql: Option<String>;

    loop {
        let prompt = if buffer.is_empty() {
            state.dialect.prompt()
        } else {
            "  ...> "
        };
        let line = match rl.readline(prompt) {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(rustyline::error::ReadlineError::Interrupted) => {
                // Abandon any partial multi-line statement.
                buffer.clear();
                continuation_noted = false;
                continue;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        };

        // `.cancel` works mid-continuation; every other meta-command needs an
        // empty buffer so a `.` inside a statement stays part of the statement.
        if is_cancel_line(&line) {
            rl.add_history_entry(line.trim()).ok();
            cancel_buffer(&mut buffer);
            continuation_noted = false;
            continue;
        }

        one_off_sql = None;

        // Meta-commands are only recognized at the start of a statement.
        if buffer.is_empty() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // ── Meta-commands ──────────────────────────────────────────
            if trimmed.starts_with('.') {
                rl.add_history_entry(trimmed).ok();
                match handle_shared_meta(trimmed, &mut state) {
                    MetaOutcome::Quit => break,
                    MetaOutcome::Handled => continue,
                    MetaOutcome::RunSql(stmt) => one_off_sql = Some(stmt),
                    MetaOutcome::Unhandled => {
                        run_embedded_meta(trimmed, &engine);
                        continue;
                    }
                }
            }
        }

        // Accumulate input until braces/parens balance outside strings.
        let (statement, statement_dialect) = match one_off_sql {
            Some(stmt) => (stmt, Dialect::Sql),
            None => {
                buffer.push_str(&line);
                buffer.push('\n');
                if needs_continuation(&buffer) {
                    note_continuation_when_piped(interactive, &mut continuation_noted);
                    continue;
                }
                continuation_noted = false;
                let statement = buffer.trim().to_string();
                buffer.clear();
                if is_effectively_blank_in(&statement, session.dialect) {
                    continue;
                }
                rl.add_history_entry(&statement).ok();
                (statement, state.dialect)
            }
        };

        // ── Execute the statement ──────────────────────────────────────
        let start = Instant::now();
        let executed = match statement_dialect {
            Dialect::Powql => engine.execute_powql(&statement),
            Dialect::Sql => engine.execute_sql(&statement),
        };
        match executed {
            Ok(result) => {
                print_local_result(&result, state.output);
                if state.timing {
                    let elapsed = start.elapsed();
                    if elapsed.as_secs() >= 1 {
                        println!("Time: {:.2}s", elapsed.as_secs_f64());
                    } else {
                        println!("Time: {:.2}ms", elapsed.as_secs_f64() * 1000.0);
                    }
                }
            }
            Err(e) => eprintln!("Error: {e}"),
        }
    }

    warn_unterminated_at_eof(&buffer);
    rl.save_history(&hist).ok();
    eprintln!("\nBye!");
}

/// Meta-commands that only exist in embedded mode, where the catalog is local.
pub(crate) fn run_embedded_meta(trimmed: &str, engine: &Engine) {
    match trimmed {
        ".help" => {
            println!("Meta-commands:");
            println!("  .tables          List all tables");
            println!("  .schema <TABLE>  Show columns and types for a table");
            println!("  .sql [STMT]      Run STMT as SQL, or switch the REPL to SQL");
            println!("  .powql           Switch the REPL back to PowQL");
            println!("  .mode <FMT>      Render results as table (default), json, or csv");
            println!("  .cancel          Discard an unterminated multi-line statement");
            println!("  .timing          Toggle query timing on/off");
            println!("  .help            Show this help");
            println!("  .quit / .exit    Exit the REPL");
        }
        ".tables" => {
            let tables = engine.catalog().list_tables();
            if tables.is_empty() {
                println!("(no tables)");
            } else {
                for t in &tables {
                    println!("  {t}");
                }
                println!(
                    "({} table{})",
                    tables.len(),
                    if tables.len() == 1 { "" } else { "s" }
                );
            }
        }
        cmd if cmd.starts_with(".schema") => {
            let table_name = cmd.strip_prefix(".schema").unwrap().trim();
            if table_name.is_empty() {
                eprintln!("Usage: .schema <TABLE_NAME>");
            } else if let Some(schema) = engine.catalog().schema(table_name) {
                println!("Table: {}", schema.table_name);
                println!("  {:<20} {:<12} Required", "Column", "Type");
                println!("  {:-<20} {:-<12} {:-<8}", "", "", "");
                for col in &schema.columns {
                    println!(
                        "  {:<20} {:<12} {}",
                        col.name,
                        match col.type_id {
                            powdb_storage::types::TypeId::Int => "int",
                            powdb_storage::types::TypeId::Float => "float",
                            powdb_storage::types::TypeId::Bool => "bool",
                            powdb_storage::types::TypeId::Str => "str",
                            powdb_storage::types::TypeId::DateTime => "datetime",
                            powdb_storage::types::TypeId::Uuid => "uuid",
                            powdb_storage::types::TypeId::Bytes => "bytes",
                            powdb_storage::types::TypeId::Json => "json",
                            powdb_storage::types::TypeId::Empty => "empty",
                        },
                        if col.required { "yes" } else { "no" }
                    );
                }
            } else {
                eprintln!("Error: table '{table_name}' not found");
            }
        }
        other => {
            eprintln!("Unknown meta-command: {other}");
            eprintln!("Type .help for available commands.");
        }
    }
}
