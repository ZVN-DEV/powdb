//! Compiled predicates and fast-path layout utilities.

use crate::ast::*;
use powdb_storage::row::{decode_column, RowLayout};
use powdb_storage::types::*;

/// Mission C Phase 4: precomputed byte-patch for the in-place update fast
/// path. Built once per `Update` query (outside the rid loop) and reused on
/// every matching row.
#[derive(Clone, Copy)]
pub(super) struct FastPatch {
    /// Byte offset of the fixed column within the row encoding:
    /// `2 + bitmap_size + layout.fixed_offsets[col]`.
    pub(super) field_off: usize,
    /// Byte offset of the bitmap byte containing this column's null bit
    /// (`2 + col/8`). We read-modify-write this byte to force the column
    /// non-null, so the idempotent clear is safe for already-non-null rows.
    pub(super) bitmap_byte_off: usize,
    /// Bit mask for this column's null bit within `bitmap_byte_off`.
    pub(super) bit_mask: u8,
    /// The new fixed-width value encoded as little-endian bytes.
    pub(super) bytes: FixedBytes,
}

#[derive(Clone, Copy)]
pub(super) enum FixedBytes {
    I64([u8; 8]),
    F64([u8; 8]),
    Bool(u8),
    Uuid([u8; 16]),
}

impl FixedBytes {
    #[inline]
    pub(super) fn as_slice(&self) -> &[u8] {
        match self {
            FixedBytes::I64(b) => b.as_slice(),
            FixedBytes::F64(b) => b.as_slice(),
            FixedBytes::Bool(b) => std::slice::from_ref(b),
            FixedBytes::Uuid(b) => b.as_slice(),
        }
    }
}

pub(super) fn type_name_to_id(name: &str) -> TypeId {
    match name {
        "str" => TypeId::Str,
        "int" => TypeId::Int,
        "float" => TypeId::Float,
        "bool" => TypeId::Bool,
        "datetime" => TypeId::DateTime,
        "uuid" => TypeId::Uuid,
        "bytes" => TypeId::Bytes,
        _ => TypeId::Str,
    }
}

/// The row format is:
///   [length: u16][null_bitmap][fixed cols packed][var offset table: (n_var+1) u16s][var data]
pub(super) struct FastLayout {
    /// Null bitmap size in bytes.
    pub(super) bitmap_size: usize,
    /// Byte offset within the fixed region for each column (None = var-length).
    pub(super) fixed_offsets: Vec<Option<usize>>,
    /// Size of the fixed region in bytes.
    pub(super) fixed_region_size: usize,
    /// For each column: its slot index in the var-offset table (None = fixed).
    pub(super) var_indices: Vec<Option<usize>>,
    /// Total number of variable-length columns.
    pub(super) n_var: usize,
}

impl FastLayout {
    pub(super) fn new(schema: &Schema) -> Self {
        let n_cols = schema.columns.len();
        let bitmap_size = n_cols.div_ceil(8);
        let mut fixed_offsets = vec![None; n_cols];
        let mut var_indices = vec![None; n_cols];
        let mut fixed_pos: usize = 0;
        let mut var_count: usize = 0;

        for (i, col) in schema.columns.iter().enumerate() {
            if is_fixed_size(col.type_id) {
                fixed_offsets[i] = Some(fixed_pos);
                fixed_pos += fixed_size(col.type_id).unwrap();
            } else {
                var_indices[i] = Some(var_count);
                var_count += 1;
            }
        }

        FastLayout {
            bitmap_size,
            fixed_offsets,
            fixed_region_size: fixed_pos,
            var_indices,
            n_var: var_count,
        }
    }

    /// Where the var-offset table starts within `data`.
    #[inline]
    fn var_offset_table_start(&self) -> usize {
        2 + self.bitmap_size + self.fixed_region_size
    }

    /// Where the var-data region starts within `data`.
    #[inline]
    fn var_data_start(&self) -> usize {
        self.var_offset_table_start() + (self.n_var + 1) * 2
    }
}

pub(super) type CompiledPredicate = Box<dyn Fn(&[u8]) -> bool>;

