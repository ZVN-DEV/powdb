#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Identifiers and literals
    Ident(String),     // User, name, email
    DotIdent(String),  // .name, .age (field access)
    IntLit(i64),       // 42
    FloatLit(f64),     // 3.14
    StringLit(String), // "hello"
    BoolLit(bool),     // true, false
    Param(String),     // $age, $name (query parameter)

    // Keywords
    Type,         // type
    Filter,       // filter
    Order,        // order
    Limit,        // limit
    Offset,       // offset
    Insert,       // insert
    Update,       // update
    Delete,       // delete
    Upsert,       // upsert
    Returning,    // returning
    Select,       // select (alias for projection)
    Required,     // required
    Default,      // default (column default value)
    Multi,        // multi
    Link,         // link
    Index,        // index
    Unique,       // unique
    On,           // on
    Conflict,     // conflict
    Asc,          // asc
    Desc,         // desc
    And,          // and
    Or,           // or
    Not,          // not
    Exists,       // exists
    Let,          // let
    As,           // as
    Match,        // match
    Group,        // group
    Join,         // join
    Inner,        // inner
    LeftKw,       // left  (keyword — avoids clashing with ast::JoinKind::LeftOuter naming)
    RightKw,      // right
    Outer,        // outer
    Cross,        // cross
    Transaction,  // transaction
    Begin,        // begin
    Commit,       // commit
    Rollback,     // rollback
    View,         // view
    Materialized, // materialized
    Refresh,      // refresh
    Union,        // union
    Having,       // having
    Distinct,     // distinct
    In,           // in
    Between,      // between
    Like,         // like
    Count,        // count
    Avg,          // avg
    Sum,          // sum
    Min,          // min
    Max,          // max
    Is,           // is
    Null,         // null

    // String functions
    Upper,     // upper
    Lower,     // lower
    Length,    // length
    Trim,      // trim
    Substring, // substring
    Concat,    // concat

    // Math functions
    Abs,   // abs
    Round, // round
    Ceil,  // ceil
    Floor, // floor
    Sqrt,  // sqrt
    Pow,   // pow

    // Date/time functions
    Now,      // now
    Extract,  // extract
    DateAdd,  // date_add
    DateDiff, // date_diff

    // Type conversion
    Cast, // cast

    // CASE WHEN
    Case, // case
    When, // when
    Then, // then
    Else, // else
    End,  // end

    // Window functions
    Over,      // over
    Partition, // partition
    RowNumber, // row_number
    Rank,      // rank
    DenseRank, // dense_rank

    // DDL
    Alter,   // alter
    Drop,    // drop
    Add,     // add
    Column,  // column
    Explain, // explain

    // Operators
    Eq,       // =
    Neq,      // !=
    Lt,       // <
    Gt,       // >
    Lte,      // <=
    Gte,      // >=
    Assign,   // :=
    Arrow,    // ->
    Pipe,     // |
    Coalesce, // ??
    Plus,     // +
    Minus,    // -
    Star,     // *
    Slash,    // /

    // Delimiters
    LBrace, // {
    RBrace, // }
    LParen, // (
    RParen, // )
    Comma,  // ,
    Colon,  // :
    Dot,    // .

    // Special
    Eof,
}

