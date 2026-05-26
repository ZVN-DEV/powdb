//! Expression evaluation functions for the PowDB executor.

use crate::ast::*;
use powdb_storage::catalog::Catalog;
use powdb_storage::types::*;

pub(super) fn collect_field_refs(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Field(name) => out.push(name.clone()),
        Expr::QualifiedField { qualifier, field } => {
            out.push(format!("{qualifier}.{field}"));
        }
        Expr::BinaryOp(l, _, r) => {
            collect_field_refs(l, out);
            collect_field_refs(r, out);
        }
        Expr::UnaryOp(_, inner) => collect_field_refs(inner, out),
        Expr::FunctionCall(_, inner) => collect_field_refs(inner, out),
        Expr::Coalesce(l, r) => {
            collect_field_refs(l, out);
            collect_field_refs(r, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_field_refs(expr, out);
            for item in list {
                collect_field_refs(item, out);
            }
        }
        Expr::ScalarFunc(_, args) => {
            for a in args {
                collect_field_refs(a, out);
            }
        }
        Expr::Cast(inner, _) => {
            collect_field_refs(inner, out);
        }
        Expr::Case { whens, else_expr } => {
            for (c, r) in whens {
                collect_field_refs(c, out);
                collect_field_refs(r, out);
            }
            if let Some(e) = else_expr {
                collect_field_refs(e, out);
            }
        }
        _ => {}
    }
}

/// Detect whether a subquery is correlated: any `Expr::Field` reference in
/// the subquery's filter that doesn't match a column in the subquery's
/// source table indicates a reference to an outer scope.
/// Replace outer-scope field references in a correlated subquery's filter
/// with literal values from the current outer row. Fields that belong to
/// the subquery's own source table are left unchanged.
pub(super) fn substitute_outer_refs(
    expr: &Expr,
    subquery_source: &str,
    catalog: &Catalog,
    outer_row: &[Value],
    outer_columns: &[String],
) -> Expr {
    let sub_cols: Vec<String> = catalog
        .schema(subquery_source)
        .map(|s| s.columns.iter().map(|c| c.name.clone()).collect())
        .unwrap_or_default();
    substitute_outer_refs_inner(expr, &sub_cols, outer_row, outer_columns)
}

fn substitute_outer_refs_inner(
    expr: &Expr,
    sub_cols: &[String],
    outer_row: &[Value],
    outer_columns: &[String],
) -> Expr {
    match expr {
        Expr::Field(name) => {
            if sub_cols.iter().any(|c| c == name) {
                expr.clone()
            } else if let Some(i) = outer_columns.iter().position(|c| c == name) {
                value_to_expr(outer_row[i].clone())
            } else {
                expr.clone()
            }
        }
        Expr::BinaryOp(l, op, r) => {
            let l = substitute_outer_refs_inner(l, sub_cols, outer_row, outer_columns);
            let r = substitute_outer_refs_inner(r, sub_cols, outer_row, outer_columns);
            Expr::BinaryOp(Box::new(l), *op, Box::new(r))
        }
        Expr::UnaryOp(op, inner) => {
            let inner = substitute_outer_refs_inner(inner, sub_cols, outer_row, outer_columns);
            Expr::UnaryOp(*op, Box::new(inner))
        }
        Expr::InList {
            expr: e,
            list,
            negated,
        } => {
            let e = substitute_outer_refs_inner(e, sub_cols, outer_row, outer_columns);
            let list = list
                .iter()
                .map(|item| substitute_outer_refs_inner(item, sub_cols, outer_row, outer_columns))
                .collect();
            Expr::InList {
                expr: Box::new(e),
                list,
                negated: *negated,
            }
        }
        Expr::Coalesce(l, r) => {
            let l = substitute_outer_refs_inner(l, sub_cols, outer_row, outer_columns);
            let r = substitute_outer_refs_inner(r, sub_cols, outer_row, outer_columns);
            Expr::Coalesce(Box::new(l), Box::new(r))
        }
        other => other.clone(),
    }
}

