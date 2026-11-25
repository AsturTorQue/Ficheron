//! Tokenizer for awk source.
//!
//! The one genuinely tricky part of lexing awk is that `/` starts either a
//! division or a regular-expression literal, and the source text alone does not
//! say which. We resolve it from the previous token: if that token could end an
//! operand (`$2`, `x`, `)`, `++`), then `/` divides. Otherwise it opens a
//! regex.

use crate::error::Error;
use std::rc::Rc;

/// Reserved words. Everything else — `print`, `length`, `split`, `sub`,
/// `getline`, … — is an ordinary identifier that the parser interprets by
/// position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    /// `BEGIN`
    Begin,
    /// `END`
    End,
    /// `function` / `func`
    Function,
    /// `if`
    If,
    /// `else`
    Else,
    /// `while`
    While,
    /// `for`
    For,
    /// `do`
    Do,
    /// `break`
    Break,
    /// `continue`
    Continue,
    /// `next`
    Next,
    /// `nextfile`
    Nextfile,
    /// `exit`
    Exit,
    /// `return`
    Return,
    /// `delete`
    Delete,
    /// `in`
    In,
}

/// Operators and punctuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `^`
    Caret,
    /// `!`
    Bang,
    /// `~`
    Tilde,
    /// `=`
    Assign,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `!`+`=`
    NotEq,
    /// `<`+`=`
    LtEq,
    /// `>`+`=`
    GtEq,
    /// `==`
    EqEq,
    /// `&&`
    AndAnd,
    /// `||`
    OrOr,
    /// `++`
    Incr,
    /// `--`
    Decr,
    /// `+=`
    AddAssign,
    /// `-=`
    SubAssign,
    /// `*=`
    MulAssign,
    /// `/=`
    DivAssign,
    /// `%=`
    ModAssign,
    /// `^=`
    PowAssign,
    /// `,`
    Comma,
    /// `;`
    Semicolon,
    /// `:`
    Colon,
    /// `?`
    Question,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `&&` — two logical `&`, not a single operator.
    AmpAmp,
    /// `$`
    Dollar,
    /// `|` as concatenation is implicit; this is the explicit pipe used by
    /// `getline` from a command.
    Pipe,
}

/// A token.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// A numeric literal.
    Num(f64),
    /// A string literal, with escapes already resolved.
    Str(Rc<str>),
    /// A `/.../` regular expression, escapes left intact for the regex engine.
    Regex(Rc<str>),
    /// An identifier.
    Ident(Rc<str>),
    /// A reserved word.
    Kw(Keyword),
    /// An operator or punctuation mark.
    Op(Op),
    /// A statement terminator. The parser skips these wherever awk allows a
    /// statement to continue.
    Newline,
    /// End of input.
    Eof,
}

impl Tok {
    /// A short name for error messages.
    pub fn describe(&self) -> String {
        match self {
            Tok::Num(n) => format!("number {n}"),
            Tok::Str(s) => format!("string \"{s}\""),
            Tok::Regex(s) => format!("/{s}/"),
            Tok::Ident(s) => format!("`{s}`"),
            Tok::Kw(k) => format!("`{}`", keyword_str(*k)),
            Tok::Op(o) => format!("`{}`", op_str(*o)),
            Tok::Newline => "end of line".to_string(),
            Tok::Eof => "end of input".to_string(),
        }
    }
}

/// The source text for a reserved word.
pub fn keyword_str(k: Keyword) -> &'static str {
    match k {
        Keyword::Begin => "BEGIN",
        Keyword::End => "END",
        Keyword::Function => "function",
        Keyword::If => "if",
        Keyword::Else => "else",
        Keyword::While => "while",
        Keyword::For => "for",
        Keyword::Do => "do",
        Keyword::Break => "break",
        Keyword::Continue => "continue",
        Keyword::Next => "next",
        Keyword::Nextfile => "nextfile",
        Keyword::Exit => "exit",
        Keyword::Return => "return",
        Keyword::Delete => "delete",
        Keyword::In => "in",
    }
}

