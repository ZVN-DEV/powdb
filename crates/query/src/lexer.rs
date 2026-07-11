use crate::token::Token;

/// Lowercase words recognized by the PowQL lexer as keywords or built-in
/// literal words. CLI completion imports this list so it cannot drift from
/// the parser surface.
pub const POWQL_KEYWORDS: &[&str] = &[
    "abs",
    "add",
    "alter",
    "and",
    "as",
    "asc",
    "auto",
    "avg",
    "begin",
    "between",
    "case",
    "cast",
    "ceil",
    "column",
    "commit",
    "concat",
    "conflict",
    "count",
    "cross",
    "date_add",
    "date_diff",
    "default",
    "delete",
    "dense_rank",
    "desc",
    "distinct",
    "drop",
    "else",
    "end",
    "exists",
    "explain",
    "extract",
    "false",
    "filter",
    "floor",
    "group",
    "having",
    "in",
    "index",
    "inner",
    "insert",
    "is",
    "join",
    "left",
    "length",
    "let",
    "like",
    "limit",
    "link",
    "lower",
    "match",
    "materialize",
    "materialized",
    "max",
    "min",
    "multi",
    "not",
    "now",
    "null",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "over",
    "partition",
    "pow",
    "rank",
    "refresh",
    "required",
    "returning",
    "right",
    "rollback",
    "round",
    "row_number",
    "select",
    "sqrt",
    "substring",
    "sum",
    "then",
    "transaction",
    "trim",
    "true",
    "type",
    "union",
    "unique",
    "update",
    "upper",
    "upsert",
    "view",
    "when",
];

/// Maximum allowed length for a string literal (16 MB).
/// Prevents unbounded memory consumption from queries with multi-gigabyte strings.
const MAX_STRING_LITERAL: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub struct LexError {
    pub message: String,
    pub position: usize,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "at position {}: {}", self.position, self.message)
    }
}

impl std::error::Error for LexError {}

