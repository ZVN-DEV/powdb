//! SQL frontend for PowDB.
//!
//! This module intentionally keeps SQL as a frontend: it parses a supported
//! SQL subset, lowers it to PowDB's existing statement AST, and records the
//! equivalent canonical PowQL text so plan-cache entries are shared with the
//! native PowQL spelling.

use crate::ast::{AggregateMode, Expr, QueryExpr, Statement};
use crate::parser::{self, ParseError};

#[derive(Debug, Clone)]
pub struct ParsedSql {
    pub statement: Statement,
    pub canonical_powql: String,
}

pub fn parse_sql(input: &str) -> Result<Statement, ParseError> {
    parse_sql_with_canonical(input).map(|p| p.statement)
}

pub fn parse_sql_with_canonical(input: &str) -> Result<ParsedSql, ParseError> {
    let (toks, spans) = lex_sql_with_spans(input)?;
    let mut p = SqlParser {
        toks,
        pos: 0,
        depth: 0,
        qual_ctx: QualCtx::None,
    };
    // One boundary attaches the failing token's char offset to any
    // position-free UnexpectedToken/Syntax a production raised, mirroring
    // the PowQL parser's `attach_failing_position`.
    let canonical_powql = match p.statement() {
        Ok(q) => q,
        Err(e) => return Err(attach_sql_failing_position(e, &p, &spans)),
    };
    if !p.at_end() {
        return Err(attach_sql_failing_position(
            ParseError::Syntax {
                message: format!(
                    "unexpected trailing SQL token: {}",
                    p.peek()
                        .map(|t| t.display())
                        .unwrap_or_else(|| "<eof>".into())
                ),
                position: None,
            },
            &p,
            &spans,
        ));
    }
    let mut statement = parser::parse(&canonical_powql)?;
    mark_sql_statement_raw(&mut statement);
    Ok(ParsedSql {
        statement,
        canonical_powql,
    })
}

/// True when `input` carries no SQL statement for the engine to run: it is
/// empty, whitespace, or nothing but comments.
///
/// The CLI needs this to decide whether a `--exec` / `--exec-file` segment is
/// skippable, and it cannot reuse the PowQL blank check for SQL. PowQL's
/// comment introducer is `#`; SQL's is `--`, which the PowQL lexer reads as two
/// subtractions. So a dump ending in `-- end of dump` looked like a real
/// statement, reached the engine, and failed with `expected SQL statement, got
/// <eof>` *after* every real statement had already committed, which aborts a
/// `set -e` deploy script that had in fact succeeded.
///
/// This asks the real SQL lexer rather than scanning for `--`, so the answer
/// cannot drift from the dialect: `--` inside a string literal is not a
/// comment, and `/* ... */` blocks are handled for free. `lex_sql` itself stays
/// private because its token type is an implementation detail; this predicate
/// is the minimum public surface that answers the CLI's question.
///
/// A lex error is deliberately *not* blank: that input is a real statement with
/// a real problem, and the engine should be the one to report it. The single
/// exception is a `#` comment. `#` is not SQL's comment character (see
/// `docs/SQL.md`), so `lex_sql` rejects it outright, but the CLI shares one
/// REPL across both dialects and has always skipped `#`-only lines. Flipping
/// those to exit 1 purely because the session is in SQL mode would be a
/// regression, so they are stripped and re-offered to the same lexer.
pub fn sql_is_effectively_blank(input: &str) -> bool {
    match lex_sql(input) {
        Ok(toks) => toks.is_empty(),
        // Only reachable once the input has already failed to lex as SQL, so
        // this cannot reinterpret a `#` that lives inside a string literal:
        // such an input lexes cleanly on the first attempt and never gets here.
        Err(_) => {
            let stripped = input
                .lines()
                .map(|line| line.split('#').next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");
            matches!(lex_sql(&stripped), Ok(toks) if toks.is_empty())
        }
    }
}

/// Split SQL input into statements on `;`, using SQL's own lexical rules.
///
/// The PowQL splitter (`lexer::split_statements`) knows `"` strings and `#`
/// comments and nothing about `--` or `/* */`, so it splits on a `;` that is
/// *inside* a SQL comment. That is dangerous rather than merely wrong: given
/// `-- cleanup; DELETE FROM t`, the PowQL splitter yields `-- cleanup` and
/// `DELETE FROM t`, and the second fragment is a live statement the user
/// believed was commented out. A splitter that does not share the dialect's
/// idea of a comment cannot be used to decide what runs.
///
/// The rules here mirror `lex_sql` exactly: `--` to end of line, `/* ... */`
/// blocks, `'` and `"` quoting with `''` doubling inside `'`, and a backslash
/// escaping the next character inside either quote.
pub fn split_statements_sql(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let bytes = input.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                // An unterminated block runs to EOF; the lexer reports it.
                i = (i + 2).min(bytes.len());
            }
            q @ (b'\'' | b'"') => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == q {
                        // `''` inside a single-quoted string is one quote.
                        if q == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b';' => {
                let seg = input[start..i].trim();
                if !seg.is_empty() {
                    out.push(seg);
                }
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }

    let seg = input[start..].trim();
    if !seg.is_empty() {
        out.push(seg);
    }
    out
}

fn mark_sql_statement_raw(statement: &mut Statement) {
    match statement {
        Statement::Query(query) => mark_sql_query_raw(query),
        Statement::Union(union) => {
            mark_sql_statement_raw(&mut union.left);
            mark_sql_statement_raw(&mut union.right);
        }
        Statement::Explain(inner) => mark_sql_statement_raw(inner),
        // Dead arm: the SQL frontend has no CREATE VIEW production (see
        // `create`, which only builds TABLE and INDEX), so `parse_sql` never
        // yields a `CreateView`. It is kept only so this match stays total over
        // the shared AST. WARNING: if SQL views are ever added, a stored view's
        // canonical PowQL text must spell aggregates `raw` (this is what marks
        // them so). Dropping this marking would silently flip a stored view's
        // aggregation semantics on refresh. See the CREATE VIEW rejection test.
        Statement::CreateView(view) => mark_sql_query_raw(&mut view.query),
        _ => {}
    }
}

fn mark_sql_query_raw(query: &mut QueryExpr) {
    if let Some(aggregate) = &mut query.aggregation {
        aggregate.mode = AggregateMode::Raw;
        if let Some(argument) = &mut aggregate.argument {
            mark_sql_expr_raw(argument);
        }
    }
    for join in &mut query.joins {
        if let Some(on) = &mut join.on {
            mark_sql_expr_raw(on);
        }
    }
    if let Some(filter) = &mut query.filter {
        mark_sql_expr_raw(filter);
    }
    if let Some(order) = &mut query.order {
        for key in &mut order.keys {
            mark_sql_expr_raw(&mut key.expr);
        }
    }
    if let Some(projection) = &mut query.projection {
        for field in projection {
            mark_sql_expr_raw(&mut field.expr);
        }
    }
    if let Some(group) = &mut query.group_by {
        for key in &mut group.keys {
            mark_sql_expr_raw(&mut key.expr);
        }
        if let Some(having) = &mut group.having {
            mark_sql_expr_raw(having);
        }
    }
}

fn mark_sql_expr_raw(expr: &mut Expr) {
    match expr {
        Expr::FunctionCall(_, argument, mode) => {
            *mode = AggregateMode::Raw;
            mark_sql_expr_raw(argument);
        }
        Expr::Window {
            args,
            mode,
            partition_by,
            order_by,
            ..
        } => {
            *mode = AggregateMode::Raw;
            for expr in args.iter_mut().chain(partition_by.iter_mut()) {
                mark_sql_expr_raw(expr);
            }
            for key in order_by {
                mark_sql_expr_raw(&mut key.expr);
            }
        }
        Expr::BinaryOp(left, _, right) | Expr::Coalesce(left, right) => {
            mark_sql_expr_raw(left);
            mark_sql_expr_raw(right);
        }
        Expr::UnaryOp(_, inner) | Expr::Cast(inner, _) | Expr::JsonPath { base: inner, .. } => {
            mark_sql_expr_raw(inner);
        }
        Expr::ScalarFunc(_, args) => {
            for expr in args {
                mark_sql_expr_raw(expr);
            }
        }
        Expr::InList { expr, list, .. } => {
            mark_sql_expr_raw(expr);
            for item in list {
                mark_sql_expr_raw(item);
            }
        }
        Expr::InSubquery { expr, subquery, .. } => {
            mark_sql_expr_raw(expr);
            mark_sql_query_raw(subquery);
        }
        Expr::ExistsSubquery { subquery, .. } => mark_sql_query_raw(subquery),
        Expr::Case { whens, else_expr } => {
            for (condition, result) in whens {
                mark_sql_expr_raw(condition);
                mark_sql_expr_raw(result);
            }
            if let Some(expr) = else_expr {
                mark_sql_expr_raw(expr);
            }
        }
        _ => {}
    }
}

pub(crate) fn statement_has_aggregate(statement: &Statement) -> bool {
    match statement {
        Statement::Query(query) => query_has_aggregate(query),
        Statement::Union(union) => {
            statement_has_aggregate(&union.left) || statement_has_aggregate(&union.right)
        }
        Statement::Explain(inner) => statement_has_aggregate(inner),
        Statement::CreateView(view) => query_has_aggregate(&view.query),
        _ => false,
    }
}

fn query_has_aggregate(query: &QueryExpr) -> bool {
    query.aggregation.is_some()
        || query
            .joins
            .iter()
            .filter_map(|join| join.on.as_ref())
            .any(expr_has_aggregate)
        || query.filter.as_ref().is_some_and(expr_has_aggregate)
        || query
            .order
            .as_ref()
            .is_some_and(|order| order.keys.iter().any(|key| expr_has_aggregate(&key.expr)))
        || query.projection.as_ref().is_some_and(|projection| {
            projection
                .iter()
                .any(|field| expr_has_aggregate(&field.expr))
        })
        || query.group_by.as_ref().is_some_and(|group| {
            group.keys.iter().any(|key| expr_has_aggregate(&key.expr))
                || group.having.as_ref().is_some_and(expr_has_aggregate)
        })
}

fn expr_has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall(..) | Expr::Window { .. } => true,
        Expr::BinaryOp(left, _, right) | Expr::Coalesce(left, right) => {
            expr_has_aggregate(left) || expr_has_aggregate(right)
        }
        Expr::UnaryOp(_, inner) | Expr::Cast(inner, _) | Expr::JsonPath { base: inner, .. } => {
            expr_has_aggregate(inner)
        }
        Expr::ScalarFunc(_, args) => args.iter().any(expr_has_aggregate),
        Expr::InList { expr, list, .. } => {
            expr_has_aggregate(expr) || list.iter().any(expr_has_aggregate)
        }
        Expr::InSubquery { expr, subquery, .. } => {
            expr_has_aggregate(expr) || query_has_aggregate(subquery)
        }
        Expr::ExistsSubquery { subquery, .. } => query_has_aggregate(subquery),
        Expr::Case { whens, else_expr } => {
            whens.iter().any(|(condition, result)| {
                expr_has_aggregate(condition) || expr_has_aggregate(result)
            }) || else_expr.as_deref().is_some_and(expr_has_aggregate)
        }
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum SqlTok {
    Word(String),
    Number(String),
    String(String),
    Symbol(char),
    Op(String),
    Param(String),
}

impl SqlTok {
    fn display(&self) -> String {
        match self {
            SqlTok::Word(s) => s.clone(),
            SqlTok::Number(s) => s.clone(),
            SqlTok::String(s) => format!("'{s}'"),
            SqlTok::Symbol(c) => c.to_string(),
            SqlTok::Op(s) => s.clone(),
            SqlTok::Param(s) => format!("${s}"),
        }
    }
}

fn lex_sql(input: &str) -> Result<Vec<SqlTok>, ParseError> {
    lex_sql_with_spans(input).map(|(toks, _)| toks)
}

/// [`lex_sql`], additionally returning each token's char offset in `input`
/// (the same `at position` coordinate the PowQL lexer uses), so SQL parse
/// errors can say where they happened.
fn lex_sql_with_spans(input: &str) -> Result<(Vec<SqlTok>, Vec<usize>), ParseError> {
    let mut out = Vec::new();
    let mut spans: Vec<usize> = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            if i + 1 >= chars.len() {
                return Err(ParseError::Lex {
                    message: "unterminated block comment".into(),
                    position: i,
                });
            }
            i += 2;
            continue;
        }
        // Every recognizer below starts at the current char, so this is the
        // span recorded for whatever token this iteration emits.
        let tok_start = i;
        if c == '\'' || c == '"' {
            let quote = c;
            i += 1;
            let mut s = String::new();
            while i < chars.len() {
                if chars[i] == quote {
                    if quote == '\'' && chars.get(i + 1) == Some(&'\'') {
                        s.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                if chars[i] == '\\' && i + 1 < chars.len() {
                    let next = chars[i + 1];
                    match next {
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        other => s.push(other),
                    }
                    i += 2;
                } else {
                    s.push(chars[i]);
                    i += 1;
                }
            }
            if i > chars.len() || chars.get(i.saturating_sub(1)) != Some(&quote) {
                return Err(ParseError::Lex {
                    message: if quote == '"' {
                        "unterminated quoted identifier".into()
                    } else {
                        "unterminated string".into()
                    },
                    position: i,
                });
            }
            if quote == '"' {
                // In SQL, double quotes delimit an *identifier*; single quotes
                // delimit a string. Treating both as strings meant
                // `SELECT "name" FROM t` silently returned the literal text
                // "name" once per row instead of the column, and `FROM "t"`
                // failed outright -- so every ORM, which quotes identifiers as
                // a matter of course, was broken in both directions.
                //
                // Re-emit as a Word already wrapped in PowQL's own backtick
                // quoting. That reuses the escape hatch PowQL already has, and
                // it bypasses every keyword check downstream for free: a
                // quoted `"limit"` is a column named limit, not the LIMIT
                // keyword, and `w.eq_ignore_ascii_case("from")` cannot match
                // "`from`".
                if s.is_empty() {
                    return Err(ParseError::Syntax {
                        message: "empty quoted identifier".into(),
                        position: None,
                    });
                }
                if s.contains('`') {
                    return Err(ParseError::Unsupported {
                        feature: format!(
                            "quoted identifier `{s}` contains a backtick, which PowQL uses to \
                             quote identifiers and cannot escape"
                        ),
                    });
                }
                out.push(SqlTok::Word(format!("`{s}`")));
                spans.push(tok_start);
                continue;
            }
            out.push(SqlTok::String(s));
            spans.push(tok_start);
            continue;
        }
        if c == '$' {
            i += 1;
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(SqlTok::Param(chars[start..i].iter().collect()));
            spans.push(tok_start);
            continue;
        }
        // Longest token first: `->>` must not be split into `->` plus `>`.
        if c == '-' && chars.get(i + 1) == Some(&'>') {
            if chars.get(i + 2) == Some(&'>') {
                out.push(SqlTok::Op("->>".into()));
                spans.push(tok_start);
                i += 3;
            } else {
                out.push(SqlTok::Op("->".into()));
                spans.push(tok_start);
                i += 2;
            }
            continue;
        }
        if c.is_ascii_digit() || (c == '-' && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()))
        {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i < chars.len()
                && chars[i] == '.'
                && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit())
            {
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            out.push(SqlTok::Number(chars[start..i].iter().collect()));
            spans.push(tok_start);
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(SqlTok::Word(chars[start..i].iter().collect()));
            spans.push(tok_start);
            continue;
        }
        if matches!(c, '(' | ')' | ',' | '*' | '.') {
            out.push(SqlTok::Symbol(c));
            spans.push(tok_start);
            i += 1;
            continue;
        }
        if matches!(c, '=' | '<' | '>' | '!') {
            let mut op = String::new();
            op.push(c);
            if matches!(chars.get(i + 1), Some('=') | Some('>')) {
                op.push(chars[i + 1]);
                i += 2;
            } else {
                i += 1;
            }
            if op == "<>" {
                op = "!=".into();
            }
            out.push(SqlTok::Op(op));
            spans.push(tok_start);
            continue;
        }
        if matches!(c, '+' | '-' | '/') {
            out.push(SqlTok::Op(c.to_string()));
            spans.push(tok_start);
            i += 1;
            continue;
        }
        return Err(ParseError::Lex {
            message: format!("unexpected SQL character `{c}`"),
            position: i,
        });
    }
    Ok((out, spans))
}

