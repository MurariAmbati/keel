#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Word(String),
    Int(i64),
    Float(f64),
    Str(String),
    Star,
    Comma,
    LParen,
    RParen,
    Dot,
    Semicolon,
    Plus,
    Minus,
    Slash,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub pos: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LexError {
    pub pos: usize,
    pub msg: String,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lex error at {}: {}", self.pos, self.msg)
    }
}

pub fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        let tok = match c {
            b'*' => {
                i += 1;
                Tok::Star
            }
            b',' => {
                i += 1;
                Tok::Comma
            }
            b'(' => {
                i += 1;
                Tok::LParen
            }
            b')' => {
                i += 1;
                Tok::RParen
            }
            b'.' if !(i + 1 < b.len() && b[i + 1].is_ascii_digit()) => {
                i += 1;
                Tok::Dot
            }
            b';' => {
                i += 1;
                Tok::Semicolon
            }
            b'+' => {
                i += 1;
                Tok::Plus
            }
            b'-' => {
                i += 1;
                Tok::Minus
            }
            b'/' => {
                i += 1;
                Tok::Slash
            }
            b'=' => {
                i += 1;
                Tok::Eq
            }
            b'<' => {
                i += 1;
                if i < b.len() && b[i] == b'=' {
                    i += 1;
                    Tok::Le
                } else if i < b.len() && b[i] == b'>' {
                    i += 1;
                    Tok::Ne
                } else {
                    Tok::Lt
                }
            }
            b'>' => {
                i += 1;
                if i < b.len() && b[i] == b'=' {
                    i += 1;
                    Tok::Ge
                } else {
                    Tok::Gt
                }
            }
            b'!' => {
                i += 1;
                if i < b.len() && b[i] == b'=' {
                    i += 1;
                    Tok::Ne
                } else {
                    return Err(LexError {
                        pos: start,
                        msg: "expected '=' after '!'".into(),
                    });
                }
            }
            b'\'' => {
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= b.len() {
                        return Err(LexError {
                            pos: start,
                            msg: "unterminated string".into(),
                        });
                    }
                    if b[i] == b'\'' {
                        if i + 1 < b.len() && b[i + 1] == b'\'' {
                            s.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        s.push(b[i] as char);
                        i += 1;
                    }
                }
                Tok::Str(s)
            }
            _ if c.is_ascii_digit()
                || (c == b'.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()) =>
            {
                let mut is_float = false;
                let num_start = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i < b.len() && b[i] == b'.' {
                    is_float = true;
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                    is_float = true;
                    i += 1;
                    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                        i += 1;
                    }
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let text = &src[num_start..i];
                if is_float {
                    Tok::Float(text.parse().map_err(|_| LexError {
                        pos: num_start,
                        msg: format!("bad number '{text}'"),
                    })?)
                } else {
                    match text.parse::<i64>() {
                        Ok(n) => Tok::Int(n),
                        Err(_) => Tok::Float(text.parse().map_err(|_| LexError {
                            pos: num_start,
                            msg: format!("bad number '{text}'"),
                        })?),
                    }
                }
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let ws = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                Tok::Word(src[ws..i].to_ascii_lowercase())
            }
            _ => {
                return Err(LexError {
                    pos: start,
                    msg: format!("unexpected character '{}'", c as char),
                })
            }
        };
        out.push(Token { tok, pos: start });
    }
    out.push(Token {
        tok: Tok::Eof,
        pos: b.len(),
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        lex(s).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn keywords_and_idents_fold_case() {
        assert_eq!(
            toks("SELECT Foo FROM Bar"),
            vec![
                Tok::Word("select".into()),
                Tok::Word("foo".into()),
                Tok::Word("from".into()),
                Tok::Word("bar".into()),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            toks("1 -2 3.5 1e3 42"),
            vec![
                Tok::Int(1),
                Tok::Minus,
                Tok::Int(2),
                Tok::Float(3.5),
                Tok::Float(1000.0),
                Tok::Int(42),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn operators() {
        assert_eq!(
            toks("= <> != < <= > >= * , . ; ( )"),
            vec![
                Tok::Eq,
                Tok::Ne,
                Tok::Ne,
                Tok::Lt,
                Tok::Le,
                Tok::Gt,
                Tok::Ge,
                Tok::Star,
                Tok::Comma,
                Tok::Dot,
                Tok::Semicolon,
                Tok::LParen,
                Tok::RParen,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn strings_with_escapes() {
        assert_eq!(toks("'it''s'"), vec![Tok::Str("it's".into()), Tok::Eof]);
    }

    #[test]
    fn comments_skipped() {
        assert_eq!(
            toks("1 -- comment\n2"),
            vec![Tok::Int(1), Tok::Int(2), Tok::Eof]
        );
    }
}
