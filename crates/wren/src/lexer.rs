//! Source text to tokens.
//!
//! **Allocation-free, and that is the constraint the whole design answers to.**
//! The smallest part in view has 8 KB of RAM, so a lexer that built owned
//! strings would spend the budget before the VM started. Every token here
//! borrows a slice of the original source instead: a `Token` is a kind, a span
//! and a line number, and the text is looked up rather than copied.
//!
//! The visible consequence is that **string literals are not decoded here**.
//! `"a\nb"` comes out as the six raw characters between the quotes, escapes
//! intact. Decoding needs somewhere to put the result, which means a heap, which
//! means the compiler — where one exists — rather than this file. `Token::text`
//! hands back exactly what was written.
//!
//! ## Newlines are tokens
//!
//! Wren separates statements with newlines rather than semicolons, so a newline
//! is a token and not whitespace. The compiler is what decides that a newline
//! after `{` is meaningless; the lexer's job is to report it faithfully.
//!
//! ## What this follows
//!
//! The token set is upstream Wren's, from `wren_compiler.c`, so that a program
//! that lexes here lexes there. Where this differs it is noted at the point of
//! difference, and there is currently one: see [`MAX_INTERPOLATION_NESTING`].

use core::str::FromStr;

/// How deeply `%( ... )` may nest inside string literals.
///
/// Eight, the same as upstream. It is a fixed array rather than a growable
/// stack because growth would mean an allocator, and this is the file that does
/// not have one. Exceeding it is a lex error rather than a panic.
pub const MAX_INTERPOLATION_NESTING: usize = 8;

/// What a token is.
///
/// Carries no payload: the text lives in the source and is recovered through
/// [`Token::text`], and numbers are parsed on demand by [`Token::number`]. That
/// keeps `Token` copyable and small, which matters when the parser holds two of
/// them at once on a part with kilobytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    LeftBrace,
    RightBrace,
    Colon,
    Dot,
    DotDot,
    DotDotDot,
    Comma,
    Star,
    Slash,
    Percent,
    Hash,
    Plus,
    Minus,
    LtLt,
    GtGt,
    Pipe,
    PipePipe,
    Caret,
    Amp,
    AmpAmp,
    Bang,
    Tilde,
    Question,
    Eq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    EqEq,
    BangEq,

    Break,
    Continue,
    Class,
    Construct,
    Else,
    False,
    For,
    Foreign,
    If,
    Import,
    As,
    In,
    Is,
    Null,
    Return,
    Static,
    Super,
    This,
    True,
    Var,
    While,

    /// `_name` — an instance field.
    Field,
    /// `__name` — a static field on the class.
    StaticField,
    /// An identifier, or a keyword this lexer does not know.
    Name,
    /// A numeric literal. Use [`Token::number`] to get its value.
    Number,
    /// A complete string literal, raw and undecoded.
    String,
    /// The portion of a string literal up to a `%(`.
    ///
    /// The tokens that follow are the interpolated expression; the string
    /// resumes after its closing `)`. A literal like `"a%(b)c"` lexes as
    /// `Interpolation("a")`, `Name(b)`, `String("c")`.
    Interpolation,

    /// A statement separator. See the module note on newlines.
    Line,
    /// Something this lexer could not make sense of. [`Token::text`] is the
    /// offending source, and the parser decides what to say about it.
    Error,
    Eof,
}

/// One token: what it is, where it came from, and which line it was on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte offsets into the source this token was lexed from.
    pub start: usize,
    pub end: usize,
    /// 1-based, for error messages.
    pub line: u32,
}

impl Token {
    /// The source text this token covers.
    ///
    /// **Takes the source rather than storing it**, because a token that
    /// carried a `&str` would be twice the size for information the caller
    /// already has. The caller is expected to pass the same source it lexed.
    pub fn text<'a>(&self, source: &'a str) -> &'a str {
        &source[self.start..self.end]
    }

    /// The value of a [`TokenKind::Number`], or `None` for anything else.
    ///
    /// Parsed on demand rather than at lex time: most numbers in a program are
    /// looked at once, and storing a `f64` in every token would grow the whole
    /// stream to pay for it.
    ///
    /// Wren numbers are doubles, including the hexadecimal ones — `0xff` is the
    /// value 255.0, not an integer type, because the language has only one
    /// numeric type.
    pub fn number(&self, source: &str) -> Option<f64> {
        if self.kind != TokenKind::Number {
            return None;
        }
        let text = self.text(source);
        match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            Some(digits) => u64::from_str_radix(digits, 16).ok().map(|value| value as f64),
            None => f64::from_str(text).ok(),
        }
    }
}

