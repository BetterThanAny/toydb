//! SQL lexer.
//!
//! The lexer turns a `&str` into a stream of [`Spanned<Token>`]. SQL is
//! mostly ASCII, so we operate on bytes for the fast path and fall back
//! to `char` only inside string literals (which we still scan one byte at
//! a time — multi-byte UTF-8 sequences pass through unchanged).
//!
//! Quirks of toydb's flavour:
//! - Keywords are case-insensitive (`SELECT` == `select` == `Select`)
//! - Identifiers are case-sensitive
//! - `'foo'` is a string literal; `"col"` is a quoted identifier
//! - Doubled quote inside the same quote escapes (`'it''s'` → `it's`)
//! - `--` starts a line comment; `/* ... */` is a block comment (nests)
//! - Numeric literals are either integer or float (decimal `.`, scientific `e`)

use crate::error::{Error, Result};

/// All SQL tokens that the parser understands. Each variant carries
/// the original string slice (for identifiers / numbers / strings)
/// or is a unit variant (for keywords / punctuation).
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // -- literals --------------------------------------------------------
    /// Bare identifier or `"quoted identifier"`. Quoted form preserves case.
    Ident(String),
    /// Integer literal (always non-negative; minus sign is a separate op).
    Number(String),
    /// Single-quoted string. Doubled quotes inside the literal are
    /// already collapsed by the lexer.
    String(String),

    // -- keywords --------------------------------------------------------
    KwSelect,
    KwFrom,
    KwWhere,
    KwAnd,
    KwOr,
    KwNot,
    KwInsert,
    KwInto,
    KwValues,
    KwUpdate,
    KwSet,
    KwDelete,
    KwCreate,
    KwDrop,
    KwTable,
    KwIndex,
    KwIf,
    KwExists,
    KwPrimary,
    KwKey,
    KwUnique,
    KwNull,
    KwDefault,
    KwReferences,
    KwForeign,
    KwInteger,
    KwInt,
    KwBoolean,
    KwBool,
    KwFloat,
    KwDouble,
    KwText,
    KwString,
    KwVarchar,
    KwChar,
    KwTrue,
    KwFalse,
    KwAs,
    KwOrder,
    KwBy,
    KwAsc,
    KwDesc,
    KwLimit,
    KwOffset,
    KwGroup,
    KwHaving,
    KwJoin,
    KwInner,
    KwLeft,
    KwRight,
    KwOuter,
    KwOn,
    KwIs,
    KwIn,
    KwLike,
    KwBetween,
    KwBegin,
    KwCommit,
    KwRollback,
    KwTransaction,
    KwExplain,

    // -- punctuation -----------------------------------------------------
    Comma,
    Semicolon,
    LParen,
    RParen,
    Dot,
    Star,

    // -- operators -------------------------------------------------------
    Plus,
    Minus,
    Slash,
    Percent,
    Caret,
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    Concat, // ||
}

impl Token {
    /// Human-readable rendering for error messages.
    pub fn as_str(&self) -> String {
        match self {
            Token::Ident(s) => format!("identifier `{}`", s),
            Token::Number(s) => format!("number `{}`", s),
            Token::String(s) => format!("string `{}`", s),
            other => format!("{:?}", other),
        }
    }
}

/// A token plus its source position (1-based line and column of the *first*
/// character of the token). Used by the parser for error messages.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    pub token: Token,
    pub line: usize,
    pub col: usize,
}

impl Spanned {
    pub fn new(token: Token, line: usize, col: usize) -> Self {
        Self { token, line, col }
    }
}