/// Tokenize a PowQL input string into a stream of tokens.
///
/// # Examples
///
/// ```
/// use powdb_query::lexer::lex;
/// use powdb_query::token::Token;
///
/// let tokens = lex("User filter .age > 30").unwrap();
/// assert_eq!(tokens[0], Token::Ident("User".to_string()));
/// assert_eq!(tokens[1], Token::Filter);
/// assert_eq!(tokens[2], Token::DotIdent("age".to_string()));
/// ```
pub fn lex(input: &str) -> Result<Vec<Token>, LexError> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0;

    while pos < chars.len() {
        // Skip whitespace
        if chars[pos].is_whitespace() {
            pos += 1;
            continue;
        }

        // Skip comments
        if chars[pos] == '#' {
            while pos < chars.len() && chars[pos] != '\n' {
                pos += 1;
            }
            continue;
        }

        // Dot-ident: .fieldname
        if chars[pos] == '.'
            && pos + 1 < chars.len()
            && (chars[pos + 1].is_alphabetic() || chars[pos + 1] == '_')
        {
            pos += 1; // skip dot
            let start = pos;
            while pos < chars.len() && (chars[pos].is_alphanumeric() || chars[pos] == '_') {
                pos += 1;
            }
            let name: String = chars[start..pos].iter().collect();
            tokens.push(Token::DotIdent(name));
            continue;
        }

        // Param: $name
        if chars[pos] == '$' {
            pos += 1;
            let start = pos;
            while pos < chars.len() && (chars[pos].is_alphanumeric() || chars[pos] == '_') {
                pos += 1;
            }
            let name: String = chars[start..pos].iter().collect();
            tokens.push(Token::Param(name));
            continue;
        }

        // String literal
        if chars[pos] == '"' {
            pos += 1;
            let mut s = String::new();
            while pos < chars.len() && chars[pos] != '"' {
                if chars[pos] == '\\' && pos + 1 < chars.len() {
                    match chars[pos + 1] {
                        '"' => {
                            s.push('"');
                            pos += 2;
                        }
                        '\\' => {
                            s.push('\\');
                            pos += 2;
                        }
                        'n' => {
                            s.push('\n');
                            pos += 2;
                        }
                        't' => {
                            s.push('\t');
                            pos += 2;
                        }
                        _ => {
                            s.push(chars[pos + 1]);
                            pos += 2;
                        }
                    }
                } else {
                    s.push(chars[pos]);
                    pos += 1;
                }
            }
            if pos >= chars.len() {
                return Err(LexError {
                    message: "unterminated string".into(),
                    position: pos,
                });
            }
            pos += 1; // closing quote
            if s.len() > MAX_STRING_LITERAL {
                return Err(LexError {
                    message: format!(
                        "string literal exceeds maximum size of {}MB",
                        MAX_STRING_LITERAL / (1024 * 1024)
                    ),
                    position: pos,
                });
            }
            tokens.push(Token::StringLit(s));
            continue;
        }

        // Number (int or float)
        if chars[pos].is_ascii_digit()
            || (chars[pos] == '-' && pos + 1 < chars.len() && chars[pos + 1].is_ascii_digit())
        {
            let start = pos;
            if chars[pos] == '-' {
                pos += 1;
            }
            while pos < chars.len() && chars[pos].is_ascii_digit() {
                pos += 1;
            }
            if pos < chars.len()
                && chars[pos] == '.'
                && pos + 1 < chars.len()
                && chars[pos + 1].is_ascii_digit()
            {
                pos += 1;
                while pos < chars.len() && chars[pos].is_ascii_digit() {
                    pos += 1;
                }
                let s: String = chars[start..pos].iter().collect();
                let value = s.parse::<f64>().map_err(|_| LexError {
                    message: format!("float literal out of range: {s}"),
                    position: start,
                })?;
                tokens.push(Token::FloatLit(value));
            } else {
                let s: String = chars[start..pos].iter().collect();
                let value = s.parse::<i64>().map_err(|_| LexError {
                    message: format!("integer literal out of range for i64: {s}"),
                    position: start,
                })?;
                tokens.push(Token::IntLit(value));
            }
            continue;
        }

        // Identifiers and keywords
        if chars[pos].is_alphabetic() || chars[pos] == '_' {
            let start = pos;
            while pos < chars.len() && (chars[pos].is_alphanumeric() || chars[pos] == '_') {
                pos += 1;
            }
            let word: String = chars[start..pos].iter().collect();
            let token = match word.as_str() {
                "type" => Token::Type,
                "filter" => Token::Filter,
                "order" => Token::Order,
                "limit" => Token::Limit,
                "offset" => Token::Offset,
                "insert" => Token::Insert,
                "update" => Token::Update,
                "delete" => Token::Delete,
                "default" => Token::Default,
                "upsert" => Token::Upsert,
                "returning" => Token::Returning,
                "conflict" => Token::Conflict,
                "select" => Token::Select,
                "required" => Token::Required,
                "multi" => Token::Multi,
                "link" => Token::Link,
                "index" => Token::Index,
                "unique" => Token::Unique,
                "on" => Token::On,
                "asc" => Token::Asc,
                "auto" => Token::Auto,
                "desc" => Token::Desc,
                "and" => Token::And,
                "or" => Token::Or,
                "not" => Token::Not,
                "exists" => Token::Exists,
                "let" => Token::Let,
                "as" => Token::As,
                "match" => Token::Match,
                "group" => Token::Group,
                "join" => Token::Join,
                "inner" => Token::Inner,
                "left" => Token::LeftKw,
                "right" => Token::RightKw,
                "outer" => Token::Outer,
                "cross" => Token::Cross,
                "transaction" => Token::Transaction,
                "begin" => Token::Begin,
                "commit" => Token::Commit,
                "rollback" => Token::Rollback,
                "view" => Token::View,
                "materialized" => Token::Materialized,
                "materialize" => Token::Materialized,
                "refresh" => Token::Refresh,
                "union" => Token::Union,
                "having" => Token::Having,
                "distinct" => Token::Distinct,
                "in" => Token::In,
                "between" => Token::Between,
                "like" => Token::Like,
                "count" => Token::Count,
                "avg" => Token::Avg,
                "sum" => Token::Sum,
                "min" => Token::Min,
                "max" => Token::Max,
                "is" => Token::Is,
                "null" => Token::Null,
                "upper" => Token::Upper,
                "lower" => Token::Lower,
                "length" => Token::Length,
                "trim" => Token::Trim,
                "substring" => Token::Substring,
                "concat" => Token::Concat,
                "abs" => Token::Abs,
                "round" => Token::Round,
                "ceil" => Token::Ceil,
                "floor" => Token::Floor,
                "sqrt" => Token::Sqrt,
                "pow" => Token::Pow,
                "now" => Token::Now,
                "extract" => Token::Extract,
                "date_add" => Token::DateAdd,
                "date_diff" => Token::DateDiff,
                "cast" => Token::Cast,
                "case" => Token::Case,
                "when" => Token::When,
                "then" => Token::Then,
                "else" => Token::Else,
                "end" => Token::End,
                "over" => Token::Over,
                "partition" => Token::Partition,
                "row_number" => Token::RowNumber,
                "rank" => Token::Rank,
                "dense_rank" => Token::DenseRank,
                "alter" => Token::Alter,
                "drop" => Token::Drop,
                "add" => Token::Add,
                "column" => Token::Column,
                "explain" => Token::Explain,
                "true" => Token::BoolLit(true),
                "false" => Token::BoolLit(false),
                _ => Token::Ident(word),
            };
            tokens.push(token);
            continue;
        }

        // Two-char operators
        if pos + 1 < chars.len() {
            let two: String = chars[pos..pos + 2].iter().collect();
            match two.as_str() {
                ":=" => {
                    tokens.push(Token::Assign);
                    pos += 2;
                    continue;
                }
                "->" => {
                    tokens.push(Token::Arrow);
                    pos += 2;
                    continue;
                }
                "!=" => {
                    tokens.push(Token::Neq);
                    pos += 2;
                    continue;
                }
                "<=" => {
                    tokens.push(Token::Lte);
                    pos += 2;
                    continue;
                }
                ">=" => {
                    tokens.push(Token::Gte);
                    pos += 2;
                    continue;
                }
                "??" => {
                    tokens.push(Token::Coalesce);
                    pos += 2;
                    continue;
                }
                _ => {}
            }
        }

        // Single-char operators
        let token = match chars[pos] {
            '=' => Token::Eq,
            '<' => Token::Lt,
            '>' => Token::Gt,
            '|' => Token::Pipe,
            '+' => Token::Plus,
            '-' => Token::Minus,
            '*' => Token::Star,
            '/' => Token::Slash,
            '{' => Token::LBrace,
            '}' => Token::RBrace,
            '(' => Token::LParen,
            ')' => Token::RParen,
            ',' => Token::Comma,
            ':' => Token::Colon,
            '.' => Token::Dot,
            c => {
                return Err(LexError {
                    message: format!("unexpected character: {c}"),
                    position: pos,
                })
            }
        };
        tokens.push(token);
        pos += 1;
    }

    tokens.push(Token::Eof);
    Ok(tokens)
}

