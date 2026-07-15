use powdb_storage::types::Value;

/// Top-level PowQL statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Query(QueryExpr),
    Insert(InsertExpr),
    UpdateQuery(UpdateExpr),
    DeleteQuery(DeleteExpr),
    CreateType(CreateTypeExpr),
    AlterTable(AlterTableExpr),
    DropTable(DropTableExpr),
    CreateView(CreateViewExpr),
    RefreshView(RefreshViewExpr),
    DropView(DropViewExpr),
    Union(UnionExpr),
    Upsert(UpsertExpr),
    Explain(Box<Statement>),
    Begin,
    Commit,
    Rollback,
    /// `schema` — introspection: list every type (table) in the catalog.
    ListTypes,
    /// `describe <Type>` / `schema <Type>` — introspection: the columns and
    /// indexes of one type.
    Describe(String),
}

/// `alter User add column status: str` / `alter User drop column status`
#[derive(Debug, Clone, PartialEq)]
pub struct AlterTableExpr {
    pub table: String,
    pub action: AlterAction,
}

/// A persisted index target. Stored JSON paths are table-local and therefore
/// never retain a query alias or runtime catalog/index identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IndexTarget {
    Column(String),
    JsonPath(powdb_storage::stored_json_path::StoredJsonPathV1),
}

/// An individual ALTER TABLE action.
#[derive(Debug, Clone, PartialEq)]
pub enum AlterAction {
    AddColumn {
        name: String,
        type_name: String,
        required: bool,
    },
    DropColumn {
        name: String,
        /// `drop column if exists` — a missing column is a no-op instead of
        /// an error.
        if_exists: bool,
    },
    /// `alter <Table> add index [if not exists] .<column>` — creates a
    /// B+Tree index on `column`. No-op if the index already exists.
    AddIndex {
        target: IndexTarget,
        /// Parsed for symmetry with the other DDL; `add index` is already
        /// idempotent, so this does not change behavior.
        if_not_exists: bool,
    },
    /// `alter <Table> add unique [if not exists] .<column>` — creates a
    /// UNIQUE B+Tree index on `column`. Scans existing data first and fails
    /// if any duplicate (non-null) value is present. Without `if not exists`
    /// it errors when the column is already indexed (no in-place upgrade);
    /// with it, an existing index is a no-op.
    AddUnique {
        target: IndexTarget,
        if_not_exists: bool,
    },
    /// `alter <Table> drop index [if exists] <target>` — removes either a
    /// stored-column index or an expression index with the exact path identity.
    DropIndex {
        target: IndexTarget,
        if_exists: bool,
    },
}

/// `drop [if exists] User`
#[derive(Debug, Clone, PartialEq)]
pub struct DropTableExpr {
    pub table: String,
    /// `drop if exists` — a missing table is a no-op instead of an error.
    pub if_exists: bool,
}

/// `create [materialized] view ActiveUsers as User filter .active = true`
#[derive(Debug, Clone, PartialEq)]
pub struct CreateViewExpr {
    pub name: String,
    pub query: QueryExpr,
    /// The original source query text, stored for re-execution on refresh.
    pub query_text: String,
}

/// `refresh ActiveUsers`
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshViewExpr {
    pub name: String,
}

/// `drop view [if exists] ActiveUsers`
#[derive(Debug, Clone, PartialEq)]
pub struct DropViewExpr {
    pub name: String,
    /// `drop view if exists` — a missing view is a no-op instead of an error.
    pub if_exists: bool,
}

/// `User filter .age > 30 union User filter .status = "vip"`
#[derive(Debug, Clone, PartialEq)]
pub struct UnionExpr {
    pub left: Box<Statement>,
    pub right: Box<Statement>,
    /// `true` for `union all` (keep duplicates), `false` for `union` (deduplicate).
    pub all: bool,
}

