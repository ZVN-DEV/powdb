//! SQL frontend for PowDB.
//!
//! This module intentionally keeps SQL as a frontend: it parses a supported
//! SQL subset, lowers it to PowDB's existing statement AST, and records the
//! equivalent canonical PowQL text so plan-cache entries are shared with the
//! native PowQL spelling.

use crate::ast::Statement;
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
    let toks = lex_sql(input)?;
    let mut p = SqlParser {
        toks,
        pos: 0,
        depth: 0,
    };
    let canonical_powql = p.statement()?;
    if !p.at_end() {
        return Err(ParseError::Syntax {
            message: format!(
                "unexpected trailing SQL token: {}",
                p.peek()
                    .map(|t| t.display())
                    .unwrap_or_else(|| "<eof>".into())
            ),
        });
    }
    let statement = parser::parse(&canonical_powql)?;
    Ok(ParsedSql {
        statement,
        canonical_powql,
    })
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
    let mut out = Vec::new();
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
                    message: "unterminated string".into(),
                    position: i,
                });
            }
            out.push(SqlTok::String(s));
            continue;
        }
        if c == '$' {
            i += 1;
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(SqlTok::Param(chars[start..i].iter().collect()));
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
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(SqlTok::Word(chars[start..i].iter().collect()));
            continue;
        }
        if matches!(c, '(' | ')' | ',' | '*' | '.') {
            out.push(SqlTok::Symbol(c));
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
            continue;
        }
        if matches!(c, '+' | '-' | '/') {
            out.push(SqlTok::Op(c.to_string()));
            i += 1;
            continue;
        }
        return Err(ParseError::Lex {
            message: format!("unexpected SQL character `{c}`"),
            position: i,
        });
    }
    Ok(out)
}

/// Bound on SQL expression-parser recursion. The from-scratch SQL pre-parser
/// recurses on parentheses / `NOT` / operator right-hand sides before the
/// canonical text is handed to the PowQL parser, so its own guard must match
/// PowQL's `MAX_NESTING_DEPTH` (64). Without it, a deeply nested SQL string
/// arriving over the wire overflows the stack and — with panic=abort — aborts
/// the whole server process.
const MAX_SQL_NESTING_DEPTH: usize = 64;

struct SqlParser {
    toks: Vec<SqlTok>,
    pos: usize,
    depth: usize,
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
            })
        }
    }
    fn expect_ident(&mut self, what: &str) -> Result<String, ParseError> {
        match self.bump() {
            Some(SqlTok::Word(w)) if !is_reserved_identifier(&w) => Ok(w),
            Some(SqlTok::Word(w)) => Err(ParseError::Syntax {
                message: format!("expected {what}, got reserved word `{w}`"),
            }),
            Some(t) => Err(ParseError::UnexpectedToken {
                expected: what.into(),
                got: t.display(),
            }),
            None => Err(ParseError::UnexpectedToken {
                expected: what.into(),
                got: "<eof>".into(),
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
            })
        }
    }

    fn select(&mut self) -> Result<String, ParseError> {
        self.expect_kw("select")?;
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
            Some(self.field_list_until(&["having", "order", "limit", "offset"])?)
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
        if let Some(p) = projection {
            out.push_str(" { ");
            out.push_str(&p.join(", "));
            out.push_str(" }");
        }
        Ok(out)
    }

    fn projection_list(&mut self) -> Result<Option<Vec<String>>, ParseError> {
        if self.eat_sym('*') {
            return Ok(None);
        }
        let mut fields = Vec::new();
        loop {
            let expr = self.expr_until(&["from", "as"])?;
            let field = if self.eat_kw("as") {
                let alias = self.expect_ident("projection alias")?;
                format!("{alias}: {expr}")
            } else {
                expr
            };
            fields.push(field);
            if !self.eat_sym(',') {
                break;
            }
        }
        Ok(Some(fields))
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
                if self.is_kw("primary") || self.is_kw("foreign") || self.is_kw("constraint") {
                    return Err(ParseError::Unsupported { feature: "SQL table constraints are not supported; declare UNIQUE columns or add indexes explicitly".into() });
                }
                let name = self.expect_ident("column name")?;
                let ty = self.sql_type()?;
                let mut required = false;
                let mut unique = false;
                let mut default: Option<String> = None;
                loop {
                    if self.eat_kw("not") {
                        self.expect_kw("null")?;
                        required = true;
                    } else if self.eat_kw("unique") {
                        unique = true;
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
        let col = self.expect_ident("column name")?;
        self.expect_sym(')')?;
        Ok(if unique {
            format!("alter {table} add unique .{col}")
        } else {
            format!("alter {table} add index .{col}")
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
            "blob" | "bytes" => "bytes",
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
                Some(t) => {
                    return Err(ParseError::UnexpectedToken {
                        expected: "=".into(),
                        got: t.display(),
                    })
                }
                None => {
                    return Err(ParseError::UnexpectedToken {
                        expected: "=".into(),
                        got: "<eof>".into(),
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

    fn field_list_until(&mut self, stop: &[&str]) -> Result<Vec<String>, ParseError> {
        let mut fields = Vec::new();
        loop {
            fields.push(self.field_ref()?);
            if !self.eat_sym(',') || self.next_is_stop(stop) {
                break;
            }
        }
        Ok(fields)
    }

    fn order_list_until(&mut self, stop: &[&str]) -> Result<String, ParseError> {
        let mut parts = Vec::new();
        loop {
            let mut p = self.field_ref()?;
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

    fn field_ref(&mut self) -> Result<String, ParseError> {
        let first = self.expect_ident("column name")?;
        if self.eat_sym('.') {
            let second = self.expect_ident("qualified column name")?;
            Ok(format!("{first}.{second}"))
        } else {
            Ok(format!(".{first}"))
        }
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
                });
            }
            return Err(ParseError::Syntax {
                message: "expected EXISTS (...)".into(),
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

        loop {
            if self.next_is_stop(stop)
                || self.at_end()
                || matches!(self.peek(), Some(SqlTok::Symbol(')' | ',')))
            {
                break;
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
            })?;
            if l_bp < min_bp {
                self.pos -= 1;
                break;
            }
            let rhs = self.expr_bp(r_bp, stop)?;
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
            Some(SqlTok::Word(w)) => {
                if self.eat_sym('(') {
                    let func = w.to_ascii_lowercase();
                    if func == "count" && self.eat_sym('*') {
                        self.expect_sym(')')?;
                        return Ok("count(*)".into());
                    }
                    let mut args = Vec::new();
                    while !self.eat_sym(')') {
                        args.push(self.expr_bp(0, &[])?);
                        let _ = self.eat_sym(',');
                    }
                    return Ok(format!("{}({})", func, args.join(", ")));
                }
                if self.eat_sym('.') {
                    let f = self.expect_ident("qualified column name")?;
                    Ok(format!("{w}.{f}"))
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
            }),
            None => Err(ParseError::UnexpectedToken {
                expected: "expression".into(),
                got: "<eof>".into(),
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
}