/// Turns source into tokens, one at a time.
///
/// Holds no allocation and borrows nothing but the source, so it is cheap to
/// construct and cheap to abandon.
#[derive(Clone)]
pub struct Lexer<'a> {
    source: &'a str,
    bytes: &'a [u8],
    /// Where the next token starts.
    start: usize,
    /// Where scanning is now.
    current: usize,
    line: u32,

    /// One entry per `%(` currently open, holding how many `(` have been seen
    /// inside it. A `)` that brings an entry to zero ends the interpolation and
    /// returns to scanning the string.
    interpolation: [u32; MAX_INTERPOLATION_NESTING],
    interpolation_depth: usize,
}

impl<'a> Lexer<'a> {
    /// Start lexing `source`.
    ///
    /// A leading UTF-8 byte order mark is skipped. It carries no information
    /// in a file that is already known to be UTF-8 -- editors on Windows add
    /// one anyway -- and leaving it in makes the first token of an otherwise
    /// valid program an error nobody can see.
    pub fn new(source: &'a str) -> Self {
        // U+FEFF encoded as UTF-8.
        let mark: &[u8] = &[0xef, 0xbb, 0xbf];
        let skip = if source.as_bytes().starts_with(mark) { mark.len() } else { 0 };
        Lexer {
            source,
            bytes: source.as_bytes(),
            start: skip,
            current: skip,
            line: 1,
            interpolation: [0; MAX_INTERPOLATION_NESTING],
            interpolation_depth: 0,
        }
    }

    /// The next token. Returns [`TokenKind::Eof`] for ever once the source runs
    /// out, rather than an `Option`, because every caller would immediately
    /// turn `None` into exactly that.
    pub fn next_token(&mut self) -> Token {
        if !self.skip_whitespace_and_comments() {
            // The span covers what there was, so the parser can point at where
            // the comment began.
            self.start = self.current.min(self.bytes.len());
            return self.make(TokenKind::Error);
        }

        self.start = self.current;
        let Some(c) = self.advance() else {
            return self.make(TokenKind::Eof);
        };

        // **Inside an interpolation, parentheses are counted.** A `)` that
        // closes the `%(` returns to the string rather than being an ordinary
        // token, and nothing else can tell the difference.
        if self.interpolation_depth > 0 {
            if c == b'(' {
                self.interpolation[self.interpolation_depth - 1] += 1;
            } else if c == b')' {
                let open = &mut self.interpolation[self.interpolation_depth - 1];
                if *open == 0 {
                    self.interpolation_depth -= 1;
                    return self.read_string();
                }
                *open -= 1;
            }
        }

        match c {
            b'(' => self.make(TokenKind::LeftParen),
            b')' => self.make(TokenKind::RightParen),
            b'[' => self.make(TokenKind::LeftBracket),
            b']' => self.make(TokenKind::RightBracket),
            b'{' => self.make(TokenKind::LeftBrace),
            b'}' => self.make(TokenKind::RightBrace),
            b':' => self.make(TokenKind::Colon),
            b',' => self.make(TokenKind::Comma),
            b'*' => self.make(TokenKind::Star),
            b'%' => self.make(TokenKind::Percent),
            b'#' => self.make(TokenKind::Hash),
            b'^' => self.make(TokenKind::Caret),
            b'+' => self.make(TokenKind::Plus),
            b'-' => self.make(TokenKind::Minus),
            b'~' => self.make(TokenKind::Tilde),
            b'?' => self.make(TokenKind::Question),
            b'/' => self.make(TokenKind::Slash),

            b'.' => match self.matches(b'.') {
                true => match self.matches(b'.') {
                    true => self.make(TokenKind::DotDotDot),
                    false => self.make(TokenKind::DotDot),
                },
                false => self.make(TokenKind::Dot),
            },
            b'|' => match self.matches(b'|') {
                true => self.make(TokenKind::PipePipe),
                false => self.make(TokenKind::Pipe),
            },
            b'&' => match self.matches(b'&') {
                true => self.make(TokenKind::AmpAmp),
                false => self.make(TokenKind::Amp),
            },
            b'=' => match self.matches(b'=') {
                true => self.make(TokenKind::EqEq),
                false => self.make(TokenKind::Eq),
            },
            b'!' => match self.matches(b'=') {
                true => self.make(TokenKind::BangEq),
                false => self.make(TokenKind::Bang),
            },
            b'<' => {
                if self.matches(b'<') {
                    self.make(TokenKind::LtLt)
                } else if self.matches(b'=') {
                    self.make(TokenKind::LtEq)
                } else {
                    self.make(TokenKind::Lt)
                }
            }
            b'>' => {
                if self.matches(b'>') {
                    self.make(TokenKind::GtGt)
                } else if self.matches(b'=') {
                    self.make(TokenKind::GtEq)
                } else {
                    self.make(TokenKind::Gt)
                }
            }

            b'\n' => {
                let token = self.make(TokenKind::Line);
                self.line += 1;
                token
            }

            b'"' => self.read_string_start(),
            b'_' => self.read_field(),
            b'0'..=b'9' => self.read_number(),
            c if is_name_start(c) => self.read_name(),

            _ => self.make(TokenKind::Error),
        }
    }