pub(super) fn is_correlated_subquery(subquery: &QueryExpr, catalog: &Catalog) -> bool {
    let filter = match &subquery.filter {
        Some(f) => f,
        None => return false,
    };
    let schema = match catalog.schema(&subquery.source) {
        Some(s) => s,
        None => return false, // table not found — not correlation, just an error
    };
    let table_cols: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
    let mut refs = Vec::new();
    collect_field_refs(filter, &mut refs);
    // If any referenced field doesn't exist in the subquery's source table,
    // it's (probably) a reference to an outer scope — i.e., correlated.
    refs.iter().any(|r| {
        // Skip qualified references (alias.field) — they unambiguously
        // target a specific source and will only match the subquery's own
        // source if they share the alias.
        if r.contains('.') {
            let alias = subquery.alias.as_deref().unwrap_or(&subquery.source);
            !r.starts_with(alias)
        } else {
            !table_cols.iter().any(|c| c == r)
        }
    })
}

pub(super) fn contains_subquery(expr: &Expr) -> bool {
    match expr {
        Expr::InSubquery { .. } => true,
        Expr::ExistsSubquery { .. } => true,
        Expr::BinaryOp(l, _, r) => contains_subquery(l) || contains_subquery(r),
        Expr::UnaryOp(_, inner) => contains_subquery(inner),
        Expr::InList { expr, list, .. } => {
            contains_subquery(expr) || list.iter().any(contains_subquery)
        }
        Expr::Case { whens, else_expr } => {
            whens
                .iter()
                .any(|(c, r)| contains_subquery(c) || contains_subquery(r))
                || else_expr.as_ref().is_some_and(|e| contains_subquery(e))
        }
        Expr::ScalarFunc(_, args) => args.iter().any(contains_subquery),
        Expr::Cast(inner, _) => contains_subquery(inner),
        Expr::FunctionCall(_, inner) => contains_subquery(inner),
        Expr::Coalesce(l, r) => contains_subquery(l) || contains_subquery(r),
        _ => false,
    }
}

pub(super) fn value_to_expr(val: Value) -> Expr {
    match val {
        Value::Int(v) => Expr::Literal(Literal::Int(v)),
        Value::Float(v) => Expr::Literal(Literal::Float(v)),
        Value::Str(v) => Expr::Literal(Literal::String(v)),
        Value::Bool(v) => Expr::Literal(Literal::Bool(v)),
        _ => Expr::Literal(Literal::Int(0)),
    }
}

pub(super) fn coerce_value(val: Value, col: &ColumnDef) -> Result<Value, String> {
    use TypeId::*;
    match (&val, col.type_id) {
        (Value::Empty, _) => Ok(val),
        (Value::Int(_), Int) => Ok(val),
        (Value::Float(_), Float) => Ok(val),
        (Value::Bool(_), Bool) => Ok(val),
        (Value::Str(_), Str) => Ok(val),
        (Value::Int(v), Float) => Ok(Value::Float(*v as f64)),
        (Value::Int(v), DateTime) => Ok(Value::Int(*v)),
        (Value::Str(s), DateTime) => Err(format!(
            "column '{}' is datetime — use an integer timestamp, not a string (\"{}\")",
            col.name, s
        )),
        (Value::Float(v), Int) => Ok(Value::Int(*v as i64)),
        _ => Err(format!(
            "type mismatch for column '{}': expected {:?}, got {}",
            col.name,
            col.type_id,
            match &val {
                Value::Int(_) => "int",
                Value::Float(_) => "float",
                Value::Bool(_) => "bool",
                Value::Str(_) => "str",
                Value::Empty => "null",
                _ => "other",
            }
        )),
    }
}

pub(super) fn literal_to_value(expr: &Expr) -> Result<Value, String> {
    match expr {
        Expr::Literal(Literal::Int(v)) => Ok(Value::Int(*v)),
        Expr::Literal(Literal::Float(v)) => Ok(Value::Float(*v)),
        Expr::Literal(Literal::String(v)) => Ok(Value::Str(v.clone())),
        Expr::Literal(Literal::Bool(v)) => Ok(Value::Bool(*v)),
        _ => Err("expected literal value".into()),
    }
}

/// Mission C Phase 5: direct Literal→Value conversion used by the
/// prepared-statement Insert fast path. Skips the `Expr::Literal` unwrap
/// and the `Result` plumbing of [`literal_to_value`]. String literals
/// still clone because the row needs an owned `Value::Str`.
#[inline]
pub(super) fn literal_value_from(lit: &Literal) -> Value {
    match lit {
        Literal::Int(v) => Value::Int(*v),
        Literal::Float(v) => Value::Float(*v),
        Literal::String(v) => Value::Str(v.clone()),
        Literal::Bool(v) => Value::Bool(*v),
    }
}

