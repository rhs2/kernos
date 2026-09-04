//! Hand-written lexer for the policy language.

use crate::error::ParseError;

/// One lexical token with its source position.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// The token itself.
    pub kind: TokenKind,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
}

/// Every token the grammar of 04-POLICY distinguishes. Keywords are lexed as
/// identifiers and classified by the parser, so reserved words stay in one list.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// An identifier or keyword.
    Ident(String),
    /// A decimal number, optionally with a fraction.
    Number(f64),
    /// A double-quoted string with `\"` and `\\` escapes resolved.
    Str(String),
    /// A duration literal already converted to seconds.
    Duration(u64),
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `:`
    Colon,
    /// `->`
    Arrow,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// End of input.
    Eof,
}

/// The reserved words of the language. An identifier equal to one of these can
/// never be a path segment.
pub const KEYWORDS: &[&str] = &[
    "policy",
    "allow",
    "deny",
    "require",
    "approval",
    "when",
    "approver",
    "sla",
    "escalate_to",
    "and",
    "or",
    "not",
    "in",
    "true",
    "false",
    "null",
    "role",
    "user",
    "reporting_line",
];

/// Returns true when the word is reserved.
pub fn is_keyword(word: &str) -> bool {
    KEYWORDS.contains(&word)
}

/// Turns policy text into tokens, ending with `Eof`. Comments run from `#` to the
/// end of the line. Fails with line and column on an unterminated string, a bad
/// escape, an unknown character or a malformed number.
pub fn lex(source: &str) -> Result<Vec<Token>, ParseError> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let mut line = 1u32;
    let mut column = 1u32;

    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            i += 1;
            line += 1;
            column = 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            column += 1;
            continue;
        }
        if c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        let start_line = line;
        let start_column = column;
        let push = |tokens: &mut Vec<Token>, kind: TokenKind| {
            tokens.push(Token {
                kind,
                line: start_line,
                column: start_column,
            });
        };

        if c == '"' {
            let mut value = String::new();
            i += 1;
            column += 1;
            loop {
                let Some(&ch) = chars.get(i) else {
                    return Err(ParseError::new(
                        start_line,
                        start_column,
                        "unterminated string",
                    ));
                };
                if ch == '\n' {
                    return Err(ParseError::new(
                        start_line,
                        start_column,
                        "unterminated string",
                    ));
                }
                i += 1;
                column += 1;
                match ch {
                    '"' => break,
                    '\\' => {
                        let Some(&escaped) = chars.get(i) else {
                            return Err(ParseError::new(line, column, "unterminated escape"));
                        };
                        i += 1;
                        column += 1;
                        match escaped {
                            '"' => value.push('"'),
                            '\\' => value.push('\\'),
                            'n' => value.push('\n'),
                            't' => value.push('\t'),
                            other => {
                                return Err(ParseError::new(
                                    line,
                                    column - 1,
                                    format!("unknown escape \\{other}"),
                                ))
                            }
                        }
                    }
                    other => value.push(other),
                }
            }
            push(&mut tokens, TokenKind::Str(value));
            continue;
        }

        if c.is_ascii_digit() {
            let mut text = String::new();
            let mut seen_dot = false;
            while i < chars.len() && (chars[i].is_ascii_digit() || (chars[i] == '.' && !seen_dot)) {
                if chars[i] == '.' {
                    // A dot only belongs to the number when a digit follows it.
                    match chars.get(i + 1) {
                        Some(next) if next.is_ascii_digit() => seen_dot = true,
                        _ => break,
                    }
                }
                text.push(chars[i]);
                i += 1;
                column += 1;
            }
            let unit = chars.get(i).copied();
            let after_unit = chars.get(i + 1).copied();
            let unit_is_alone = !after_unit.is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
            if let Some(u @ ('s' | 'm' | 'h' | 'd')) = unit {
                if unit_is_alone {
                    i += 1;
                    column += 1;
                    let literal = format!("{text}{u}");
                    match crate::duration::parse_duration(&literal) {
                        Some(seconds) => push(&mut tokens, TokenKind::Duration(seconds)),
                        None => {
                            return Err(ParseError::new(
                                start_line,
                                start_column,
                                format!("malformed duration {literal}"),
                            ))
                        }
                    }
                    continue;
                }
            }
            if chars
                .get(i)
                .is_some_and(|ch| ch.is_alphanumeric() || *ch == '_')
            {
                return Err(ParseError::new(
                    start_line,
                    start_column,
                    "malformed number",
                ));
            }
            let value: f64 = text
                .parse()
                .map_err(|_| ParseError::new(start_line, start_column, "malformed number"))?;
            push(&mut tokens, TokenKind::Number(value));
            continue;
        }

        if c.is_alphabetic() || c == '_' {
            let mut word = String::new();
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                word.push(chars[i]);
                i += 1;
                column += 1;
            }
            push(&mut tokens, TokenKind::Ident(word));
            continue;
        }

        let two: String = chars[i..chars.len().min(i + 2)].iter().collect();
        let (kind, width) = match two.as_str() {
            "->" => (TokenKind::Arrow, 2),
            "==" => (TokenKind::Eq, 2),
            "!=" => (TokenKind::Ne, 2),
            "<=" => (TokenKind::Le, 2),
            ">=" => (TokenKind::Ge, 2),
            _ => match c {
                '(' => (TokenKind::LParen, 1),
                ')' => (TokenKind::RParen, 1),
                '[' => (TokenKind::LBracket, 1),
                ']' => (TokenKind::RBracket, 1),
                ',' => (TokenKind::Comma, 1),
                '.' => (TokenKind::Dot, 1),
                ':' => (TokenKind::Colon, 1),
                '<' => (TokenKind::Lt, 1),
                '>' => (TokenKind::Gt, 1),
                '+' => (TokenKind::Plus, 1),
                '-' => (TokenKind::Minus, 1),
                other => {
                    return Err(ParseError::new(
                        start_line,
                        start_column,
                        format!("unexpected character {other:?}"),
                    ))
                }
            },
        };
        push(&mut tokens, kind);
        i += width;
        column += width as u32;
    }

    tokens.push(Token {
        kind: TokenKind::Eof,
        line,
        column,
    });
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<TokenKind> {
        lex(source)
            .expect("lex")
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn lexes_operators_and_literals() {
        assert_eq!(
            kinds("a.b >= 5000 -> \"x\\\"y\" 4h [1, 2.5]"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Dot,
                TokenKind::Ident("b".into()),
                TokenKind::Ge,
                TokenKind::Number(5000.0),
                TokenKind::Arrow,
                TokenKind::Str("x\"y".into()),
                TokenKind::Duration(14400),
                TokenKind::LBracket,
                TokenKind::Number(1.0),
                TokenKind::Comma,
                TokenKind::Number(2.5),
                TokenKind::RBracket,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn comments_and_positions() {
        let tokens = lex("# comment\n  deny when x").expect("lex");
        assert_eq!(tokens[0].kind, TokenKind::Ident("deny".into()));
        assert_eq!((tokens[0].line, tokens[0].column), (2, 3));
        assert_eq!((tokens[2].line, tokens[2].column), (2, 13));
    }

    #[test]
    fn errors_carry_positions() {
        let err = lex("allow when \"open").expect_err("unterminated");
        assert_eq!((err.line, err.column), (1, 12));
        let err = lex("allow when a $ b").expect_err("bad char");
        assert_eq!((err.line, err.column), (1, 14));
        let err = lex("x == 12abc").expect_err("bad number");
        assert_eq!((err.line, err.column), (1, 6));
    }

    #[test]
    fn duration_only_when_unit_stands_alone() {
        assert_eq!(kinds("5m")[0], TokenKind::Duration(300));
        assert!(lex("5mx").is_err());
        assert_eq!(kinds("5 m")[0], TokenKind::Number(5.0));
    }
}