/// A query expression: Type [join ...]* [filter ...] [order ...] [limit ...] [{ projection }]
#[derive(Debug, Clone, PartialEq)]
pub struct QueryExpr {
    pub source: String,
    /// Optional alias for the primary source (e.g. `User as u`). Used to
    /// disambiguate qualified column references in join queries. `None` for
    /// single-table queries.
    pub alias: Option<String>,
    /// Zero or more join clauses chained to the primary source. For a
    /// single-table query this is always empty so existing code paths are
    /// untouched.
    pub joins: Vec<JoinClause>,
    pub filter: Option<Expr>,
    pub order: Option<OrderClause>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    pub projection: Option<Vec<ProjectionField>>,
    pub aggregation: Option<AggregateExpr>,
    pub distinct: bool,
    pub group_by: Option<GroupByClause>,
}

/// GROUP BY clause: `group .field1, alias.field2 [having <expr>]`.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupByClause {
    pub keys: Vec<GroupKey>,
    pub having: Option<Expr>,
}

/// A single expression-valued GROUP BY key.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupKey {
    pub expr: Expr,
    pub output_name: String,
}

impl GroupKey {
    /// Name of the output column this key produces. Unqualified keys keep
    /// their bare field name; qualified keys are emitted as `alias.field` so
    /// HAVING and downstream projections can reference them consistently.
    pub fn output_name(&self) -> String {
        self.output_name.clone()
    }
}

