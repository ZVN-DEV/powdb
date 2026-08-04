//! Compiled predicates and fast-path layout utilities.

use crate::ast::*;
use crate::executor::eval::int_f64_cmp;
use powdb_storage::pj1::{pj1_get, PathSeg as Pj1Seg};
use powdb_storage::row::{decode_column, row_is_v2, RowLayout, ROW_MAGIC, ROW_PREFIX_SIZE};
use powdb_storage::types::*;
use std::cmp::Ordering;

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

pub(super) fn type_name_to_id(name: &str) -> Result<TypeId, String> {
    match name.to_ascii_lowercase().as_str() {
        "str" | "string" => Ok(TypeId::Str),
        "int" => Ok(TypeId::Int),
        "float" => Ok(TypeId::Float),
        "bool" | "boolean" => Ok(TypeId::Bool),
        "datetime" => Ok(TypeId::DateTime),
        "uuid" => Ok(TypeId::Uuid),
        "bytes" => Ok(TypeId::Bytes),
        // Lane B: `type T { data: json }` declares a canonical-binary JSON
        // (PJ1) column. This is the type-name lookup table, not the compiled
        // predicate fast path (which a later lane owns).
        "json" => Ok(TypeId::Json),
        _ => Err(format!("unknown type name: '{name}'")),
    }
}

/// The canonical PowQL spelling of a `TypeId`, for schema introspection
/// output. Inverse of [`type_name_to_id`].
pub(super) fn type_id_to_name(type_id: TypeId) -> &'static str {
    match type_id {
        TypeId::Int => "int",
        TypeId::Float => "float",
        TypeId::Bool => "bool",
        TypeId::Str => "str",
        TypeId::DateTime => "datetime",
        TypeId::Uuid => "uuid",
        TypeId::Bytes => "bytes",
        // Lane B/C own the `json` column-declaration keyword and path grammar;
        // this arm only gives schema introspection a name to print.
        TypeId::Json => "json",
        TypeId::Empty => "empty",
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
                fixed_pos += fixed_size(col.type_id)
                    .expect("is_fixed_size guard ensures fixed_size returns Some");
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
    /// `.jsoncol->seg->... <op> literal` (or reversed) over an INLINE PJ1
    /// document (design 4.4, the compiled inline-document leaf). Locates the
    /// json column's var-slot bytes (raw canonical PJ1, no inner length prefix,
    /// exactly like `StrEq`), walks the path zero-copy via `pj1_get`, and
    /// compares the addressed scalar node against a literal pre-encoded once at
    /// compile time. No parse, no allocation on the hot path: numeric/bool nodes
    /// are read straight from their fixed LE payload and string nodes compare
    /// byte-for-byte against the literal's UTF-8 bytes. The comparison honors the
    /// exact `Value` `PartialEq`/`Ord` semantics the decode path uses (via
    /// [`json_scalar_eq`]/[`json_scalar_cmp`]), so a compiled JSON filter is
    /// byte-for-byte identical in results to the generic `pj1_scalarize` +
    /// `eval_binop` fallback. Only built when the base column's `TypeId` is
    /// `Json`; the "table has no overflow rows" half of the gate is enforced by
    /// callers (every compiled read/mutation path checks `table_has_overflow`
    /// first), and the per-row `row_is_v2` guard in `compile_predicate` routes
    /// any stray v2 row to the fallback.
    Json {
        var_offset_table_start: usize,
        var_data_start: usize,
        var_idx: usize,
        bitmap_byte: usize,
        bitmap_bit: u8,
        segments: Vec<JsonSeg>,
        op: BinOp,
        /// The comparison literal, pre-encoded once as a scalar `Value`
        /// (`Int`/`Float`/`Str`/`Bool`). `.as_bytes()` on the `Str` variant is
        /// the needle for the zero-alloc string-node compare.
        literal: Value,
    },
}

/// An owned path segment for a compiled JSON leaf. Mirrors [`crate::ast::PathSeg`]
/// but is stored inside the `'static` closure; the borrowed
/// [`powdb_storage::pj1::PathSeg`] the walker needs is reconstructed per segment
/// with no allocation (a `&str` borrow of the owned `String`).
enum JsonSeg {
    Key(String),
    Index(u32),
}