// Map an upper-cased keyword to its [`Token`]. We keep this as a static
// list so adding a keyword is one line — `match` arms compile to a
// jump-table anyway.
fn keyword(upper: &str) -> Option<Token> {
    use Token::*;
    Some(match upper {
        "SELECT" => KwSelect,
        "FROM" => KwFrom,
        "WHERE" => KwWhere,
        "AND" => KwAnd,
        "OR" => KwOr,
        "NOT" => KwNot,
        "INSERT" => KwInsert,
        "INTO" => KwInto,
        "VALUES" => KwValues,
        "UPDATE" => KwUpdate,
        "SET" => KwSet,
        "DELETE" => KwDelete,
        "CREATE" => KwCreate,
        "DROP" => KwDrop,
        "TABLE" => KwTable,
        "INDEX" => KwIndex,
        "IF" => KwIf,
        "EXISTS" => KwExists,
        "PRIMARY" => KwPrimary,
        "KEY" => KwKey,
        "UNIQUE" => KwUnique,
        "NULL" => KwNull,
        "DEFAULT" => KwDefault,
        "REFERENCES" => KwReferences,
        "FOREIGN" => KwForeign,
        "INTEGER" => KwInteger,
        "INT" => KwInt,
        "BOOLEAN" => KwBoolean,
        "BOOL" => KwBool,
        "FLOAT" => KwFloat,
        "DOUBLE" => KwDouble,
        "TEXT" => KwText,
        "STRING" => KwString,
        "VARCHAR" => KwVarchar,
        "CHAR" => KwChar,
        "TRUE" => KwTrue,
        "FALSE" => KwFalse,
        "AS" => KwAs,
        "ORDER" => KwOrder,
        "BY" => KwBy,
        "ASC" => KwAsc,
        "DESC" => KwDesc,
        "LIMIT" => KwLimit,
        "OFFSET" => KwOffset,
        "GROUP" => KwGroup,
        "HAVING" => KwHaving,
        "JOIN" => KwJoin,
        "INNER" => KwInner,
        "LEFT" => KwLeft,
        "RIGHT" => KwRight,
        "OUTER" => KwOuter,
        "ON" => KwOn,
        "IS" => KwIs,
        "IN" => KwIn,
        "LIKE" => KwLike,
        "BETWEEN" => KwBetween,
        "BEGIN" => KwBegin,
        "COMMIT" => KwCommit,
        "ROLLBACK" => KwRollback,
        "TRANSACTION" => KwTransaction,
        "EXPLAIN" => KwExplain,
        _ => return None,
    })
}

