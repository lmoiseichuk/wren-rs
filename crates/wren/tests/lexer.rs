//! What the lexer must do, checked against Wren's own syntax.
//!
//! These run on the host with `cargo test`. They are the reason the lexer was
//! written first: none of it needs a board, and a token stream is the one part
//! of a language implementation that can be checked exhaustively without a VM
//! to run it in.
//!
//! Where a case exists because Wren differs from the obvious reading — `1..2`,
//! a lone `_`, nested block comments — the test says so, because those are the
//! ones a later change will break without noticing.

use wren::{Lexer, TokenKind};

/// Every token kind in order, ignoring the newlines that separate statements.
fn kinds(source: &str) -> Vec<TokenKind> {
    let mut lexer = Lexer::new(source);
    let mut out = Vec::new();
    loop {
        let token = lexer.next_token();
        match token.kind {
            TokenKind::Eof => return out,
            TokenKind::Line => continue,
            kind => out.push(kind),
        }
    }
}

/// Tokens with their source text, for the cases where the span matters.
fn spans(source: &str) -> Vec<(TokenKind, String)> {
    let mut lexer = Lexer::new(source);
    let mut out = Vec::new();
    loop {
        let token = lexer.next_token();
        if token.kind == TokenKind::Eof {
            return out;
        }
        out.push((token.kind, token.text(source).to_owned()));
    }
}

#[test]
fn an_empty_source_is_just_eof() {
    assert_eq!(kinds(""), []);
    assert_eq!(kinds("   \t  "), []);
}

#[test]
fn keywords_are_not_names() {
    use TokenKind::*;
    assert_eq!(
        kinds("class Foo is Bar { construct new() { super() } }"),
        [
            Class, Name, Is, Name, LeftBrace, Construct, Name, LeftParen, RightParen, LeftBrace,
            Super, LeftParen, RightParen, RightBrace, RightBrace
        ]
    );
}

#[test]
fn a_word_that_merely_contains_a_keyword_is_a_name() {
    // `classy` starts with `class`; the table must match whole words only.
    assert_eq!(kinds("classy isnt vars"), [TokenKind::Name; 3]);
}

#[test]
fn newlines_are_tokens_because_they_separate_statements() {
    use TokenKind::*;
    let mut lexer = Lexer::new("var a\nvar b");
    let mut seen = Vec::new();
    loop {
        let token = lexer.next_token();
        if token.kind == Eof {
            break;
        }
        seen.push(token.kind);
    }
    assert_eq!(seen, [Var, Name, Line, Var, Name]);
}

#[test]
fn a_line_number_survives_comments_and_strings() {
    // Every error message downstream depends on this, and nothing else checks
    // it: a multi-line block comment that forgot to count would put every later
    // complaint on the wrong line.
    let source = "a\n/* two\nthree */\nb";
    let mut lexer = Lexer::new(source);
    assert_eq!(lexer.next_token().line, 1); // a
    assert_eq!(lexer.next_token().kind, TokenKind::Line);
    let b = {
        let mut token = lexer.next_token();
        while token.kind == TokenKind::Line {
            token = lexer.next_token();
        }
        token
    };
    assert_eq!(b.text(source), "b");
    assert_eq!(b.line, 4, "the comment spanned two newlines");
}

#[test]
fn block_comments_nest() {
    // The reason block comments exist is to comment out a region, and regions
    // contain comments. A scan for the first `*/` gets this wrong.
    assert_eq!(
        kinds("a /* outer /* inner */ still outer */ b"),
        [TokenKind::Name; 2]
    );
}

#[test]
fn line_comments_end_at_the_newline_and_not_before() {
    use TokenKind::*;
    let mut lexer = Lexer::new("a // b c\nd");
    assert_eq!(lexer.next_token().kind, Name);
    assert_eq!(lexer.next_token().kind, Line);
    assert_eq!(lexer.next_token().kind, Name);
    assert_eq!(lexer.next_token().kind, Eof);
}