/// Bound on the nesting the SQL frontend may produce. The from-scratch SQL
/// pre-parser recurses on parentheses / `NOT` / operator right-hand sides
/// before the canonical text is handed to the PowQL parser, so its own guard
/// must match PowQL's `MAX_NESTING_DEPTH` (64). Without it, a deeply nested SQL
/// string arriving over the wire overflows the stack and, with panic=abort,
/// aborts the whole server process.
///
/// The infix loop in `expr_bp` needs the same bound even though it recurses
/// only on right-hand sides: it appends left-associatively to a flat string
/// (`a AND b AND c ...`), and the PowQL parse of that canonical text builds one
/// AST level per appended operator. Counting loop iterations here keeps the
/// produced tree bounded (and stops the O(n^2) string rebuild) instead of
/// leaving the whole load on PowQL's own chain guard.
const MAX_SQL_NESTING_DEPTH: usize = 64;

/// Attach the char offset of the token the SQL parser stopped on to an
/// error that carries none; a deliberate position set in a production wins.
/// A parser standing past the last token (an EOF failure) points at the end
/// of the last token's offset — close enough to be actionable, and the only
/// coordinate the span table still has.
fn attach_sql_failing_position(
    error: ParseError,
    parser: &SqlParser,
    spans: &[usize],
) -> ParseError {
    let offset = spans
        .get(parser.pos.min(spans.len().saturating_sub(1)))
        .copied();
    match error {
        ParseError::UnexpectedToken {
            expected,
            got,
            position: None,
        } => ParseError::UnexpectedToken {
            expected,
            got,
            position: offset,
        },
        ParseError::Syntax {
            message,
            position: None,
        } => ParseError::Syntax {
            message,
            position: offset,
        },
        other => other,
    }
}

struct SqlParser {
    toks: Vec<SqlTok>,
    pos: usize,
    depth: usize,
    /// How qualified column references (`t.col`) resolve in the statement
    /// currently being parsed. Set by SELECT/UPDATE/DELETE before their
    /// expressions are parsed.
    qual_ctx: QualCtx,
}