    // --- scanning helpers ---------------------------------------------------

    fn peek(&self) -> u8 {
        self.bytes.get(self.current).copied().unwrap_or(0)
    }

    fn peek_next(&self) -> u8 {
        self.bytes.get(self.current + 1).copied().unwrap_or(0)
    }

    fn advance(&mut self) -> Option<u8> {
        let c = self.bytes.get(self.current).copied()?;
        self.current += 1;
        Some(c)
    }

    fn matches(&mut self, expected: u8) -> bool {
        if self.peek() != expected {
            return false;
        }
        self.current += 1;
        true
    }

    fn make(&self, kind: TokenKind) -> Token {
        Token { kind, start: self.start, end: self.current, line: self.line }
    }

    /// A token whose text is a sub-slice of what was scanned.
    ///
    /// Strings want this: the token should cover the contents, not the quotes,
    /// so that `text()` gives what was written between them.
    fn make_spanning(&self, kind: TokenKind, start: usize, end: usize) -> Token {
        Token { kind, start, end, line: self.line }
    }

    /// Whitespace, line comments and nestable block comments.
    ///
    /// Newlines are *not* skipped — they are tokens. A newline inside a block
    /// comment is skipped but still counted, or every error message after a
    /// multi-line comment would point at the wrong line.
    /// Returns `false` if a block comment ran off the end of the file.
    fn skip_whitespace_and_comments(&mut self) -> bool {
        loop {
            match self.peek() {
                b' ' | b'\r' | b'\t' => {
                    self.current += 1;
                }
                b'/' if self.peek_next() == b'/' => {
                    while self.peek() != b'\n' && self.current < self.bytes.len() {
                        self.current += 1;
                    }
                }
                b'/' if self.peek_next() == b'*' => {
                    if !self.skip_block_comment() {
                        return false;
                    }
                }
                _ => return true,
            }
        }
    }

    /// `/* ... */`, and they nest.
    ///
    /// Nesting is Wren's rule and it is the reason this cannot be a simple
    /// search for `*/`: commenting out a region that already contains a comment
    /// has to work, which is the whole point of block comments in the first
    /// place.
    /// Returns `false` for an unterminated comment.
    ///
    /// **Running quietly to end of file was wrong.** A `/*` with no close
    /// swallows the whole rest of the program, and reporting nothing means the
    /// file compiles to an empty one -- the most confusing possible outcome
    /// for a missing two characters.
    fn skip_block_comment(&mut self) -> bool {
        self.current += 2; // the opening `/*`
        let mut depth = 1;
        while depth > 0 {
            match self.peek() {
                0 if self.current >= self.bytes.len() => return false,
                b'/' if self.peek_next() == b'*' => {
                    self.current += 2;
                    depth += 1;
                }
                b'*' if self.peek_next() == b'/' => {
                    self.current += 2;
                    depth -= 1;
                }
                b'\n' => {
                    self.line += 1;
                    self.current += 1;
                }
                _ => self.current += 1,
            }
        }
        true
    }

    // --- literals -----------------------------------------------------------

    fn read_number(&mut self) -> Token {
        // Hexadecimal, and only with a `0x` prefix at the very start.
        if self.bytes[self.start] == b'0' && (self.peek() == b'x' || self.peek() == b'X') {
            self.current += 1;
            while self.peek().is_ascii_hexdigit() {
                self.current += 1;
            }
            return self.make(TokenKind::Number);
        }

        while self.peek().is_ascii_digit() {
            self.current += 1;
        }

        // A fraction, but only if a digit follows the dot: `1..2` is a range
        // between two integers, not a malformed float, and `1.foo` is a method
        // call on a number.
        if self.peek() == b'.' && self.peek_next().is_ascii_digit() {
            self.current += 1;
            while self.peek().is_ascii_digit() {
                self.current += 1;
            }
        }

        if self.peek() == b'e' || self.peek() == b'E' {
            let mut ahead = self.current + 1;
            if matches!(self.bytes.get(ahead).copied(), Some(b'+') | Some(b'-')) {
                ahead += 1;
            }
            // Only consume the exponent if it actually has digits, so `1e` is
            // the number 1 followed by the name `e` rather than a lex error.
            if self.bytes.get(ahead).copied().unwrap_or(0).is_ascii_digit() {
                self.current = ahead;
                while self.peek().is_ascii_digit() {
                    self.current += 1;
                }
            }
        }

        self.make(TokenKind::Number)
    }