/// Map an f64 bit pattern to a u64 that orders under unsigned integer
/// comparison the same way `f64::total_cmp` orders the floats. Classic
/// sortable-float transform:
///   - Positive floats (sign bit 0): flip the sign bit. This maps
///     [+0, +∞, +NaN] to [0x8000…, 0xFFF0…, 0xFFF8…] — increasing as u64.
///   - Negative floats (sign bit 1): flip every bit. This maps
///     [-∞, -0] to [0x000F…, 0x7FFF…] — increasing as u64, and placed
///     *below* the positive range so negatives < positives.
///
/// Used by Mission D10 Float fast paths so we can key heaps on `u64`
/// (branch-cheap, folds into LLVM xor/sar/xor) instead of a `TotalF64`
/// newtype with `Ord::cmp` calling `total_cmp`.
#[inline]
pub(super) fn f64_bits_to_sortable_u64(bits: u64) -> u64 {
    // `((bits >> 63) as i64 * -1) as u64 | 0x8000_0000_0000_0000`
    // would also work; the branchless form below is equally good on
    // modern CPUs and easier to read.
    if bits & 0x8000_0000_0000_0000 == 0 {
        bits ^ 0x8000_0000_0000_0000
    } else {
        !bits
    }
}

/// A single flattened predicate leaf — pure data, no closures, no allocation
/// per call. Mission D3: replaces recursive Box<dyn Fn> conjunctions with a
/// `Vec<CompiledLeaf>` so the inner scan loop becomes a tight match instead
/// of N+1 vtable indirect calls per row.
enum CompiledLeaf {
    /// `.field <op> literal_int` (or reversed)
    Int {
        data_offset: usize,
        bitmap_byte: usize,
        bitmap_bit: u8,
        op: BinOp,
        literal: i64,
    },
    /// `.field <op> literal_float` (or reversed), where `.field` is a
    /// Float column. Int literals that bound a Float column (e.g.
    /// `.price > 100` on `price: float`) are also routed here, promoted
    /// to `f64` at compile time so the hot loop only sees one shape.
    /// Comparisons use `f64::total_cmp` so NaN handling is deterministic
    /// and consistent with `Value::Ord` across every read path.
    Float {
        data_offset: usize,
        bitmap_byte: usize,
        bitmap_bit: u8,
        op: BinOp,
        literal: f64,
    },
    /// `.field is null` or `.field is not null`
    IsNull {
        bitmap_byte: usize,
        bitmap_bit: u8,
        want_null: bool,
    },
    /// `.field = string_literal` or `.field != string_literal`
    StrEq {
        var_offset_table_start: usize,
        var_data_start: usize,
        var_idx: usize,
        bitmap_byte: usize,
        bitmap_bit: u8,
        negate: bool,
        needle: Vec<u8>,
    },
}

impl CompiledLeaf {
    /// Evaluate this leaf against a row's raw bytes. `#[inline]` so the
    /// match folds into the caller's tight loop with LTO.
    #[inline]
    fn eval(&self, data: &[u8]) -> bool {
        match self {
            CompiledLeaf::Int {
                data_offset,
                bitmap_byte,
                bitmap_bit,
                op,
                literal,
            } => {
                if data.len() < *data_offset + 8 || data.len() < 3 + bitmap_byte {
                    return false;
                }
                let is_null = (data[2 + bitmap_byte] >> bitmap_bit) & 1 == 1;
                if is_null {
                    return false;
                }
                let val = i64::from_le_bytes(
                    // SAFETY: bounds checked above
                    data[*data_offset..*data_offset + 8]
                        .try_into()
                        .unwrap_or_else(|_| unreachable!()),
                );
                match op {
                    BinOp::Eq => val == *literal,
                    BinOp::Neq => val != *literal,
                    BinOp::Lt => val < *literal,
                    BinOp::Gt => val > *literal,
                    BinOp::Lte => val <= *literal,
                    BinOp::Gte => val >= *literal,
                    _ => false,
                }
            }
            CompiledLeaf::Float {
                data_offset,
                bitmap_byte,
                bitmap_bit,
                op,
                literal,
            } => {
                if data.len() < *data_offset + 8 || data.len() < 3 + bitmap_byte {
                    return false;
                }
                let is_null = (data[2 + bitmap_byte] >> bitmap_bit) & 1 == 1;
                if is_null {
                    return false;
                }
                let val = f64::from_le_bytes(
                    data[*data_offset..*data_offset + 8]
                        .try_into()
                        .unwrap_or_else(|_| unreachable!()),
                );
                // `total_cmp` matches Value::Ord: NaN > everything,
                // -0.0 < +0.0, finite order as expected. Keeps compiled
                // WHERE identical in semantics to the generic row-decode
                // path (which calls Value::cmp directly).
                let ord = val.total_cmp(literal);
                match op {
                    BinOp::Eq => ord.is_eq(),
                    BinOp::Neq => !ord.is_eq(),
                    BinOp::Lt => ord.is_lt(),
                    BinOp::Gt => ord.is_gt(),
                    BinOp::Lte => !ord.is_gt(),
                    BinOp::Gte => !ord.is_lt(),
                    _ => false,
                }
            }
            CompiledLeaf::IsNull {
                bitmap_byte,
                bitmap_bit,
                want_null,
            } => {
                let is_null = (data[2 + bitmap_byte] >> bitmap_bit) & 1 == 1;
                if *want_null {
                    is_null
                } else {
                    !is_null
                }
            }
            CompiledLeaf::StrEq {
                var_offset_table_start,
                var_data_start,
                var_idx,
                bitmap_byte,
                bitmap_bit,
                negate,
                needle,
            } => {
                if data.len() < 3 + bitmap_byte {
                    return false;
                }
                let is_null = (data[2 + bitmap_byte] >> bitmap_bit) & 1 == 1;
                if is_null {
                    return false;
                }
                let off_pos = var_offset_table_start + var_idx * 2;
                let next_pos = var_offset_table_start + (var_idx + 1) * 2;
                if data.len() < next_pos + 2 {
                    return false;
                }
                let start = u16::from_le_bytes(
                    data[off_pos..off_pos + 2]
                        .try_into()
                        .unwrap_or_else(|_| unreachable!()),
                ) as usize;
                let end = u16::from_le_bytes(
                    data[next_pos..next_pos + 2]
                        .try_into()
                        .unwrap_or_else(|_| unreachable!()),
                ) as usize;
                let abs_start = var_data_start + start;
                let abs_end = var_data_start + end;
                if abs_end > data.len() || abs_start > abs_end {
                    return false;
                }
                let slice = &data[abs_start..abs_end];
                let eq = slice == needle.as_slice();
                if *negate {
                    !eq
                } else {
                    eq
                }
            }
        }
    }
}

