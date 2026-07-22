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
        Expr::FunctionCall(_, inner, _) => collect_field_refs(inner, out),
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
        // A JSON path references only the column named by its base; the
        // segments address into that value and name no additional columns.
        Expr::JsonPath { base, .. } => collect_field_refs(base, out),
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
        Expr::FunctionCall(_, inner, _) => contains_subquery(inner),
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
        Value::Empty => Expr::Null,
        // DateTime / Uuid / Bytes have no Literal form; carry the value verbatim
        // so subquery comparisons see the real value, not a bogus Int(0).
        other => Expr::ValueLit(other),
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
        // An already-encoded PJ1 document passes through untouched (e.g. a
        // `returning`/subquery value flowing back into a json column).
        (Value::Json(_), Json) => Ok(val),
        // A string literal into a json column is parsed and canonicalized to
        // PJ1 at write time (the storage layer does not auto-coerce text). The
        // error is prefixed `invalid JSON` so it survives the server's
        // safe-error allowlist ("invalid" is a known-safe prefix).
        (Value::Str(s), Json) => powdb_storage::pj1::parse_json_text(s)
            .map(|bytes| Value::Json(bytes.into_boxed_slice()))
            .map_err(|e| format!("invalid JSON for column '{}': {}", col.name, e)),
        (Value::DateTime(_), DateTime) => Ok(val),
        (Value::Uuid(_), Uuid) => Ok(val),
        (Value::Bytes(_), Bytes) => Ok(val),
        (Value::Int(v), Float) => Ok(Value::Float(*v as f64)),
        (Value::Int(v), DateTime) => Ok(Value::Int(*v)),
        (Value::Str(s), DateTime) => Err(format!(
            "column '{}' is datetime — use an integer timestamp, not a string (\"{}\")",
            col.name, s
        )),
        // A plain string literal into a uuid/bytes column is coerced (and
        // validated) per row at execution time — so a bulk-load insert keeps
        // one cached plan and each row's value is checked independently.
        (Value::Str(s), Uuid) => parse_uuid_str(s).map(Value::Uuid).ok_or_else(|| {
            format!(
                "column '{}' is uuid — expected canonical 8-4-4-4-12 hex, got \"{}\"",
                col.name, s
            )
        }),
        (Value::Str(s), Bytes) => parse_hex_bytes(s).map(Value::Bytes).ok_or_else(|| {
            format!(
                "column '{}' is bytes — expected Postgres bytea hex (\\x-prefixed, even length), got \"{}\"",
                col.name, s
            )
        }),
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
        Expr::Null => Ok(Value::Empty),
        // Const-fold cast sugar in value position: `uuid("…")`, `bytes("…")`,
        // `cast(1718000000, "datetime")`. A failed cast errors (the whole
        // statement aborts before any write) rather than silently inserting
        // null.
        Expr::Cast(inner, cast_type) => {
            let v = literal_to_value(inner)?;
            match eval_cast(v, *cast_type) {
                Value::Empty => Err(format!("cast to {cast_type:?} produced no value")),
                out => Ok(out),
            }
        }
        _ => Err("expected literal value".into()),
    }
}

/// Parse a canonical hyphenated `8-4-4-4-12` UUID string (case-insensitive)
/// into its 16 raw bytes. Returns `None` on any deviation from the canonical
/// form (wrong length, misplaced hyphens, non-hex digit).
fn parse_uuid_str(s: &str) -> Option<[u8; 16]> {
    let b = s.as_bytes();
    if b.len() != 36 || b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
        return None;
    }
    let mut out = [0u8; 16];
    let mut oi = 0usize;
    let mut hi: Option<u8> = None;
    for (pos, &c) in b.iter().enumerate() {
        if matches!(pos, 8 | 13 | 18 | 23) {
            continue;
        }
        let nib = hex_nibble(c)?;
        match hi {
            None => hi = Some(nib),
            Some(h) => {
                out[oi] = (h << 4) | nib;
                oi += 1;
                hi = None;
            }
        }
    }
    Some(out)
}

