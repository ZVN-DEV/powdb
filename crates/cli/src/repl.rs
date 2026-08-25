//! REPL machinery shared by both modes: tab completion, multi-line input,
//! meta-commands, and statement splitting.

use super::*;

// ─── Tab completion helper ─────────────────────────────────────────────────

pub(crate) const CLI_COMMANDS: &[&str] = &["exec", "prepare"];
pub(crate) const DEFAULT_DB_NAME: &str = "default";
pub(crate) const META_COMMANDS: &[&str] = &[
    ".cancel", ".exit", ".help", ".mode", ".powql", ".quit", ".schema", ".sql", ".tables",
    ".timing",
];

pub(crate) struct PowqlHelper;

impl Helper for PowqlHelper {}

impl Completer for PowqlHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (start, word) = find_word_start(line, pos);
        let lower = word.to_lowercase();

        let mut matches: Vec<Pair> = Vec::new();

        if word.starts_with('.') {
            // Complete meta-commands
            for cmd in META_COMMANDS {
                if cmd.starts_with(&lower) {
                    matches.push(Pair {
                        display: cmd.to_string(),
                        replacement: cmd.to_string(),
                    });
                }
            }
        } else if start == 0 && !word.is_empty() {
            push_keyword_matches(
                POWQL_KEYWORDS.iter().chain(CLI_COMMANDS.iter()),
                &lower,
                first_char_uppercase(word),
                &mut matches,
            );
        } else if !word.is_empty() {
            push_keyword_matches(
                POWQL_KEYWORDS.iter(),
                &lower,
                first_char_uppercase(word),
                &mut matches,
            );
        }

        Ok((start, matches))
    }
}

pub(crate) fn first_char_uppercase(word: &str) -> bool {
    word.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Push completion candidates for every keyword that prefix-matches `lower`,
/// preserving the case style the user typed (leading uppercase is echoed back).
pub(crate) fn push_keyword_matches<'a>(
    keywords: impl Iterator<Item = &'a &'a str>,
    lower: &str,
    uppercase_first: bool,
    matches: &mut Vec<Pair>,
) {
    for kw in keywords {
        if kw.starts_with(lower) {
            let replacement = if uppercase_first {
                let mut s = kw.to_string();
                s[..1].make_ascii_uppercase();
                s
            } else {
                kw.to_string()
            };
            matches.push(Pair {
                display: kw.to_string(),
                replacement,
            });
        }
    }
}

impl Hinter for PowqlHelper {
    type Hint = String;

    fn hint(&self, _line: &str, _pos: usize, _ctx: &rustyline::Context<'_>) -> Option<String> {
        None
    }
}

impl Highlighter for PowqlHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Borrowed(hint)
    }
}

impl Validator for PowqlHelper {}

pub(crate) fn find_word_start(line: &str, pos: usize) -> (usize, &str) {
    let bytes = &line.as_bytes()[..pos];
    let start = bytes
        .iter()
        .rposition(|&b| b == b' ' || b == b'\t')
        .map(|i| i + 1)
        .unwrap_or(0);
    (start, &line[start..pos])
}

pub(crate) fn history_path() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".powdb_history")
}

// ─── Multi-line input ───────────────────────────────────────────────────────

/// True when `buffer` has unbalanced `{`/`(` outside string literals, i.e. the
/// REPL should read another line before executing. String literals follow the
/// lexer's rules (`crates/query/src/lexer.rs`): a backslash escapes the next
/// character, so `\"` inside a string does not terminate it.
pub(crate) fn needs_continuation(buffer: &str) -> bool {
    let mut depth: i64 = 0;
    let mut in_str = false;
    let mut chars = buffer.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_str => in_str = false,
            '"' => in_str = true,
            // Lexer treats backslash as an escape inside strings; skip the
            // escaped char so `\"` doesn't toggle the string state.
            '\\' if in_str => {
                chars.next();
            }
            '{' | '(' if !in_str => depth += 1,
            '}' | ')' if !in_str => depth -= 1,
            _ => {}
        }
    }
    depth > 0 || in_str
}