/// Attempt to compile a predicate expression into a closure over raw row
/// bytes. Returns None if the predicate contains shapes we don't handle
/// (arithmetic, Or, Coalesce, non-literal comparands, etc.). Supported:
///   - `.field <op> literal_int` and its reversed form
///   - `.field = string_literal` / `string_literal = .field`
///   - `And` conjunctions of any number of the above
///
/// Mission D3: AND chains are flattened into a single `Vec<CompiledLeaf>`
/// closed over by ONE outer closure. The previous implementation built a
/// recursive `Box<Fn>` per AND combinator, costing N+1 indirect vtable
/// calls per row for an N-leaf conjunction. The flat version dispatches
/// each leaf via match (predictable branch, fully inlinable with LTO),
/// short-circuiting on the first failing leaf.
pub(super) fn compile_predicate(
    expr: &Expr,
    columns: &[String],
    layout: &FastLayout,
    schema: &Schema,
) -> Option<CompiledPredicate> {
    let mut leaves: Vec<CompiledLeaf> = Vec::new();
    flatten_and_compile(expr, columns, layout, schema, &mut leaves)?;
    if leaves.is_empty() {
        return None;
    }
    if leaves.len() == 1 {
        // Single-leaf fast path: skip the Vec iteration entirely.
        let leaf = leaves.into_iter().next().unwrap();
        return Some(Box::new(move |data: &[u8]| leaf.eval(data)));
    }
    Some(Box::new(move |data: &[u8]| {
        // Tight short-circuit AND loop. With CompiledLeaf::eval marked
        // #[inline], LTO can fold the match arms into this loop body.
        for leaf in &leaves {
            if !leaf.eval(data) {
                return false;
            }
        }
        true
    }))
}