/// Split a PowQL source string into individual statements on top-level `;`,
/// mirroring the lexer's string- and comment-scanning rules so a `;` inside a
/// `"..."` literal or a `#` comment is never a boundary. Segments are trimmed
/// and empty ones dropped, so leading/trailing/doubled `;` and blank lines are
/// harmless.
///
/// Infallible by design: an unterminated string leaves its remainder as the
/// final segment, so the "unterminated string" error surfaces once, at
/// execution time, from [`lex`] — this function never reports it.
pub fn split_statements(input: &str) -> Vec<&str> {
    #[derive(PartialEq)]
    enum State {
        Normal,
        InString,
        InComment,
    }

    let mut out = Vec::new();
    let mut start = 0usize;
    let mut state = State::Normal;
    let mut chars = input.char_indices();

    while let Some((i, c)) = chars.next() {
        match state {
            State::Normal => match c {
                ';' => {
                    let seg = input[start..i].trim();
                    if !seg.is_empty() {
                        out.push(seg);
                    }
                    start = i + 1; // `;` is one byte
                }
                '"' => state = State::InString,
                '#' => state = State::InComment,
                _ => {}
            },
            State::InString => match c {
                // Mirror the lexer: a backslash consumes the next char
                // unconditionally, so `\"` and `\;` stay inside the string.
                '\\' => {
                    chars.next();
                }
                '"' => state = State::Normal,
                _ => {}
            },
            State::InComment => {
                if c == '\n' {
                    state = State::Normal;
                }
            }
        }
    }

    let seg = input[start..].trim();
    if !seg.is_empty() {
        out.push(seg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Token;

    #[test]
    fn test_lex_simple_query() {
        let tokens = lex("User filter .age > 30").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Ident("User".into()),
                Token::Filter,
                Token::DotIdent("age".into()),
                Token::Gt,
                Token::IntLit(30),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn test_lex_projection() {
        let tokens = lex("User { name, email }").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Ident("User".into()),
                Token::LBrace,
                Token::Ident("name".into()),
                Token::Comma,
                Token::Ident("email".into()),
                Token::RBrace,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn test_lex_insert() {
        let tokens = lex(r#"insert User { name := "Alice", age := 30 }"#).unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Insert,
                Token::Ident("User".into()),
                Token::LBrace,
                Token::Ident("name".into()),
                Token::Assign,
                Token::StringLit("Alice".into()),
                Token::Comma,
                Token::Ident("age".into()),
                Token::Assign,
                Token::IntLit(30),
                Token::RBrace,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn test_lex_params() {
        let tokens = lex("User filter .age > $min_age").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Ident("User".into()),
                Token::Filter,
                Token::DotIdent("age".into()),
                Token::Gt,
                Token::Param("min_age".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn test_lex_string_with_escapes() {
        let tokens = lex(r#""hello \"world\"""#).unwrap();
        assert_eq!(
            tokens,
            vec![Token::StringLit("hello \"world\"".into()), Token::Eof,]
        );
    }

    #[test]
    fn test_lex_aggregation() {
        let tokens = lex("count(User)").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Count,
                Token::LParen,
                Token::Ident("User".into()),
                Token::RParen,
                Token::Eof,
            ]
        );
    }

    /// Regression for issue #24: an integer literal with more digits than
    /// i64 can hold previously reached `s.parse::<i64>().unwrap()` and
    /// panicked. It must return a `LexError` instead.
    #[test]
    fn test_lex_intlit_overflow_returns_err() {
        // 22 digits — well past i64::MAX (19 digits).
        let err = lex("4444444441111111144444").expect_err("must error, not panic");
        assert!(
            err.message.contains("integer literal out of range"),
            "unexpected message: {}",
            err.message
        );
        assert_eq!(err.position, 0);
    }

    /// Same bug, reached via the exact fuzzer reproducer from the
    /// libFuzzer artifact attached to issue #24 (base64
    /// `YXMJCQkJCQkJCQkJCQkJNDQ0NDQ0NDQ0MTExMTExMTQ0NDQJCQkJCQk=`).
    #[test]
    fn test_lex_fuzz_repro_issue_24() {
        let input = "as\t\t\t\t\t\t\t\t\t\t\t\t\t44444444411111114444\t\t\t\t\t\t";
        let err = lex(input).expect_err("fuzz reproducer must now error, not panic");
        assert!(err.message.contains("integer literal"));
    }

    // ── split_statements (issue #150) ──────────────────────────────────

    #[test]
    fn test_split_top_level_semicolons() {
        assert_eq!(
            split_statements("insert A { a := 1 }; insert B { b := 2 }"),
            vec!["insert A { a := 1 }", "insert B { b := 2 }"]
        );
    }

    #[test]
    fn test_split_semicolon_in_string_not_split() {
        // The core #150 repro: a `;` inside a string literal must not split.
        assert_eq!(
            split_statements(r#"insert Note { body := "hello; world" }"#),
            vec![r#"insert Note { body := "hello; world" }"#]
        );
    }

    #[test]
    fn test_split_escaped_quote_then_semicolon() {
        // `\"` keeps us inside the string, so the following `;` does not
        // split; the string closes at the final unescaped `"`, then the
        // top-level `;` splits.
        let input = r#"insert A { v := "a\"; b" }; insert B { c := 1 }"#;
        assert_eq!(
            split_statements(input),
            vec![r#"insert A { v := "a\"; b" }"#, "insert B { c := 1 }"]
        );
    }

    #[test]
    fn test_split_backslash_consumes_any_char() {
        // `"\\"` is a single-backslash string (the `\` escapes the `\`); the
        // string closes at the second `"`, so the trailing `;` splits.
        let input = r#"insert A { v := "\\" }; x"#;
        assert_eq!(
            split_statements(input),
            vec![r#"insert A { v := "\\" }"#, "x"]
        );
    }

    #[test]
    fn test_split_semicolon_in_comment_not_split() {
        let input = "insert A { a := 1 } # trailing; comment\n; insert B { b := 2 }";
        assert_eq!(
            split_statements(input),
            vec![
                "insert A { a := 1 } # trailing; comment",
                "insert B { b := 2 }"
            ]
        );
    }

    #[test]
    fn test_split_drops_empty_segments() {
        // Leading, doubled, and trailing `;` plus blank lines all drop.
        assert_eq!(split_statements("; A ;; B ;\n\n"), vec!["A", "B"]);
    }

    #[test]
    fn test_split_no_semicolon_backcompat() {
        // Byte-identical to the old `split(';').map(trim).filter(non-empty)`
        // single-statement behavior.
        assert_eq!(split_statements("count(User)"), vec!["count(User)"]);
        assert!(split_statements("   ").is_empty());
    }

    #[test]
    fn test_split_unterminated_string_tail() {
        // Never errors: the unterminated string becomes the final segment.
        let input = r#"insert A { a := 1 }; insert B { b := "oops"#;
        assert_eq!(
            split_statements(input),
            vec!["insert A { a := 1 }", r#"insert B { b := "oops"#]
        );
    }

    #[test]
    fn test_split_multiline_string_with_semicolon() {
        let input = "insert A { body := \"line1;\nline2\" }; insert B { b := 2 }";
        assert_eq!(
            split_statements(input),
            vec![
                "insert A { body := \"line1;\nline2\" }",
                "insert B { b := 2 }"
            ]
        );
    }
}