/// The source text for an operator.
pub fn op_str(o: Op) -> &'static str {
    match o {
        Op::Plus => "+",
        Op::Minus => "-",
        Op::Star => "*",
        Op::Slash => "/",
        Op::Percent => "%",
        Op::Caret => "^",
        Op::Bang => "!",
        Op::Tilde => "~",
        Op::Assign => "=",
        Op::Lt => "<",
        Op::Gt => ">",
        Op::NotEq => "!=",
        Op::LtEq => "<=",
        Op::GtEq => ">=",
        Op::EqEq => "==",
        Op::AndAnd => "&&",
        Op::OrOr => "||",
        Op::Incr => "++",
        Op::Decr => "--",
        Op::AddAssign => "+=",
        Op::SubAssign => "-=",
        Op::MulAssign => "*=",
        Op::DivAssign => "/=",
        Op::ModAssign => "%=",
        Op::PowAssign => "^=",
        Op::Comma => ",",
        Op::Semicolon => ";",
        Op::Colon => ":",
        Op::Question => "?",
        Op::LParen => "(",
        Op::RParen => ")",
        Op::LBracket => "[",
        Op::RBracket => "]",
        Op::LBrace => "{",
        Op::RBrace => "}",
        Op::AmpAmp => "&",
        Op::Dollar => "$",
        Op::Pipe => "|",
    }
}

/// A token with its source position.
#[derive(Debug, Clone)]
pub struct Spanned {
    /// The token.
    pub tok: Tok,
    /// 1-based line.
    pub line: usize,
    /// 1-based column.
    pub column: usize,
}

/// Tokenizes awk source text.
pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: usize,
    /// Byte offset of the start of the current line, for column reporting.
    line_start: usize,
}