/// Streaming SQL lexer over a `&str`. Built ad-hoc rather than via a
/// regex/state-machine framework so that error positions stay precise.
pub struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self { input: input.as_bytes(), pos: 0, line: 1, col: 1 }
    }

    /// Lex the whole input into a vector. Convenient for testing; the
    /// parser pulls tokens via [`Lexer::next_token`] one at a time.
    pub fn collect_all(input: &'a str) -> Result<Vec<Spanned>> {
        let mut lex = Self::new(input);
        let mut out = Vec::new();
        while let Some(s) = lex.next_token()? {
            out.push(s);
        }
        Ok(out)
    }

    /// Return the next [`Spanned`] token, or `Ok(None)` on EOF.
    pub fn next_token(&mut self) -> Result<Option<Spanned>> {
        self.skip_whitespace_and_comments()?;
        if self.pos >= self.input.len() {
            return Ok(None);
        }
        let line = self.line;
        let col = self.col;
        let b = self.input[self.pos];
        let token = match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.read_word()?,
            b'0'..=b'9' => self.read_number()?,
            b'\'' => self.read_string()?,
            b'"' => self.read_quoted_ident()?,
            b',' => self.bump_op(Token::Comma),
            b';' => self.bump_op(Token::Semicolon),
            b'(' => self.bump_op(Token::LParen),
            b')' => self.bump_op(Token::RParen),
            b'.' => {
                // `.5` is technically a numeric literal, but we require a
                // leading digit to keep the grammar simple — `.` is dot.
                self.bump_op(Token::Dot)
            }
            b'*' => self.bump_op(Token::Star),
            b'+' => self.bump_op(Token::Plus),
            b'-' => self.bump_op(Token::Minus),
            b'/' => self.bump_op(Token::Slash),
            b'%' => self.bump_op(Token::Percent),
            b'^' => self.bump_op(Token::Caret),
            b'=' => self.bump_op(Token::Eq),
            b'!' => {
                self.advance(1);
                if self.peek() == Some(b'=') {
                    self.advance(1);
                    Token::NotEq
                } else {
                    return Err(Error::lex(line, col, "unexpected `!` (did you mean `!=`?)"));
                }
            }
            b'<' => {
                self.advance(1);
                match self.peek() {
                    Some(b'=') => {
                        self.advance(1);
                        Token::LtEq
                    }
                    Some(b'>') => {
                        self.advance(1);
                        Token::NotEq
                    }
                    _ => Token::Lt,
                }
            }
            b'>' => {
                self.advance(1);
                if self.peek() == Some(b'=') {
                    self.advance(1);
                    Token::GtEq
                } else {
                    Token::Gt
                }
            }
            b'|' => {
                self.advance(1);
                if self.peek() == Some(b'|') {
                    self.advance(1);
                    Token::Concat
                } else {
                    return Err(Error::lex(line, col, "unexpected `|` (did you mean `||`?)"));
                }
            }
            other => {
                return Err(Error::lex(
                    line,
                    col,
                    format!("unexpected byte `{}` (0x{:02x})", other as char, other),
                ));
            }
        };
        Ok(Some(Spanned::new(token, line, col)))
    }

    // ------------------------------------------------------------------
    // helpers
    // ------------------------------------------------------------------

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.input.get(self.pos + off).copied()
    }

    fn advance(&mut self, n: usize) {
        for _ in 0..n {
            if let Some(&b) = self.input.get(self.pos) {
                self.pos += 1;
                if b == b'\n' {
                    self.line += 1;
                    self.col = 1;
                } else {
                    self.col += 1;
                }
            } else {
                return;
            }
        }
    }

    fn bump_op(&mut self, t: Token) -> Token {
        self.advance(1);
        t
    }

    fn skip_whitespace_and_comments(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') => self.advance(1),
                Some(b'-') if self.peek_at(1) == Some(b'-') => {
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.advance(1);
                    }
                }
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    let line = self.line;
                    let col = self.col;
                    self.advance(2);
                    let mut depth = 1usize;
                    while depth > 0 {
                        match (self.peek(), self.peek_at(1)) {
                            (Some(b'/'), Some(b'*')) => {
                                self.advance(2);
                                depth += 1;
                            }
                            (Some(b'*'), Some(b'/')) => {
                                self.advance(2);
                                depth -= 1;
                            }
                            (Some(_), _) => self.advance(1),
                            (None, _) => {
                                return Err(Error::lex(line, col, "unterminated /* ... */ comment"));
                            }
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn read_word(&mut self) -> Result<Token> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.advance(1);
            } else {
                break;
            }
        }
        // Safe: ASCII alnum / underscore is valid UTF-8.
        let raw = std::str::from_utf8(&self.input[start..self.pos]).expect("ASCII slice");
        let upper = raw.to_ascii_uppercase();
        Ok(match keyword(&upper) {
            Some(kw) => kw,
            None => Token::Ident(raw.to_string()),
        })
    }

    fn read_number(&mut self) -> Result<Token> {
        let start = self.pos;
        // integer part
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.advance(1);
        }
        // optional fractional
        if self.peek() == Some(b'.') && matches!(self.peek_at(1), Some(b'0'..=b'9')) {
            self.advance(1);
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.advance(1);
            }
        }
        // optional exponent
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let save = self.pos;
            self.advance(1);
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.advance(1);
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                // not a real exponent — rewind
                self.pos = save;
            } else {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.advance(1);
                }
            }
        }
        let raw = std::str::from_utf8(&self.input[start..self.pos]).expect("ASCII slice");
        Ok(Token::Number(raw.to_string()))
    }

    fn read_string(&mut self) -> Result<Token> {
        let line = self.line;
        let col = self.col;
        self.advance(1); // opening '
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(Error::lex(line, col, "unterminated string literal")),
                Some(b'\'') => {
                    if self.peek_at(1) == Some(b'\'') {
                        // doubled quote escape
                        out.push('\'');
                        self.advance(2);
                    } else {
                        self.advance(1);
                        return Ok(Token::String(out));
                    }
                }
                Some(b) => {
                    // To stay UTF-8-safe we copy raw bytes into the String only
                    // when we are sure we are at a char boundary. The simplest
                    // correct way is to find the next char boundary with
                    // std::str::from_utf8 on a 1..=4 byte window.
                    let rest = &self.input[self.pos..];
                    let ch_len = utf8_char_len(b);
                    let end = self.pos + ch_len;
                    if end > self.input.len() {
                        return Err(Error::lex(self.line, self.col, "invalid UTF-8 in string"));
                    }
                    let s = std::str::from_utf8(&rest[..ch_len]).map_err(|_| {
                        Error::lex(self.line, self.col, "invalid UTF-8 in string")
                    })?;
                    out.push_str(s);
                    self.advance(ch_len);
                }
            }
        }
    }

    fn read_quoted_ident(&mut self) -> Result<Token> {
        let line = self.line;
        let col = self.col;
        self.advance(1); // opening "
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(Error::lex(line, col, "unterminated quoted identifier")),
                Some(b'"') => {
                    if self.peek_at(1) == Some(b'"') {
                        out.push('"');
                        self.advance(2);
                    } else {
                        self.advance(1);
                        if out.is_empty() {
                            return Err(Error::lex(line, col, "empty quoted identifier"));
                        }
                        return Ok(Token::Ident(out));
                    }
                }
                Some(b) => {
                    let ch_len = utf8_char_len(b);
                    let s = std::str::from_utf8(&self.input[self.pos..self.pos + ch_len])
                        .map_err(|_| {
                            Error::lex(self.line, self.col, "invalid UTF-8 in identifier")
                        })?;
                    out.push_str(s);
                    self.advance(ch_len);
                }
            }
        }
    }
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Result<Spanned>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_token() {
            Ok(Some(t)) => Some(Ok(t)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// Length in bytes of a UTF-8 character whose first byte is `b`. Returns
/// `1` for invalid leading bytes — the caller will then surface a UTF-8
/// error when it tries to decode the slice.
fn utf8_char_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

// ----------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(input: &str) -> Vec<Token> {
        Lexer::collect_all(input).unwrap().into_iter().map(|s| s.token).collect()
    }

    fn lex_err(input: &str) -> String {
        Lexer::collect_all(input).unwrap_err().to_string()
    }

    #[test]
    fn keywords_case_insensitive() {
        use Token::*;
        assert_eq!(lex("select FROM Where"), vec![KwSelect, KwFrom, KwWhere]);
        assert_eq!(lex("CREATE TABLE"), vec![KwCreate, KwTable]);
    }

    #[test]
    fn identifiers_case_sensitive() {
        let toks = lex("foo Foo FOO_bar _x x9");
        assert_eq!(
            toks,
            vec![
                Token::Ident("foo".into()),
                Token::Ident("Foo".into()),
                Token::Ident("FOO_bar".into()),
                Token::Ident("_x".into()),
                Token::Ident("x9".into()),
            ]
        );
    }

    #[test]
    fn quoted_identifier_preserves_case() {
        assert_eq!(lex(r#""SeLeCt""#), vec![Token::Ident("SeLeCt".into())]);
    }

    #[test]
    fn quoted_identifier_doubled_quote_escape() {
        assert_eq!(lex(r#""a""b""#), vec![Token::Ident("a\"b".into())]);
    }

    #[test]
    fn quoted_identifier_unterminated() {
        assert!(lex_err(r#""abc"#).contains("unterminated quoted identifier"));
    }

    #[test]
    fn empty_quoted_identifier_rejected() {
        assert!(lex_err(r#""""#).contains("empty quoted identifier"));
    }

    #[test]
    fn integer_literals() {
        assert_eq!(lex("42 0 1234567890"), vec![
            Token::Number("42".into()),
            Token::Number("0".into()),
            Token::Number("1234567890".into()),
        ]);
    }

    #[test]
    fn float_literals() {
        assert_eq!(lex("3.14 0.5 100.0"), vec![
            Token::Number("3.14".into()),
            Token::Number("0.5".into()),
            Token::Number("100.0".into()),
        ]);
    }

    #[test]
    fn scientific_notation() {
        assert_eq!(lex("1e10 1.5e-3 2E+5"), vec![
            Token::Number("1e10".into()),
            Token::Number("1.5e-3".into()),
            Token::Number("2E+5".into()),
        ]);
    }

    #[test]
    fn dotted_path_after_number_not_floats() {
        // `1.foo` should be Number(1), Dot, Ident(foo) — fractional needs digits.
        assert_eq!(lex("1.foo"), vec![
            Token::Number("1".into()),
            Token::Dot,
            Token::Ident("foo".into()),
        ]);
    }

    #[test]
    fn string_simple() {
        assert_eq!(lex("'hello'"), vec![Token::String("hello".into())]);
    }

    #[test]
    fn string_doubled_quote_escape() {
        assert_eq!(lex("'it''s'"), vec![Token::String("it's".into())]);
    }

    #[test]
    fn string_unterminated() {
        assert!(lex_err("'oops").contains("unterminated string literal"));
    }

    #[test]
    fn string_with_utf8() {
        assert_eq!(lex("'café'"), vec![Token::String("café".into())]);
        assert_eq!(lex("'你好'"), vec![Token::String("你好".into())]);
    }

    #[test]
    fn punctuation_and_operators() {
        use Token::*;
        let toks = lex(", ; ( ) . * + - / % ^ = != <> < > <= >= ||");
        assert_eq!(
            toks,
            vec![
                Comma, Semicolon, LParen, RParen, Dot, Star, Plus, Minus, Slash, Percent, Caret,
                Eq, NotEq, NotEq, Lt, Gt, LtEq, GtEq, Concat,
            ]
        );
    }

    #[test]
    fn line_comment_eats_to_eol() {
        use Token::*;
        let toks = lex("SELECT -- comment here\n FROM t");
        assert_eq!(toks, vec![KwSelect, KwFrom, Token::Ident("t".into())]);
    }

    #[test]
    fn block_comment_can_nest() {
        use Token::*;
        let toks = lex("SELECT /* outer /* inner */ still in */ FROM");
        assert_eq!(toks, vec![KwSelect, KwFrom]);
    }

    #[test]
    fn block_comment_unterminated() {
        assert!(lex_err("/* never closes").contains("unterminated"));
    }

    #[test]
    fn span_tracks_line_and_col() {
        let v = Lexer::collect_all("SELECT\n  *\n  FROM t").unwrap();
        assert_eq!(v[0].line, 1);
        assert_eq!(v[0].col, 1);
        assert_eq!(v[1].line, 2);
        assert_eq!(v[1].col, 3);
        assert_eq!(v[2].line, 3);
        assert_eq!(v[2].col, 3);
        assert_eq!(v[3].line, 3);
        assert_eq!(v[3].col, 8);
    }

    #[test]
    fn unexpected_byte_reports_position() {
        let e = lex_err("SELECT @ FROM t");
        assert!(e.contains("line 1, col 8"), "got: {e}");
        assert!(e.contains("unexpected byte"), "got: {e}");
    }

    #[test]
    fn full_select() {
        use Token::*;
        let toks = lex(
            "SELECT id, name FROM users WHERE age >= 21 AND name <> 'bob' ORDER BY id LIMIT 10",
        );
        assert_eq!(
            toks,
            vec![
                KwSelect,
                Ident("id".into()),
                Comma,
                Ident("name".into()),
                KwFrom,
                Ident("users".into()),
                KwWhere,
                Ident("age".into()),
                GtEq,
                Number("21".into()),
                KwAnd,
                Ident("name".into()),
                NotEq,
                Token::String("bob".into()),
                KwOrder,
                KwBy,
                Ident("id".into()),
                KwLimit,
                Number("10".into()),
            ]
        );
    }

    #[test]
    fn full_insert() {
        use Token::*;
        let toks = lex("INSERT INTO t (a, b) VALUES (1, 'x')");
        assert_eq!(
            toks,
            vec![
                KwInsert,
                KwInto,
                Ident("t".into()),
                LParen,
                Ident("a".into()),
                Comma,
                Ident("b".into()),
                RParen,
                KwValues,
                LParen,
                Number("1".into()),
                Comma,
                Token::String("x".into()),
                RParen,
            ]
        );
    }

    #[test]
    fn semicolon_terminates() {
        use Token::*;
        let toks = lex("SELECT 1; SELECT 2;");
        assert_eq!(
            toks,
            vec![
                KwSelect,
                Number("1".into()),
                Semicolon,
                KwSelect,
                Number("2".into()),
                Semicolon,
            ]
        );
    }

    #[test]
    fn type_keywords() {
        use Token::*;
        let toks = lex("INTEGER INT BOOLEAN BOOL FLOAT DOUBLE TEXT STRING VARCHAR CHAR");
        assert_eq!(
            toks,
            vec![
                KwInteger, KwInt, KwBoolean, KwBool, KwFloat, KwDouble, KwText, KwString,
                KwVarchar, KwChar,
            ]
        );
    }
}
