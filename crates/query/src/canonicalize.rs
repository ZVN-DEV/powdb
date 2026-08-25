//! Mission D9: query canonicalization for the plan cache.
//!
//! Two queries that differ only in literal values share the same *shape* —
//! e.g. `User filter .id = 1` and `User filter .id = 2`. We want both to
//! hit the same cached plan and only substitute the literal values at
//! execute time. This module provides:
//!
//!   - [`canonicalize`] — lex an input query, hash the token stream with
//!     literal values *replaced by placeholders*, and collect the literal
//!     values in source order. The hash is the cache key; the literal
//!     vector is what we re-bind into the cached plan on a hit.
//!
//! The canonicalisation is **token-level**, not string-level. That means
//! whitespace and comment differences (`User filter .id=1` vs
//! `User filter .id = 1  # foo`) collapse to the same hash. It also means
//! literal extraction is unambiguous — no regex tricks, no risk of
//! confusing `42` inside a string with a real int literal.
//!
//! ## Hash format
//!
//! FNV-1a over a stream of "canonical bytes":
//!   - Every keyword and operator token contributes a unique 1-byte tag.
//!   - Identifier tokens contribute `[tag, len, bytes...]`.
//!   - Literal tokens contribute *only* their tag (`0xF0..0xF3`) — the
//!     value is collected separately into the literal list.
//!
//! The tag scheme is bespoke (not `mem::discriminant`) so the cache key is
//! stable across crate rebuilds. This matters for test reproducibility,
//! not for correctness; the cache itself is in-memory only.

use crate::ast::Literal;
use crate::lexer::{lex, LexError};
use crate::token::Token;

/// FNV-1a constants. The hash is u64 because the cache map is keyed on u64.
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

/// The canonical-stream consumer the token fold writes into.
///
/// A 64-bit FNV-1a key alone is not enough to serve a cached plan: FNV is
/// non-cryptographic, so colliding shapes are constructible by anyone who
/// can submit query text, and a collision would execute the *other* shape's
/// plan — a silent wrong answer. The cache therefore stores the canonical
/// byte stream next to each plan and re-compares it on every hit
/// ([`crate::plan_cache::PlanCache::get_with_substitution`]).
///
/// Making the fold generic over this sink keeps one token→bytes mapping for
/// both consumers: `u64` folds the hash only (shape-hash users like log
/// redaction pay zero allocation), while [`HashAndBytes`] additionally
/// records the exact bytes the hash covers. The per-token match arms are
/// untouched either way, so the two can never drift apart.
trait CanonSink: Sized {
    fn put(self, b: u8) -> Self;
    /// Bulk form of [`CanonSink::put`]; sinks that record bytes override it
    /// to copy the slice in one memcpy instead of a byte at a time.
    #[inline(always)]
    fn put_slice(mut self, bytes: &[u8]) -> Self {
        for b in bytes {
            self = self.put(*b);
        }
        self
    }
}

impl CanonSink for u64 {
    #[inline(always)]
    fn put(mut self, b: u8) -> Self {
        self ^= b as u64;
        self.wrapping_mul(FNV_PRIME)
    }
}

/// Sink that folds the FNV-1a hash AND keeps the bytes it covered.
struct HashAndBytes {
    hash: u64,
    bytes: Vec<u8>,
}

impl CanonSink for HashAndBytes {
    #[inline(always)]
    fn put(mut self, b: u8) -> Self {
        self.hash = self.hash.put(b);
        self.bytes.push(b);
        self
    }

    #[inline(always)]
    fn put_slice(mut self, bytes: &[u8]) -> Self {
        self.hash = self.hash.put_slice(bytes);
        self.bytes.extend_from_slice(bytes);
        self
    }
}

#[inline(always)]
fn hash_byte<S: CanonSink>(h: S, b: u8) -> S {
    h.put(b)
}

#[inline(always)]
fn hash_bytes<S: CanonSink>(h: S, bytes: &[u8]) -> S {
    h.put_slice(bytes)
}

/// Lex `input`, hash the canonicalised token stream, and collect literal
/// values in source order.
///
/// Returns `(canonical_hash, literals)`. The hash is the cache key; on a
/// hit, [`crate::plan_cache::PlanCache::get_with_substitution`] re-binds the
/// new literals into the cached plan.
pub fn canonicalize(input: &str) -> Result<(u64, Vec<Literal>), LexError> {
    let (hash, literals) = canonicalize_into(input, FNV_OFFSET)?;
    Ok((hash, literals))
}