/// Resolution context for qualified column references.
///
/// PowQL only understands the `alias.field` form inside joins; in a
/// single-table statement it must not reach the PowQL parser (the executor
/// would resolve it to Empty, silently corrupting projections and filters).
/// The SQL frontend therefore resolves single-table qualifiers itself.
#[derive(Clone, PartialEq)]
enum QualCtx {
    /// No table in scope (e.g. INSERT ... VALUES): qualified refs are errors.
    None,
    /// Single-table statement: a qualifier naming the table (or its alias,
    /// which per SQL hides the table name) lowers to a bare `.col`; any other
    /// qualifier is a hard error, matching SQLite's "no such column: x.y".
    Single { visible_name: String },
    /// Query with joins: qualifiers pass through for PowQL join resolution.
    Joined,
}

/// One item in a SELECT projection, after lowering to canonical PowQL text.
struct Projection {
    /// Canonical PowQL for this item, e.g. `count(*)`, `sum(.x)`, `n: .x + 1`.
    /// Used for the row/grouped projection path (`Table { ... }`).
    text: String,
    /// Set when the whole item is a single aggregate call. Drives the rewrite
    /// of an ungrouped aggregate SELECT into PowQL's aggregate form
    /// (`count(Table filter ...)`), which the row-projection path can't express.
    agg: Option<AggCall>,
}

/// A standalone aggregate call in a projection (`count(*)`, `sum(x)`, ...).
struct AggCall {
    /// Lowercased function name: `count` | `sum` | `avg` | `min` | `max`.
    func: String,
    arg: AggArg,
}

enum AggArg {
    /// `count(*)`.
    Star,
    /// `sum(x)` etc. — the lowered PowQL field reference (e.g. `.x`).
    Field(String),
}

impl AggCall {
    /// Canonical PowQL text for the grouped/row projection path.
    fn canonical(&self) -> String {
        match &self.arg {
            AggArg::Star => format!("{}(*)", self.func),
            AggArg::Field(f) => format!("{}({f})", self.func),
        }
    }
}

/// Lower a single ungrouped aggregate over `inner` (an already-lowered PowQL
/// source pipeline, e.g. `T filter .x > 3`) into PowQL's aggregate form. Every
/// aggregate carries its column in a trailing PowQL projection
/// (`sum(T { .x })`); only `COUNT(*)` has no column and counts rows.
/// `COUNT(col)` counts non-null values, like the grouped path and like SQL.
fn build_ungrouped_aggregate(agg: &AggCall, inner: &str) -> Result<String, ParseError> {
    match agg.func.as_str() {
        "count" if matches!(agg.arg, AggArg::Star) => Ok(format!("count({inner})")),
        "count" | "sum" | "avg" | "min" | "max" => match &agg.arg {
            AggArg::Field(f) => Ok(format!("{}({inner} {{ {f} }})", agg.func)),
            AggArg::Star => Err(ParseError::Unsupported {
                feature: format!("{0}(*) is not valid; {0}() needs a column", agg.func),
            }),
        },
        // try_aggregate only constructs the five names above.
        other => Err(ParseError::Syntax {
            message: format!("unknown aggregate function `{other}`"),
            position: None,
        }),
    }
}