/// A join clause appended to a query's primary source.
///
/// Example syntax (Mission E1.1 parser accepts this; executor still errors):
///   `User as u inner join Order as o on u.id = o.user_id filter o.total > 100`
#[derive(Debug, Clone, PartialEq)]
pub struct JoinClause {
    pub kind: JoinKind,
    pub source: String,
    pub alias: Option<String>,
    /// `on <expr>` — required for every kind except `Cross`.
    pub on: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    LeftOuter,
    RightOuter,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionField {
    pub alias: Option<String>,
    pub expr: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderClause {
    pub keys: Vec<OrderKey>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderKey {
    pub expr: Expr,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertExpr {
    pub target: String,
    /// One assignment-block per row. Always contains at least one row;
    /// `insert T { .. }` yields one, `insert T { .. }, { .. }` yields many.
    pub rows: Vec<Vec<Assignment>>,
    /// `true` when the statement ends with `returning` — the executor returns
    /// the inserted rows (all columns) instead of a modified-count.
    pub returning: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateExpr {
    pub source: String,
    pub filter: Option<Expr>,
    pub assignments: Vec<Assignment>,
    /// `true` when the statement ends with `returning` — the executor returns
    /// the post-update rows (all columns) instead of a modified-count.
    pub returning: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteExpr {
    pub source: String,
    pub filter: Option<Expr>,
    /// `true` when the statement ends with `returning` — the executor returns
    /// the pre-delete rows (all columns) instead of a modified-count.
    pub returning: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub field: String,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
/// `upsert User on .id { id := 1, name := "Alice" } [on conflict { name := "Alice" }]`
pub struct UpsertExpr {
    pub target: String,
    pub key_column: String,
    pub assignments: Vec<Assignment>,
    /// Assignments to apply on conflict. If empty, all non-key assignments
    /// from `assignments` are used as the update set.
    pub on_conflict: Vec<Assignment>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTypeExpr {
    pub name: String,
    pub fields: Vec<FieldDef>,
    /// `type X if not exists { ... }` — re-declaring an existing type is a
    /// no-op instead of an error.
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    pub name: String,
    pub type_name: String,
    pub required: bool,
    /// `true` when declared with the `unique` modifier — auto-creates a
    /// unique B+Tree index on this column at table-create time.
    pub unique: bool,
    /// Literal default applied when an insert omits this column. `None` means
    /// the column has no default (omitting it yields the empty set / null).
    pub default: Option<Literal>,
    /// `true` when declared `auto` — an integer column whose value is assigned
    /// from a monotonic per-table sequence when an insert omits it.
    pub auto: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AggregateExpr {
    pub function: AggFunc,
    pub argument: Option<Expr>,
    pub mode: AggregateMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AggregateMode {
    Symmetric,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AggFunc {
    Count,
    CountDistinct,
    Avg,
    Sum,
    Min,
    Max,
}

/// Window function identifier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowFunc {
    RowNumber,
    Rank,
    DenseRank,
    Sum,
    Avg,
    Count,
    Min,
    Max,
}

/// Scalar (non-aggregate) function — operates on single values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScalarFn {
    Upper,
    Lower,
    Length,
    Trim,
    Substring, // substring(expr, start, len) — 1-indexed
    Concat,    // concat(expr, expr, ...) — variadic
    // Math
    Abs,
    Round, // round(expr) or round(expr, decimals)
    Ceil,
    Floor,
    Sqrt,
    Pow, // pow(base, exponent)
    // Date/time
    Now,      // now() — returns current unix timestamp in microseconds
    Extract,  // extract("year"|"month"|..., datetime_expr)
    DateAdd,  // date_add(datetime_expr, amount, "unit")
    DateDiff, // date_diff(dt1, dt2, "unit")
    // JSON
    JsonType, // json_type(expr) — 'null'|'string'|'number'|'bool'|'object'|'array', Empty when missing
    JsonText, // json_text(expr) — SQL ->> text, canonical JSON for object/array
}

/// Target type for CAST expressions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CastType {
    Int,
    Float,
    Str,
    Bool,
    DateTime,
    Uuid,
    Bytes,
}

/// A single step in a JSON `->` path expression.
///
/// This is the query-crate's OWNED path-segment type (distinct from
/// [`powdb_storage::pj1::PathSeg`], which borrows `&str` keys). Segments are
/// STRUCTURAL: they are part of the query shape, never literal slots, so the
/// plan cache hashes them into the canonical token stream and neither counts
/// nor substitutes them (see `plan_cache::count_expr` / `substitute_expr`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathSeg {
    /// Object member access: `->author` or `->"weird key!"`.
    Key(String),
    /// Array element access: `->0`.
    Index(u32),
}

/// Expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Field(String),
    /// A table-qualified field reference: `table.field` or `alias.field`.
    /// Used by join queries to disambiguate columns that appear in multiple
    /// sources. The single-table read path never emits this variant, so
    /// existing fast paths keep matching `Expr::Field` unchanged.
    QualifiedField {
        qualifier: String,
        field: String,
    },
    Literal(Literal),
    Param(String),
    BinaryOp(Box<Expr>, BinOp, Box<Expr>),
    UnaryOp(UnaryOp, Box<Expr>),
    FunctionCall(AggFunc, Box<Expr>, AggregateMode),
    /// Scalar (non-aggregate) function call.
    ScalarFunc(ScalarFn, Vec<Expr>),
    Coalesce(Box<Expr>, Box<Expr>),
    /// `expr in (val1, val2, ...)` or `expr not in (val1, val2, ...)`
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `expr [not] in (subquery)` — the subquery is a full QueryExpr
    /// that produces a single column.
    InSubquery {
        expr: Box<Expr>,
        subquery: Box<QueryExpr>,
        negated: bool,
    },
    /// `[not] exists (subquery)` — the subquery is a full QueryExpr.
    /// Currently uncorrelated only: the executor runs the subquery once
    /// before the scan loop and replaces this node with a Bool literal.
    ExistsSubquery {
        subquery: Box<QueryExpr>,
        negated: bool,
    },
    /// CASE WHEN ... THEN ... [ELSE ...] END
    Case {
        whens: Vec<(Box<Expr>, Box<Expr>)>,
        else_expr: Option<Box<Expr>>,
    },
    /// Window function: `func(args) over (partition ... order ...)`
    Window {
        function: WindowFunc,
        args: Vec<Expr>,
        mode: AggregateMode,
        partition_by: Vec<Expr>,
        order_by: Vec<OrderKey>,
    },
    /// Type cast: `cast(expr, "int")` or `cast(expr, "str")` etc.
    Cast(Box<Expr>, CastType),
    /// A runtime-materialized literal carrying a concrete Value. Produced only
    /// during subquery/correlated substitution (post-planning) for values that
    /// have no Literal form (NULL, datetime, uuid, bytes); never emitted by the
    /// parser/canonicalizer.
    ValueLit(Value),
    /// The `null` literal — produces `Value::Empty`.
    Null,
    /// A JSON path access: `base->seg->seg...`. `base` is restricted at parse
    /// time to `Field`, `QualifiedField`, or (nested) `JsonPath`. Evaluating it
    /// walks the base `Value::Json` document and scalarizes the addressed node
    /// (see `eval_expr`); the `segments` are structural (see [`PathSeg`]).
    JsonPath {
        base: Box<Expr>,
        segments: Vec<PathSeg>,
    },
}

/// Versioned structural identity for a query JSON path. Unlike the stored
/// table-local form, this retains a qualified root until binding resolves it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JsonPathIdentityV1 {
    pub root: JsonPathRootV1,
    pub segments: Vec<PathSeg>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JsonPathRootV1 {
    Unqualified(String),
    Qualified { qualifier: String, field: String },
}

impl JsonPathIdentityV1 {
    pub const VERSION: u8 = 1;

    pub fn from_expr(expr: &Expr) -> Option<Self> {
        let Expr::JsonPath { base, segments } = expr else {
            return None;
        };
        let root = match base.as_ref() {
            Expr::Field(field) => JsonPathRootV1::Unqualified(field.clone()),
            Expr::QualifiedField { qualifier, field } => JsonPathRootV1::Qualified {
                qualifier: qualifier.clone(),
                field: field.clone(),
            },
            _ => return None,
        };
        Some(Self {
            root,
            segments: segments.clone(),
        })
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = vec![Self::VERSION];
        match &self.root {
            JsonPathRootV1::Unqualified(field) => {
                out.push(1);
                push_identity_str(&mut out, field);
            }
            JsonPathRootV1::Qualified { qualifier, field } => {
                out.push(2);
                push_identity_str(&mut out, qualifier);
                push_identity_str(&mut out, field);
            }
        }
        out.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        for segment in &self.segments {
            match segment {
                PathSeg::Key(key) => {
                    out.push(1);
                    push_identity_str(&mut out, key);
                }
                PathSeg::Index(index) => {
                    out.push(2);
                    out.extend_from_slice(&index.to_le_bytes());
                }
            }
        }
        out
    }

    pub fn canonical_text(&self) -> String {
        let mut out = match &self.root {
            JsonPathRootV1::Unqualified(field) => format!("v1:.{field}"),
            JsonPathRootV1::Qualified { qualifier, field } => {
                format!("v1:{qualifier}.{field}")
            }
        };
        for segment in &self.segments {
            match segment {
                PathSeg::Key(key) => {
                    out.push_str("->\"");
                    push_identity_escaped(&mut out, key);
                    out.push('"');
                }
                PathSeg::Index(index) => {
                    out.push_str("->");
                    out.push_str(&index.to_string());
                }
            }
        }
        out
    }

    /// Bind an unqualified root, or a qualified root matching `qualifier`, to
    /// the storage-owned table-local identity used by future expression-index
    /// catalog entries.
    pub fn bind_table_local(
        &self,
        qualifier: Option<&str>,
    ) -> Option<powdb_storage::stored_json_path::StoredJsonPathV1> {
        use powdb_storage::stored_json_path::{StoredJsonPathSegmentV1, StoredJsonPathV1};
        let column = match &self.root {
            JsonPathRootV1::Unqualified(field) => field.clone(),
            JsonPathRootV1::Qualified {
                qualifier: actual,
                field,
            } if qualifier == Some(actual.as_str()) => field.clone(),
            JsonPathRootV1::Qualified { .. } => return None,
        };
        Some(StoredJsonPathV1::new(
            column,
            self.segments
                .iter()
                .map(|segment| match segment {
                    PathSeg::Key(key) => StoredJsonPathSegmentV1::Key(key.clone()),
                    PathSeg::Index(index) => StoredJsonPathSegmentV1::Index(*index),
                })
                .collect(),
        ))
    }
}

pub fn expression_output_name(expr: &Expr) -> String {
    match expr {
        Expr::Field(field) => field.clone(),
        Expr::QualifiedField { qualifier, field } => format!("{qualifier}.{field}"),
        Expr::JsonPath { .. } => JsonPathIdentityV1::from_expr(expr)
            .map(|path| path.canonical_text())
            .unwrap_or_else(|| "?".into()),
        _ => "?".into(),
    }
}

fn push_identity_str(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn push_identity_escaped(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c <= '\u{1f}' => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Float(f64),
    String(String),
    Bool(bool),
}

/// A bound value supplied for a `$N` placeholder in
/// [`crate::parser::parse_with_params`].
///
/// Unlike [`Literal`], this carries a `Null` variant so a parameter can
/// bind PowQL `null` (substituted as `Token::Null`, not a string). Values
/// are turned into literal *tokens* before parsing, so an injection-shaped
/// string is inert data — it can never change the query's shape.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Eq,
    Neq,
    Lt,
    Gt,
    Lte,
    Gte,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    Like,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnaryOp {
    Not,
    Exists,
    NotExists,
    IsNull,
    IsNotNull,
}

#[cfg(test)]
mod json_path_identity_tests {
    use super::*;

    #[test]
    fn canonical_path_identity_goldens() {
        let unquoted = JsonPathIdentityV1 {
            root: JsonPathRootV1::Unqualified("data".into()),
            segments: vec![PathSeg::Key("author".into())],
        };
        let quoted = JsonPathIdentityV1 {
            root: JsonPathRootV1::Unqualified("data".into()),
            segments: vec![PathSeg::Key("author".into())],
        };
        assert_eq!(unquoted, quoted);
        assert_eq!(unquoted.canonical_text(), "v1:.data->\"author\"");
        assert_eq!(
            unquoted.canonical_bytes(),
            vec![
                1, 1, 4, 0, 0, 0, b'd', b'a', b't', b'a', 1, 0, 0, 0, 1, 6, 0, 0, 0, b'a', b'u',
                b't', b'h', b'o', b'r',
            ]
        );

        let key_zero = JsonPathIdentityV1 {
            root: JsonPathRootV1::Unqualified("data".into()),
            segments: vec![PathSeg::Key("0".into())],
        };
        let index_zero = JsonPathIdentityV1 {
            root: JsonPathRootV1::Unqualified("data".into()),
            segments: vec![PathSeg::Index(0)],
        };
        assert_ne!(key_zero.canonical_bytes(), index_zero.canonical_bytes());

        let unicode = JsonPathIdentityV1 {
            root: JsonPathRootV1::Unqualified("data".into()),
            segments: vec![PathSeg::Key("café\n\"x\\y".into())],
        };
        assert_eq!(unicode.canonical_text(), "v1:.data->\"café\\n\\\"x\\\\y\"");
    }

    #[test]
    fn qualified_root_binding_is_explicit() {
        let qualified = JsonPathIdentityV1 {
            root: JsonPathRootV1::Qualified {
                qualifier: "p".into(),
                field: "data".into(),
            },
            segments: vec![PathSeg::Key("age".into())],
        };
        assert!(qualified.bind_table_local(Some("other")).is_none());
        let stored = qualified
            .bind_table_local(Some("p"))
            .expect("matching root");
        assert_eq!(stored.canonical_text(), "v1:.data->\"age\"");
    }
}