impl CompiledLeaf {
    /// Evaluate this leaf against a row's raw bytes. `#[inline]` so the
    /// match folds into the caller's tight loop with LTO.
    #[inline]
    fn eval(&self, data: &[u8]) -> bool {
        let base = if data.len() >= ROW_PREFIX_SIZE && &data[0..4] == ROW_MAGIC {
            ROW_PREFIX_SIZE
        } else {
            0
        };
        let data = &data[base..];
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
            CompiledLeaf::Json {
                var_offset_table_start,
                var_data_start,
                var_idx,
                bitmap_byte,
                bitmap_bit,
                segments,
                op,
                literal,
            } => {
                let node = json_locate_node(
                    data,
                    *var_offset_table_start,
                    *var_data_start,
                    *var_idx,
                    *bitmap_byte,
                    *bitmap_bit,
                    segments,
                );
                json_compare(node, *op, literal)
            }
        }
    }
}

/// Locate and path-walk the inline PJ1 document for a compiled JSON leaf,
/// returning the addressed node as a borrowed sub-slice (itself a valid
/// standalone PJ1 document). Returns `None` — semantically the empty set, i.e.
/// scalarizes to `Value::Empty` — when the json column is NULL, the path
/// misses, or the bytes are truncated. The document slice is located exactly
/// like [`CompiledLeaf::StrEq`]: a var column stores its raw payload directly
/// between the offset-table entries (no inner length prefix), and for a `Json`
/// column that payload IS the canonical PJ1 bytes.
#[inline]
fn json_locate_node<'a>(
    data: &'a [u8],
    var_offset_table_start: usize,
    var_data_start: usize,
    var_idx: usize,
    bitmap_byte: usize,
    bitmap_bit: u8,
    segments: &[JsonSeg],
) -> Option<&'a [u8]> {
    if data.len() < 3 + bitmap_byte {
        return None;
    }
    // NULL json column: no document at all -> empty set.
    if (data[2 + bitmap_byte] >> bitmap_bit) & 1 == 1 {
        return None;
    }
    let off_pos = var_offset_table_start + var_idx * 2;
    let next_pos = var_offset_table_start + (var_idx + 1) * 2;
    if data.len() < next_pos + 2 {
        return None;
    }
    let start = u16::from_le_bytes(data[off_pos..off_pos + 2].try_into().ok()?) as usize;
    let end = u16::from_le_bytes(data[next_pos..next_pos + 2].try_into().ok()?) as usize;
    let abs_start = var_data_start + start;
    let abs_end = var_data_start + end;
    if abs_end > data.len() || abs_start > abs_end {
        return None;
    }
    let mut cur: &[u8] = &data[abs_start..abs_end];
    for seg in segments {
        let pj = match seg {
            JsonSeg::Key(k) => Pj1Seg::Key(k.as_str()),
            JsonSeg::Index(i) => Pj1Seg::Index(*i),
        };
        cur = pj1_get(cur, &pj)?;
    }
    Some(cur)
}

/// True when the addressed JSON `node` scalarizes to the empty set, mirroring
/// `eval::pj1_scalarize`: a missing path (`None`), a JSON `null`
/// (tag 0), or a truncated / invalid scalar. Arrays and objects (tags 6/7)
/// scalarize to `Value::Json`, so they are NOT empty. A missing value never
/// matches a comparison, so these must be excluded — the same guard
/// `eval_binop` applies on the generic path.
#[inline]
fn json_node_is_empty(node: Option<&[u8]>) -> bool {
    let Some(n) = node else {
        return true;
    };
    match n.first() {
        Some(1) | Some(2) | Some(6) | Some(7) => false, // bool / object / array
        Some(3) | Some(4) => n.len() < 9,               // int / float (truncated => empty)
        Some(5) => {
            if n.len() < 5 {
                return true;
            }
            let len = u32::from_le_bytes(n[1..5].try_into().unwrap()) as usize;
            match n.get(5..5 + len) {
                Some(b) => std::str::from_utf8(b).is_err(),
                None => true,
            }
        }
        // tag 0 (JSON null), reserved / unknown tag, or empty slice.
        _ => true,
    }
}