/// Parse Postgres bytea text encoding — a `\x` prefix followed by an
/// even-length run of hex digits — into raw bytes. Returns `None` if the
/// prefix is missing, the length is odd, or a non-hex digit appears. PowQL
/// source spells the prefix `"\\x…"` (the lexer collapses `\\` to one `\`).
fn parse_hex_bytes(s: &str) -> Option<Vec<u8>> {
    let rest = s.strip_prefix("\\x")?;
    let b = rest.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0usize;
    while i < b.len() {
        out.push((hex_nibble(b[i])? << 4) | hex_nibble(b[i + 1])?);
        i += 2;
    }
    Some(out)
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
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

/// Comparison-semantics mode for the six comparison operators
/// (`= != < > <= >=`).
///
/// - `Filter`: a missing (`Empty`) operand never matches — the row is excluded.
///   This is SQL NULL semantics and matches the compiled predicate leaves.
///   Used for every filter, HAVING, CASE, and nested-projection residual.
/// - `Join`: comparisons keep the raw `Value` order / equality, so a JOIN key
///   equality still matches `Empty = Empty` (PowDB deliberately joins on
///   missing keys; see `plan_exec/join.rs`). Used only for JOIN `ON` / residual
///   conditions so the hash and nested-loop join paths stay identical and
///   unchanged.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CmpMode {
    Filter,
    Join,
}

pub(super) fn eval_expr(expr: &Expr, row: &[Value], columns: &[String]) -> Value {
    eval_expr_mode(expr, row, columns, CmpMode::Filter)
}

pub(super) fn eval_expr_mode(
    expr: &Expr,
    row: &[Value],
    columns: &[String],
    mode: CmpMode,
) -> Value {
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
        // Nested sub-queries only appear inside a projection; the planner
        // routes them to `NestedProject`, so they never reach evaluation.
        Expr::NestedQuery(_) => Value::Empty,
        // Scalar link paths likewise route to `NestedProjectField::Link` and
        // never reach evaluation.
        Expr::LinkPath { .. } => Value::Empty,
        Expr::BinaryOp(left, op, right) => {
            let l = eval_expr_mode(left, row, columns, mode);
            let r = eval_expr_mode(right, row, columns, mode);
            eval_binop_mode(&l, *op, &r, mode)
        }
        Expr::Coalesce(left, right) => {
            let l = eval_expr_mode(left, row, columns, mode);
            if l.is_empty() {
                eval_expr_mode(right, row, columns, mode)
            } else {
                l
            }
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let val = eval_expr_mode(expr, row, columns, mode);
            let found = list.iter().any(|item| {
                let iv = eval_expr_mode(item, row, columns, mode);
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
            let v = eval_expr_mode(inner, row, columns, mode);
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
            // `json_type` needs the raw PJ1 node, not a scalarized Value, so it
            // can tell a JSON `null` (node present, tag 0) from a missing path
            // (no node). Intercept it before the generic arg scalarization.
            if *func == ScalarFn::JsonType {
                return eval_json_type(args.first(), row, columns);
            }
            if *func == ScalarFn::JsonText {
                return eval_json_text(args.first(), row, columns);
            }
            let vals: Vec<Value> = args
                .iter()
                .map(|a| eval_expr_mode(a, row, columns, mode))
                .collect();
            eval_scalar_func(*func, &vals)
        }
        Expr::JsonPath { base, segments } => {
            let base_val = eval_expr_mode(base, row, columns, mode);
            match walk_json_path(&base_val, segments) {
                // Scalarize the addressed node per design 4.4. JSON `null` and
                // a missing path both collapse to `Empty` here (use `json_type`
                // to distinguish them).
                Some(node) => pj1_scalarize(node),
                None => Value::Empty,
            }
        }
        Expr::Case { whens, else_expr } => {
            for (condition, result) in whens {
                if eval_predicate_mode(condition, row, columns, mode) {
                    return eval_expr_mode(result, row, columns, mode);
                }
            }
            match else_expr {
                Some(e) => eval_expr_mode(e, row, columns, mode),
                None => Value::Empty,
            }
        }
        Expr::Cast(inner, cast_type) => {
            let val = eval_expr_mode(inner, row, columns, mode);
            eval_cast(val, *cast_type)
        }
        Expr::ValueLit(v) => v.clone(),
        Expr::FunctionCall(..) | Expr::Param(_) | Expr::Window { .. } | Expr::Null => Value::Empty,
    }
}

/// Walk `segments` over a JSON `base` value, returning the addressed PJ1 node
/// bytes (a borrow into `base`), or `None` if `base` is not a JSON document or
/// any segment misses. The returned slice is itself a self-contained PJ1
/// document (see [`powdb_storage::pj1`]), so it can be scalarized or walked
/// further with no re-basing.
fn walk_json_path<'a>(base: &'a Value, segments: &[PathSeg]) -> Option<&'a [u8]> {
    let Value::Json(bytes) = base else {
        // Missing column (`Empty`) or a base that is not a JSON document.
        return None;
    };
    let mut cur: &[u8] = bytes;
    for seg in segments {
        let pj = match seg {
            PathSeg::Key(k) => powdb_storage::pj1::PathSeg::Key(k.as_str()),
            PathSeg::Index(i) => powdb_storage::pj1::PathSeg::Index(*i),
        };
        cur = powdb_storage::pj1::pj1_get(cur, &pj)?;
    }
    Some(cur)
}