/// Mutable REPL settings shared by embedded and remote mode.
pub(crate) struct ReplState {
    pub(crate) timing: bool,
    pub(crate) dialect: Dialect,
    pub(crate) output: OutputMode,
}

/// What the caller must do after a meta-command line.
pub(crate) enum MetaOutcome {
    /// Fully handled; read the next line.
    Handled,
    /// Leave the REPL.
    Quit,
    /// Run this text once as SQL, whatever the current dialect is (`.sql <STMT>`).
    RunSql(String),
    /// Not one of the shared meta-commands; mode-specific handling applies.
    Unhandled,
}

/// Handle the meta-commands that mean the same thing in embedded and remote
/// mode. Mode-specific ones (`.tables`, `.schema`, `.help`) stay with their
/// REPL loop.
pub(crate) fn handle_shared_meta(trimmed: &str, state: &mut ReplState) -> MetaOutcome {
    match trimmed {
        ".quit" | ".exit" => MetaOutcome::Quit,
        ".timing" => {
            state.timing = !state.timing;
            println!("Timing is {}.", if state.timing { "on" } else { "off" });
            MetaOutcome::Handled
        }
        ".powql" => {
            state.dialect = Dialect::Powql;
            println!("Query language is PowQL.");
            MetaOutcome::Handled
        }
        ".sql" => {
            state.dialect = Dialect::Sql;
            println!("Query language is SQL (see docs/SQL.md for the supported subset).");
            println!("Type .powql to switch back.");
            MetaOutcome::Handled
        }
        _ if trimmed.starts_with(".sql ") => {
            let stmt = trimmed[".sql ".len()..].trim().to_string();
            if stmt.is_empty() {
                MetaOutcome::Handled
            } else {
                MetaOutcome::RunSql(stmt)
            }
        }
        ".mode" => {
            println!(
                "Output mode is {}.",
                match state.output {
                    OutputMode::Table => "table",
                    OutputMode::Json => "json",
                    OutputMode::Csv => "csv",
                }
            );
            println!("Usage: .mode <table|json|csv>");
            MetaOutcome::Handled
        }
        _ if trimmed.starts_with(".mode ") => {
            let want = &trimmed[".mode ".len()..];
            match parse_output_mode(want) {
                Some(mode) => {
                    state.output = mode;
                    println!("Output mode is {}.", want.trim().to_ascii_lowercase());
                }
                None => eprintln!(
                    "Error: unknown output mode '{}' (want table, json, or csv)",
                    want.trim()
                ),
            }
            MetaOutcome::Handled
        }
        _ => MetaOutcome::Unhandled,
    }
}

/// Whether the remote connection should use TLS, from the environment alone.
///
/// Configuring TLS implies asking for TLS. `--tls-ca` and `--tls-server-name`
/// have always implied `--tls`, and `crates/cli/README.md` documents the env
/// vars as implying it too -- but the env fallbacks used to set only their own
/// value, so `POWDB_TLS_CA=/ca.pem powdb-cli -r host:5433` connected in
/// *cleartext*, silently, while the operator had every reason to believe the
/// session was encrypted. A variable that says how to verify a certificate must
/// never leave the connection unencrypted.
pub(crate) fn tls_enabled_from_env(
    tls: Option<&str>,
    ca: Option<&str>,
    server_name: Option<&str>,
) -> bool {
    parse_tls_enabled(tls) || ca.is_some() || server_name.is_some()
}