/// Compare the addressed JSON `node` (None = the empty set) against a
/// pre-encoded scalar `literal` under `op`, reproducing `eval_binop` exactly.
/// A node that scalarizes to the empty set (missing path, JSON `null`, or a
/// truncated scalar) never matches a comparison — the row is excluded, matching
/// the generic path's `Empty` guard. For present scalars, `Eq`/`Neq` use
/// `Value::PartialEq` (so `Int` never equals `Float`) and the ordering ops use
/// `Value::Ord` (cross-numeric promotion, unrelated types by `TypeId`).
/// `And`/`Or`/arithmetic/`Like` never build a `Json` leaf, so they return
/// `false` here defensively.
#[inline]
fn json_compare(node: Option<&[u8]>, op: BinOp, literal: &Value) -> bool {
    if json_node_is_empty(node) {
        return false;
    }
    match op {
        BinOp::Eq => json_scalar_eq(node, literal),
        BinOp::Neq => !json_scalar_eq(node, literal),
        BinOp::Lt => json_scalar_cmp(node, literal) == Ordering::Less,
        BinOp::Gt => json_scalar_cmp(node, literal) == Ordering::Greater,
        BinOp::Lte => json_scalar_cmp(node, literal) != Ordering::Greater,
        BinOp::Gte => json_scalar_cmp(node, literal) != Ordering::Less,
        _ => false,
    }
}

/// `TypeId`-rank comparison, the tail arm of `Value::cmp` for unrelated types.
#[inline]
fn type_id_cmp(a: TypeId, b: TypeId) -> Ordering {
    (a as u8).cmp(&(b as u8))
}

/// Equality of a scalarized JSON `node` against `lit`, matching what
/// `eval::eval_binop_mode` computes for the same pair. A missing/null node (the
/// empty set) equals no scalar literal. String nodes compare byte-for-byte
/// (zero alloc); numbers compare NUMERICALLY across int and float, exactly as
/// `json_scalar_cmp` below and the generic evaluator both do, so `->v = 3`
/// cannot answer one thing when the predicate compiles and another when it does
/// not. `<` and `>` were already numeric here; leaving `=` strict is what made
/// the six operators over a JSON path disagree with each other.
#[inline]
fn json_scalar_eq(node: Option<&[u8]>, lit: &Value) -> bool {
    let Some(n) = node else { return false };
    match n.first() {
        Some(1) => matches!(lit, Value::Bool(false)),
        Some(2) => matches!(lit, Value::Bool(true)),
        Some(3) if n.len() >= 9 => {
            let a = i64::from_le_bytes(n[1..9].try_into().unwrap());
            match lit {
                Value::Int(l) => a == *l,
                Value::Float(l) => int_f64_cmp(a, *l) == Ordering::Equal,
                _ => false,
            }
        }
        Some(4) if n.len() >= 9 => {
            let a = f64::from_le_bytes(n[1..9].try_into().unwrap());
            match lit {
                Value::Float(l) => a.total_cmp(l) == Ordering::Equal,
                Value::Int(l) => int_f64_cmp(*l, a) == Ordering::Equal,
                _ => false,
            }
        }
        Some(5) if n.len() >= 5 => match lit {
            Value::Str(l) => {
                let len = u32::from_le_bytes(n[1..5].try_into().unwrap()) as usize;
                n.get(5..5 + len) == Some(l.as_bytes())
            }
            _ => false,
        },
        // null (0), object (6), array (7), truncated, or missing: not equal to
        // any scalar literal.
        _ => false,
    }
}