/// Mission C Phase 13: moving companion to [`literal_value_from`] used
/// by [`Engine::execute_prepared_take`]. Pulls the `String` out of a
/// `Literal::String` via `mem::take`, leaving an empty string behind
/// so the caller's slice remains valid (but with blanked-out strings).
/// On the insert fast path this removes one heap alloc per string
/// column per row.
#[inline]
pub(super) fn literal_value_take(lit: &mut Literal) -> Value {
    match lit {
        Literal::Int(v) => Value::Int(*v),
        Literal::Float(v) => Value::Float(*v),
        Literal::String(v) => Value::Str(std::mem::take(v)),
        Literal::Bool(v) => Value::Bool(*v),
    }
}

pub(super) fn eval_expr(expr: &Expr, row: &[Value], columns: &[String]) -> Value {
    match expr {
        Expr::Field(name) => columns
            .iter()
            .position(|c| c == name)
            .map(|i| row[i].clone())
            .unwrap_or(Value::Empty),
        Expr::QualifiedField { qualifier, field } => {
            // Mission E1.2: join queries emit columns named `alias.field`,
            // so the lookup is a direct prefix+tail match. We compare in
            // pieces to avoid allocating a fresh `format!("{q}.{f}")` on
            // every row — the join loop can evaluate this tens of thousands
            // of times per query.
            let q = qualifier.as_bytes();
            let f = field.as_bytes();
            let idx = columns.iter().position(|c| {
                let b = c.as_bytes();
                b.len() == q.len() + 1 + f.len()
                    && b[..q.len()] == *q
                    && b[q.len()] == b'.'
                    && b[q.len() + 1..] == *f
            });
            idx.map(|i| row[i].clone()).unwrap_or(Value::Empty)
        }
        Expr::Literal(lit) => match lit {
            Literal::Int(v) => Value::Int(*v),
            Literal::Float(v) => Value::Float(*v),
            Literal::String(v) => Value::Str(v.clone()),
            Literal::Bool(v) => Value::Bool(*v),
        },
        Expr::BinaryOp(left, op, right) => {
            let l = eval_expr(left, row, columns);
            let r = eval_expr(right, row, columns);
            eval_binop(&l, *op, &r)
        }
        Expr::Coalesce(left, right) => {
            let l = eval_expr(left, row, columns);
            if l.is_empty() {
                eval_expr(right, row, columns)
            } else {
                l
            }
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let val = eval_expr(expr, row, columns);
            let found = list.iter().any(|item| {
                let iv = eval_expr(item, row, columns);
                val == iv
            });
            Value::Bool(if *negated { !found } else { found })
        }
        Expr::InSubquery { .. } => {
            // Should have been materialized into InList before eval_expr.
            Value::Empty
        }
        Expr::ExistsSubquery { .. } => {
            // Should have been materialized into a Bool literal before
            // eval_expr (see materialize_subqueries).
            Value::Empty
        }
        Expr::UnaryOp(op, inner) => {
            let v = eval_expr(inner, row, columns);
            match op {
                UnaryOp::Not => match v {
                    Value::Bool(b) => Value::Bool(!b),
                    _ => Value::Empty,
                },
                UnaryOp::Exists => Value::Bool(!v.is_empty()),
                UnaryOp::NotExists => Value::Bool(v.is_empty()),
                UnaryOp::IsNull => Value::Bool(v.is_empty()),
                UnaryOp::IsNotNull => Value::Bool(!v.is_empty()),
            }
        }
        Expr::ScalarFunc(func, args) => {
            let vals: Vec<Value> = args.iter().map(|a| eval_expr(a, row, columns)).collect();
            eval_scalar_func(*func, &vals)
        }
        Expr::Case { whens, else_expr } => {
            for (condition, result) in whens {
                if eval_predicate(condition, row, columns) {
                    return eval_expr(result, row, columns);
                }
            }
            match else_expr {
                Some(e) => eval_expr(e, row, columns),
                None => Value::Empty,
            }
        }
        Expr::Cast(inner, cast_type) => {
            let val = eval_expr(inner, row, columns);
            eval_cast(val, *cast_type)
        }
        Expr::FunctionCall(_, _) | Expr::Param(_) | Expr::Window { .. } => Value::Empty,
    }
}

pub(super) fn eval_predicate(expr: &Expr, row: &[Value], columns: &[String]) -> bool {
    match eval_expr(expr, row, columns) {
        Value::Bool(b) => b,
        _ => false,
    }
}