/// Scalarize a PJ1 node (a self-contained document slice) into a `Value` per
/// design 4.4:
///   - JSON string -> `Str`, integral number -> `Int`, other number -> `Float`,
///     bool -> `Bool`, object/array -> `Json` (owned sub-document copy),
///     JSON `null` -> `Empty`.
///
/// Reads only the node's documented leading tag byte and, for fixed scalars,
/// its fixed little-endian payload. Stored PJ1 is always canonical and
/// validated, so the defensive `Empty` fallbacks (truncated/reserved) are
/// unreachable in practice and, crucially, never panic.
fn pj1_scalarize(node: &[u8]) -> Value {
    match node.first() {
        // null (tag 0) and a missing path are indistinguishable after
        // scalarization — both are the empty set.
        Some(0) => Value::Empty,
        Some(1) => Value::Bool(false),
        Some(2) => Value::Bool(true),
        // int: [tag][i64 LE]
        Some(3) if node.len() >= 9 => {
            Value::Int(i64::from_le_bytes(node[1..9].try_into().unwrap()))
        }
        // float: [tag][f64 LE]
        Some(4) if node.len() >= 9 => {
            Value::Float(f64::from_le_bytes(node[1..9].try_into().unwrap()))
        }
        // string: [tag][len u32 LE][UTF-8]
        Some(5) if node.len() >= 5 => {
            let len = u32::from_le_bytes(node[1..5].try_into().unwrap()) as usize;
            match node
                .get(5..5 + len)
                .and_then(|b| std::str::from_utf8(b).ok())
            {
                Some(s) => Value::Str(s.to_string()),
                None => Value::Empty,
            }
        }
        // array (6) / object (7): return the owned sub-document.
        Some(6) | Some(7) => Value::Json(node.to_vec().into_boxed_slice()),
        // Truncated scalar, reserved tag, or empty slice: never panic.
        _ => Value::Empty,
    }
}

/// The `json_type(expr)` scalar: `'null'|'string'|'number'|'bool'|'object'|
/// 'array'` for the addressed JSON node, or `Empty` when the path is missing.
/// Operates on the raw PJ1 node (not a scalarized Value) so it can distinguish
/// a JSON `null` (returns `'null'`) from a missing path (returns `Empty`).
fn eval_json_type(arg: Option<&Expr>, row: &[Value], columns: &[String]) -> Value {
    let Some(arg) = arg else {
        return Value::Empty;
    };
    // Resolve the raw node: for a `->` path, walk without scalarizing so a
    // present JSON null survives; for any other expression, a `Value::Json`
    // IS a node and anything else is treated as missing.
    let node: Option<Vec<u8>> = match arg {
        Expr::JsonPath { base, segments } => {
            let base_val = eval_expr(base, row, columns);
            walk_json_path(&base_val, segments).map(|n| n.to_vec())
        }
        other => match eval_expr(other, row, columns) {
            Value::Json(b) => Some(b.into_vec()),
            _ => None,
        },
    };
    match node.as_deref().and_then(<[u8]>::first) {
        Some(0) => Value::Str("null".into()),
        Some(1) | Some(2) => Value::Str("bool".into()),
        Some(3) | Some(4) => Value::Str("number".into()),
        Some(5) => Value::Str("string".into()),
        Some(6) => Value::Str("array".into()),
        Some(7) => Value::Str("object".into()),
        // Missing path (no node) or a defensive truncated/reserved node.
        _ => Value::Empty,
    }
}

/// SQL `->>` semantics over the raw addressed PJ1 node. Strings are returned
/// without JSON quotes; numbers and booleans use canonical JSON spelling;
/// objects and arrays return canonical JSON text. Missing paths and JSON null
/// both map to `Empty`.
fn eval_json_text(arg: Option<&Expr>, row: &[Value], columns: &[String]) -> Value {
    let Some(arg) = arg else {
        return Value::Empty;
    };
    let node: Option<Vec<u8>> = match arg {
        Expr::JsonPath { base, segments } => {
            let base_val = eval_expr(base, row, columns);
            walk_json_path(&base_val, segments).map(|node| node.to_vec())
        }
        other => match eval_expr(other, row, columns) {
            Value::Json(bytes) => Some(bytes.into_vec()),
            _ => None,
        },
    };
    let Some(node) = node else {
        return Value::Empty;
    };
    match node.first().copied() {
        Some(0) => Value::Empty,
        Some(5) => pj1_scalarize(&node),
        Some(1..=4) | Some(6) | Some(7) => powdb_storage::pj1::pj1_to_text(&node)
            .map(Value::Str)
            .unwrap_or(Value::Empty),
        _ => Value::Empty,
    }
}