/// Order a scalarized JSON `node` against `lit` under `Value::Ord`. Mirrors
/// `Value::cmp`: numbers promote across int/float via `total_cmp`, the empty
/// set (missing/null node) sorts before every literal, and unrelated types
/// fall back to `TypeId` rank. Zero allocation for every same-type comparison
/// (the realistic case); cross-type paths read only the node's tag.
#[inline]
fn json_scalar_cmp(node: Option<&[u8]>, lit: &Value) -> Ordering {
    let Some(n) = node else { return Ordering::Less };
    match n.first() {
        // JSON null scalarizes to Empty, which sorts before any literal.
        Some(0) => Ordering::Less,
        Some(1) => cmp_bool_lit(false, lit),
        Some(2) => cmp_bool_lit(true, lit),
        Some(3) if n.len() >= 9 => {
            let a = i64::from_le_bytes(n[1..9].try_into().unwrap());
            match lit {
                Value::Int(b) => a.cmp(b),
                Value::Float(b) => int_f64_cmp(a, *b),
                other => type_id_cmp(TypeId::Int, other.type_id()),
            }
        }
        Some(4) if n.len() >= 9 => {
            let a = f64::from_le_bytes(n[1..9].try_into().unwrap());
            match lit {
                Value::Float(b) => a.total_cmp(b),
                Value::Int(b) => int_f64_cmp(*b, a).reverse(),
                other => type_id_cmp(TypeId::Float, other.type_id()),
            }
        }
        Some(5) if n.len() >= 5 => {
            let len = u32::from_le_bytes(n[1..5].try_into().unwrap()) as usize;
            match (n.get(5..5 + len), lit) {
                (Some(s), Value::Str(b)) => s.cmp(b.as_bytes()),
                (Some(_), other) => type_id_cmp(TypeId::Str, other.type_id()),
                // Truncated string node scalarizes to Empty.
                (None, _) => Ordering::Less,
            }
        }
        // object/array node vs a scalar literal: ordered by TypeId rank.
        Some(6) | Some(7) => type_id_cmp(TypeId::Json, lit.type_id()),
        // Truncated/reserved/missing: the empty set.
        _ => Ordering::Less,
    }
}

