/// Tokenizer for the Basalt SQL dialect.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Ident(String),
    /// Quoted identifier: "col name" or [col name]
    QuotedIdent(String),
    Integer(i128),
    Real(f64),
    Str(String),
    // punctuation / operators
    LParen,
    RParen,
    Comma,
    Semi,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Eq,
    NotEq, // != or <>
    Lt,
    LtEq,
    Gt,
    GtEq,
    Dot,
    Eof,
}

#[derive(Debug, Clone)]
pub struct TokenSpan {
    pub token: Token,
    pub offset: usize,
}

#[derive(Debug)]
pub struct LexError {
    pub message: String,
    pub offset: usize,
}

pub fn lex(input: &str) -> Result<Vec<TokenSpan>, LexError> {
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                // line comment
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                let mut closed = false;
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        closed = true;
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                if !closed {
                    return Err(LexError {
                        message: "unterminated block comment".into(),
                        offset: i,
                    });
                }
            }
            b'(' => {
                out.push_tok(Token::LParen, i);
                i += 1;
            }
            b')' => {
                out.push_tok(Token::RParen, i);
                i += 1;
            }
            b',' => {
                out.push_tok(Token::Comma, i);
                i += 1;
            }
            b';' => {
                out.push_tok(Token::Semi, i);
                i += 1;
            }
            b'*' => {
                out.push_tok(Token::Star, i);
                i += 1;
            }
            b'+' => {
                out.push_tok(Token::Plus, i);
                i += 1;
            }
            b'-' => {
                out.push_tok(Token::Minus, i);
                i += 1;
            }
            b'/' => {
                out.push_tok(Token::Slash, i);
                i += 1;
            }
            b'%' => {
                out.push_tok(Token::Percent, i);
                i += 1;
            }
            b'.' if i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() => {
                let (token, end) = scan_number(input, i)?;
                out.push_tok(token, i);
                i = end;
            }
            b'.' => {
                out.push_tok(Token::Dot, i);
                i += 1;
            }
            b'=' => {
                out.push_tok(Token::Eq, i);
                i += 1;
            }
            b'!' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push_tok(Token::NotEq, i);
                    i += 2;
                } else {
                    return Err(LexError {
                        message: "unexpected '!'".into(),
                        offset: i,
                    });
                }
            }
            b'<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push_tok(Token::LtEq, i);
                    i += 2;
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                    out.push_tok(Token::NotEq, i);
                    i += 2;
                } else {
                    out.push_tok(Token::Lt, i);
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push_tok(Token::GtEq, i);
                    i += 2;
                } else {
                    out.push_tok(Token::Gt, i);
                    i += 1;
                }
            }
            b'\'' => {
                // single-quoted string with '' escape
                let start = i;
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= bytes.len() {
                        return Err(LexError {
                            message: "unterminated string literal".into(),
                            offset: start,
                        });
                    }
                    if bytes[i] == b'\'' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            s.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        let ch_len = utf8_len(bytes[i]);
                        s.push_str(std::str::from_utf8(&bytes[i..i + ch_len]).map_err(|_| {
                            LexError {
                                message: "invalid UTF-8".into(),
                                offset: i,
                            }
                        })?);
                        i += ch_len;
                    }
                }
                out.push_tok(Token::Str(s), start);
            }
            b'"' => {
                let start = i;
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= bytes.len() {
                        return Err(LexError {
                            message: "unterminated quoted identifier".into(),
                            offset: start,
                        });
                    }
                    if bytes[i] == b'"' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                            s.push('"');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    }
                    let ch_len = utf8_len(bytes[i]);
                    s.push_str(std::str::from_utf8(&bytes[i..i + ch_len]).map_err(|_| {
                        LexError {
                            message: "invalid UTF-8".into(),
                            offset: i,
                        }
                    })?);
                    i += ch_len;
                }
                out.push_tok(Token::QuotedIdent(s), start);
            }
            b'[' => {
                let start = i;
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= bytes.len() {
                        return Err(LexError {
                            message: "unterminated bracketed identifier".into(),
                            offset: start,
                        });
                    }
                    if bytes[i] == b']' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                            s.push(']');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        let ch_len = utf8_len(bytes[i]);
                        s.push_str(std::str::from_utf8(&bytes[i..i + ch_len]).map_err(|_| {
                            LexError {
                                message: "invalid UTF-8".into(),
                                offset: i,
                            }
                        })?);
                        i += ch_len;
                    }
                }
                out.push_tok(Token::QuotedIdent(s), start);
            }
            b'0'..=b'9' => {
                let (token, end) = scan_number(input, i)?;
                out.push_tok(token, i);
                i = end;
            }
            _ if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                out.push_tok(Token::Ident(input[start..i].to_string()), start);
            }
            _ => {
                let ch_len = utf8_len(b);
                // allow unicode identifiers
                let ch = input[i..].chars().next().unwrap();
                if ch.is_alphabetic() {
                    let start = i;
                    i += ch_len;
                    while i < bytes.len() {
                        let c = input[i..].chars().next().unwrap();
                        if c.is_alphanumeric() || c == '_' {
                            i += c.len_utf8();
                        } else {
                            break;
                        }
                    }
                    out.push_tok(Token::Ident(input[start..i].to_string()), start);
                } else {
                    return Err(LexError {
                        message: format!("unexpected character {:?}", ch),
                        offset: i,
                    });
                }
            }
        }
    }
    out.push_tok(Token::Eof, bytes.len());
    Ok(out)
}