#[test]
// The name says it: this is about doubles.
#[cfg(not(feature = "no-fp"))]
fn numbers_are_doubles_including_the_hexadecimal_ones() {
    // Wren has one numeric type. `0xff` is 255.0, not an integer.
    let source = "0 1 42 2.75 1e3 1e-3 0xff 0X10";
    let mut lexer = Lexer::new(source);
    let mut values = Vec::new();
    loop {
        let token = lexer.next_token();
        match token.kind {
            TokenKind::Eof => break,
            TokenKind::Number => values.push(token.number(source).unwrap()),
            other => panic!("expected a number, got {other:?}"),
        }
    }
    // The fractions are not representable without floating point, and the
    // lexer refuses them there rather than rounding -- so the expectation is
    // the float one and the test is a float test.
    #[cfg(not(feature = "no-fp"))]
    assert_eq!(values, [0.0, 1.0, 42.0, 2.75, 1000.0, 0.001, 255.0, 16.0]);
}

#[test]
fn a_dot_after_a_number_is_only_a_fraction_if_a_digit_follows() {
    use TokenKind::*;
    // `1..2` is a range between two integers. Lexing it as `1.` `.2` would make
    // the most common loop in the language fail to parse.
    assert_eq!(kinds("1..2"), [Number, DotDot, Number]);
    // `1.foo` is a method call on a number.
    assert_eq!(kinds("1.foo"), [Number, Dot, Name]);
    assert_eq!(kinds("1.5"), [Number]);
}

#[test]
fn an_exponent_without_digits_is_not_part_of_the_number() {
    use TokenKind::*;
    // `1e` should be the number 1 and the name `e`, not a malformed literal.
    assert_eq!(spans("1e"), [(Number, "1".into()), (Name, "e".into())]);
}

#[test]
fn fields_are_distinguished_by_their_underscores() {
    use TokenKind::*;
    assert_eq!(
        spans("_field __static plain"),
        [
            (Field, "_field".into()),
            (StaticField, "__static".into()),
            (Name, "plain".into())
        ]
    );
}

#[test]
fn a_bare_underscore_is_a_name_not_a_field() {
    // It is the conventional throwaway parameter, and there is no field to name.
    assert_eq!(kinds("_"), [TokenKind::Name]);
    assert_eq!(kinds("__"), [TokenKind::Name]);
}