fn eval_scalar_func(func: ScalarFn, args: &[Value]) -> Value {
    match func {
        ScalarFn::Upper => match args.first() {
            Some(Value::Str(s)) => Value::Str(s.to_uppercase()),
            _ => Value::Empty,
        },
        ScalarFn::Lower => match args.first() {
            Some(Value::Str(s)) => Value::Str(s.to_lowercase()),
            _ => Value::Empty,
        },
        ScalarFn::Length => match args.first() {
            Some(Value::Str(s)) => Value::Int(s.len() as i64),
            _ => Value::Empty,
        },
        ScalarFn::Trim => match args.first() {
            Some(Value::Str(s)) => Value::Str(s.trim().to_string()),
            _ => Value::Empty,
        },
        ScalarFn::Substring => {
            if args.len() < 3 {
                return Value::Empty;
            }
            match (&args[0], &args[1], &args[2]) {
                (Value::Str(s), Value::Int(start), Value::Int(len)) => {
                    let start = (*start as usize).saturating_sub(1); // 1-indexed
                    let len = *len as usize;
                    let sub: String = s.chars().skip(start).take(len).collect();
                    Value::Str(sub)
                }
                _ => Value::Empty,
            }
        }
        ScalarFn::Concat => {
            let mut result = String::new();
            for v in args {
                match v {
                    Value::Str(s) => result.push_str(s),
                    Value::Int(n) => result.push_str(&n.to_string()),
                    Value::Float(f) => result.push_str(&f.to_string()),
                    Value::Bool(b) => result.push_str(if *b { "true" } else { "false" }),
                    _ => {}
                }
            }
            Value::Str(result)
        }
        // Math functions
        ScalarFn::Abs => match args.first() {
            Some(Value::Int(n)) => Value::Int(n.abs()),
            Some(Value::Float(f)) => Value::Float(f.abs()),
            _ => Value::Empty,
        },
        ScalarFn::Round => {
            let decimals = match args.get(1) {
                Some(Value::Int(d)) => *d as i32,
                _ => 0,
            };
            match args.first() {
                Some(Value::Float(f)) => {
                    let factor = 10_f64.powi(decimals);
                    Value::Float((f * factor).round() / factor)
                }
                Some(Value::Int(n)) => Value::Int(*n),
                _ => Value::Empty,
            }
        }
        ScalarFn::Ceil => match args.first() {
            Some(Value::Float(f)) => Value::Float(f.ceil()),
            Some(Value::Int(n)) => Value::Int(*n),
            _ => Value::Empty,
        },
        ScalarFn::Floor => match args.first() {
            Some(Value::Float(f)) => Value::Float(f.floor()),
            Some(Value::Int(n)) => Value::Int(*n),
            _ => Value::Empty,
        },
        ScalarFn::Sqrt => match args.first() {
            Some(Value::Float(f)) if *f >= 0.0 => Value::Float(f.sqrt()),
            Some(Value::Int(n)) if *n >= 0 => Value::Float((*n as f64).sqrt()),
            _ => Value::Empty,
        },
        ScalarFn::Pow => match (args.first(), args.get(1)) {
            (Some(Value::Float(base)), Some(Value::Float(exp))) => Value::Float(base.powf(*exp)),
            (Some(Value::Float(base)), Some(Value::Int(exp))) => {
                Value::Float(base.powi(*exp as i32))
            }
            (Some(Value::Int(base)), Some(Value::Int(exp))) => {
                if *exp >= 0 && *exp <= u32::MAX as i64 {
                    match base.checked_pow(*exp as u32) {
                        Some(v) => Value::Int(v),
                        None => Value::Float((*base as f64).powi(*exp as i32)),
                    }
                } else {
                    Value::Float((*base as f64).powi(*exp as i32))
                }
            }
            (Some(Value::Int(base)), Some(Value::Float(exp))) => {
                Value::Float((*base as f64).powf(*exp))
            }
            _ => Value::Empty,
        },
        // Date/time functions
        ScalarFn::Now => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let micros = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as i64;
            Value::DateTime(micros)
        }
        ScalarFn::Extract => {
            // extract("part", datetime_expr)
            let part = match args.first() {
                Some(Value::Str(s)) => s.as_str(),
                _ => return Value::Empty,
            };
            let micros = match args.get(1) {
                Some(Value::DateTime(m)) => *m,
                Some(Value::Int(m)) => *m, // treat raw int as micros
                _ => return Value::Empty,
            };
            datetime_extract(part, micros)
        }
        ScalarFn::DateAdd => {
            // date_add(datetime_expr, amount, "unit")
            let micros = match args.first() {
                Some(Value::DateTime(m)) => *m,
                Some(Value::Int(m)) => *m,
                _ => return Value::Empty,
            };
            let amount = match args.get(1) {
                Some(Value::Int(n)) => *n,
                _ => return Value::Empty,
            };
            let unit = match args.get(2) {
                Some(Value::Str(s)) => s.as_str(),
                _ => return Value::Empty,
            };
            let delta_micros = match unit {
                "microsecond" | "microseconds" | "us" => amount,
                "millisecond" | "milliseconds" | "ms" => amount * 1_000,
                "second" | "seconds" | "s" => amount * 1_000_000,
                "minute" | "minutes" | "m" => amount * 60_000_000,
                "hour" | "hours" | "h" => amount * 3_600_000_000,
                "day" | "days" | "d" => amount * 86_400_000_000,
                _ => return Value::Empty,
            };
            Value::DateTime(micros + delta_micros)
        }
        ScalarFn::DateDiff => {
            // date_diff(dt1, dt2, "unit")
            let m1 = match args.first() {
                Some(Value::DateTime(m)) => *m,
                Some(Value::Int(m)) => *m,
                _ => return Value::Empty,
            };
            let m2 = match args.get(1) {
                Some(Value::DateTime(m)) => *m,
                Some(Value::Int(m)) => *m,
                _ => return Value::Empty,
            };
            let unit = match args.get(2) {
                Some(Value::Str(s)) => s.as_str(),
                _ => return Value::Empty,
            };
            let diff = m1 - m2;
            let result = match unit {
                "microsecond" | "microseconds" | "us" => diff,
                "millisecond" | "milliseconds" | "ms" => diff / 1_000,
                "second" | "seconds" | "s" => diff / 1_000_000,
                "minute" | "minutes" | "m" => diff / 60_000_000,
                "hour" | "hours" | "h" => diff / 3_600_000_000,
                "day" | "days" | "d" => diff / 86_400_000_000,
                _ => return Value::Empty,
            };
            Value::Int(result)
        }
    }
}