fn scan_number(input: &str, start: usize) -> Result<(Token, usize), LexError> {
    let bytes = input.as_bytes();
    let mut i = start;
    let mut is_real = false;

    if bytes[i] == b'.' {
        is_real = true;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    } else {
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if bytes.get(i) == Some(&b'.') {
            is_real = true;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }

    if bytes
        .get(i)
        .is_some_and(|byte| *byte == b'e' || *byte == b'E')
    {
        is_real = true;
        i += 1;
        if bytes
            .get(i)
            .is_some_and(|byte| *byte == b'+' || *byte == b'-')
        {
            i += 1;
        }
        let exponent_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exponent_start {
            return Err(LexError {
                message: "malformed number exponent".into(),
                offset: start,
            });
        }
    }

    if bytes.get(i) == Some(&b'.')
        || bytes
            .get(i)
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
    {
        return Err(LexError {
            message: "malformed number".into(),
            offset: start,
        });
    }

    let text = &input[start..i];
    if is_real {
        let f: f64 = text.parse().map_err(|_| LexError {
            message: "malformed number".into(),
            offset: start,
        })?;
        if !f.is_finite() {
            return Err(LexError {
                message: "real number out of range".into(),
                offset: start,
            });
        }
        Ok((Token::Real(f), i))
    } else {
        let n: i128 = text.parse().map_err(|_| LexError {
            message: "integer out of range".into(),
            offset: start,
        })?;
        Ok((Token::Integer(n), i))
    }
}

trait PushToken {
    fn push_tok(&mut self, t: Token, off: usize);
}
impl PushToken for Vec<TokenSpan> {
    fn push_tok(&mut self, t: Token, off: usize) {
        self.push(TokenSpan {
            token: t,
            offset: off,
        });
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_quoted_identifiers() {
        let tokens = lex("SELECT \"a\"\"b\", [c]]d] FROM t").unwrap();
        assert!(
            tokens
                .iter()
                .any(|span| span.token == Token::QuotedIdent("a\"b".into()))
        );
        assert!(
            tokens
                .iter()
                .any(|span| span.token == Token::QuotedIdent("c]d".into()))
        );
    }

    #[test]
    fn scans_decimal_and_exponent_literals() {
        let tokens = lex("SELECT .5, 1., 1e3, 1.25E-2").unwrap();
        assert!(tokens.iter().any(|span| span.token == Token::Real(0.5)));
        assert!(tokens.iter().any(|span| span.token == Token::Real(1.0)));
        assert!(tokens.iter().any(|span| span.token == Token::Real(1000.0)));
        assert!(tokens.iter().any(|span| span.token == Token::Real(0.0125)));
    }

    #[test]
    fn rejects_malformed_numbers() {
        assert!(lex("1.2.3").is_err());
        assert!(lex("1e").is_err());
        assert!(lex("1foo").is_err());
        assert!(lex("1e309").is_err());
    }
}