impl Token {
    /// Human-readable name for error messages. Avoids exposing raw Rust Debug
    /// format like `IntLit(42)` to end users.
    pub fn display_name(&self) -> String {
        match self {
            // Literals
            Token::Ident(s) => format!("identifier '{s}'"),
            Token::DotIdent(s) => format!("field '.{s}'"),
            Token::IntLit(v) => format!("number {v}"),
            Token::FloatLit(v) => format!("decimal number {v}"),
            Token::StringLit(s) => {
                let preview = if s.len() > 20 {
                    let end = s.floor_char_boundary(20);
                    format!("{}...", &s[..end])
                } else {
                    s.clone()
                };
                format!("string \"{preview}\"")
            }
            Token::BoolLit(v) => format!("{v}"),
            Token::Param(s) => format!("parameter '${s}'"),

            // Keywords
            Token::Type => "'type'".into(),
            Token::Filter => "'filter'".into(),
            Token::Order => "'order'".into(),
            Token::Limit => "'limit'".into(),
            Token::Offset => "'offset'".into(),
            Token::Insert => "'insert'".into(),
            Token::Update => "'update'".into(),
            Token::Delete => "'delete'".into(),
            Token::Upsert => "'upsert'".into(),
            Token::Returning => "'returning'".into(),
            Token::Select => "'select'".into(),
            Token::Required => "'required'".into(),
            Token::Default => "'default'".into(),
            Token::Multi => "'multi'".into(),
            Token::Link => "'link'".into(),
            Token::Index => "'index'".into(),
            Token::Unique => "'unique'".into(),
            Token::On => "'on'".into(),
            Token::Conflict => "'conflict'".into(),
            Token::Asc => "'asc'".into(),
            Token::Desc => "'desc'".into(),
            Token::And => "'and'".into(),
            Token::Or => "'or'".into(),
            Token::Not => "'not'".into(),
            Token::Exists => "'exists'".into(),
            Token::Let => "'let'".into(),
            Token::As => "'as'".into(),
            Token::Match => "'match'".into(),
            Token::Group => "'group'".into(),
            Token::Join => "'join'".into(),
            Token::Inner => "'inner'".into(),
            Token::LeftKw => "'left'".into(),
            Token::RightKw => "'right'".into(),
            Token::Outer => "'outer'".into(),
            Token::Cross => "'cross'".into(),
            Token::Transaction => "'transaction'".into(),
            Token::Begin => "'begin'".into(),
            Token::Commit => "'commit'".into(),
            Token::Rollback => "'rollback'".into(),
            Token::View => "'view'".into(),
            Token::Materialized => "'materialized'".into(),
            Token::Refresh => "'refresh'".into(),
            Token::Union => "'union'".into(),
            Token::Having => "'having'".into(),
            Token::Distinct => "'distinct'".into(),
            Token::In => "'in'".into(),
            Token::Between => "'between'".into(),
            Token::Like => "'like'".into(),
            Token::Count => "'count'".into(),
            Token::Avg => "'avg'".into(),
            Token::Sum => "'sum'".into(),
            Token::Min => "'min'".into(),
            Token::Max => "'max'".into(),
            Token::Is => "'is'".into(),
            Token::Null => "'null'".into(),

            // Functions
            Token::Upper => "'upper'".into(),
            Token::Lower => "'lower'".into(),
            Token::Length => "'length'".into(),
            Token::Trim => "'trim'".into(),
            Token::Substring => "'substring'".into(),
            Token::Concat => "'concat'".into(),
            Token::Abs => "'abs'".into(),
            Token::Round => "'round'".into(),
            Token::Ceil => "'ceil'".into(),
            Token::Floor => "'floor'".into(),
            Token::Sqrt => "'sqrt'".into(),
            Token::Pow => "'pow'".into(),
            Token::Now => "'now'".into(),
            Token::Extract => "'extract'".into(),
            Token::DateAdd => "'date_add'".into(),
            Token::DateDiff => "'date_diff'".into(),
            Token::Cast => "'cast'".into(),
            Token::Case => "'case'".into(),
            Token::When => "'when'".into(),
            Token::Then => "'then'".into(),
            Token::Else => "'else'".into(),
            Token::End => "'end'".into(),

            // Window
            Token::Over => "'over'".into(),
            Token::Partition => "'partition'".into(),
            Token::RowNumber => "'row_number'".into(),
            Token::Rank => "'rank'".into(),
            Token::DenseRank => "'dense_rank'".into(),

            // DDL
            Token::Alter => "'alter'".into(),
            Token::Drop => "'drop'".into(),
            Token::Add => "'add'".into(),
            Token::Column => "'column'".into(),
            Token::Explain => "'explain'".into(),

            // Operators
            Token::Eq => "'='".into(),
            Token::Neq => "'!='".into(),
            Token::Lt => "'<'".into(),
            Token::Gt => "'>'".into(),
            Token::Lte => "'<='".into(),
            Token::Gte => "'>='".into(),
            Token::Assign => "':='".into(),
            Token::Arrow => "'->'".into(),
            Token::Pipe => "'|'".into(),
            Token::Coalesce => "'??'".into(),
            Token::Plus => "'+'".into(),
            Token::Minus => "'-'".into(),
            Token::Star => "'*'".into(),
            Token::Slash => "'/'".into(),

            // Delimiters
            Token::LBrace => "'{'".into(),
            Token::RBrace => "'}'".into(),
            Token::LParen => "'('".into(),
            Token::RParen => "')'".into(),
            Token::Comma => "','".into(),
            Token::Colon => "':'".into(),
            Token::Dot => "'.'".into(),

            Token::Eof => "end of input".into(),
        }
    }
}