/// Extract a component from a DateTime value (microseconds since epoch).
fn datetime_extract(part: &str, micros: i64) -> Value {
    // Convert micros to seconds + remainder for calendar calculations
    let total_secs = micros / 1_000_000;
    let micro_rem = micros % 1_000_000;

    // Simple civil calendar from Unix timestamp (no TZ — UTC assumed)
    let days_since_epoch = if total_secs >= 0 {
        total_secs / 86400
    } else {
        (total_secs - 86399) / 86400
    };
    let secs_of_day = total_secs - days_since_epoch * 86400;

    match part {
        "hour" => Value::Int(secs_of_day / 3600),
        "minute" => Value::Int((secs_of_day % 3600) / 60),
        "second" => Value::Int(secs_of_day % 60),
        "millisecond" => Value::Int(micro_rem / 1000),
        "microsecond" => Value::Int(micro_rem),
        "epoch" => Value::Int(total_secs),
        "year" | "month" | "day" => {
            // Civil date from days since 1970-01-01 (algorithm from Howard Hinnant)
            let z = days_since_epoch + 719468;
            let era = if z >= 0 { z } else { z - 146096 } / 146097;
            let doe = (z - era * 146097) as u32;
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let y = (yoe as i64) + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = if m <= 2 { y + 1 } else { y };
            match part {
                "year" => Value::Int(y),
                "month" => Value::Int(m as i64),
                "day" => Value::Int(d as i64),
                _ => unreachable!(),
            }
        }
        _ => Value::Empty,
    }
}