/// Order a bool JSON node against a literal per `Value::cmp`.
#[inline]
fn cmp_bool_lit(b: bool, lit: &Value) -> Ordering {
    match lit {
        Value::Bool(lb) => b.cmp(lb),
        other => type_id_cmp(TypeId::Bool, other.type_id()),
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
    // Per-row v2 routing (design 3.7): the compiled leaves read fixed columns
    // at v1 byte offsets, which are wrong for a v2 row (its overflow bitmap
    // shifts every downstream offset). v2 rows are rare, so each one falls back
    // to a generic decode-based evaluation; v0/v1 rows keep the zero-copy
    // compiled path. One `row_is_v2` compare per row gates it. NOTE (v0.11
    // limitation): the fallback decodes via `decode_column`, which reports a
    // SPILLED var column as Empty (it cannot reach the heap to reassemble the
    // value). Predicates over out-of-line values therefore evaluate against
    // Empty until the v0.12 RowCtx/overflow-fetch leaf lands — reads via
    // `Table::get`/`scan` are unaffected (they reassemble).
    let fb_expr = expr.clone();
    let fb_columns = columns.to_vec();
    let fb_schema = schema.clone();
    let fb_layout = RowLayout::new(schema);
    let fallback = move |data: &[u8]| -> bool {
        let row: Vec<Value> = (0..fb_schema.columns.len())
            .map(|ci| decode_column(&fb_schema, &fb_layout, data, ci))
            .collect();
        super::eval::eval_predicate(&fb_expr, &row, &fb_columns)
    };

    if leaves.len() == 1 {
        // Single-leaf fast path: skip the Vec iteration entirely.
        let leaf = leaves
            .into_iter()
            .next()
            .expect("leaves.len() == 1 checked above");
        return Some(Box::new(move |data: &[u8]| {
            if row_is_v2(data) {
                fallback(data)
            } else {
                leaf.eval(data)
            }
        }));
    }
    Some(Box::new(move |data: &[u8]| {
        if row_is_v2(data) {
            return fallback(data);
        }
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
            if let Some(leaf) = build_json_leaf(left, *op, right, columns, layout, schema) {
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
    // Guard: the compiled Int leaf reads the column's 8 bytes as i64. That is
    // valid for an Int column and equally valid for a DateTime column, which
    // stores i64 micros in the same fixed 8 bytes (`row.rs` encodes both with
    // `to_le_bytes`). A timestamp literal has no distinct spelling in PowQL, so
    // `.created_at > 1700000000000000` is an Int literal against a DateTime
    // column: the shape an ORM emits constantly. Compiling it here keeps the
    // fast path in step with the generic comparison and with the index path,
    // all three of which compare the raw micros.
    if !matches!(
        schema.columns[col_idx].type_id,
        TypeId::Int | TypeId::DateTime
    ) {
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
    //
    // An Int literal is widened to the leaf's f64 ONLY when the leaf's
    // `total_cmp` then agrees with the reference rule (`eval::int_f64_cmp`),
    // which compares by exact numeric value at every magnitude. Two literals
    // fail that: one past 2^53 that rounds when widened (9223372036854775807
    // becomes 2^63.0) would match floats the interpreted path correctly
    // refuses, and literal `0`, because `total_cmp` says `-0.0 < 0.0` while
    // the numeric rule says a stored `-0.0` equals int `0`. Declining
    // compilation routes the predicate to the generic evaluator, which is
    // exact; a zero spelled as a FLOAT literal (`.v > 0.0`) keeps the leaf
    // and its pinned total-order zero semantics.
    let exact_widened = |v: i64| {
        let w = v as f64;
        (v != 0 && int_f64_cmp(v, w) == Ordering::Equal).then_some(w)
    };
    let (field_name, literal_val, op) = match (left, right) {
        (Expr::Field(name), Expr::Literal(Literal::Float(v))) => (name, *v, op),
        (Expr::Field(name), Expr::Literal(Literal::Int(v))) => (name, exact_widened(*v)?, op),
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
            (name, exact_widened(*v)?, flipped)
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

/// Build a `Json` leaf from `.jsoncol->seg... <op> literal` (or the reversed
/// `literal <op> .jsoncol->seg...`), the compiled inline-document path (design
/// 4.4). Fires only when:
///   - one side is an `Expr::JsonPath` whose base is a bare `Expr::Field`,
///   - that field is a column of declared type `TypeId::Json`, and
///   - the other side is a scalar `Literal` and `op` is a comparison.
///
/// Everything else (a `QualifiedField` base, a non-json column, a non-literal
/// comparand, `And`/`Or`/arithmetic) returns `None` and falls back to the
/// generic `pj1_scalarize` + `eval_binop` decode path, which stays correct.
/// The "table has no overflow rows" half of the design 4.4 gate is the
/// caller's responsibility (every compiled scan path checks `table_has_overflow`
/// before compiling); a stray v2 row is still routed to the fallback by the
/// `row_is_v2` guard in `compile_predicate`.
fn build_json_leaf(
    left: &Expr,
    op: BinOp,
    right: &Expr,
    columns: &[String],
    layout: &FastLayout,
    schema: &Schema,
) -> Option<CompiledLeaf> {
    // Normalise to (path, literal, op-with-field-on-left). When the literal is
    // on the left, flip the comparison so eval can assume the path is the LHS.
    let (base, segments, lit, op) = match (left, right) {
        (Expr::JsonPath { base, segments }, Expr::Literal(l)) => (base, segments, l, op),
        (Expr::Literal(l), Expr::JsonPath { base, segments }) => {
            let flipped = match op {
                BinOp::Lt => BinOp::Gt,
                BinOp::Gt => BinOp::Lt,
                BinOp::Lte => BinOp::Gte,
                BinOp::Gte => BinOp::Lte,
                other => other, // Eq, Neq symmetric
            };
            (base, segments, l, flipped)
        }
        _ => return None,
    };

    // Only comparison operators build a leaf.
    if !matches!(
        op,
        BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Lte | BinOp::Gte
    ) {
        return None;
    }

    // The base must be a bare field naming a scan column (a qualified base is
    // left to the generic path).
    let Expr::Field(name) = base.as_ref() else {
        return None;
    };
    let col_idx = columns.iter().position(|c| c == name)?;
    if schema.columns[col_idx].type_id != TypeId::Json {
        return None;
    }
    let var_idx = layout.var_indices[col_idx]?;

    // Pre-encode the comparison literal once as a scalar Value.
    let literal = match lit {
        Literal::Int(v) => Value::Int(*v),
        Literal::Float(v) => Value::Float(*v),
        Literal::String(s) => Value::Str(s.clone()),
        Literal::Bool(b) => Value::Bool(*b),
    };

    let segs: Vec<JsonSeg> = segments
        .iter()
        .map(|s| match s {
            PathSeg::Key(k) => JsonSeg::Key(k.clone()),
            PathSeg::Index(i) => JsonSeg::Index(*i),
        })
        .collect();

    Some(CompiledLeaf::Json {
        var_offset_table_start: layout.var_offset_table_start(),
        var_data_start: layout.var_data_start(),
        var_idx,
        bitmap_byte: col_idx / 8,
        bitmap_bit: (col_idx % 8) as u8,
        segments: segs,
        op,
        literal,
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
        Expr::FunctionCall(_, inner, _) => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_name_to_id_lowercase() {
        assert_eq!(type_name_to_id("int").unwrap(), TypeId::Int);
        assert_eq!(type_name_to_id("str").unwrap(), TypeId::Str);
        assert_eq!(type_name_to_id("float").unwrap(), TypeId::Float);
        assert_eq!(type_name_to_id("bool").unwrap(), TypeId::Bool);
        assert_eq!(type_name_to_id("datetime").unwrap(), TypeId::DateTime);
        assert_eq!(type_name_to_id("uuid").unwrap(), TypeId::Uuid);
        assert_eq!(type_name_to_id("bytes").unwrap(), TypeId::Bytes);
    }

    #[test]
    fn type_name_to_id_case_insensitive() {
        assert_eq!(type_name_to_id("Int").unwrap(), TypeId::Int);
        assert_eq!(type_name_to_id("INT").unwrap(), TypeId::Int);
        assert_eq!(type_name_to_id("Str").unwrap(), TypeId::Str);
        assert_eq!(type_name_to_id("Float").unwrap(), TypeId::Float);
        assert_eq!(type_name_to_id("Bool").unwrap(), TypeId::Bool);
        assert_eq!(type_name_to_id("DateTime").unwrap(), TypeId::DateTime);
        assert_eq!(type_name_to_id("DATETIME").unwrap(), TypeId::DateTime);
        assert_eq!(type_name_to_id("Uuid").unwrap(), TypeId::Uuid);
        assert_eq!(type_name_to_id("UUID").unwrap(), TypeId::Uuid);
        assert_eq!(type_name_to_id("Bytes").unwrap(), TypeId::Bytes);
        assert_eq!(type_name_to_id("String").unwrap(), TypeId::Str);
        assert_eq!(type_name_to_id("Boolean").unwrap(), TypeId::Bool);
    }

    #[test]
    fn type_name_to_id_unknown_returns_error() {
        let err = type_name_to_id("Foo").unwrap_err();
        assert!(err.contains("unknown type name"), "got: {err}");
        assert!(err.contains("Foo"), "got: {err}");
    }

    // ── compiled JSON leaf (design 4.4) ─────────────────────────────────────
    //
    // The compiled inline-document leaf must be byte-for-byte identical in
    // results to the generic decode fallback (`pj1_scalarize` + `eval_binop`).
    // These tests build a real row, compile a JSON predicate, and assert the
    // compiled closure agrees with `eval_predicate` over the decoded row for a
    // wide matrix of node types, operators, and literal types — including the
    // cross-type and missing/null corner cases where the two code paths could
    // most easily diverge.

    use powdb_storage::row::{decode_row, encode_row};

    fn json_schema() -> Schema {
        Schema {
            table_name: "Post".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    type_id: TypeId::Int,
                    required: true,
                    position: 0,
                },
                ColumnDef {
                    name: "data".into(),
                    type_id: TypeId::Json,
                    required: false,
                    position: 1,
                },
            ],
        }
    }

    /// Encode a row `{ id, data }` where `data` is `Some(json_text)` (coerced
    /// to canonical PJ1) or `None` (a NULL json column).
    fn json_row(schema: &Schema, id: i64, data: Option<&str>) -> Vec<u8> {
        let dv = match data {
            Some(t) => Value::Json(
                powdb_storage::pj1::parse_json_text(t)
                    .expect("valid json")
                    .into_boxed_slice(),
            ),
            None => Value::Empty,
        };
        encode_row(schema, &[Value::Int(id), dv])
    }

    fn path(base: &str, segs: &[PathSeg]) -> Expr {
        Expr::JsonPath {
            base: Box::new(Expr::Field(base.into())),
            segments: segs.to_vec(),
        }
    }

    /// Assert the compiled leaf and the generic decode fallback agree for
    /// `expr` on every `(id, data)` row.
    fn assert_agrees(expr: &Expr, rows: &[(i64, Option<&str>)]) {
        let schema = json_schema();
        let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
        let fast = FastLayout::new(&schema);
        let compiled =
            compile_predicate(expr, &columns, &fast, &schema).expect("JSON predicate must compile");
        for &(id, data) in rows {
            let encoded = json_row(&schema, id, data);
            let decoded = decode_row(&schema, &encoded);
            let want = super::super::eval::eval_predicate(expr, &decoded, &columns);
            let got = compiled(&encoded);
            assert_eq!(
                got, want,
                "compiled != decode for id={id} data={data:?} expr={expr:?}"
            );
        }
    }

    const SAMPLE_ROWS: &[(i64, Option<&str>)] = &[
        (
            1,
            Some(r#"{"author":"alice","age":30,"score":9.5,"active":true}"#),
        ),
        (
            2,
            Some(r#"{"author":"bob","age":18,"score":9.5,"active":false}"#),
        ),
        (
            3,
            Some(r#"{"author":"carol","age":30,"tags":["x","y"],"nested":{"k":7}}"#),
        ),
        (4, Some(r#"{"maybe":null,"age":21}"#)),
        (5, Some(r#"{"other":1}"#)), // author/age missing
        (6, None),                   // NULL json column
    ];

    fn cmp(left: Expr, op: BinOp, right: Expr) -> Expr {
        Expr::BinaryOp(Box::new(left), op, Box::new(right))
    }

    fn lit_str(s: &str) -> Expr {
        Expr::Literal(Literal::String(s.into()))
    }
    fn lit_int(v: i64) -> Expr {
        Expr::Literal(Literal::Int(v))
    }
    fn lit_float(v: f64) -> Expr {
        Expr::Literal(Literal::Float(v))
    }
    fn lit_bool(v: bool) -> Expr {
        Expr::Literal(Literal::Bool(v))
    }

    #[test]
    fn json_leaf_matches_fallback_string_eq() {
        let author = || path("data", &[PathSeg::Key("author".into())]);
        assert_agrees(&cmp(author(), BinOp::Eq, lit_str("alice")), SAMPLE_ROWS);
        assert_agrees(&cmp(author(), BinOp::Neq, lit_str("alice")), SAMPLE_ROWS);
        // Reversed operand order (literal on the left) flips correctly.
        assert_agrees(&cmp(lit_str("bob"), BinOp::Eq, author()), SAMPLE_ROWS);
        // String ordering.
        assert_agrees(&cmp(author(), BinOp::Lt, lit_str("bob")), SAMPLE_ROWS);
        assert_agrees(&cmp(author(), BinOp::Gte, lit_str("bob")), SAMPLE_ROWS);
    }

    #[test]
    fn json_leaf_matches_fallback_numeric() {
        let age = || path("data", &[PathSeg::Key("age".into())]);
        for op in [
            BinOp::Eq,
            BinOp::Neq,
            BinOp::Lt,
            BinOp::Gt,
            BinOp::Lte,
            BinOp::Gte,
        ] {
            assert_agrees(&cmp(age(), op, lit_int(21)), SAMPLE_ROWS);
            // Reversed order.
            assert_agrees(&cmp(lit_int(21), op, age()), SAMPLE_ROWS);
            // Int node vs Float literal (cross-numeric: Eq is strict-false,
            // ordering promotes) — the sharpest divergence risk.
            assert_agrees(&cmp(age(), op, lit_float(21.0)), SAMPLE_ROWS);
        }
    }

    #[test]
    fn json_leaf_matches_fallback_float_and_bool() {
        let score = || path("data", &[PathSeg::Key("score".into())]);
        let active = || path("data", &[PathSeg::Key("active".into())]);
        for op in [BinOp::Eq, BinOp::Neq, BinOp::Lt, BinOp::Gte] {
            assert_agrees(&cmp(score(), op, lit_float(9.5)), SAMPLE_ROWS);
            assert_agrees(&cmp(score(), op, lit_int(9)), SAMPLE_ROWS);
            assert_agrees(&cmp(active(), op, lit_bool(true)), SAMPLE_ROWS);
        }
    }

    #[test]
    fn json_leaf_matches_fallback_nested_and_array() {
        let nested_k = path(
            "data",
            &[PathSeg::Key("nested".into()), PathSeg::Key("k".into())],
        );
        assert_agrees(&cmp(nested_k, BinOp::Eq, lit_int(7)), SAMPLE_ROWS);
        let tag0 = path("data", &[PathSeg::Key("tags".into()), PathSeg::Index(0)]);
        assert_agrees(&cmp(tag0, BinOp::Eq, lit_str("x")), SAMPLE_ROWS);
    }

    #[test]
    fn json_leaf_matches_fallback_type_mismatch_and_composite() {
        // Type-mismatched comparisons must still match Value semantics exactly.
        let author = || path("data", &[PathSeg::Key("author".into())]);
        // String node vs int literal (unrelated types -> TypeId rank ordering).
        assert_agrees(&cmp(author(), BinOp::Lt, lit_int(5)), SAMPLE_ROWS);
        assert_agrees(&cmp(author(), BinOp::Eq, lit_int(5)), SAMPLE_ROWS);
        // Composite (object/array) node compared to a scalar literal.
        let tags = || path("data", &[PathSeg::Key("tags".into())]);
        assert_agrees(&cmp(tags(), BinOp::Eq, lit_str("x")), SAMPLE_ROWS);
        assert_agrees(&cmp(tags(), BinOp::Gt, lit_int(0)), SAMPLE_ROWS);
        // JSON null value vs missing path vs NULL column: all scalarize Empty.
        let maybe = || path("data", &[PathSeg::Key("maybe".into())]);
        assert_agrees(&cmp(maybe(), BinOp::Eq, lit_str("x")), SAMPLE_ROWS);
        assert_agrees(&cmp(maybe(), BinOp::Lt, lit_int(0)), SAMPLE_ROWS);
    }

    #[test]
    fn json_leaf_composes_with_and_conjunction() {
        // A JSON leaf ANDed with an Int-column leaf: both must flatten and the
        // compiled conjunction must agree with the fallback.
        let expr = cmp(
            path("data", &[PathSeg::Key("author".into())]),
            BinOp::Eq,
            lit_str("alice"),
        );
        let and = Expr::BinaryOp(
            Box::new(expr),
            BinOp::And,
            Box::new(cmp(Expr::Field("id".into()), BinOp::Lt, lit_int(3))),
        );
        assert_agrees(&and, SAMPLE_ROWS);
    }

    #[test]
    fn json_leaf_not_built_for_non_json_column() {
        // `.id->x` on an int column must NOT compile a JSON leaf (the base type
        // guard). `compile_predicate` returns None so the caller uses the
        // fallback (where `validate_json_path_types` has already rejected it).
        let schema = json_schema();
        let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
        let fast = FastLayout::new(&schema);
        let expr = cmp(
            path("id", &[PathSeg::Key("x".into())]),
            BinOp::Eq,
            lit_int(1),
        );
        assert!(compile_predicate(&expr, &columns, &fast, &schema).is_none());
    }
}