/// Recursively walk an AND chain and push each leaf into `out`. Returns
/// `None` if any sub-expression isn't a supported leaf shape.
fn flatten_and_compile(
    expr: &Expr,
    columns: &[String],
    layout: &FastLayout,
    schema: &Schema,
    out: &mut Vec<CompiledLeaf>,
) -> Option<()> {
    match expr {
        Expr::BinaryOp(left, BinOp::And, right) => {
            flatten_and_compile(left, columns, layout, schema, out)?;
            flatten_and_compile(right, columns, layout, schema, out)?;
            Some(())
        }
        Expr::BinaryOp(left, op, right) => {
            if let Some(leaf) = build_int_leaf(left, *op, right, columns, layout, schema) {
                out.push(leaf);
                return Some(());
            }
            if let Some(leaf) = build_float_leaf(left, *op, right, columns, layout, schema) {
                out.push(leaf);
                return Some(());
            }
            if let Some(leaf) = build_str_eq_leaf(left, *op, right, columns, layout, schema) {
                out.push(leaf);
                return Some(());
            }
            None
        }
        Expr::UnaryOp(op, inner) if *op == UnaryOp::IsNull || *op == UnaryOp::IsNotNull => {
            if let Expr::Field(name) = inner.as_ref() {
                let col_idx = columns.iter().position(|c| c == name)?;
                let bitmap_byte = col_idx / 8;
                let bitmap_bit = (col_idx % 8) as u8;
                let want_null = *op == UnaryOp::IsNull;
                out.push(CompiledLeaf::IsNull {
                    bitmap_byte,
                    bitmap_bit,
                    want_null,
                });
                Some(())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Build an `Int` leaf from `.field <op> literal_int` (or reversed).
///
/// Only fires for columns whose declared type is `TypeId::Int`. If the
/// column is a different numeric type (Float, DateTime) we return `None`
/// so the caller falls back to the generic `Value::cmp` evaluation path,
/// which correctly handles cross-type numeric comparison (e.g. Int literal
/// vs Float column in `BETWEEN 100 AND 500` on a `price: float` column).
/// Previously this function read 8 bytes of a Float column as little-endian
/// i64, producing nonsense comparisons.
fn build_int_leaf(
    left: &Expr,
    op: BinOp,
    right: &Expr,
    columns: &[String],
    layout: &FastLayout,
    schema: &Schema,
) -> Option<CompiledLeaf> {
    let (field_name, literal_val, op) = match (left, right) {
        (Expr::Field(name), Expr::Literal(Literal::Int(v))) => (name, *v, op),
        (Expr::Literal(Literal::Int(v)), Expr::Field(name)) => {
            let flipped = match op {
                BinOp::Lt => BinOp::Gt,
                BinOp::Gt => BinOp::Lt,
                BinOp::Lte => BinOp::Gte,
                BinOp::Gte => BinOp::Lte,
                other => other, // Eq, Neq are symmetric
            };
            (name, *v, flipped)
        }
        _ => return None,
    };

    let col_idx = columns.iter().position(|c| c == field_name)?;
    // Guard: the compiled Int leaf reads the column's 8 bytes as i64.
    // Only valid when the column is actually an Int column.
    if schema.columns[col_idx].type_id != TypeId::Int {
        return None;
    }
    let byte_offset = layout.fixed_offsets[col_idx]?;
    let bitmap_byte = col_idx / 8;
    let bitmap_bit = (col_idx % 8) as u8;
    let data_offset = 2 + layout.bitmap_size + byte_offset;

    Some(CompiledLeaf::Int {
        data_offset,
        bitmap_byte,
        bitmap_bit,
        op,
        literal: literal_val,
    })
}

/// Build a `Float` leaf from `.field <op> literal` where `.field` is a
/// Float column and `literal` is numeric (Float or Int — Int literals are
/// promoted to `f64` at compile time so the hot loop only sees one shape).
///
/// Mission D10: adds the Float fast-path counterpart to `build_int_leaf`.
/// Without this, `WHERE .price > 100.0` on a `price: float` column falls
/// through `compile_predicate`, forcing the whole query to the generic
/// `decode_row → Value::cmp` path which allocates a `Vec<Value>` per row.
fn build_float_leaf(
    left: &Expr,
    op: BinOp,
    right: &Expr,
    columns: &[String],
    layout: &FastLayout,
    schema: &Schema,
) -> Option<CompiledLeaf> {
    // Accept either direction: field-op-literal or literal-op-field.
    // When the literal is on the left, flip the operator so the hot-loop
    // eval can assume the field is always the LHS.
    let (field_name, literal_val, op) = match (left, right) {
        (Expr::Field(name), Expr::Literal(Literal::Float(v))) => (name, *v, op),
        (Expr::Field(name), Expr::Literal(Literal::Int(v))) => (name, *v as f64, op),
        (Expr::Literal(Literal::Float(v)), Expr::Field(name)) => {
            let flipped = match op {
                BinOp::Lt => BinOp::Gt,
                BinOp::Gt => BinOp::Lt,
                BinOp::Lte => BinOp::Gte,
                BinOp::Gte => BinOp::Lte,
                other => other,
            };
            (name, *v, flipped)
        }
        (Expr::Literal(Literal::Int(v)), Expr::Field(name)) => {
            let flipped = match op {
                BinOp::Lt => BinOp::Gt,
                BinOp::Gt => BinOp::Lt,
                BinOp::Lte => BinOp::Gte,
                BinOp::Gte => BinOp::Lte,
                other => other,
            };
            (name, *v as f64, flipped)
        }
        _ => return None,
    };

    let col_idx = columns.iter().position(|c| c == field_name)?;
    // Symmetric guard to build_int_leaf: only fire on Float columns. If
    // the column is Int but the literal was Float, we want the generic
    // path (which promotes Int → f64 via Value::cmp) — compiling a
    // Float leaf would read the i64 bytes as f64 and produce nonsense.
    if schema.columns[col_idx].type_id != TypeId::Float {
        return None;
    }
    let byte_offset = layout.fixed_offsets[col_idx]?;
    let bitmap_byte = col_idx / 8;
    let bitmap_bit = (col_idx % 8) as u8;
    let data_offset = 2 + layout.bitmap_size + byte_offset;

    Some(CompiledLeaf::Float {
        data_offset,
        bitmap_byte,
        bitmap_bit,
        op,
        literal: literal_val,
    })
}

/// Build a `StrEq` leaf from `.field = string_literal` (or reversed).
fn build_str_eq_leaf(
    left: &Expr,
    op: BinOp,
    right: &Expr,
    columns: &[String],
    layout: &FastLayout,
    schema: &Schema,
) -> Option<CompiledLeaf> {
    if op != BinOp::Eq && op != BinOp::Neq {
        return None;
    }
    let (field_name, literal_str) = match (left, right) {
        (Expr::Field(name), Expr::Literal(Literal::String(s))) => (name, s.clone()),
        (Expr::Literal(Literal::String(s)), Expr::Field(name)) => (name, s.clone()),
        _ => return None,
    };

    let col_idx = columns.iter().position(|c| c == field_name)?;
    if schema.columns[col_idx].type_id != TypeId::Str {
        return None;
    }
    let var_idx = layout.var_indices[col_idx]?;
    let var_offset_table_start = layout.var_offset_table_start();
    let var_data_start = layout.var_data_start();
    let bitmap_byte = col_idx / 8;
    let bitmap_bit = (col_idx % 8) as u8;
    let negate = op == BinOp::Neq;

    Some(CompiledLeaf::StrEq {
        var_offset_table_start,
        var_data_start,
        var_idx,
        bitmap_byte,
        bitmap_bit,
        negate,
        needle: literal_str.into_bytes(),
    })
}

/// Collect the column indices referenced by a predicate expression.
pub(super) fn predicate_column_indices(expr: &Expr, columns: &[String]) -> Vec<usize> {
    let mut indices = Vec::new();
    collect_field_indices(expr, columns, &mut indices);
    indices.sort_unstable();
    indices.dedup();
    indices
}

fn collect_field_indices(expr: &Expr, columns: &[String], out: &mut Vec<usize>) {
    match expr {
        Expr::Field(name) => {
            if let Some(idx) = columns.iter().position(|c| c == name) {
                out.push(idx);
            }
        }
        Expr::BinaryOp(left, _, right) => {
            collect_field_indices(left, columns, out);
            collect_field_indices(right, columns, out);
        }
        Expr::Coalesce(left, right) => {
            collect_field_indices(left, columns, out);
            collect_field_indices(right, columns, out);
        }
        Expr::UnaryOp(_, inner) => {
            collect_field_indices(inner, columns, out);
        }
        Expr::FunctionCall(_, inner) => {
            collect_field_indices(inner, columns, out);
        }
        Expr::ScalarFunc(_, args) => {
            for arg in args {
                collect_field_indices(arg, columns, out);
            }
        }
        Expr::Cast(inner, _) => {
            collect_field_indices(inner, columns, out);
        }
        Expr::Case { whens, else_expr } => {
            for (cond, result) in whens {
                collect_field_indices(cond, columns, out);
                collect_field_indices(result, columns, out);
            }
            if let Some(e) = else_expr {
                collect_field_indices(e, columns, out);
            }
        }
        Expr::InList { expr, list, .. } => {
            collect_field_indices(expr, columns, out);
            for item in list {
                collect_field_indices(item, columns, out);
            }
        }
        Expr::InSubquery { expr, .. } => {
            collect_field_indices(expr, columns, out);
        }
        _ => {}
    }
}

/// Decode only the specified columns from raw row bytes, filling the rest
/// with `Value::Empty`. This avoids heap allocations for String/Bytes
/// columns that the predicate doesn't reference.
pub(super) fn decode_selective(
    schema: &Schema,
    layout: &RowLayout,
    data: &[u8],
    col_indices: &[usize],
) -> Vec<Value> {
    let n_cols = schema.columns.len();
    let mut values = vec![Value::Empty; n_cols];
    for &ci in col_indices {
        values[ci] = decode_column(schema, layout, data, ci);
    }
    values
}