/// Evaluate a CAST expression.
fn eval_cast(val: Value, target: CastType) -> Value {
    match target {
        CastType::Int => match val {
            Value::Int(n) => Value::Int(n),
            Value::Float(f) => Value::Int(f as i64),
            Value::Bool(b) => Value::Int(if b { 1 } else { 0 }),
            Value::Str(s) => s.parse::<i64>().map(Value::Int).unwrap_or(Value::Empty),
            Value::DateTime(m) => Value::Int(m),
            _ => Value::Empty,
        },
        CastType::Float => match val {
            Value::Float(f) => Value::Float(f),
            Value::Int(n) => Value::Float(n as f64),
            Value::Str(s) => s.parse::<f64>().map(Value::Float).unwrap_or(Value::Empty),
            Value::Bool(b) => Value::Float(if b { 1.0 } else { 0.0 }),
            _ => Value::Empty,
        },
        CastType::Str => match val {
            Value::Str(s) => Value::Str(s),
            Value::Int(n) => Value::Str(n.to_string()),
            Value::Float(f) => Value::Str(f.to_string()),
            Value::Bool(b) => Value::Str(b.to_string()),
            Value::DateTime(m) => Value::Str(m.to_string()),
            _ => Value::Empty,
        },
        CastType::Bool => match val {
            Value::Bool(b) => Value::Bool(b),
            Value::Int(n) => Value::Bool(n != 0),
            Value::Str(s) => match s.as_str() {
                "true" | "1" | "yes" => Value::Bool(true),
                "false" | "0" | "no" => Value::Bool(false),
                _ => Value::Empty,
            },
            _ => Value::Empty,
        },
        CastType::DateTime => match val {
            Value::DateTime(m) => Value::DateTime(m),
            Value::Int(m) => Value::DateTime(m),
            _ => Value::Empty,
        },
    }
}

pub(super) fn eval_binop(left: &Value, op: BinOp, right: &Value) -> Value {
    match op {
        BinOp::Eq => Value::Bool(left == right),
        BinOp::Neq => Value::Bool(left != right),
        BinOp::Lt => Value::Bool(left < right),
        BinOp::Gt => Value::Bool(left > right),
        BinOp::Lte => Value::Bool(left <= right),
        BinOp::Gte => Value::Bool(left >= right),
        BinOp::And => match (left, right) {
            (Value::Bool(a), Value::Bool(b)) => Value::Bool(*a && *b),
            _ => Value::Bool(false),
        },
        BinOp::Or => match (left, right) {
            (Value::Bool(a), Value::Bool(b)) => Value::Bool(*a || *b),
            _ => Value::Bool(false),
        },
        BinOp::Add => match (left, right) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a.saturating_add(*b)),
            (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
            (Value::Int(a), Value::Float(b)) => Value::Float(*a as f64 + b),
            (Value::Float(a), Value::Int(b)) => Value::Float(a + *b as f64),
            _ => Value::Empty,
        },
        BinOp::Sub => match (left, right) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a.saturating_sub(*b)),
            (Value::Float(a), Value::Float(b)) => Value::Float(a - b),
            (Value::Int(a), Value::Float(b)) => Value::Float(*a as f64 - b),
            (Value::Float(a), Value::Int(b)) => Value::Float(a - *b as f64),
            _ => Value::Empty,
        },
        BinOp::Mul => match (left, right) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a.saturating_mul(*b)),
            (Value::Float(a), Value::Float(b)) => Value::Float(a * b),
            (Value::Int(a), Value::Float(b)) => Value::Float(*a as f64 * b),
            (Value::Float(a), Value::Int(b)) => Value::Float(a * *b as f64),
            _ => Value::Empty,
        },
        BinOp::Div => match (left, right) {
            (Value::Int(a), Value::Int(b)) if *b != 0 => Value::Int(a / b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a / b),
            (Value::Int(a), Value::Float(b)) => Value::Float(*a as f64 / b),
            (Value::Float(a), Value::Int(b)) => Value::Float(a / *b as f64),
            _ => Value::Empty,
        },
        BinOp::Like => match (left, right) {
            (Value::Str(text), Value::Str(pattern)) => Value::Bool(like_match(text, pattern)),
            _ => Value::Bool(false),
        },
    }
}

/// SQL LIKE pattern match. `%` matches any sequence (including empty),
/// `_` matches exactly one character. No escape character for now.
pub(super) fn like_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    like_dp(&t, &p, 0, 0)
}

fn like_dp(t: &[char], p: &[char], ti: usize, pi: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    if p[pi] == '%' {
        // '%' can match zero or more characters — try both.
        // Skip consecutive '%' to avoid exponential blowup.
        let mut pi2 = pi;
        while pi2 < p.len() && p[pi2] == '%' {
            pi2 += 1;
        }
        for i in ti..=t.len() {
            if like_dp(t, p, i, pi2) {
                return true;
            }
        }
        false
    } else if ti < t.len() && (p[pi] == '_' || p[pi] == t[ti]) {
        like_dp(t, p, ti + 1, pi + 1)
    } else {
        false
    }
}