pub(super) fn eval_predicate(expr: &Expr, row: &[Value], columns: &[String]) -> bool {
    eval_predicate_mode(expr, row, columns, CmpMode::Filter)
}

/// Evaluate a JOIN `ON` / residual condition. Join comparisons keep raw `Value`
/// semantics, so a join key equality still matches `Empty = Empty` (PowDB joins
/// on missing keys by design) rather than applying the filter Empty guard. See
/// `plan_exec/join.rs`.
pub(super) fn eval_join_predicate(expr: &Expr, row: &[Value], columns: &[String]) -> bool {
    eval_predicate_mode(expr, row, columns, CmpMode::Join)
}

pub(super) fn eval_predicate_mode(
    expr: &Expr,
    row: &[Value],
    columns: &[String],
    mode: CmpMode,
) -> bool {
    match eval_expr_mode(expr, row, columns, mode) {
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
        // `json_type` is intercepted in `eval_expr` (it needs the raw PJ1 node,
        // not a scalarized Value); it never reaches this Value-based dispatch.
        ScalarFn::JsonType | ScalarFn::JsonText => Value::Empty,
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
        CastType::Uuid => match val {
            Value::Uuid(u) => Value::Uuid(u),
            Value::Str(s) => parse_uuid_str(&s).map(Value::Uuid).unwrap_or(Value::Empty),
            _ => Value::Empty,
        },
        CastType::Bytes => match val {
            Value::Bytes(b) => Value::Bytes(b),
            Value::Str(s) => parse_hex_bytes(&s)
                .map(Value::Bytes)
                .unwrap_or(Value::Empty),
            _ => Value::Empty,
        },
    }
}

pub(super) fn eval_binop_mode(left: &Value, op: BinOp, right: &Value, mode: CmpMode) -> Value {
    // In `Filter` mode a missing (`Empty`) operand never matches an ordered or
    // equality / inequality comparison: the comparison is false, so the row is
    // excluded. This mirrors the compiled predicate leaves (`compiled.rs`:
    // Int/Float/StrEq/Json null-guard and never match Empty) and SQL NULL
    // semantics, keeping the generic and compiled paths in agreement regardless
    // of whether a predicate compiles. Presence is tested with `exists` /
    // `not exists`, not with a comparison.
    //
    // In `Join` mode the guard is skipped so JOIN key equality stays direct
    // `Value` equality (including `Empty = Empty`) — PowDB deliberately joins on
    // missing keys, and the hash and nested-loop join paths must agree.
    if mode == CmpMode::Filter
        && matches!(
            op,
            BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Lte | BinOp::Gte
        )
        && (left.is_empty() || right.is_empty())
    {
        return Value::Bool(false);
    }
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
            // `checked_div` guards BOTH divide-by-zero AND the `i64::MIN / -1`
            // overflow case, which panics even in release builds (and with
            // `panic = "abort"` that is a remotely-craftable process crash).
            // Returning `Empty` on either matches the sibling arithmetic arms.
            (Value::Int(a), Value::Int(b)) => a.checked_div(*b).map_or(Value::Empty, Value::Int),
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
/// `_` matches exactly one character. No escape character. Iterative
/// two-pointer with backtracking — O(n·m) time, O(1) stack (a recursive
/// matcher stack-overflows / backtracks exponentially on adversarial input).
pub(super) fn like_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let mut star: Option<usize> = None; // index in p of the last '%'
    let mut star_ti = 0usize; // text index when that '%' was taken
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::like_match;

    #[test]
    fn test_like_match_correctness() {
        assert!(like_match("abc", "a%c"));
        assert!(like_match("abc", "a_c"));
        assert!(like_match("abc", "a%"));
        assert!(like_match("abc", "%b%"));
        assert!(like_match("abc", "abc"));
        assert!(!like_match("abc", "a%d"));
        assert!(like_match("", "%"));
        assert!(like_match("ax", "a_"));
        assert!(!like_match("a", "a_"));
    }

    #[test]
    fn test_like_match_adversarial_no_overflow() {
        // A recursive / exponential matcher would overflow the stack or hang
        // here. The iterative two-pointer matcher returns quickly.
        let text = "a".repeat(50_000);
        assert!(!like_match(&text, "%a%a%a%b"));
    }
}