impl SqlParser {
    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }
    fn peek(&self) -> Option<&SqlTok> {
        self.toks.get(self.pos)
    }
    fn bump(&mut self) -> Option<SqlTok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(SqlTok::Word(w)) if w.eq_ignore_ascii_case(kw))
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect_kw(&mut self, kw: &str) -> Result<(), ParseError> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(ParseError::UnexpectedToken {
                expected: kw.into(),
                got: self
                    .peek()
                    .map(|t| t.display())
                    .unwrap_or_else(|| "<eof>".into()),
                position: None,
            })
        }
    }
    fn eat_sym(&mut self, c: char) -> bool {
        if matches!(self.peek(), Some(SqlTok::Symbol(got)) if *got == c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect_sym(&mut self, c: char) -> Result<(), ParseError> {
        if self.eat_sym(c) {
            Ok(())
        } else {
            Err(ParseError::UnexpectedToken {
                expected: c.to_string(),
                got: self
                    .peek()
                    .map(|t| t.display())
                    .unwrap_or_else(|| "<eof>".into()),
                position: None,
            })
        }
    }
    fn expect_ident(&mut self, what: &str) -> Result<String, ParseError> {
        match self.bump() {
            Some(SqlTok::Word(w)) if !is_reserved_identifier(&w) => Ok(w),
            Some(SqlTok::Word(w)) => Err(ParseError::Syntax {
                message: format!("expected {what}, got reserved word `{w}`"),
                position: None,
            }),
            Some(t) => Err(ParseError::UnexpectedToken {
                expected: what.into(),
                got: t.display(),
                position: None,
            }),
            None => Err(ParseError::UnexpectedToken {
                expected: what.into(),
                got: "<eof>".into(),
                position: None,
            }),
        }
    }

    fn statement(&mut self) -> Result<String, ParseError> {
        if self.is_kw("select") {
            self.select()
        } else if self.is_kw("insert") {
            self.insert()
        } else if self.is_kw("update") {
            self.update()
        } else if self.is_kw("delete") {
            self.delete()
        } else if self.is_kw("create") {
            self.create()
        } else if self.is_kw("drop") {
            self.drop_stmt()
        } else if self.is_kw("alter") {
            self.alter()
        } else if self.eat_kw("begin") {
            let _ = self.eat_kw("transaction");
            Ok("begin".into())
        } else if self.eat_kw("commit") {
            Ok("commit".into())
        } else if self.eat_kw("rollback") {
            Ok("rollback".into())
        } else {
            Err(ParseError::UnexpectedToken {
                expected: "SQL statement".into(),
                got: self
                    .peek()
                    .map(|t| t.display())
                    .unwrap_or_else(|| "<eof>".into()),
                position: None,
            })
        }
    }

    /// Establish the qualified-reference context for a SELECT before its
    /// projection list is parsed. The projection precedes FROM in the token
    /// stream, so this scans ahead (at paren depth 0; subqueries are
    /// parenthesized and rejected elsewhere anyway) for the FROM table, its
    /// optional alias, and whether any join clause follows.
    fn scan_select_qual_ctx(&self) -> QualCtx {
        let mut i = self.pos;
        let mut depth = 0usize;
        loop {
            match self.toks.get(i) {
                None => return QualCtx::None,
                Some(SqlTok::Symbol('(')) => depth += 1,
                Some(SqlTok::Symbol(')')) => depth = depth.saturating_sub(1),
                Some(SqlTok::Word(w)) if depth == 0 && w.eq_ignore_ascii_case("from") => break,
                Some(_) => {}
            }
            i += 1;
        }
        let Some(SqlTok::Word(table)) = self.toks.get(i + 1) else {
            // Malformed FROM; let the main parse produce the error.
            return QualCtx::None;
        };
        let mut visible = table.clone();
        let mut j = i + 2;
        // Mirror `table_ref`: an alias is either `AS ident` or a bare word
        // that is not a clause keyword or join modifier.
        match self.toks.get(j) {
            Some(SqlTok::Word(w)) if w.eq_ignore_ascii_case("as") => {
                if let Some(SqlTok::Word(a)) = self.toks.get(j + 1) {
                    visible = a.clone();
                    j += 2;
                }
            }
            Some(SqlTok::Word(w)) if !is_clause_kw(w) && !is_join_modifier(w) => {
                visible = w.clone();
                j += 1;
            }
            _ => {}
        }
        match self.toks.get(j) {
            Some(SqlTok::Word(w)) if is_join_modifier(w) => QualCtx::Joined,
            _ => QualCtx::Single {
                visible_name: visible,
            },
        }
    }

    fn select(&mut self) -> Result<String, ParseError> {
        self.expect_kw("select")?;
        self.qual_ctx = self.scan_select_qual_ctx();
        let distinct = self.eat_kw("distinct");
        let projection = self.projection_list()?;
        self.expect_kw("from")?;
        let source = self.table_ref()?;
        let mut joins = Vec::new();
        while self.starts_join() {
            joins.push(self.join_clause()?);
        }
        let filter = if self.eat_kw("where") {
            Some(self.expr_until(&["group", "having", "order", "limit", "offset"])?)
        } else {
            None
        };
        let group = if self.eat_kw("group") {
            self.expect_kw("by")?;
            Some(self.expression_list_until(&["having", "order", "limit", "offset"])?)
        } else {
            None
        };
        let having = if self.eat_kw("having") {
            Some(self.expr_until(&["order", "limit", "offset"])?)
        } else {
            None
        };
        let order = if self.eat_kw("order") {
            self.expect_kw("by")?;
            Some(self.order_list_until(&["limit", "offset"])?)
        } else {
            None
        };
        let limit = if self.eat_kw("limit") {
            Some(self.expr_until(&["offset"])?)
        } else {
            None
        };
        let offset = if self.eat_kw("offset") {
            Some(self.expr_until(&[])?)
        } else {
            None
        };

        let has_group = group.is_some();

        let mut out = source;
        for j in joins {
            out.push(' ');
            out.push_str(&j);
        }
        if distinct {
            out.push_str(" distinct");
        }
        if let Some(f) = filter {
            out.push_str(" filter ");
            out.push_str(&f);
        }
        if let Some(keys) = group {
            out.push_str(" group ");
            out.push_str(&keys.join(", "));
            if let Some(h) = having {
                out.push_str(" having ");
                out.push_str(&h);
            }
        } else if having.is_some() {
            return Err(ParseError::Syntax {
                message: "HAVING requires GROUP BY".into(),
                position: None,
            });
        }
        if let Some(o) = order {
            out.push_str(" order ");
            out.push_str(&o);
        }
        if let Some(l) = limit {
            out.push_str(" limit ");
            out.push_str(&l);
        }
        if let Some(o) = offset {
            out.push_str(" offset ");
            out.push_str(&o);
        }
        if let Some(items) = projection {
            // An ungrouped aggregate (`SELECT count(*) FROM t`) is not a row
            // projection — PowQL expresses it as `count(t filter ...)`, which
            // yields a scalar. Without this the SQL frontend lowered it to
            // `t { count(*) }` and returned one null row per source row.
            if !has_group && items.iter().any(|p| p.agg.is_some()) {
                if distinct {
                    return Err(ParseError::Unsupported {
                        feature: "aggregates with DISTINCT and no GROUP BY are not supported by the SQL frontend".into(),
                    });
                }
                if items.len() != 1 {
                    return Err(ParseError::Unsupported {
                        feature: "multiple aggregates, or an aggregate mixed with plain columns, without GROUP BY are not supported; aggregate a single expression or add GROUP BY".into(),
                    });
                }
                // Invariant: len == 1 and the item is an aggregate (any() above).
                let agg = items.into_iter().next().unwrap().agg.unwrap();
                return build_ungrouped_aggregate(&agg, &out);
            }
            out.push_str(" { ");
            out.push_str(
                &items
                    .iter()
                    .map(|p| p.text.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            out.push_str(" }");
        }
        Ok(out)
    }

    fn projection_list(&mut self) -> Result<Option<Vec<Projection>>, ParseError> {
        if self.eat_sym('*') {
            return Ok(None);
        }
        let mut fields = Vec::new();
        loop {
            // Detect a standalone aggregate (`count(*)`, `sum(x)`) so an
            // ungrouped aggregate SELECT can be rewritten into PowQL's aggregate
            // form. Anything else (incl. an aggregate inside a larger
            // expression) falls through to the generic expression lowering.
            let (expr, agg) = match self.try_aggregate()? {
                Some(a) => (a.canonical(), Some(a)),
                None => (self.expr_until(&["from", "as"])?, None),
            };
            let text = if self.eat_kw("as") {
                let alias = self.expect_ident("projection alias")?;
                format!("{alias}: {expr}")
            } else {
                expr
            };
            fields.push(Projection { text, agg });
            if !self.eat_sym(',') {
                break;
            }
        }
        Ok(Some(fields))
    }

    /// Parse a standalone aggregate call (`count(*)`, `count(x)`, `sum(x)`, ...)
    /// when it is the *entire* projection item. Returns `None` (restoring the
    /// cursor) for non-aggregates, `count(distinct ...)`, or an aggregate that
    /// is only part of a larger expression — those take the generic path.
    fn try_aggregate(&mut self) -> Result<Option<AggCall>, ParseError> {
        let Some(SqlTok::Word(w)) = self.peek().cloned() else {
            return Ok(None);
        };
        let func = w.to_ascii_lowercase();
        if !matches!(func.as_str(), "count" | "sum" | "avg" | "min" | "max") {
            return Ok(None);
        }
        let save = self.pos;
        self.pos += 1; // consume the function name
        if !self.eat_sym('(') {
            self.pos = save;
            return Ok(None);
        }
        let arg = if func == "count" && self.eat_sym('*') {
            AggArg::Star
        } else if func == "count" && self.is_kw("distinct") {
            // count(distinct ...) has different semantics and is not part of the
            // SQL subset. Restore and let the generic expression path reject it
            // with the named unsupported-feature error (see `primary_expr`).
            self.pos = save;
            return Ok(None);
        } else {
            AggArg::Field(self.expr_bp(0, &[])?)
        };
        // Only an aggregate that fills the whole projection item is rewritable;
        // otherwise (e.g. `count(*) + 1`) restore and reparse as an expression.
        if self.eat_sym(')')
            && (matches!(self.peek(), Some(SqlTok::Symbol(',')))
                || self.is_kw("as")
                || self.is_kw("from"))
        {
            Ok(Some(AggCall { func, arg }))
        } else {
            self.pos = save;
            Ok(None)
        }
    }

    fn table_ref(&mut self) -> Result<String, ParseError> {
        let table = self.expect_ident("table name")?;
        let has_alias = self.eat_kw("as")
            || matches!(self.peek(), Some(SqlTok::Word(w)) if !is_clause_kw(w) && !is_join_modifier(w));
        if has_alias {
            let alias = self.expect_ident("table alias")?;
            Ok(format!("{table} as {alias}"))
        } else {
            Ok(table)
        }
    }

    fn starts_join(&self) -> bool {
        self.is_kw("join")
            || self.is_kw("inner")
            || self.is_kw("left")
            || self.is_kw("right")
            || self.is_kw("cross")
    }

    fn join_clause(&mut self) -> Result<String, ParseError> {
        let kind = if self.eat_kw("inner") {
            self.expect_kw("join")?;
            "inner join"
        } else if self.eat_kw("left") {
            let _ = self.eat_kw("outer");
            self.expect_kw("join")?;
            "left join"
        } else if self.eat_kw("right") {
            let _ = self.eat_kw("outer");
            self.expect_kw("join")?;
            "right join"
        } else if self.eat_kw("cross") {
            self.expect_kw("join")?;
            "cross join"
        } else {
            self.expect_kw("join")?;
            "inner join"
        };
        let table = self.table_ref()?;
        if kind == "cross join" {
            return Ok(format!("{kind} {table}"));
        }
        self.expect_kw("on")?;
        let on = self.expr_until(&[
            "join", "inner", "left", "right", "cross", "where", "group", "having", "order",
            "limit", "offset",
        ])?;
        Ok(format!("{kind} {table} on {on}"))
    }

    fn insert(&mut self) -> Result<String, ParseError> {
        self.expect_kw("insert")?;
        self.expect_kw("into")?;
        let table = self.expect_ident("table name")?;
        self.expect_sym('(')?;
        let mut cols = Vec::new();
        loop {
            cols.push(self.expect_ident("column name")?);
            if !self.eat_sym(',') {
                break;
            }
        }
        self.expect_sym(')')?;
        self.expect_kw("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect_sym('(')?;
            let mut vals = Vec::new();
            loop {
                vals.push(self.expr_until(&[])?);
                if !self.eat_sym(',') {
                    break;
                }
            }
            self.expect_sym(')')?;
            if vals.len() != cols.len() {
                return Err(ParseError::Syntax {
                    message: format!(
                        "INSERT has {} column(s) but {} value(s)",
                        cols.len(),
                        vals.len()
                    ),
                    position: None,
                });
            }
            let assigns = cols
                .iter()
                .zip(vals)
                .map(|(c, v)| format!("{c} := {v}"))
                .collect::<Vec<_>>();
            rows.push(format!("{{ {} }}", assigns.join(", ")));
            if !self.eat_sym(',') {
                break;
            }
        }
        let mut out = format!("insert {table} {}", rows.join(", "));
        if self.returning_clause()? {
            out.push_str(" returning");
        }
        Ok(out)
    }

    fn update(&mut self) -> Result<String, ParseError> {
        self.expect_kw("update")?;
        let table = self.expect_ident("table name")?;
        self.qual_ctx = QualCtx::Single {
            visible_name: table.clone(),
        };
        self.expect_kw("set")?;
        let assigns = self.assignment_list_until(&["where", "returning"])?;
        let filter = if self.eat_kw("where") {
            Some(self.expr_until(&["returning"])?)
        } else {
            None
        };
        let mut out = table;
        if let Some(f) = filter {
            out.push_str(" filter ");
            out.push_str(&f);
        }
        out.push_str(" update { ");
        out.push_str(&assigns.join(", "));
        out.push_str(" }");
        if self.returning_clause()? {
            out.push_str(" returning");
        }
        Ok(out)
    }

    fn delete(&mut self) -> Result<String, ParseError> {
        self.expect_kw("delete")?;
        self.expect_kw("from")?;
        let table = self.expect_ident("table name")?;
        self.qual_ctx = QualCtx::Single {
            visible_name: table.clone(),
        };
        let filter = if self.eat_kw("where") {
            Some(self.expr_until(&["returning"])?)
        } else {
            None
        };
        let mut out = table;
        if let Some(f) = filter {
            out.push_str(" filter ");
            out.push_str(&f);
        }
        out.push_str(" delete");
        if self.returning_clause()? {
            out.push_str(" returning");
        }
        Ok(out)
    }

    /// Parse a single literal following a column `DEFAULT`, rendered as PowQL
    /// literal text. Only scalar literals are accepted (no expression
    /// defaults), matching the PowQL `default` modifier.
    fn default_literal(&mut self) -> Result<String, ParseError> {
        match self.bump() {
            Some(SqlTok::Number(n)) => Ok(n),
            Some(SqlTok::String(s)) => Ok(quote_powql_string(&s)),
            Some(SqlTok::Word(w))
                if w.eq_ignore_ascii_case("true") || w.eq_ignore_ascii_case("false") =>
            {
                Ok(w.to_ascii_lowercase())
            }
            other => Err(ParseError::Syntax {
                message: format!(
                    "DEFAULT requires a literal value, got {}",
                    other.map(|t| t.display()).unwrap_or_else(|| "<eof>".into())
                ),
                position: None,
            }),
        }
    }

    /// Parse an optional trailing `RETURNING *`, returning whether it was
    /// present. PowQL's `returning` clause always yields every column, so a
    /// projected `RETURNING a, b` is rejected rather than silently widened to
    /// all columns.
    fn returning_clause(&mut self) -> Result<bool, ParseError> {
        if !self.eat_kw("returning") {
            return Ok(false);
        }
        if !self.eat_sym('*') {
            return Err(ParseError::Syntax {
                message: "RETURNING currently supports only `RETURNING *` \
                          (column projection is not yet supported)"
                    .into(),
                position: None,
            });
        }
        Ok(true)
    }

    fn create(&mut self) -> Result<String, ParseError> {
        self.expect_kw("create")?;
        if self.eat_kw("table") {
            let table = self.expect_ident("table name")?;
            self.expect_sym('(')?;
            let mut fields = Vec::new();
            while !self.eat_sym(')') {
                // A table-level constraint sits where a column name is expected.
                // `UNIQUE` and `CHECK` are also *column* constraints, but those
                // follow the type, so reaching them here can only be the table
                // form. A column genuinely named `unique` has to be quoted, and
                // a quoted identifier never matches `is_kw`.
                if self.is_kw("primary")
                    || self.is_kw("foreign")
                    || self.is_kw("constraint")
                    || self.is_kw("unique")
                    || self.is_kw("check")
                {
                    return Err(ParseError::Unsupported { feature: "SQL table constraints are not supported; declare UNIQUE columns or add indexes explicitly".into() });
                }
                let name = self.expect_ident("column name")?;
                let ty = self.sql_type()?;
                let mut required = false;
                let mut unique = false;
                let mut auto = false;
                let mut default: Option<String> = None;
                loop {
                    if self.eat_kw("not") {
                        self.expect_kw("null")?;
                        required = true;
                    } else if self.eat_kw("unique") {
                        unique = true;
                    } else if self.eat_kw("autoincrement") || self.eat_kw("auto_increment") {
                        auto = true;
                    } else if self.eat_kw("default") {
                        default = Some(self.default_literal()?);
                    } else if self.eat_kw("null") {
                    } else {
                        break;
                    }
                }
                let mut mods = Vec::new();
                if required {
                    mods.push("required");
                }
                if unique {
                    mods.push("unique");
                }
                if auto {
                    mods.push("auto");
                }
                let prefix = if mods.is_empty() {
                    String::new()
                } else {
                    format!("{} ", mods.join(" "))
                };
                let suffix = match default {
                    Some(lit) => format!(" default {lit}"),
                    None => String::new(),
                };
                fields.push(format!("{prefix}{name}: {ty}{suffix}"));
                let _ = self.eat_sym(',');
            }
            return Ok(format!("type {table} {{ {} }}", fields.join(", ")));
        }
        let unique = self.eat_kw("unique");
        self.expect_kw("index")?;
        let _idx = self.expect_ident("index name")?;
        self.expect_kw("on")?;
        let table = self.expect_ident("table name")?;
        self.expect_sym('(')?;
        let expression_parenthesized = self.eat_sym('(');
        if !matches!(self.peek(), Some(SqlTok::Word(_))) {
            return Err(ParseError::Unsupported {
                feature: "SQL expression indexes are not supported; use PowQL `alter <table> add index (.<json-column>-><path>)`"
                    .into(),
            });
        }
        let col = self.expect_ident("column name")?;
        let mut path = format!(".{col}");
        let mut has_json_path = false;
        loop {
            match self.peek() {
                Some(SqlTok::Op(operator)) if operator == "->" => {
                    self.bump();
                    has_json_path = true;
                    match self.bump() {
                        Some(SqlTok::String(key)) => {
                            path.push_str("->");
                            path.push_str(&quote_powql_string(&key));
                        }
                        Some(SqlTok::Number(index))
                            if !index.starts_with('-')
                                && !index.contains('.')
                                && index.parse::<u32>().is_ok() =>
                        {
                            path.push_str("->");
                            path.push_str(&index);
                        }
                        Some(segment) => {
                            return Err(ParseError::Unsupported {
                                feature: format!(
                                    "SQL JSON expression indexes require string keys or non-negative integer path segments after ->, got {}",
                                    segment.display()
                                ),
                            });
                        }
                        None => {
                            return Err(ParseError::UnexpectedToken {
                                expected: "JSON path segment after ->".into(),
                                got: "<eof>".into(),
                                position: None,
                            });
                        }
                    }
                }
                Some(SqlTok::Op(operator)) if operator == "->>" => {
                    return Err(ParseError::Unsupported {
                        feature:
                            "SQL ->> text expressions cannot be indexed; use a direct JSON -> path"
                                .into(),
                    });
                }
                _ => break,
            }
        }
        if expression_parenthesized && !has_json_path {
            return Err(ParseError::Unsupported {
                feature: "SQL expression indexes support only direct JSON -> paths; use a plain column without extra parentheses"
                    .into(),
            });
        }
        if expression_parenthesized && !self.eat_sym(')') {
            return Err(ParseError::Unsupported {
                feature: "SQL expression indexes support only direct JSON -> paths".into(),
            });
        }
        if !self.eat_sym(')') {
            return Err(ParseError::Unsupported {
                feature: "SQL expression and multi-column indexes are not supported; use PowQL `alter <table> add index (.<json-column>-><path>)` for a JSON path"
                    .into(),
            });
        }
        Ok(if unique {
            if has_json_path {
                format!("alter {table} add unique ({path})")
            } else {
                format!("alter {table} add unique .{col}")
            }
        } else {
            if has_json_path {
                format!("alter {table} add index ({path})")
            } else {
                format!("alter {table} add index .{col}")
            }
        })
    }

    fn drop_stmt(&mut self) -> Result<String, ParseError> {
        self.expect_kw("drop")?;
        if self.eat_kw("table") {
            let table = self.expect_ident("table name")?;
            Ok(format!("drop {table}"))
        } else if self.eat_kw("view") {
            let view = self.expect_ident("view name")?;
            Ok(format!("drop view {view}"))
        } else {
            Err(ParseError::UnexpectedToken {
                expected: "TABLE or VIEW".into(),
                got: self
                    .peek()
                    .map(|t| t.display())
                    .unwrap_or_else(|| "<eof>".into()),
                position: None,
            })
        }
    }

    fn alter(&mut self) -> Result<String, ParseError> {
        self.expect_kw("alter")?;
        self.expect_kw("table")?;
        let table = self.expect_ident("table name")?;
        if self.eat_kw("add") {
            let _ = self.eat_kw("column");
            let name = self.expect_ident("column name")?;
            let ty = self.sql_type()?;
            let mut required = false;
            if self.eat_kw("not") {
                self.expect_kw("null")?;
                required = true;
            }
            let prefix = if required { "required " } else { "" };
            Ok(format!("alter {table} add column {prefix}{name}: {ty}"))
        } else if self.eat_kw("drop") {
            let _ = self.eat_kw("column");
            let name = self.expect_ident("column name")?;
            Ok(format!("alter {table} drop column {name}"))
        } else {
            Err(ParseError::UnexpectedToken {
                expected: "ADD or DROP".into(),
                got: self
                    .peek()
                    .map(|t| t.display())
                    .unwrap_or_else(|| "<eof>".into()),
                position: None,
            })
        }
    }

    fn sql_type(&mut self) -> Result<String, ParseError> {
        let raw = self.expect_ident("type name")?;
        // Ignore VARCHAR(255)-style length specifiers.
        if self.eat_sym('(') {
            while !self.eat_sym(')') {
                if self.at_end() {
                    return Err(ParseError::Syntax {
                        message: "unterminated SQL type length".into(),
                        position: None,
                    });
                }
                self.bump();
            }
        }
        let ty = match raw.to_ascii_lowercase().as_str() {
            "text" | "varchar" | "char" | "string" | "str" => "str",
            "int" | "integer" | "bigint" | "smallint" => "int",
            "real" | "double" | "float" | "decimal" | "numeric" => "float",
            "bool" | "boolean" => "bool",
            "datetime" | "timestamp" => "datetime",
            "uuid" => "uuid",
            "blob" | "bytes" | "bytea" => "bytes",
            other => {
                return Err(ParseError::Unsupported {
                    feature: format!("unsupported SQL type `{other}`"),
                })
            }
        };
        Ok(ty.into())
    }

    fn assignment_list_until(&mut self, stop: &[&str]) -> Result<Vec<String>, ParseError> {
        let mut out = Vec::new();
        loop {
            let name = self.expect_ident("column name")?;
            match self.bump() {
                Some(SqlTok::Op(op)) if op == "=" => {}
                // A JSON path target (`SET data->'x' = ...`) reads a column name
                // and then a `->`/`->>` where `=` is expected. Report the
                // unsupported position precisely instead of a generic
                // "expected '='" so the user knows path mutation (json_set) is
                // not yet available and can write the whole JSON column instead.
                Some(SqlTok::Op(op)) if op == "->" || op == "->>" => {
                    return Err(ParseError::Unsupported {
                        feature: format!(
                            "cannot assign to a JSON path target `{name}{op}...`: JSON path \
                             assignment targets are not supported; write the whole JSON column \
                             instead (path mutation such as json_set is not yet available)"
                        ),
                    })
                }
                Some(t) => {
                    return Err(ParseError::UnexpectedToken {
                        expected: "=".into(),
                        got: t.display(),
                        position: None,
                    })
                }
                None => {
                    return Err(ParseError::UnexpectedToken {
                        expected: "=".into(),
                        got: "<eof>".into(),
                        position: None,
                    })
                }
            }
            let v = self.expr_until(stop)?;
            out.push(format!("{name} := {v}"));
            if !self.eat_sym(',') {
                break;
            }
        }
        Ok(out)
    }

    fn expression_list_until(&mut self, stop: &[&str]) -> Result<Vec<String>, ParseError> {
        let mut expressions = Vec::new();
        loop {
            expressions.push(self.expr_until(stop)?);
            if !self.eat_sym(',') || self.next_is_stop(stop) {
                break;
            }
        }
        Ok(expressions)
    }

    fn order_list_until(&mut self, stop: &[&str]) -> Result<String, ParseError> {
        let mut parts = Vec::new();
        let mut expression_stop = Vec::with_capacity(stop.len() + 2);
        expression_stop.extend_from_slice(stop);
        expression_stop.extend_from_slice(&["asc", "desc"]);
        loop {
            let mut p = self.expr_until(&expression_stop)?;
            if self.eat_kw("desc") {
                p.push_str(" desc");
            } else if self.eat_kw("asc") {
                p.push_str(" asc");
            }
            parts.push(p);
            if !self.eat_sym(',') || self.next_is_stop(stop) {
                break;
            }
        }
        Ok(parts.join(", "))
    }

    fn expr_until(&mut self, stop: &[&str]) -> Result<String, ParseError> {
        self.expr_bp(0, stop)
    }

    fn expr_bp(&mut self, min_bp: u8, stop: &[&str]) -> Result<String, ParseError> {
        // Guard the recursive descent against stack overflow. Error paths below
        // abort the whole parse, so only the success path needs to restore the
        // counter (done right before the final `Ok`).
        self.depth += 1;
        if self.depth > MAX_SQL_NESTING_DEPTH {
            return Err(ParseError::NestingDepthExceeded {
                max: MAX_SQL_NESTING_DEPTH,
            });
        }
        let mut lhs = if self.eat_kw("not") {
            // Standard SQL: `NOT` binds looser than comparison, so `NOT x = 1`
            // is `NOT (x = 1)`. Parse the comparison (min_bp 5 admits `=`/`<`/…
            // but stops before AND/OR) and parenthesize it so the canonical
            // PowQL re-parse is unambiguous regardless of PowQL's own NOT
            // precedence.
            format!("not ({})", self.expr_bp(5, stop)?)
        } else if self.eat_kw("exists") {
            if self.eat_sym('(') {
                if self.is_kw("select") {
                    return Err(ParseError::Unsupported {
                        feature:
                            "SQL EXISTS subqueries are not supported yet; use PowQL EXISTS for now"
                                .into(),
                    });
                }
                return Err(ParseError::Syntax {
                    message: "expected subquery after EXISTS".into(),
                    position: None,
                });
            }
            return Err(ParseError::Syntax {
                message: "expected EXISTS (...)".into(),
                position: None,
            });
        } else if self.eat_sym('(') {
            if self.is_kw("select") {
                return Err(ParseError::Unsupported {
                    feature:
                        "SQL scalar subqueries are not supported yet; use PowQL subqueries for now"
                            .into(),
                });
            }
            let inner = self.expr_bp(0, stop)?;
            self.expect_sym(')')?;
            format!("({inner})")
        } else {
            self.primary_expr()?
        };

        // Every iteration below appends one more level to `lhs`, which becomes
        // one more AST level once the canonical text is re-parsed as PowQL.
        let mut chain = 0usize;
        loop {
            if self.next_is_stop(stop)
                || self.at_end()
                || matches!(self.peek(), Some(SqlTok::Symbol(')' | ',')))
            {
                break;
            }
            chain += 1;
            if self.depth + chain > MAX_SQL_NESTING_DEPTH {
                return Err(ParseError::NestingDepthExceeded {
                    max: MAX_SQL_NESTING_DEPTH,
                });
            }
            if matches!(self.peek(), Some(SqlTok::Op(op)) if op == "->" || op == "->>") {
                let text = matches!(self.bump(), Some(SqlTok::Op(op)) if op == "->>");
                let segment = match self.bump() {
                    Some(SqlTok::String(key)) => quote_powql_string(&key),
                    Some(SqlTok::Number(index))
                        if !index.starts_with('-') && !index.contains('.') =>
                    {
                        index
                    }
                    Some(token) => {
                        return Err(ParseError::Syntax {
                            message: format!(
                                "SQL JSON arrows require a string key or non-negative integer index, got {}",
                                token.display()
                            ),
                            position: None,
                        });
                    }
                    None => {
                        return Err(ParseError::UnexpectedToken {
                            expected: "JSON object key or array index".into(),
                            got: "<eof>".into(),
                            position: None,
                        });
                    }
                };
                let path = format!("{lhs}->{segment}");
                lhs = if text {
                    format!("json_text({path})")
                } else {
                    path
                };
                continue;
            }
            if self.eat_kw("is") {
                let not = self.eat_kw("not");
                self.expect_kw("null")?;
                lhs = if not {
                    format!("{lhs} != null")
                } else {
                    format!("{lhs} = null")
                };
                continue;
            }
            if self.eat_kw("not") {
                if self.eat_kw("in") {
                    return Err(ParseError::Unsupported {
                        feature:
                            "SQL IN lists/subqueries are not supported yet in the SQL frontend"
                                .into(),
                    });
                }
                if self.eat_kw("like") {
                    let rhs = self.expr_bp(6, stop)?;
                    lhs = format!("{lhs} not like {rhs}");
                    continue;
                }
                if self.eat_kw("between") {
                    return Err(ParseError::Unsupported {
                        feature: "SQL BETWEEN is not supported yet in the SQL frontend".into(),
                    });
                }
                return Err(ParseError::UnexpectedToken {
                    expected: "IN, LIKE, or BETWEEN after NOT".into(),
                    got: self
                        .peek()
                        .map(|t| t.display())
                        .unwrap_or_else(|| "<eof>".into()),
                    position: None,
                });
            }
            if self.eat_kw("in") {
                return Err(ParseError::Unsupported {
                    feature: "SQL IN lists/subqueries are not supported yet in the SQL frontend"
                        .into(),
                });
            }
            if self.eat_kw("between") {
                return Err(ParseError::Unsupported {
                    feature: "SQL BETWEEN is not supported yet in the SQL frontend".into(),
                });
            }
            // A window call parses its function part fine and then leaves
            // `OVER` sitting where a clause keyword should be, so the failure
            // surfaced at the clause boundary ("expected from, got OVER")
            // rather than at the feature. `is_kw` (not `eat_kw`) keeps the
            // cursor on OVER; the error is terminal either way.
            if self.is_kw("over") {
                return Err(ParseError::Unsupported {
                    feature: "SQL window functions (OVER) are not supported yet in the SQL \
                              frontend; PowQL has them: row_number() over (order .col)"
                        .into(),
                });
            }
            if self.eat_kw("like") {
                let (l_bp, r_bp) = (5, 6);
                if l_bp < min_bp {
                    self.pos -= 1;
                    break;
                }
                let rhs = self.expr_bp(r_bp, stop)?;
                lhs = format!("{lhs} like {rhs}");
                continue;
            }

            let op = if self.eat_kw("or") {
                "or".to_string()
            } else if self.eat_kw("and") {
                "and".to_string()
            } else if let Some(SqlTok::Op(op)) = self.peek().cloned() {
                self.pos += 1;
                op
            } else if self.eat_sym('*') {
                "*".into()
            } else {
                break;
            };
            let (l_bp, r_bp) = infix_bp(&op).ok_or_else(|| ParseError::Syntax {
                message: format!("unsupported SQL operator `{op}`"),
                position: None,
            })?;
            if l_bp < min_bp {
                self.pos -= 1;
                break;
            }
            let rhs = self.expr_bp(r_bp, stop)?;
            // SQL text must mean SQL. PowQL deliberately desugars `x = null`
            // to `x is null` as a convenience, but in SQL a comparison against
            // NULL is UNKNOWN, so `WHERE x = NULL` and `WHERE x <> NULL` both
            // select no rows in every other engine. Emitting the PowQL
            // spelling here would silently hand back the `IS NULL` rows
            // (the opposite row set), so lower it to a constant-false
            // predicate instead.
            //
            // The `IS NULL` / `IS NOT NULL` path above is unaffected: it sets
            // `lhs` directly and never reaches this operator loop.
            //
            // Corner: PowDB filters are two-valued, so `NOT (x = NULL)`
            // yields every row where SQL's three-valued logic would yield
            // none. That is the already-documented 2VL divergence, not a new
            // one.
            if rhs == "null" && matches!(op.as_str(), "=" | "!=" | "<>") {
                lhs = "false".to_string();
                continue;
            }
            lhs = format!("{lhs} {op} {rhs}");
        }
        self.depth -= 1;
        Ok(lhs)
    }

    fn primary_expr(&mut self) -> Result<String, ParseError> {
        match self.bump() {
            Some(SqlTok::Word(w)) if w.eq_ignore_ascii_case("null") => Ok("null".into()),
            Some(SqlTok::Word(w))
                if w.eq_ignore_ascii_case("true") || w.eq_ignore_ascii_case("false") =>
            {
                Ok(w.to_ascii_lowercase())
            }
            // `CASE WHEN ... THEN ... END` otherwise lowers to a bare `.CASE`
            // field and dies at the *next* clause boundary ("expected from, got
            // WHEN"), which reads exactly like a user typo. Name the gap here.
            // A quoted `"case"` lexes as the backticked Word `` `case` `` and so
            // never reaches this arm: it stays a column named case.
            Some(SqlTok::Word(w)) if w.eq_ignore_ascii_case("case") => {
                Err(ParseError::Unsupported {
                    feature: "SQL CASE/WHEN is not supported yet in the SQL frontend; \
                              PowQL has it: case when <cond> then <value> else <value> end"
                        .into(),
                })
            }
            Some(SqlTok::Word(w)) => {
                if self.eat_sym('(') {
                    let func = w.to_ascii_lowercase();
                    if func == "count" && self.eat_sym('*') {
                        self.expect_sym(')')?;
                        return Ok("count(*)".into());
                    }
                    // Every refusal below is here because the construct would
                    // otherwise lower to a syntactically valid but *wrong*
                    // canonical PowQL call (`count(.DISTINCT, .k)`,
                    // `cast(.id, .AS, .INT)`, `coalesce(.k, "none")`) and fail
                    // in the PowQL re-parse with a low-level message
                    // ("expected ')', got ','") that names neither SQL nor the
                    // feature. Refuse in the frontend instead, in the same
                    // shape as the BETWEEN/IN refusals below.
                    if func == "coalesce" {
                        return Err(ParseError::Unsupported {
                            feature: "SQL COALESCE is not supported yet in the SQL frontend; \
                                      PowQL spells it with the ?? operator: .a ?? .b"
                                .into(),
                        });
                    }
                    if self.is_kw("distinct") {
                        // PowQL only has `count(distinct ...)`, so only claim
                        // that workaround for COUNT.
                        let hint = if func == "count" {
                            "; PowQL spells it count(distinct T { .col })"
                        } else {
                            ""
                        };
                        return Err(ParseError::Unsupported {
                            feature: format!(
                                "SQL {}(DISTINCT ...) is not supported yet in the SQL \
                                 frontend{hint}",
                                func.to_ascii_uppercase()
                            ),
                        });
                    }
                    let mut args = Vec::new();
                    while !self.eat_sym(')') {
                        args.push(self.expr_bp(0, &[])?);
                        // `CAST(x AS TYPE)`: the argument parse stops on the
                        // bare `AS` keyword, which no other supported function
                        // call can be followed by inside its own parentheses.
                        if func == "cast" && self.is_kw("as") {
                            return Err(ParseError::Unsupported {
                                feature: "SQL CAST(x AS TYPE) is not supported yet in the SQL \
                                          frontend; PowDB spells a cast cast(x, 'int') with the \
                                          target type as a string argument"
                                    .into(),
                            });
                        }
                        let _ = self.eat_sym(',');
                    }
                    return Ok(format!("{}({})", func, args.join(", ")));
                }
                if self.eat_sym('.') {
                    let f = self.expect_ident("qualified column name")?;
                    match &self.qual_ctx {
                        QualCtx::Joined => Ok(format!("{w}.{f}")),
                        QualCtx::Single { visible_name } => {
                            if w.eq_ignore_ascii_case(visible_name) {
                                Ok(format!(".{f}"))
                            } else {
                                Err(ParseError::Syntax {
                                    message: format!(
                                        "no such column: {w}.{f} (the only table in this \
                                         statement is `{visible_name}`)"
                                    ),
                                    position: None,
                                })
                            }
                        }
                        QualCtx::None => Err(ParseError::Syntax {
                            message: format!(
                                "qualified column reference `{w}.{f}` is not allowed here"
                            ),
                            position: None,
                        }),
                    }
                } else {
                    Ok(format!(".{w}"))
                }
            }
            Some(SqlTok::Number(n)) => Ok(n),
            Some(SqlTok::String(s)) => Ok(quote_powql_string(&s)),
            Some(SqlTok::Param(p)) => Ok(format!("${p}")),
            Some(SqlTok::Symbol('*')) => Ok("*".into()),
            Some(t) => Err(ParseError::Syntax {
                message: format!("unexpected SQL token in expression: {}", t.display()),
                position: None,
            }),
            None => Err(ParseError::UnexpectedToken {
                expected: "expression".into(),
                got: "<eof>".into(),
                position: None,
            }),
        }
    }

    fn next_is_stop(&self, stop: &[&str]) -> bool {
        matches!(self.peek(), Some(SqlTok::Word(w)) if stop.iter().any(|kw| w.eq_ignore_ascii_case(kw)))
    }
}

fn infix_bp(op: &str) -> Option<(u8, u8)> {
    Some(match op.to_ascii_lowercase().as_str() {
        "or" => (1, 2),
        "and" => (3, 4),
        "=" | "!=" | "<" | ">" | "<=" | ">=" => (5, 6),
        "+" | "-" => (7, 8),
        "*" | "/" => (9, 10),
        _ => return None,
    })
}

fn quote_powql_string(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\t', "\\t")
    )
}

fn is_clause_kw(w: &str) -> bool {
    matches!(
        w.to_ascii_lowercase().as_str(),
        "where"
            | "group"
            | "having"
            | "order"
            | "limit"
            | "offset"
            | "join"
            | "inner"
            | "left"
            | "right"
            | "cross"
            | "on"
            | "values"
            | "set"
    )
}
fn is_join_modifier(w: &str) -> bool {
    matches!(
        w.to_ascii_lowercase().as_str(),
        "join" | "inner" | "left" | "right" | "cross" | "outer"
    )
}
fn is_reserved_identifier(w: &str) -> bool {
    matches!(
        w.to_ascii_lowercase().as_str(),
        "select"
            | "from"
            | "where"
            | "insert"
            | "into"
            | "values"
            | "update"
            | "set"
            | "delete"
            | "create"
            | "table"
            | "drop"
            | "alter"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AlterAction, IndexTarget};

    #[test]
    fn a_comment_only_sql_segment_is_effectively_blank() {
        // A SQL dump ending in `-- end of dump` used to exit 1 after every
        // real statement had committed, because the CLI asked the *PowQL*
        // lexer, which reads `--` as two subtractions.
        for blank in [
            "",
            "   ",
            "\n\t \n",
            "-- comment only",
            "--comment only",
            "-- a\n-- b\n",
            "# comment only",
            "  # comment only  ",
            "# a\n# b",
            "/* block */",
            "/* multi\nline */\n-- and a line comment\n",
            "-- end of dump\n",
        ] {
            assert!(
                sql_is_effectively_blank(blank),
                "{blank:?} must be effectively blank"
            );
        }

        for real in [
            "SELECT 1",
            // A comment AFTER a real statement must not blank the whole input.
            "SELECT 1 -- trailing",
            "SELECT 1\n-- trailing",
            "-- leading\nSELECT 1",
            "# leading\nSELECT 1",
            "/* leading */ SELECT 1",
            // The obvious way this class of fix goes wrong: `--` inside a
            // string literal is not a comment. Asking the real lexer is what
            // makes this correct for free.
            "SELECT '-- not a comment'",
            "SELECT '# not a comment'",
            "SELECT '/* not a comment */'",
            // A lex error is a real statement with a real problem, not blank.
            "SELECT @",
            "SELECT 'unterminated",
        ] {
            assert!(
                !sql_is_effectively_blank(real),
                "{real:?} must not be treated as blank"
            );
        }
    }

    #[test]
    fn sql_frontend_rejects_create_view() {
        // The SQL frontend has no CREATE VIEW production, so the dead
        // `Statement::CreateView` arm in `mark_sql_statement_raw` is truly
        // unreachable. If this ever starts parsing, that arm (and its
        // `raw`-marking warning) must be revisited before views can round-trip.
        let result = parse_sql("CREATE VIEW v AS SELECT id FROM Post");
        assert!(
            result.is_err(),
            "CREATE VIEW must be rejected by the SQL frontend, got {result:?}"
        );
    }

    #[test]
    fn json_path_update_target_is_targeted_unsupported() {
        // `UPDATE t SET data->'x' = 5` must not die with a generic
        // "expected '='": it must name the unsupported feature and the
        // whole-column alternative.
        for stmt in [
            "UPDATE Doc SET data->'x' = 5",
            "UPDATE Doc SET data->>'x' = 5",
        ] {
            let err = parse_sql(stmt).unwrap_err();
            assert!(
                matches!(err, ParseError::Unsupported { .. }),
                "{stmt}: expected Unsupported, got {err:?}"
            );
            let msg = err.to_string();
            assert!(
                msg.contains("JSON path assignment targets are not supported"),
                "{stmt}: message must state the unsupported feature: {msg}"
            );
            assert!(
                msg.contains("json_set"),
                "{stmt}: message must point at the whole-column alternative: {msg}"
            );
        }
        // A normal whole-column update still parses.
        assert!(parse_sql("UPDATE Doc SET data = '{}'").is_ok());
    }

    #[test]
    fn json_arrows_lex_longest_token_and_lower_to_powql_paths() {
        assert_eq!(
            lex_sql("data->>'name'").unwrap(),
            vec![
                SqlTok::Word("data".into()),
                SqlTok::Op("->>".into()),
                SqlTok::String("name".into()),
            ]
        );
        let parsed = parse_sql_with_canonical(
            "SELECT data -> 'author' ->> 'name' AS name, data -> 'tags' -> 0 AS first FROM Post WHERE data ->> 'state' = 'ready'",
        )
        .unwrap();
        assert_eq!(
            parsed.canonical_powql,
            "Post filter json_text(.data->\"state\") = \"ready\" { name: json_text(.data->\"author\"->\"name\"), first: .data->\"tags\"->0 }"
        );
        let raw = parse_sql_with_canonical("SELECT data -> 'name' FROM Post").unwrap();
        let text = parse_sql_with_canonical("SELECT data ->> 'name' FROM Post").unwrap();
        assert_ne!(
            crate::canonicalize::canonicalize(&raw.canonical_powql)
                .unwrap()
                .0,
            crate::canonicalize::canonicalize(&text.canonical_powql)
                .unwrap()
                .0,
            "-> and ->> must never share a cached plan"
        );

        let ordered = parse_sql_with_canonical(
            "SELECT id FROM Post ORDER BY data ->> 'rank' DESC, data -> 'tie' ASC",
        )
        .unwrap();
        assert_eq!(
            ordered.canonical_powql,
            "Post order json_text(.data->\"rank\") desc, .data->\"tie\" asc { .id }"
        );

        let grouped = parse_sql_with_canonical(
            "SELECT data ->> 'kind' AS kind, COUNT(*) AS n FROM Post GROUP BY data ->> 'kind'",
        )
        .unwrap();
        assert_eq!(
            grouped.canonical_powql,
            "Post group json_text(.data->\"kind\") { kind: json_text(.data->\"kind\"), n: count(*) }"
        );
    }

    #[test]
    fn json_arrows_reject_invalid_path_segments() {
        for sql in [
            "SELECT data -> other FROM Post",
            "SELECT data -> -1 FROM Post",
            "SELECT data -> 1.5 FROM Post",
        ] {
            let err = parse_sql_with_canonical(sql).unwrap_err();
            assert!(
                err.to_string()
                    .contains("string key or non-negative integer index"),
                "unexpected error for `{sql}`: {err}"
            );
        }
    }

    #[test]
    fn select_lowers_to_powql_ast() {
        let sql = parse_sql_with_canonical(
            "SELECT name, age FROM User WHERE age > 25 ORDER BY age DESC LIMIT 10",
        )
        .unwrap();
        assert_eq!(
            sql.canonical_powql,
            "User filter .age > 25 order .age desc limit 10 { .name, .age }"
        );
        assert_eq!(
            sql.statement,
            parser::parse("User filter .age > 25 order .age desc limit 10 { .name, .age }")
                .unwrap()
        );
    }

    #[test]
    fn insert_update_delete_and_ddl_lower_to_existing_ast() {
        assert!(matches!(
            parse_sql("CREATE TABLE User (id INTEGER NOT NULL UNIQUE, name TEXT)").unwrap(),
            Statement::CreateType(_)
        ));
        assert!(matches!(
            parse_sql("INSERT INTO User (id, name) VALUES (1, 'Ada')").unwrap(),
            Statement::Insert(_)
        ));
        assert!(matches!(
            parse_sql("UPDATE User SET name = 'Grace' WHERE id = 1").unwrap(),
            Statement::UpdateQuery(_)
        ));
        assert!(matches!(
            parse_sql("DELETE FROM User WHERE id = 1").unwrap(),
            Statement::DeleteQuery(_)
        ));
    }

    #[test]
    fn unsupported_sql_gets_explicit_error() {
        let err = parse_sql("SELECT name FROM User WHERE id IN (SELECT user_id FROM Orders)")
            .unwrap_err();
        assert!(err.to_string().contains("SQL IN"));
    }

    #[test]
    fn sql_expression_index_has_targeted_powql_guidance() {
        assert!(matches!(
            parse_sql("CREATE INDEX post_slug ON Post (slug)").unwrap(),
            Statement::AlterTable(_)
        ));
        for sql in [
            "CREATE INDEX post_age ON Post ((data -> 'age'))",
            "CREATE INDEX post_first ON Post (data -> 'scores' -> 0)",
            "CREATE UNIQUE INDEX post_code ON Post ((data -> 'code'))",
        ] {
            let Statement::AlterTable(alter) = parse_sql(sql).unwrap() else {
                panic!("expected expression-index ALTER lowering for `{sql}`");
            };
            let target = match alter.action {
                AlterAction::AddIndex { target, .. } | AlterAction::AddUnique { target, .. } => {
                    target
                }
                action => panic!("expected add-index action, got {action:?}"),
            };
            assert!(matches!(target, IndexTarget::JsonPath(_)), "{sql}");
        }
        let error = parse_sql("CREATE INDEX post_age ON Post (data->>'age')")
            .expect_err("SQL text extraction is not an indexable path")
            .to_string();
        assert!(error.contains("->>"));
        let error = parse_sql("CREATE INDEX post_age ON Post ((data + 1))")
            .expect_err("arbitrary SQL expressions remain unsupported")
            .to_string();
        assert!(error.contains("only direct JSON -> paths"));
    }

    #[test]
    fn sql_aggregates_lower_with_raw_mode() {
        let Statement::Query(query) =
            parse_sql("SELECT dept, SUM(balance) AS total FROM Account GROUP BY dept").unwrap()
        else {
            panic!("expected query");
        };
        let projection = query.projection.expect("projection");
        assert!(matches!(
            projection[1].expr,
            Expr::FunctionCall(_, _, AggregateMode::Raw)
        ));
    }

    #[test]
    fn ungrouped_join_aggregate_lowers_to_raw_powql_aggregate() {
        let lowered = parse_sql_with_canonical(
            "SELECT AVG(a.balance) FROM Account a JOIN Entry e ON a.id = e.account_id",
        )
        .unwrap();
        assert_eq!(
            lowered.canonical_powql,
            "avg(Account as a inner join Entry as e on a.id = e.account_id { a.balance })"
        );
        let Statement::Query(query) = lowered.statement else {
            panic!("expected query");
        };
        assert_eq!(
            query.aggregation.expect("aggregate").mode,
            AggregateMode::Raw
        );
    }
}

#[cfg(test)]
mod split_tests {
    use super::split_statements_sql;

    /// The dangerous case: a `;` inside a `--` comment is not a boundary. The
    /// PowQL splitter cuts `-- cleanup; DELETE FROM t` in two, which turns a
    /// commented-out statement into a live one.
    ///
    /// Splitting and blankness are separate jobs: the splitter must keep the
    /// comment whole (one segment, not two), and `sql_is_effectively_blank`
    /// then decides that segment runs nothing. Asserting the count is the
    /// property that matters, because two segments is what deletes data.
    #[test]
    fn a_semicolon_inside_a_comment_is_not_a_boundary() {
        let segs = split_statements_sql("-- cleanup; DELETE FROM t");
        assert_eq!(segs, vec!["-- cleanup; DELETE FROM t"]);
        assert!(super::sql_is_effectively_blank(segs[0]));

        assert_eq!(
            split_statements_sql("SELECT 1 FROM t;\n-- trailing; DELETE FROM t\n"),
            vec!["SELECT 1 FROM t", "-- trailing; DELETE FROM t"]
        );
        assert_eq!(
            split_statements_sql("/* drop it; DELETE FROM t */ SELECT 1 FROM t"),
            vec!["/* drop it; DELETE FROM t */ SELECT 1 FROM t"]
        );
    }

    /// A `;` inside a string literal is data, in both quote styles, including
    /// the `''` doubling and backslash-escape forms the SQL lexer accepts.
    #[test]
    fn a_semicolon_inside_a_string_is_not_a_boundary() {
        assert_eq!(
            split_statements_sql("INSERT INTO t VALUES ('a;b')"),
            vec!["INSERT INTO t VALUES ('a;b')"]
        );
        assert_eq!(
            split_statements_sql(r#"INSERT INTO t VALUES ("a;b")"#),
            vec![r#"INSERT INTO t VALUES ("a;b")"#]
        );
        assert_eq!(
            split_statements_sql("INSERT INTO t VALUES ('it''s; here')"),
            vec!["INSERT INTO t VALUES ('it''s; here')"]
        );
        assert_eq!(
            split_statements_sql(r#"INSERT INTO t VALUES ('a\';b')"#),
            vec![r#"INSERT INTO t VALUES ('a\';b')"#]
        );
    }

    /// Ordinary splitting still works, and empty segments are dropped.
    #[test]
    fn real_boundaries_still_split() {
        assert_eq!(
            split_statements_sql("SELECT 1 FROM t; SELECT 2 FROM t;"),
            vec!["SELECT 1 FROM t", "SELECT 2 FROM t"]
        );
        assert_eq!(split_statements_sql(";;  ;"), Vec::<&str>::new());
        assert_eq!(split_statements_sql(""), Vec::<&str>::new());
    }
}