    fn read_name(&mut self) -> Token {
        while is_name_part(self.peek()) {
            self.current += 1;
        }
        let kind = keyword(&self.source[self.start..self.current]).unwrap_or(TokenKind::Name);
        self.make(kind)
    }

    /// `_field` or `__static_field`.
    ///
    /// A lone `_` is a name, not a field, which matches upstream and is what
    /// lets `_` be used as a throwaway parameter.
    fn read_field(&mut self) -> Token {
        let kind = match self.peek() == b'_' {
            true => {
                self.current += 1;
                TokenKind::StaticField
            }
            false => TokenKind::Field,
        };
        while is_name_part(self.peek()) {
            self.current += 1;
        }
        // `_` and `__` alone are names rather than fields: there is no field to
        // name, and rejecting them here would be a worse message than letting
        // the compiler complain about an unknown variable.
        match self.current - self.start {
            1 | 2 if kind == TokenKind::StaticField || self.current - self.start == 1 => {
                let text = &self.source[self.start..self.current];
                if text == "_" || text == "__" {
                    return self.make(TokenKind::Name);
                }
                self.make(kind)
            }
            _ => self.make(kind),
        }
    }

    /// The opening quote has been consumed. Decides raw versus ordinary.
    fn read_string_start(&mut self) -> Token {
        if self.peek() == b'"' && self.peek_next() == b'"' {
            self.current += 2;
            return self.read_raw_string();
        }
        self.read_string()
    }

    /// Scan to the closing quote or to a `%(`.
    ///
    /// Called both for a fresh literal and to resume one after an interpolated
    /// expression closes, which is why it does not expect to consume an opening
    /// quote: by the time it runs, the quote or the `)` is already behind us.
    fn read_string(&mut self) -> Token {
        let contents_start = self.current;
        loop {
            let Some(c) = self.advance() else {
                // Unterminated. The span covers what there was, so the parser
                // can point at where the string began.
                return self.make(TokenKind::Error);
            };
            match c {
                b'"' => {
                    return self.make_spanning(
                        TokenKind::String,
                        contents_start,
                        self.current - 1,
                    )
                }
                b'\\' => {
                    // Skip whatever follows, without interpreting it: an escaped
                    // quote must not end the string, and decoding happens later.
                    // A trailing backslash falls through to the unterminated
                    // case on the next pass rather than reading past the end.
                    if self.advance().is_none() {
                        return self.make(TokenKind::Error);
                    }
                }
                b'%' if self.peek() == b'(' => {
                    self.current += 1;
                    if self.interpolation_depth >= MAX_INTERPOLATION_NESTING {
                        return self.make(TokenKind::Error);
                    }
                    self.interpolation[self.interpolation_depth] = 0;
                    self.interpolation_depth += 1;
                    return self.make_spanning(
                        TokenKind::Interpolation,
                        contents_start,
                        self.current - 2,
                    );
                }
                b'\n' => self.line += 1,
                _ => {}
            }
        }
    }

    /// `"""..."""` — no escapes, no interpolation, newlines allowed.
    fn read_raw_string(&mut self) -> Token {
        let contents_start = self.current;
        loop {
            if self.current >= self.bytes.len() {
                return self.make(TokenKind::Error);
            }
            if self.peek() == b'"' && self.peek_next() == b'"' && self.byte_at(self.current + 2) == b'"' {
                let end = self.current;
                self.current += 3;
                return self.make_spanning(TokenKind::String, contents_start, end);
            }
            if self.peek() == b'\n' {
                self.line += 1;
            }
            self.current += 1;
        }
    }

    fn byte_at(&self, at: usize) -> u8 {
        self.bytes.get(at).copied().unwrap_or(0)
    }
}

fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_name_part(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// The keyword table.
///
/// A `match` rather than a map: it compiles to a length check and a handful of
/// comparisons, costs no static memory, and cannot be out of date with respect
/// to itself. On a part with 62 KB of flash the difference is worth having.
fn keyword(text: &str) -> Option<TokenKind> {
    Some(match text {
        "as" => TokenKind::As,
        "break" => TokenKind::Break,
        "class" => TokenKind::Class,
        "construct" => TokenKind::Construct,
        "continue" => TokenKind::Continue,
        "else" => TokenKind::Else,
        "false" => TokenKind::False,
        "for" => TokenKind::For,
        "foreign" => TokenKind::Foreign,
        "if" => TokenKind::If,
        "import" => TokenKind::Import,
        "in" => TokenKind::In,
        "is" => TokenKind::Is,
        "null" => TokenKind::Null,
        "return" => TokenKind::Return,
        "static" => TokenKind::Static,
        "super" => TokenKind::Super,
        "this" => TokenKind::This,
        "true" => TokenKind::True,
        "var" => TokenKind::Var,
        "while" => TokenKind::While,
        _ => return None,
    })
}