#[test]
fn a_string_is_returned_raw_with_its_escapes_intact() {
    use TokenKind::*;
    // Decoding needs a heap; see the module note. The span covers the contents,
    // not the quotes.
    assert_eq!(spans(r#""hello""#), [(String, "hello".into())]);
    assert_eq!(spans(r#""a\nb""#), [(String, r"a\nb".into())]);
}

#[test]
fn an_escaped_quote_does_not_end_a_string() {
    use TokenKind::*;
    assert_eq!(spans(r#""a\"b""#), [(String, r#"a\"b"#.into())]);
}

#[test]
fn an_unterminated_string_is_an_error_rather_than_running_to_the_end() {
    let mut lexer = Lexer::new("\"no closing quote");
    assert_eq!(lexer.next_token().kind, TokenKind::Error);
}

#[test]
fn a_trailing_backslash_does_not_read_past_the_end() {
    // The escape handler consumes the next byte; at the end of input there is
    // not one, and reading it anyway would be a panic on malformed source.
    let mut lexer = Lexer::new("\"abc\\");
    assert_eq!(lexer.next_token().kind, TokenKind::Error);
}

#[test]
fn raw_strings_take_no_escapes_and_span_lines() {
    use TokenKind::*;
    assert_eq!(spans("\"\"\"a\\nb\"\"\""), [(String, "a\\nb".into())]);
    assert_eq!(spans("\"\"\"one\ntwo\"\"\""), [(String, "one\ntwo".into())]);
}

#[test]
fn interpolation_splits_a_string_around_its_expression() {
    use TokenKind::*;
    // `"a%(b)c"` is the pieces `a`, the expression `b`, and the rest `c`.
    assert_eq!(
        spans(r#""a%(b)c""#),
        [
            (Interpolation, "a".into()),
            (Name, "b".into()),
            (String, "c".into())
        ]
    );
}

#[test]
fn interpolation_holds_parentheses_inside_the_expression() {
    use TokenKind::*;
    // The `)` of `f(x)` must not be mistaken for the one closing the `%(`.
    assert_eq!(
        spans(r#""v=%(f(x))!""#),
        [
            (Interpolation, "v=".into()),
            (Name, "f".into()),
            (LeftParen, "(".into()),
            (Name, "x".into()),
            (RightParen, ")".into()),
            (String, "!".into()),
        ]
    );
}

#[test]
fn interpolation_nests() {
    use TokenKind::*;
    let kinds = kinds(r#""a%("b%(c)d")e""#);
    assert_eq!(kinds, [Interpolation, Interpolation, Name, String, String]);
}

#[test]
fn interpolation_deeper_than_the_limit_is_an_error_not_a_panic() {
    // The stack is a fixed array because growth would need an allocator. Going
    // past it has to fail cleanly.
    let mut source = std::string::String::new();
    for _ in 0..wren::lexer::MAX_INTERPOLATION_NESTING + 1 {
        source.push_str("\"%(");
    }
    let mut lexer = Lexer::new(&source);
    let mut saw_error = false;
    for _ in 0..64 {
        let token = lexer.next_token();
        if token.kind == TokenKind::Error {
            saw_error = true;
            break;
        }
        if token.kind == TokenKind::Eof {
            break;
        }
    }
    assert!(saw_error, "exceeding the nesting limit should be an error");
}

#[test]
fn every_operator_lexes_to_its_own_kind() {
    use TokenKind::*;
    assert_eq!(
        kinds("( ) [ ] { } : , . .. ... * / % # + - << >> | || ^ & && ! ~ ? = < > <= >= == !="),
        [
            LeftParen,
            RightParen,
            LeftBracket,
            RightBracket,
            LeftBrace,
            RightBrace,
            Colon,
            Comma,
            Dot,
            DotDot,
            DotDotDot,
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
        ]
    );
}

#[test]
fn the_longest_operator_wins() {
    use TokenKind::*;
    // `<<` must not lex as two `<`, and `...` must not lex as `..` then `.`.
    assert_eq!(kinds("<<"), [LtLt]);
    assert_eq!(kinds("<="), [LtEq]);
    assert_eq!(kinds("..."), [DotDotDot]);
    assert_eq!(kinds("=="), [EqEq]);
}

#[test]
fn a_realistic_program_lexes_without_errors() {
    // From Wren's own documentation, near enough. The point is that nothing in
    // an ordinary program produces `Error`.
    let source = r#"
class Tree {
  construct new(value, left, right) {
    _value = value
    _left = left
    _right = right
  }

  sum {
    var total = _value
    if (_left != null) total = total + _left.sum
    if (_right != null) total = total + _right.sum
    return total
  }

  static build(depth) {
    if (depth <= 0) return Tree.new(1, null, null)
    return Tree.new(depth, build(depth - 1), build(depth - 1))
  }
}

var t = Tree.build(4)
System.print("sum is %(t.sum)")
"#;
    let mut lexer = Lexer::new(source);
    let mut count = 0;
    loop {
        let token = lexer.next_token();
        assert_ne!(
            token.kind,
            TokenKind::Error,
            "unexpected error at line {}: {:?}",
            token.line,
            token.text(source)
        );
        if token.kind == TokenKind::Eof {
            break;
        }
        count += 1;
    }
    assert!(
        count > 100,
        "expected a substantial token stream, got {count}"
    );
}