/// [`canonicalize`], additionally returning the canonical byte stream the
/// hash covers. The plan cache stores these bytes next to the plan and
/// re-compares them on every hit, so a 64-bit hash collision degrades to a
/// cache miss (replan from source) instead of executing the wrong plan.
pub fn canonicalize_with_form(input: &str) -> Result<(u64, Vec<u8>, Vec<Literal>), LexError> {
    let sink = HashAndBytes {
        hash: FNV_OFFSET,
        bytes: Vec::with_capacity(input.len()),
    };
    let (sink, literals) = canonicalize_into(input, sink)?;
    Ok((sink.hash, sink.bytes, literals))
}

fn canonicalize_into<S: CanonSink>(input: &str, sink: S) -> Result<(S, Vec<Literal>), LexError> {
    let tokens = lex(input)?;
    // Normalize `offset <lit> limit <lit>` → `limit <lit> offset <lit>`.
    // The planner always builds the same plan shape for both orderings
    // (`Limit(Offset(...))`), and the substitute walk visits literals in
    // that fixed plan order. Without this normalization, the two source
    // orderings would hash to different cache entries and also disagree
    // with the walk order of the second — leading to limit/offset swaps
    // on cache hits for the `offset-before-limit` form. Hashing the
    // normalized token stream collapses the two orderings to one entry
    // and keeps the literal vector in `[limit_lit, offset_lit]` order.
    let tokens = normalize_limit_offset_order(tokens);
    let mut hash = sink;
    // Most queries hold 0-3 literals; we pre-size to avoid allocation
    // churn on the bench's tight loops.
    let mut literals: Vec<Literal> = Vec::with_capacity(4);
    // A JSON `->` path segment (the token immediately after an `Arrow`) is
    // STRUCTURAL, not a literal. `.data->tags->0` array indexes and
    // `->"key"` string keys must hash into the query SHAPE (so a different
    // path is a different plan) and must NOT be collected as literal slots
    // (so `count_literal_slots(plan)` still equals the source literal count —
    // the #137 invariant). Track whether the previous token was `->` and, if
    // so, hash the segment structurally instead of as a literal.
    let mut prev_arrow = false;
    for tok in &tokens {
        if prev_arrow {
            hash = hash_path_segment(hash, tok);
        } else {
            hash = hash_token(hash, tok, &mut literals);
        }
        prev_arrow = matches!(tok, Token::Arrow);
    }
    Ok((hash, literals))
}

/// If the token stream contains `offset <expr> limit <expr>` as a contiguous
/// tail segment (the normal shape for queries that use both clauses), swap
/// them in place so that the canonical form is always `limit <expr> offset
/// <expr>`. Bails out (no change) for any unusual shape the planner/parser
/// would also reject or reorder identically — the worst-case is two cache
/// entries instead of one, not incorrect substitution.
fn normalize_limit_offset_order(mut tokens: Vec<Token>) -> Vec<Token> {
    // Find the `offset` and `limit` keyword positions. We only reorder
    // when *both* are present and `offset` comes first. Multiple
    // limit/offset keywords are a parse error downstream, so we only
    // look at the first of each.
    let mut limit_pos: Option<usize> = None;
    let mut offset_pos: Option<usize> = None;
    for (i, tok) in tokens.iter().enumerate() {
        match tok {
            Token::Limit if limit_pos.is_none() => limit_pos = Some(i),
            Token::Offset if offset_pos.is_none() => offset_pos = Some(i),
            _ => {}
        }
    }
    let (Some(off_at), Some(lim_at)) = (offset_pos, limit_pos) else {
        return tokens;
    };
    if off_at >= lim_at {
        // Already in canonical `limit ... offset ...` order (or adjacent
        // the wrong way — handled by the generic walk).
        return tokens;
    }
    // Delimit each clause's expression region. The expression ends at the
    // next pipeline keyword or structural token that terminates a tail
    // clause (`filter`, `order`, `group`, `limit`, `offset`, `{`, `update`,
    // `delete`, or EOF).
    fn clause_end(tokens: &[Token], start: usize) -> usize {
        let mut i = start;
        while i < tokens.len() {
            match &tokens[i] {
                Token::Limit
                | Token::Offset
                | Token::Filter
                | Token::Order
                | Token::Group
                | Token::LBrace
                | Token::Update
                | Token::Delete
                | Token::Eof => return i,
                _ => i += 1,
            }
        }
        i
    }
    let off_expr_end = clause_end(&tokens, off_at + 1);
    if off_expr_end != lim_at {
        // Offset's expression runs up to something other than `limit` —
        // bail out, the shape is unusual.
        return tokens;
    }
    let lim_expr_end = clause_end(&tokens, lim_at + 1);
    // Extract the two chunks and splice them back in swapped order:
    // [..., OFFSET_KW, off_expr..., LIMIT_KW, lim_expr..., tail]
    //   →  [..., LIMIT_KW, lim_expr..., OFFSET_KW, off_expr..., tail]
    let tail: Vec<Token> = tokens.drain(lim_expr_end..).collect();
    let limit_chunk: Vec<Token> = tokens.drain(lim_at..lim_expr_end).collect();
    let offset_chunk: Vec<Token> = tokens.drain(off_at..lim_at).collect();
    // `tokens` now holds [..prefix..] up to just before OFFSET_KW.
    tokens.extend(limit_chunk);
    tokens.extend(offset_chunk);
    tokens.extend(tail);
    tokens
}