impl<'a> Lexer<'a> {
    /// Create a lexer over `src`.
    pub fn new(src: &'a str) -> Lexer<'a> {
        Lexer { src, bytes: src.as_bytes(), pos: 0, line: 1, line_start: 0 }
    }

    /// Tokenize the whole input. Newlines are emitted as [`Tok::Newline`].
    pub fn tokenize(mut self) -> Result<Vec<Spanned>, Error> {
        let mut out = Vec::new();
        loop {
            let t = self.next_token(out.last().map(|s: &Spanned| &s.tok))?;
            let line = self.line;
            let column = self.pos.saturating_sub(self.line_start).max(1);
            let is_eof = matches!(t.tok, Tok::Eof);
            out.push(Spanned { tok: t.tok, line, column });
            if is_eof {
                return Ok(out);
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.pos + n).copied()
    }

    /// Skip spaces, comments, and backslash continuations. Does not skip newlines.
    fn skip_trivia(&mut self) -> Result<(), Error> {
        loop {
            match self.peek() {
                Some(b' ') | Some(b'\t') | Some(b'\r') => self.pos += 1,
                Some(b'\\') if self.peek_at(1) == Some(b'\n') => {
                    self.pos += 1;
                    self.bump_line();
                }
                Some(b'#') => {
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn bump_line(&mut self) {
        self.pos += 1;
        self.line += 1;
        self.line_start = self.pos;
    }

    /// Produce the next token. `prev` is the previous token, used only to
    /// disambiguate `/`.
    fn next_token(&mut self, prev: Option<&Tok>) -> Result<Spanned, Error> {
        self.skip_trivia()?;
        let start = self.pos;
        let line = self.line;
        let col = start.saturating_sub(self.line_start).max(1);

        let Some(c) = self.peek() else {
            return Ok(Spanned { tok: Tok::Eof, line, column: col });
        };

        let tok = match c {
            b'\n' => {
                self.bump_line();
                Tok::Newline
            }
            b'"' => Tok::Str(self.lex_string()?),
            b'/' if !prev_can_end_operand(prev) => Tok::Regex(self.lex_regex()?),
            b'0'..=b'9' => self.lex_number(),
            b'.' if matches!(self.peek_at(1), Some(b'0'..=b'9')) => self.lex_number(),
            c if is_ident_start(c) => self.lex_ident(),
            _ => Tok::Op(self.lex_op(c).ok_or_else(|| {
                Error::parse(format!("unexpected character `{}`", c as char)).at(line, col)
            })?),
        };
        Ok(Spanned { tok, line, column: col })
    }

    /// Classify the operator at the current position and consume it. The
    /// position must still be on the first character of the operator.
    fn lex_op(&mut self, c: u8) -> Option<Op> {
        let op = self.classify_op(c)?;
        self.pos += if op_is_two_chars(op) { 2 } else { 1 };
        Some(op)
    }

    fn classify_op(&self, c: u8) -> Option<Op> {
        // Two-character operators first; `&` is not a conjunction operator.
        let next = self.peek_at(1);
        let two = match (c, next) {
            (b'=', Some(b'=')) => Some(Op::EqEq),
            (b'!', Some(b'=')) => Some(Op::NotEq),
            (b'<', Some(b'=')) => Some(Op::LtEq),
            (b'>', Some(b'=')) => Some(Op::GtEq),
            (b'&', Some(b'&')) => Some(Op::AndAnd),
            (b'|', Some(b'|')) => Some(Op::OrOr),
            (b'+', Some(b'+')) => Some(Op::Incr),
            (b'+', Some(b'=')) => Some(Op::AddAssign),
            (b'-', Some(b'-')) => Some(Op::Decr),
            (b'-', Some(b'=')) => Some(Op::SubAssign),
            (b'*', Some(b'=')) => Some(Op::MulAssign),
            (b'/', Some(b'=')) => Some(Op::DivAssign),
            (b'%', Some(b'=')) => Some(Op::ModAssign),
            (b'^', Some(b'=')) => Some(Op::PowAssign),
            _ => None,
        };
        if two.is_some() {
            return two;
        }
        Some(match c {
            b'+' => Op::Plus,
            b'-' => Op::Minus,
            b'*' => Op::Star,
            b'/' => Op::Slash,
            b'%' => Op::Percent,
            b'^' => Op::Caret,
            b'!' => Op::Bang,
            b'~' => Op::Tilde,
            b'=' => Op::Assign,
            b'<' => Op::Lt,
            b'>' => Op::Gt,
            b'?' => Op::Question,
            b':' => Op::Colon,
            b',' => Op::Comma,
            b';' => Op::Semicolon,
            b'(' => Op::LParen,
            b')' => Op::RParen,
            b'[' => Op::LBracket,
            b']' => Op::RBracket,
            b'{' => Op::LBrace,
            b'}' => Op::RBrace,
            b'$' => Op::Dollar,
            b'&' => Op::AmpAmp,
            b'|' => Op::Pipe,
            _ => return None,
        })
    }

    fn lex_number(&mut self) -> Tok {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            } else {
                // Not an exponent after all; rewind so `1e` lexes as 1 then ident e.
                self.pos = save;
            }
        }
        // A number directly followed by an identifier char is really an
        // identifier-like token in awk (`0x10`); we take the numeric prefix.
        Tok::Num(self.src[start..self.pos].parse::<f64>().unwrap_or(0.0))
    }

    fn lex_ident(&mut self) -> Tok {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            self.pos += 1;
        }
        let word = &self.src[start..self.pos];
        let kw = match word {
            "BEGIN" => Some(Keyword::Begin),
            "END" => Some(Keyword::End),
            "function" | "func" => Some(Keyword::Function),
            "if" => Some(Keyword::If),
            "else" => Some(Keyword::Else),
            "while" => Some(Keyword::While),
            "for" => Some(Keyword::For),
            "do" => Some(Keyword::Do),
            "break" => Some(Keyword::Break),
            "continue" => Some(Keyword::Continue),
            "next" => Some(Keyword::Next),
            "nextfile" => Some(Keyword::Nextfile),
            "exit" => Some(Keyword::Exit),
            "return" => Some(Keyword::Return),
            "delete" => Some(Keyword::Delete),
            "in" => Some(Keyword::In),
            _ => None,
        };
        match kw {
            Some(k) => Tok::Kw(k),
            None => Tok::Ident(Rc::from(word)),
        }
    }

    fn lex_string(&mut self) -> Result<Rc<str>, Error> {
        let line = self.line;
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(Error::parse("unterminated string literal").at(line, 1));
            };
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(Rc::from(out.as_str()));
                }
                b'\\' => {
                    self.pos += 1;
                    self.lex_escape(&mut out, line)?;
                }
                b'\n' => {
                    return Err(Error::parse("newline in string literal").at(self.line, 1));
                }
                _ => {
                    let ch = self.src[self.pos..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    /// Consume an escape sequence after the backslash has been consumed.
    fn lex_escape(&mut self, out: &mut String, line: usize) -> Result<(), Error> {
        let Some(c) = self.peek() else {
            return Err(Error::parse("unterminated escape sequence").at(line, 1));
        };
        self.pos += 1;
        let ch = match c {
            b'n' => '\n',
            b't' => '\t',
            b'r' => '\r',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'v' => '\u{b}',
            b'a' => '\u{7}',
            b'\\' => '\\',
            b'"' => '"',
            b'/' => '/',
            b'0'..=b'7' => {
                // Up to three octal digits.
                let mut v = (c - b'0') as u32;
                for _ in 0..2 {
                    match self.peek() {
                        Some(d @ b'0'..=b'7') => {
                            v = v * 8 + (d - b'0') as u32;
                            self.pos += 1;
                        }
                        _ => break,
                    }
                }
                char::from_u32(v).unwrap_or('\u{fffd}')
            }
            other => {
                // awk leaves unknown escapes as the literal character.
                other as char
            }
        };
        out.push(ch);
        Ok(())
    }

    /// Lex the body of a `/.../` literal. Escapes are left intact: the regex
    /// engine is the one that decides what `\w` means.
    fn lex_regex(&mut self) -> Result<Rc<str>, Error> {
        let line = self.line;
        self.pos += 1; // opening slash
        let start = self.pos;
        loop {
            let Some(c) = self.peek() else {
                return Err(Error::parse("unterminated regular expression").at(line, 1));
            };
            match c {
                b'\\' => {
                    // Copy the escaped character through verbatim.
                    self.pos += 1;
                    if self.peek().is_some() {
                        let ch = self.src[self.pos..].chars().next().unwrap_or('\0');
                        self.pos += ch.len_utf8();
                    }
                }
                b'/' => {
                    let body = &self.src[start..self.pos];
                    self.pos += 1;
                    return Ok(Rc::from(body));
                }
                b'\n' => {
                    return Err(Error::parse("newline in regular expression").at(self.line, 1));
                }
                _ => {
                    let ch = self.src[self.pos..].chars().next().unwrap_or('\0');
                    self.pos += ch.len_utf8();
                }
            }
        }
    }
}

/// Whether an operator is spelled with two characters.
fn op_is_two_chars(o: Op) -> bool {
    matches!(
        o,
        Op::EqEq
            | Op::NotEq
            | Op::LtEq
            | Op::GtEq
            | Op::AndAnd
            | Op::OrOr
            | Op::Incr
            | Op::Decr
            | Op::AddAssign
            | Op::SubAssign
            | Op::MulAssign
            | Op::DivAssign
            | Op::ModAssign
            | Op::PowAssign
    )
}

fn is_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic()
}

fn is_ident_continue(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

/// Whether `prev` could end an operand, which is what makes a following `/` a
/// division rather than the start of a regex.
///
/// Note `}` is deliberately *not* in this set: `{ print 1 } /re/ { }` is a pair
/// of rules, so a regex must be allowed to start after a rule's action.
fn prev_can_end_operand(prev: Option<&Tok>) -> bool {
    match prev {
        Some(Tok::Ident(_)) | Some(Tok::Num(_)) | Some(Tok::Str(_)) | Some(Tok::Regex(_)) => true,
        Some(Tok::Op(Op::RParen)) | Some(Tok::Op(Op::RBracket)) => true,
        Some(Tok::Op(Op::Incr)) | Some(Tok::Op(Op::Decr)) => true,
        Some(Tok::Op(Op::Dollar)) => true,
        Some(Tok::Kw(Keyword::In)) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        Lexer::new(src).tokenize().unwrap().into_iter().map(|s| s.tok).collect()
    }

    #[test]
    fn division_versus_regex() {
        // After an operand, `/` divides.
        assert_eq!(toks("x = a / b"), vec![
            Tok::Ident("x".into()),
            Tok::Op(Op::Assign),
            Tok::Ident("a".into()),
            Tok::Op(Op::Slash),
            Tok::Ident("b".into()),
            Tok::Eof,
        ]);
        // After `=`, `/` opens a regex.
        assert_eq!(toks("x = /a/"), vec![
            Tok::Ident("x".into()),
            Tok::Op(Op::Assign),
            Tok::Regex("a".into()),
            Tok::Eof,
        ]);
        // After `)`, `/` divides again.
        assert_eq!(toks("print (1+2)/3"), vec![
            Tok::Ident("print".into()),
            Tok::Op(Op::LParen),
            Tok::Num(1.0),
            Tok::Op(Op::Plus),
            Tok::Num(2.0),
            Tok::Op(Op::RParen),
            Tok::Op(Op::Slash),
            Tok::Num(3.0),
            Tok::Eof,
        ]);
    }

    #[test]
    fn regex_after_a_rule_action_starts_a_new_rule() {
        // `{ print 1 } /re/ { }` must lex the second `/` as a regex.
        assert_eq!(toks("{} /re/ {}"), vec![
            Tok::Op(Op::LBrace),
            Tok::Op(Op::RBrace),
            Tok::Regex("re".into()),
            Tok::Op(Op::LBrace),
            Tok::Op(Op::RBrace),
            Tok::Eof,
        ]);
    }

    #[test]
    fn string_escapes() {
        assert_eq!(toks(r#""a\nb""#), vec![Tok::Str("a\nb".into()), Tok::Eof]);
        assert_eq!(toks(r#""\101""#), vec![Tok::Str("A".into()), Tok::Eof]);
        assert_eq!(toks(r#""a\"b""#), vec![Tok::Str("a\"b".into()), Tok::Eof]);
        assert!(Lexer::new("\"unterminated").tokenize().is_err());
    }

    #[test]
    fn regex_escapes_are_left_intact() {
        // The regex engine decides what `\w` means, not the lexer.
        assert_eq!(toks(r"/a\/b/"), vec![Tok::Regex(r"a\/b".into()), Tok::Eof]);
        assert_eq!(toks(r"/\t/"), vec![Tok::Regex(r"\t".into()), Tok::Eof]);
    }

    #[test]
    fn numbers() {
        assert_eq!(toks("1 1.5 .5 1e3 1E-2"), vec![
            Tok::Num(1.0),
            Tok::Num(1.5),
            Tok::Num(0.5),
            Tok::Num(1000.0),
            Tok::Num(0.01),
            Tok::Eof,
        ]);
    }

    #[test]
    fn comments_and_continuations_are_trivia() {
        // The comment line still contributes a statement-terminating newline,
        // which the parser skips.
        assert_eq!(toks("# hi\n1 # there\n"), vec![
            Tok::Newline,
            Tok::Num(1.0),
            Tok::Newline,
            Tok::Eof
        ]);
        // A continuation joins the lines, so no statement terminator appears.
        assert_eq!(toks("1 \\\n+ 2"), vec![
            Tok::Num(1.0),
            Tok::Op(Op::Plus),
            Tok::Num(2.0),
            Tok::Eof
        ]);
    }

    #[test]
    fn keywords_and_operators() {
        assert_eq!(toks("BEGIN{print x!=1}"), vec![
            Tok::Kw(Keyword::Begin),
            Tok::Op(Op::LBrace),
            Tok::Ident("print".into()),
            Tok::Ident("x".into()),
            Tok::Op(Op::NotEq),
            Tok::Num(1.0),
            Tok::Op(Op::RBrace),
            Tok::Eof,
        ]);
    }
}