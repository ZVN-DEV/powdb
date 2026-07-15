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
    let mut statement = parser::parse(&canonical_powql)?;
    mark_sql_statement_raw(&mut statement);
    Ok(ParsedSql {
        statement,
        canonical_powql,
    })
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
        // Longest token first: `->>` must not be split into `->` plus `>`.
        if c == '-' && chars.get(i + 1) == Some(&'>') {
            if chars.get(i + 2) == Some(&'>') {
                out.push(SqlTok::Op("->>".into()));
                i += 3;
            } else {
                out.push(SqlTok::Op("->".into()));
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
/// source pipeline, e.g. `T filter .x > 3`) into PowQL's aggregate form.
/// `count(*)`/`count(col)` both count rows; the non-null nuance of SQL
/// `count(col)` is not yet modeled. The other aggregates carry their column in
/// a trailing PowQL projection (`sum(T { .x })`).
fn build_ungrouped_aggregate(agg: &AggCall, inner: &str) -> Result<String, ParseError> {
    match agg.func.as_str() {
        "count" => Ok(format!("count({inner})")),
        "sum" | "avg" | "min" | "max" => match &agg.arg {
            AggArg::Field(f) => Ok(format!("{}({inner} {{ {f} }})", agg.func)),
            AggArg::Star => Err(ParseError::Unsupported {
                feature: format!("{0}(*) is not valid; {0}() needs a column", agg.func),
            }),
        },
        // try_aggregate only constructs the five names above.
        other => Err(ParseError::Syntax {
            message: format!("unknown aggregate function `{other}`"),
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
            // count(distinct ...) has different semantics — let the generic path
            // (which already understands `distinct`) handle it.
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
                        });
                    }
                    None => {
                        return Err(ParseError::UnexpectedToken {
                            expected: "JSON object key or array index".into(),
                            got: "<eof>".into(),
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
    use crate::ast::{AlterAction, IndexTarget};

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
        // "expected '='" — it must name the unsupported feature and the
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