/// Hash a JSON `->` path segment token STRUCTURALLY, folding its value into
/// the query-shape hash and collecting NO literal. Object keys (`Ident` /
/// `StringLit`) and array indexes (`IntLit`) each get a distinct segment tag
/// so `->0` (index) and `->"0"` (string key) hash differently, and so a
/// path segment can never collide with the same token in literal position.
/// Any other token after `->` is malformed and rejected by the parser; here
/// it just contributes its ordinary shape tag (with no literal push) so the
/// cache key stays deterministic.
fn hash_path_segment<S: CanonSink>(h: S, tok: &Token) -> S {
    match tok {
        Token::Ident(s) => {
            let h = hash_byte(h, 0x08);
            let h = hash_byte(h, s.len() as u8);
            hash_bytes(h, s.as_bytes())
        }
        Token::StringLit(s) => {
            // Quoted and unquoted object-key spellings have one structural
            // identity: `->author` and `->"author"` share a plan shape.
            let h = hash_byte(h, 0x08);
            let h = hash_byte(h, s.len() as u8);
            hash_bytes(h, s.as_bytes())
        }
        Token::IntLit(v) => {
            let h = hash_byte(h, 0x0A);
            hash_bytes(h, &v.to_le_bytes())
        }
        // Any other token after `->` is a malformed path the parser rejects;
        // fold a single marker byte so canonicalization stays deterministic
        // and, crucially, never collects a literal for it.
        _ => hash_byte(h, 0x0B),
    }
}