/// True when the input carries no statement for the engine to run: empty,
/// whitespace, or nothing but `#` comments.
///
/// A blank check on the raw text is not enough. PowQL comments start with `#`
/// and run to end of line, so a comment-only line is non-empty text that the
/// lexer reduces to zero tokens, and the parser then reports "expected
/// statement, got end of input". Pasting any documented example that opens with
/// a comment produced an error per comment line.
///
/// This asks the lexer rather than scanning for `#` so the CLI cannot drift
/// from the language's own definition of a comment. A lex error is *not*
/// treated as blank: that input is a real statement with a real problem, and
/// the engine should be the one to report it.
///
/// Note that `lex` terminates every stream with `Token::Eof`, so the empty
/// program is `[Eof]`, not `[]`.
pub(crate) fn is_effectively_blank(statement: &str) -> bool {
    if statement.trim().is_empty() {
        return true;
    }
    matches!(
        powdb_query::lexer::lex(statement),
        Ok(tokens) if tokens.iter().all(|t| *t == powdb_query::token::Token::Eof)
    )
}

/// Split input into statements using the lexical rules of the dialect in use.
/// A `;` inside a comment is not a boundary, and the two dialects disagree
/// about what a comment is, so splitting with the wrong one can hand the engine
/// a fragment the user believed was commented out.
pub(crate) fn split_statements_in(input: &str, dialect: Dialect) -> Vec<&str> {
    match dialect {
        Dialect::Powql => split_statements(input),
        Dialect::Sql => powdb_query::sql::split_statements_sql(input),
    }
}

/// Dialect-aware blank check. The two languages disagree about what a comment
/// is: `--` opens a comment in SQL and is subtraction in PowQL, so asking the
/// PowQL lexer about a SQL dump that ends in `-- done` reports "not blank" and
/// the segment reaches the engine as a syntax error, exiting 1 after every real
/// statement has already committed. Each dialect answers with its own lexer so
/// neither can drift from the grammar it actually parses.
pub(crate) fn is_effectively_blank_in(statement: &str, dialect: Dialect) -> bool {
    match dialect {
        Dialect::Powql => is_effectively_blank(statement),
        Dialect::Sql => powdb_query::sql::sql_is_effectively_blank(statement),
    }
}

/// True when the line is the continuation escape hatch. Recognized anywhere,
/// including in the middle of an unterminated statement, which is the whole
/// point: without it an unbalanced `(` swallows every later line, meta-commands
/// included, until EOF.
pub(crate) fn is_cancel_line(line: &str) -> bool {
    matches!(line.trim(), ".cancel" | "\\c")
}

/// Discard a partial statement, reporting what was thrown away.
pub(crate) fn cancel_buffer(buffer: &mut String) {
    if buffer.trim().is_empty() {
        buffer.clear();
        println!("(nothing to cancel)");
    } else {
        let lines = buffer.lines().count();
        buffer.clear();
        println!(
            "(discarded {lines} line{} of unterminated input)",
            if lines == 1 { "" } else { "s" }
        );
    }
}

/// Warn about input abandoned at EOF. Without this an unbalanced delimiter
/// silently eats the rest of a piped session and exits 0.
pub(crate) fn warn_unterminated_at_eof(buffer: &str) {
    if buffer.trim().is_empty() {
        return;
    }
    let lines = buffer.lines().count();
    eprintln!(
        "Warning: {lines} line{} of unterminated input were discarded at end of input.",
        if lines == 1 { "" } else { "s" }
    );
    eprintln!(
        "Warning: an unbalanced '(' , '{{' or '\"' put the session into a continuation, so \
         everything after it (meta-commands included) was buffered and never run. \
         Use .cancel to escape a continuation."
    );
}

/// One-time note when a piped (non-terminal) session enters a continuation.
/// On a terminal the `  ...> ` prompt already makes this visible.
pub(crate) fn note_continuation_when_piped(interactive: bool, already_noted: &mut bool) {
    if interactive || *already_noted {
        return;
    }
    *already_noted = true;
    eprintln!(
        "note: unterminated statement (unbalanced '(' , '{{' or '\"'); \
         reading continuation lines. Send .cancel to discard it."
    );
}