fn hash_token<S: CanonSink>(h: S, tok: &Token, literals: &mut Vec<Literal>) -> S {
    match tok {
        // Identifiers — hash tag + length + bytes so two queries with
        // different identifiers (e.g. `User` vs `Order`) get different
        // hashes.
        Token::Ident(s) => {
            let h = hash_byte(h, 0x01);
            let h = hash_byte(h, s.len() as u8);
            hash_bytes(h, s.as_bytes())
        }
        Token::DotIdent(s) => {
            let h = hash_byte(h, 0x02);
            let h = hash_byte(h, s.len() as u8);
            hash_bytes(h, s.as_bytes())
        }
        Token::Param(s) => {
            let h = hash_byte(h, 0x03);
            let h = hash_byte(h, s.len() as u8);
            hash_bytes(h, s.as_bytes())
        }

        // Literals — hash *only* the tag, collect value separately. This
        // is the whole point of the module: 100 calls with 100 different
        // literal values produce 1 hash, 1 cache entry, 1 plan.
        Token::IntLit(v) => {
            literals.push(Literal::Int(*v));
            hash_byte(h, 0xF0)
        }
        Token::FloatLit(v) => {
            literals.push(Literal::Float(*v));
            hash_byte(h, 0xF1)
        }
        Token::StringLit(s) => {
            literals.push(Literal::String(s.clone()));
            hash_byte(h, 0xF2)
        }
        Token::BoolLit(v) => {
            literals.push(Literal::Bool(*v));
            hash_byte(h, 0xF3)
        }

        // Keywords. Every token below must hash a tag that no other token
        // uses: two tokens sharing a tag make two different statements
        // canonicalize identically, and the plan cache will then run the
        // first statement's plan for the second one. `every_token_hashes_to
        // _a_distinct_tag` enforces this over the whole enum; do not pick a
        // tag by eye. Free tags are scarce, so widen the range rather than
        // reusing one.
        Token::Type => hash_byte(h, 0x10),
        Token::Filter => hash_byte(h, 0x11),
        Token::Order => hash_byte(h, 0x12),
        Token::Limit => hash_byte(h, 0x13),
        Token::Offset => hash_byte(h, 0x14),
        Token::Insert => hash_byte(h, 0x15),
        Token::Update => hash_byte(h, 0x16),
        Token::Delete => hash_byte(h, 0x17),
        Token::Upsert => hash_byte(h, 0x18),
        Token::Returning => hash_byte(h, 0x6F),
        Token::Select => hash_byte(h, 0x19),
        Token::Required => hash_byte(h, 0x1A),
        Token::Default => hash_byte(h, 0x22),
        Token::Auto => hash_byte(h, 0x24),
        Token::Multi => hash_byte(h, 0x1B),
        Token::Link => hash_byte(h, 0x1C),
        Token::Index => hash_byte(h, 0x1D),
        Token::Unique => hash_byte(h, 0x7E),
        Token::On => hash_byte(h, 0x1E),
        Token::Asc => hash_byte(h, 0x1F),
        Token::Desc => hash_byte(h, 0x20),
        Token::And => hash_byte(h, 0x21),
        Token::Or => hash_byte(h, 0x4E),
        Token::Not => hash_byte(h, 0x23),
        Token::Exists => hash_byte(h, 0x4F),
        Token::Let => hash_byte(h, 0x25),
        Token::As => hash_byte(h, 0x26),
        Token::Match => hash_byte(h, 0x27),
        Token::Group => hash_byte(h, 0x28),
        Token::Transaction => hash_byte(h, 0x29),
        Token::View => hash_byte(h, 0x2A),
        Token::Materialized => hash_byte(h, 0x2B),
        Token::Count => hash_byte(h, 0x2C),
        Token::Avg => hash_byte(h, 0x2D),
        Token::Sum => hash_byte(h, 0x2E),
        Token::Min => hash_byte(h, 0x2F),
        Token::Max => hash_byte(h, 0x30),
        Token::Raw => hash_byte(h, 0x8F),
        Token::Join => hash_byte(h, 0x31),
        Token::Inner => hash_byte(h, 0x32),
        Token::LeftKw => hash_byte(h, 0x33),
        Token::RightKw => hash_byte(h, 0x34),
        Token::Outer => hash_byte(h, 0x35),
        Token::Cross => hash_byte(h, 0x36),
        Token::Distinct => hash_byte(h, 0x37),
        Token::In => hash_byte(h, 0x38),
        Token::Between => hash_byte(h, 0x39),
        Token::Like => hash_byte(h, 0x3A),
        Token::Having => hash_byte(h, 0x3B),
        Token::Is => hash_byte(h, 0x3C),
        Token::Null => hash_byte(h, 0x3D),
        Token::Schema => hash_byte(h, 0x3E),
        Token::Describe => hash_byte(h, 0x3F),
        Token::Upper => hash_byte(h, 0x50),
        Token::Lower => hash_byte(h, 0x51),
        Token::Length => hash_byte(h, 0x52),
        Token::Trim => hash_byte(h, 0x53),
        Token::Substring => hash_byte(h, 0x54),
        Token::Concat => hash_byte(h, 0x55),
        Token::Case => hash_byte(h, 0x56),
        Token::When => hash_byte(h, 0x57),
        Token::Then => hash_byte(h, 0x58),
        Token::Else => hash_byte(h, 0x59),
        Token::End => hash_byte(h, 0x5A),
        Token::Alter => hash_byte(h, 0x5B),
        Token::Drop => hash_byte(h, 0x5C),
        Token::Add => hash_byte(h, 0x5D),
        Token::Column => hash_byte(h, 0x5E),
        Token::Refresh => hash_byte(h, 0x5F),
        Token::Union => hash_byte(h, 0x67),
        Token::Over => hash_byte(h, 0x68),
        Token::Partition => hash_byte(h, 0x69),
        Token::RowNumber => hash_byte(h, 0x6A),
        Token::Rank => hash_byte(h, 0x6B),
        Token::DenseRank => hash_byte(h, 0x6C),
        Token::Explain => hash_byte(h, 0x6D),
        Token::Conflict => hash_byte(h, 0x6E),
        Token::Abs => hash_byte(h, 0x70),
        Token::Round => hash_byte(h, 0x71),
        Token::Ceil => hash_byte(h, 0x72),
        Token::Floor => hash_byte(h, 0x73),
        Token::Sqrt => hash_byte(h, 0x74),
        Token::Pow => hash_byte(h, 0x75),
        Token::Now => hash_byte(h, 0x76),
        Token::Extract => hash_byte(h, 0x77),
        Token::DateAdd => hash_byte(h, 0x78),
        Token::DateDiff => hash_byte(h, 0x79),
        Token::JsonType => hash_byte(h, 0x0C),
        Token::JsonText => hash_byte(h, 0x0D),
        Token::Cast => hash_byte(h, 0x7A),
        Token::Begin => hash_byte(h, 0x7B),
        Token::Commit => hash_byte(h, 0x7C),
        Token::Rollback => hash_byte(h, 0x7D),

        // Operators.
        Token::Eq => hash_byte(h, 0x40),
        Token::Neq => hash_byte(h, 0x41),
        Token::Lt => hash_byte(h, 0x42),
        Token::Gt => hash_byte(h, 0x43),
        Token::Lte => hash_byte(h, 0x44),
        Token::Gte => hash_byte(h, 0x45),
        Token::Assign => hash_byte(h, 0x46),
        Token::Arrow => hash_byte(h, 0x47),
        Token::Pipe => hash_byte(h, 0x48),
        Token::Coalesce => hash_byte(h, 0x49),
        Token::Plus => hash_byte(h, 0x4A),
        Token::Minus => hash_byte(h, 0x4B),
        Token::Star => hash_byte(h, 0x4C),
        Token::Slash => hash_byte(h, 0x4D),

        // Delimiters.
        Token::LBrace => hash_byte(h, 0x60),
        Token::RBrace => hash_byte(h, 0x61),
        Token::LParen => hash_byte(h, 0x62),
        Token::RParen => hash_byte(h, 0x63),
        Token::Comma => hash_byte(h, 0x64),
        Token::Colon => hash_byte(h, 0x65),
        Token::Dot => hash_byte(h, 0x66),

        Token::Eof => hash_byte(h, 0x7F),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One representative of every `Token` variant.
    ///
    /// Keep in sync with `token_variant_name` below: that match has no
    /// wildcard arm, so adding a variant to `Token` breaks this module's
    /// compilation and lands the author here.
    fn one_of_every_token() -> Vec<Token> {
        vec![
            Token::Ident("x".into()),
            Token::DotIdent("x".into()),
            Token::IntLit(1),
            Token::FloatLit(1.0),
            Token::StringLit("x".into()),
            Token::BoolLit(true),
            Token::Param("x".into()),
            Token::Type,
            Token::Filter,
            Token::Order,
            Token::Limit,
            Token::Offset,
            Token::Insert,
            Token::Update,
            Token::Delete,
            Token::Upsert,
            Token::Returning,
            Token::Select,
            Token::Required,
            Token::Default,
            Token::Auto,
            Token::Multi,
            Token::Link,
            Token::Index,
            Token::Unique,
            Token::On,
            Token::Conflict,
            Token::Asc,
            Token::Desc,
            Token::And,
            Token::Or,
            Token::Not,
            Token::Exists,
            Token::Let,
            Token::As,
            Token::Match,
            Token::Group,
            Token::Join,
            Token::Inner,
            Token::LeftKw,
            Token::RightKw,
            Token::Outer,
            Token::Cross,
            Token::Transaction,
            Token::Begin,
            Token::Commit,
            Token::Rollback,
            Token::View,
            Token::Materialized,
            Token::Refresh,
            Token::Union,
            Token::Having,
            Token::Distinct,
            Token::In,
            Token::Between,
            Token::Like,
            Token::Count,
            Token::Avg,
            Token::Sum,
            Token::Min,
            Token::Max,
            Token::Raw,
            Token::Is,
            Token::Null,
            Token::Upper,
            Token::Lower,
            Token::Length,
            Token::Trim,
            Token::Substring,
            Token::Concat,
            Token::Abs,
            Token::Round,
            Token::Ceil,
            Token::Floor,
            Token::Sqrt,
            Token::Pow,
            Token::Now,
            Token::Extract,
            Token::DateAdd,
            Token::DateDiff,
            Token::JsonType,
            Token::JsonText,
            Token::Cast,
            Token::Case,
            Token::When,
            Token::Then,
            Token::Else,
            Token::End,
            Token::Over,
            Token::Partition,
            Token::RowNumber,
            Token::Rank,
            Token::DenseRank,
            Token::Alter,
            Token::Drop,
            Token::Add,
            Token::Column,
            Token::Explain,
            Token::Schema,
            Token::Describe,
            Token::Eq,
            Token::Neq,
            Token::Lt,
            Token::Gt,
            Token::Lte,
            Token::Gte,
            Token::Assign,
            Token::Arrow,
            Token::Pipe,
            Token::Coalesce,
            Token::Plus,
            Token::Minus,
            Token::Star,
            Token::Slash,
            Token::LBrace,
            Token::RBrace,
            Token::LParen,
            Token::RParen,
            Token::Comma,
            Token::Colon,
            Token::Dot,
            Token::Eof,
        ]
    }

    /// Exhaustive by construction: no wildcard arm, so a new `Token` variant
    /// stops this crate's tests from compiling until it is named here and
    /// added to `one_of_every_token`.
    fn token_variant_name(t: &Token) -> &'static str {
        match t {
            Token::Ident(_) => "Ident",
            Token::DotIdent(_) => "DotIdent",
            Token::IntLit(_) => "IntLit",
            Token::FloatLit(_) => "FloatLit",
            Token::StringLit(_) => "StringLit",
            Token::BoolLit(_) => "BoolLit",
            Token::Param(_) => "Param",
            Token::Type => "Type",
            Token::Filter => "Filter",
            Token::Order => "Order",
            Token::Limit => "Limit",
            Token::Offset => "Offset",
            Token::Insert => "Insert",
            Token::Update => "Update",
            Token::Delete => "Delete",
            Token::Upsert => "Upsert",
            Token::Returning => "Returning",
            Token::Select => "Select",
            Token::Required => "Required",
            Token::Default => "Default",
            Token::Auto => "Auto",
            Token::Multi => "Multi",
            Token::Link => "Link",
            Token::Index => "Index",
            Token::Unique => "Unique",
            Token::On => "On",
            Token::Conflict => "Conflict",
            Token::Asc => "Asc",
            Token::Desc => "Desc",
            Token::And => "And",
            Token::Or => "Or",
            Token::Not => "Not",
            Token::Exists => "Exists",
            Token::Let => "Let",
            Token::As => "As",
            Token::Match => "Match",
            Token::Group => "Group",
            Token::Join => "Join",
            Token::Inner => "Inner",
            Token::LeftKw => "LeftKw",
            Token::RightKw => "RightKw",
            Token::Outer => "Outer",
            Token::Cross => "Cross",
            Token::Transaction => "Transaction",
            Token::Begin => "Begin",
            Token::Commit => "Commit",
            Token::Rollback => "Rollback",
            Token::View => "View",
            Token::Materialized => "Materialized",
            Token::Refresh => "Refresh",
            Token::Union => "Union",
            Token::Having => "Having",
            Token::Distinct => "Distinct",
            Token::In => "In",
            Token::Between => "Between",
            Token::Like => "Like",
            Token::Count => "Count",
            Token::Avg => "Avg",
            Token::Sum => "Sum",
            Token::Min => "Min",
            Token::Max => "Max",
            Token::Raw => "Raw",
            Token::Is => "Is",
            Token::Null => "Null",
            Token::Upper => "Upper",
            Token::Lower => "Lower",
            Token::Length => "Length",
            Token::Trim => "Trim",
            Token::Substring => "Substring",
            Token::Concat => "Concat",
            Token::Abs => "Abs",
            Token::Round => "Round",
            Token::Ceil => "Ceil",
            Token::Floor => "Floor",
            Token::Sqrt => "Sqrt",
            Token::Pow => "Pow",
            Token::Now => "Now",
            Token::Extract => "Extract",
            Token::DateAdd => "DateAdd",
            Token::DateDiff => "DateDiff",
            Token::JsonType => "JsonType",
            Token::JsonText => "JsonText",
            Token::Cast => "Cast",
            Token::Case => "Case",
            Token::When => "When",
            Token::Then => "Then",
            Token::Else => "Else",
            Token::End => "End",
            Token::Over => "Over",
            Token::Partition => "Partition",
            Token::RowNumber => "RowNumber",
            Token::Rank => "Rank",
            Token::DenseRank => "DenseRank",
            Token::Alter => "Alter",
            Token::Drop => "Drop",
            Token::Add => "Add",
            Token::Column => "Column",
            Token::Explain => "Explain",
            Token::Schema => "Schema",
            Token::Describe => "Describe",
            Token::Eq => "Eq",
            Token::Neq => "Neq",
            Token::Lt => "Lt",
            Token::Gt => "Gt",
            Token::Lte => "Lte",
            Token::Gte => "Gte",
            Token::Assign => "Assign",
            Token::Arrow => "Arrow",
            Token::Pipe => "Pipe",
            Token::Coalesce => "Coalesce",
            Token::Plus => "Plus",
            Token::Minus => "Minus",
            Token::Star => "Star",
            Token::Slash => "Slash",
            Token::LBrace => "LBrace",
            Token::RBrace => "RBrace",
            Token::LParen => "LParen",
            Token::RParen => "RParen",
            Token::Comma => "Comma",
            Token::Colon => "Colon",
            Token::Dot => "Dot",
            Token::Eof => "Eof",
        }
    }

    #[test]
    fn one_of_every_token_covers_the_whole_enum() {
        let toks = one_of_every_token();
        let mut names: Vec<&str> = toks.iter().map(token_variant_name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(
            before,
            names.len(),
            "one_of_every_token lists a variant twice; the distinctness test \
             below would then compare a token against itself and pass vacuously"
        );
    }

    /// Two tokens that hash to the same tag canonicalize two different
    /// statements to the same cache key, and the plan cache runs the first
    /// statement's plan for the second. That shipped: `Default` and `Or`
    /// both hashed 0x22 and `Auto` and `Exists` both hashed 0x24, which let
    /// `type W { exists a: int }` -- a syntax error on a cold engine --
    /// execute a cached `type W { auto a: int }` and create a table.
    ///
    /// Asserted over the whole enum, not over the two pairs that were found,
    /// so a partial fix or a future hand-picked tag fails the build.
    #[test]
    fn every_token_hashes_to_a_distinct_tag() {
        let mut by_hash: std::collections::HashMap<u64, &'static str> =
            std::collections::HashMap::new();
        let mut collisions: Vec<String> = Vec::new();

        for tok in one_of_every_token() {
            let name = token_variant_name(&tok);
            let mut literals = Vec::new();
            let h = hash_token(FNV_OFFSET, &tok, &mut literals);
            if let Some(prev) = by_hash.insert(h, name) {
                collisions.push(format!("{prev} and {name} share a tag"));
            }
        }

        assert!(
            collisions.is_empty(),
            "colliding canonical tags: {}",
            collisions.join("; ")
        );
    }

    #[test]
    fn test_same_query_same_hash() {
        let (h1, lits1) = canonicalize("User filter .age > 30").unwrap();
        let (h2, lits2) = canonicalize("User filter .age > 30").unwrap();
        assert_eq!(h1, h2);
        assert_eq!(lits1, lits2);
        assert_eq!(lits1, vec![Literal::Int(30)]);
    }

    #[test]
    fn test_different_literal_same_hash() {
        // The whole point of the module — `30` and `99` collapse to the
        // same canonical form.
        let (h1, lits1) = canonicalize("User filter .age > 30").unwrap();
        let (h2, lits2) = canonicalize("User filter .age > 99").unwrap();
        assert_eq!(h1, h2);
        assert_eq!(lits1, vec![Literal::Int(30)]);
        assert_eq!(lits2, vec![Literal::Int(99)]);
    }

    #[test]
    fn test_whitespace_and_comments_normalised() {
        let (h1, _) = canonicalize("User filter .age > 30").unwrap();
        let (h2, _) = canonicalize("User  filter\n.age   >   30   # foo\n").unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_different_field_different_hash() {
        let (h1, _) = canonicalize("User filter .age > 30").unwrap();
        let (h2, _) = canonicalize("User filter .id > 30").unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_different_table_different_hash() {
        let (h1, _) = canonicalize("User filter .age > 30").unwrap();
        let (h2, _) = canonicalize("Order filter .age > 30").unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_different_operator_different_hash() {
        let (h1, _) = canonicalize("User filter .age > 30").unwrap();
        let (h2, _) = canonicalize("User filter .age < 30").unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_string_literal_canonicalised() {
        let (h1, lits1) = canonicalize(r#"User filter .status = "active""#).unwrap();
        let (h2, lits2) = canonicalize(r#"User filter .status = "pending""#).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(lits1, vec![Literal::String("active".into())]);
        assert_eq!(lits2, vec![Literal::String("pending".into())]);
    }

    #[test]
    fn test_multi_literal_collected_in_source_order() {
        let (_, lits) =
            canonicalize(r#"User filter .age > 30 and .status = "active" { .name }"#).unwrap();
        assert_eq!(
            lits,
            vec![Literal::Int(30), Literal::String("active".into()),]
        );
    }

    #[test]
    fn test_insert_literals_in_assignment_order() {
        let (_, lits) =
            canonicalize(r#"insert User { id := 42, name := "Alice", age := 30 }"#).unwrap();
        assert_eq!(
            lits,
            vec![
                Literal::Int(42),
                Literal::String("Alice".into()),
                Literal::Int(30),
            ]
        );
    }

    #[test]
    fn test_update_by_pk_literals_in_source_order() {
        let (_, lits) = canonicalize("User filter .id = 42 update { age := 31 }").unwrap();
        assert_eq!(lits, vec![Literal::Int(42), Literal::Int(31)]);
    }

    // ── JSON path (#137: segments are structural, never literals) ──────────

    #[test]
    fn json_path_array_index_is_not_a_literal() {
        // The `0` in `.data->tags->0` is a STRUCTURAL path index, so it must
        // NOT be collected as a literal — only the comparison `5` is.
        let (_, lits) = canonicalize(r#"Post filter .data->tags->0 = 5"#).unwrap();
        assert_eq!(lits, vec![Literal::Int(5)]);
    }

    #[test]
    fn json_path_string_key_is_not_a_literal() {
        // The `"key"` in `.data->"key"` is a structural key, not a literal;
        // only the comparison string is collected.
        let (_, lits) = canonicalize(r#"Post filter .data->"key" = "v""#).unwrap();
        assert_eq!(lits, vec![Literal::String("v".into())]);
    }

    #[test]
    fn different_literal_same_path_shares_hash() {
        // Same path, different comparison literal → same plan (one cache entry).
        let (h1, l1) = canonicalize(r#"Post filter .data->age > 21"#).unwrap();
        let (h2, l2) = canonicalize(r#"Post filter .data->age > 65"#).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(l1, vec![Literal::Int(21)]);
        assert_eq!(l2, vec![Literal::Int(65)]);
    }

    #[test]
    fn different_path_different_hash() {
        // Different key → different plan.
        let (h1, _) = canonicalize(r#"Post filter .data->age > 21"#).unwrap();
        let (h2, _) = canonicalize(r#"Post filter .data->year > 21"#).unwrap();
        assert_ne!(h1, h2);
        // Different array index → different plan.
        let (h3, _) = canonicalize(r#"Post filter .data->tags->0 = "x""#).unwrap();
        let (h4, _) = canonicalize(r#"Post filter .data->tags->1 = "x""#).unwrap();
        assert_ne!(h3, h4);
    }

    #[test]
    fn string_key_and_int_index_hash_differently() {
        // `->0` (index) and `->"0"` (string key) must not collide.
        let (h1, _) = canonicalize(r#"Post filter .data->0 = 1"#).unwrap();
        let (h2, _) = canonicalize(r#"Post filter .data->"0" = 1"#).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn quoted_and_unquoted_path_keys_share_identity() {
        let (unquoted, _) = canonicalize(r#"Post filter .data->author = "x""#).unwrap();
        let (quoted, _) = canonicalize(r#"Post filter .data->"author" = "x""#).unwrap();
        assert_eq!(unquoted, quoted);
    }

    #[test]
    fn expression_index_ddl_paths_are_structural_and_key_index_safe() {
        let (unquoted, literals) = canonicalize("alter Post add index (.data->author->0)").unwrap();
        let (quoted, quoted_literals) =
            canonicalize("alter Post add index (.data->\"author\"->0)").unwrap();
        assert_eq!(unquoted, quoted);
        assert!(literals.is_empty());
        assert!(quoted_literals.is_empty());

        let (array_index, _) = canonicalize("alter Post add index (.data->0)").unwrap();
        let (object_key, _) = canonicalize("alter Post add index (.data->\"0\")").unwrap();
        assert_ne!(array_index, object_key);
    }

    #[test]
    fn aggregate_mode_is_part_of_canonical_identity() {
        let (symmetric, _) = canonicalize("Post group .id { n: sum(.id) }").unwrap();
        let (raw, _) = canonicalize("Post group .id { n: sum(raw .id) }").unwrap();
        assert_ne!(symmetric, raw);
    }
}
