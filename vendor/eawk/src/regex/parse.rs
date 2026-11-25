//! POSIX ERE parser: pattern text -> AST.
//!
//! The grammar is the one from POSIX (Stations 1019-1025), not Perl's:
//!
//! ```text
//! branch   := piece ('|' piece)*
//! piece    := atom postfix*
//! postfix  := '*' | '+' | '?' | '{' bound '}'
//! atom     := '(' branch ')' | bracket | '.' | '^' | '$' | escape | literal
//! ```
//!
//! Two deliberate deviations from "textbook" ERE handling, both matching the
//! oracles:
//!
//! * `\d \w \s \D \W \S` are **not** special. BWK awk and mawk treat `\w` as a
//!   literal `w`, so we do too — otherwise differential testing against them
//!   diverges on the very patterns users actually type.
//! * A bracket expression has no escape mechanism: inside `[...]` a `]` always
//!   closes the class (except as the first character, where it is a literal).
//!   `[\]]` is therefore the class containing `\` and `]`.
//!
//! Recursion is depth-limited (see [`MAX_DEPTH`]) so a pathological pattern
//! like `((((((...))))))` returns an error instead of overflowing the stack —
//! a stack overflow cannot be caught by `Result`.

use crate::error::Error;

/// Maximum nested-paren / repeated-quantifier depth before we bail out.
pub(crate) const MAX_DEPTH: usize = 100;

/// A parsed atom, before repetition is applied.
#[derive(Debug, Clone)]
pub(crate) enum Atom {
    /// Match any character except `\n`.
    Any,
    /// Match one character from `set`.
    Class(CharClass),
    /// Match this exact character.
    Literal(char),
    /// Assert we are at offset 0 of the haystack.
    Bol,
    /// Assert we are at the end of the haystack.
    Eol,
    /// Concatenation of sub-branches; matches the first that matches.
    Group(Vec<Branch>),
    /// An atom wrapped in a repetition operator. The atom is a *single*
    /// character-consuming unit or a group; the bound is enforced by the NFA
    /// builder, never by expanding the pattern.
    Repeat(Box<Atom>, RepeatKind),
}

/// A concatenation of atoms. An empty branch matches the empty string.
pub(crate) type Branch = Vec<Atom>;

/// A set of characters, with an optional negation flag.
#[derive(Debug, Clone)]
pub(crate) struct CharClass {
    /// POSIX named class, or `None` for a set built from literals/ranges.
    pub named: Option<NamedClass>,
    /// Literals in the class.
    pub singles: Vec<char>,
    /// Inclusive ranges in the class, as `(lo, hi)` codepoint pairs.
    pub ranges: Vec<(char, char)>,
    /// True if the class was written `[^...]`.
    pub negated: bool,
}

/// The POSIX bracket-expression character classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamedClass {
    Alpha,
    Digit,
    Alnum,
    Space,
    Upper,
    Lower,
    Punct,
    Xdigit,
    Cntrl,
    Print,
    Graph,
    Blank,
}

impl NamedClass {
    /// Parse the text between `[:` and `:]`.
    fn from_name(name: &str) -> Option<NamedClass> {
        Some(match name {
            "alpha" => NamedClass::Alpha,
            "digit" => NamedClass::Digit,
            "alnum" => NamedClass::Alnum,
            "space" => NamedClass::Space,
            "upper" => NamedClass::Upper,
            "lower" => NamedClass::Lower,
            "punct" => NamedClass::Punct,
            "xdigit" => NamedClass::Xdigit,
            "cntrl" => NamedClass::Cntrl,
            "print" => NamedClass::Print,
            "graph" => NamedClass::Graph,
            "blank" => NamedClass::Blank,
            _ => return None,
        })
    }
}

impl NamedClass {
    /// True if `c` is in this class.
    pub(crate) fn contains(self, c: char) -> bool {
        match self {
            NamedClass::Alpha => c.is_alphabetic(),
            NamedClass::Digit => c.is_ascii_digit(),
            NamedClass::Alnum => c.is_ascii_alphanumeric(),
            NamedClass::Space => matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'),
            NamedClass::Upper => c.is_uppercase(),
            NamedClass::Lower => c.is_lowercase(),
            NamedClass::Punct => c.is_ascii_punctuation(),
            NamedClass::Xdigit => c.is_ascii_hexdigit(),
            NamedClass::Cntrl => c.is_control(),
            NamedClass::Print => !c.is_control(),
            NamedClass::Graph => !c.is_control() && !matches!(c, ' '),
            NamedClass::Blank => matches!(c, ' ' | '\t'),
        }
    }
}

impl CharClass {
    /// True if `c` is in this class, honouring the negation flag.
    pub(crate) fn contains(&self, c: char) -> bool {
        let mut hit = if let Some(named) = self.named {
            named.contains(c)
        } else {
            self.singles.contains(&c) || self.ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi)
        };
        if self.negated {
            hit = !hit;
        }
        hit
    }
}

/// Cursor over the pattern text, decoding one `char` at a time.
pub(crate) struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    depth: usize,
    _src: &'a str,
}

impl<'a> Parser<'a> {
    /// Build a parser over `src`.
    pub(crate) fn new(src: &'a str) -> Parser<'a> {
        Parser { chars: src.chars().collect(), pos: 0, depth: 0, _src: src }
    }

    /// The character under the cursor, if any.
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// The character `n` positions ahead of the cursor.
    fn peek_at(&self, n: usize) -> Option<char> {
        self.chars.get(self.pos + n).copied()
    }

    /// Consume and return the character under the cursor.
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// Consume `c` if it is next; report whether it was.
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Enter a nested group, or fail if we are too deep.
    fn enter(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Error::regex(format!("regex nested too deeply (limit {MAX_DEPTH})")));
        }
        Ok(())
    }

    /// Leave a nested group.
    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Parse the whole pattern as one alternation.
    pub(crate) fn parse(&mut self) -> Result<Vec<Branch>, Error> {
        let branches = self.parse_branches()?;
        if self.peek().is_some() {
            // Only a stray `)` can land us here.
            return Err(Error::regex("unmatched `)` in regular expression"));
        }
        Ok(branches)
    }

    /// Parse `piece ('|' piece)*`. If `stop_at_paren` the list ends at a `)`.
    fn parse_branches(&mut self) -> Result<Vec<Branch>, Error> {
        let mut branches = vec![self.parse_branch()?];
        while self.eat('|') {
            branches.push(self.parse_branch()?);
        }
        Ok(branches)
    }

    /// Parse a concatenation of atoms with postfix operators.
    fn parse_branch(&mut self) -> Result<Branch, Error> {
        let mut out: Branch = Vec::new();
        loop {
            match self.peek() {
                None => break,
                Some('|') | Some(')') => break,
                _ => {}
            }
            let atom = self.parse_atom()?;
            self.parse_postfix(atom, &mut out)?;
        }
        Ok(out)
    }

    /// Apply any postfix `*`, `+`, `?`, `{n,m}` to `atom`, pushing the result
    /// onto `out`.
    ///
    /// Consecutive quantifiers are legal in the oracles and fold onto the
    /// *current* repeated value, not onto the bare atom: `a**` is `(a*)*`.
    /// Rather than nest forever, we fold the pair into a single bounded form
    /// where we can (`(a*)*` -> `a*`) and otherwise reject. Folding here keeps
    /// `((a*)*)*` from compiling to a thousand-state NFA.
    fn parse_postfix(&mut self, atom: Atom, out: &mut Branch) -> Result<(), Error> {
        let mut cur = atom;
        loop {
            let kind = match self.peek() {
                Some('*') => RepeatKind::Star,
                Some('+') => RepeatKind::Plus,
                Some('?') => RepeatKind::Opt,
                Some('{') => match self.try_parse_bound()? {
                    // `try_parse_bound` consumes the whole `{n,m}`, so unlike
                    // the single-character quantifiers there is nothing left
                    // for the common `self.bump()` below to skip.
                    Some(b) => {
                        self.enter()?; // guard against `a{2}{2}{2}...`
                        cur = self.apply_quantifier(cur, RepeatKind::Bound(b))?;
                        self.leave();
                        continue;
                    }
                    // `{` not followed by a valid bound is a literal brace,
                    // matching what awk does for `a{b`.
                    None => {
                        out.push(cur);
                        return Ok(());
                    }
                },
                _ => {
                    out.push(cur);
                    return Ok(());
                }
            };
            self.bump();
            self.enter()?; // guard against `a**********...`
            cur = self.apply_quantifier(cur, kind)?;
            self.leave();
        }
    }

    /// Fold a quantifier onto an atom that may itself already be a
    /// [`Atom::Repeat`], simplifying where the algebra allows it.
    fn apply_quantifier(&mut self, atom: Atom, kind: RepeatKind) -> Result<Atom, Error> {
        let inner_repeat = match atom {
            // `(...)* * ` style: quantifier applied to a repetition. Simplify
            // the common cases, reject the rest rather than build a
            // combinatorially large NFA from something like `a{1000}{1000}`.
            Atom::Repeat(inner, prev) => Some((inner, prev)),
            other => return Ok(other.repeat(kind)),
        };
        let (inner, prev) = match inner_repeat {
            Some(v) => v,
            None => unreachable!(),
        };
        let simplified = match (prev, kind) {
            // `(x*)*` == `(x*)+` == `x*`
            (RepeatKind::Star, RepeatKind::Star)
            | (RepeatKind::Star, RepeatKind::Plus)
            | (RepeatKind::Plus, RepeatKind::Star)
            | (RepeatKind::Plus, RepeatKind::Plus) => Some(RepeatKind::Star),
            // `(x*)?` == `(x{0,1})*` == `x*`
            (RepeatKind::Star, RepeatKind::Opt) => Some(RepeatKind::Star),
            // `(x+)?` == `x*`
            (RepeatKind::Plus, RepeatKind::Opt) => Some(RepeatKind::Star),
            _ => None,
        };
        match simplified {
            Some(k) => Ok(Atom::Repeat(inner, k)),
            None => Err(Error::regex("too many repetition operators in regular expression")),
        }
    }

    /// Parse `{n}`, `{n,}` or `{n,m}`. If the braces are not a valid bound,
    /// return `Ok(None)` and leave the cursor untouched — the caller then
    /// treats `{` as a literal.
    fn try_parse_bound(&mut self) -> Result<Option<Bound>, Error> {
        let save = self.pos;
        self.bump(); // consume '{'
        let lo = match self.parse_number() {
            Some(n) => n,
            None => {
                self.pos = save;
                return Ok(None);
            }
        };
        let hi = if self.eat(',') {
            // `{n,}` has an open upper bound: `parse_number` returns `None`
            // for the empty side, which means unbounded, *not* malformed.
            // `{n,` with no closing brace is malformed, and the `eat('}')`
            // below catches it by rewinding.
            self.parse_number()
        } else {
            Some(lo)
        };
        if !self.eat('}') {
            self.pos = save;
            return Ok(None);
        }
        Ok(Some(Bound { min: lo, max: hi }))
    }

    /// Parse a run of decimal digits as a `usize`, saturating rather than
    /// overflowing so that `a{999999999999999999999}` is an ordinary
    /// "never matches" bound instead of a panic.
    fn parse_number(&mut self) -> Option<usize> {
        let mut n: usize = 0;
        let mut any = false;
        while let Some(c) = self.peek() {
            if let Some(d) = c.to_digit(10) {
                n = n.saturating_mul(10).saturating_add(d as usize);
                any = true;
                self.bump();
            } else {
                break;
            }
        }
        if any {
            Some(n)
        } else {
            None
        }
    }

    /// Parse a single atom.
    fn parse_atom(&mut self) -> Result<Atom, Error> {
        let c = match self.bump() {
            Some(c) => c,
            None => return Err(Error::regex("unexpected end of regular expression")),
        };
        match c {
            '.' => Ok(Atom::Any),
            '^' => Ok(Atom::Bol),
            '$' => Ok(Atom::Eol),
            '(' => {
                self.enter()?;
                let branches = self.parse_branches()?;
                self.leave();
                if !self.eat(')') {
                    return Err(Error::regex("unmatched `(` in regular expression"));
                }
                Ok(Atom::Group(branches))
            }
            ')' => Err(Error::regex("unmatched `)` in regular expression")),
            '[' => self.parse_bracket(),
            '\\' => {
                // Control escapes expand to the character they name; every
                // other escaped character stands for itself. That is what
                // makes `\.` a literal dot and, importantly, `\w` a literal
                // `w` — POSIX ERE has no `\w`, and matching BWK awk and mawk
                // here is what keeps differential testing against them valid.
                //
                // A trailing backslash escapes nothing; the oracles treat it as
                // a literal backslash rather than erroring.
                let e = self.bump().unwrap_or('\\');
                Ok(Atom::Literal(match e {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    'f' => '\x0c',
                    'v' => '\x0b',
                    other => other,
                }))
            }
            '*' | '+' | '?' => {
                Err(Error::regex(format!("repetition operator `{c}` has nothing to repeat")))
            }
            _ => Ok(Atom::Literal(c)),
        }
    }

    /// Parse the body of a bracket expression. The opening `[` is consumed;
    /// the closing `]` is consumed on success.
    fn parse_bracket(&mut self) -> Result<Atom, Error> {
        let mut class = CharClass { named: None, singles: Vec::new(), ranges: Vec::new(), negated: false };

        // A `^` immediately after `[` negates; anywhere else it is a literal.
        if self.eat('^') {
            class.negated = true;
        }

        // A `]` as the very first character of the class is a literal, so
        // `[]]` is the class containing `]`. `first` must already be false by
        // the time the loop starts, otherwise the *closing* `]` of `[]]` would
        // be taken as another leading literal and the class would never close.
        if self.peek() == Some(']') {
            self.bump();
            class.singles.push(']');
        }

        let mut first = false;
        loop {
            let c = match self.peek() {
                Some(c) => c,
                None => return Err(Error::regex("unterminated `[` in regular expression")),
            };
            if c == ']' && !first {
                self.bump();
                break;
            }
            first = false;

            // `[:name:]` — only when the whole `[:...:]` is present. Otherwise
            // treat the characters literally, as the oracles do.
            if c == '[' && self.peek_at(1) == Some(':') {
                if let Some(named) = self.try_parse_named() {
                    class.named = Some(named);
                    continue;
                }
            }

            // Read one character of the class. There is no escape here.
            let lo = self.bump().unwrap_or(c);
            // `-` is literal unless it sits between two class members.
            if self.peek() == Some('-') {
                if let Some(next) = self.peek_at(1) {
                    if next != ']' {
                        self.bump(); // consume '-'
                        let hi = self.bump().unwrap_or(next);
                        if (hi as u32) < (lo as u32) {
                            return Err(Error::regex(format!(
                                "invalid range `{lo}-{hi}` in bracket expression"
                            )));
                        }
                        class.ranges.push((lo, hi));
                        continue;
                    }
                }
            }
            class.singles.push(lo);
        }

        // An empty `[]` or `[^]` is malformed; the oracles reject it.
        if class.named.is_none() && class.singles.is_empty() && class.ranges.is_empty() {
            return Err(Error::regex("empty bracket expression"));
        }

        Ok(Atom::Class(class))
    }

    /// Try to read a `[:name:]` sequence at the cursor. On success the cursor
    /// advances past it; on failure it is left untouched.
    fn try_parse_named(&mut self) -> Option<NamedClass> {
        let save = self.pos;
        self.bump(); // '['
        self.bump(); // ':'
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if c == ':' {
                break;
            }
            if c == ']' {
                // Malformed; `:]` never closed properly.
                self.pos = save;
                return None;
            }
            name.push(c);
            self.bump();
        }
        // Need `:]` to close.
        if self.peek() != Some(':') || self.peek_at(1) != Some(']') {
            self.pos = save;
            return None;
        }
        self.bump(); // ':'
        self.bump(); // ']'
        NamedClass::from_name(&name)
    }
}

/// An inclusive `{n}` / `{n,}` / `{n,m}` repetition bound.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bound {
    /// Minimum repetitions.
    pub(crate) min: usize,
    /// Maximum repetitions, or `None` for unbounded.
    pub(crate) max: Option<usize>,
}

/// How many times an atom may repeat.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RepeatKind {
    /// `*` — zero or more.
    Star,
    /// `+` — one or more.
    Plus,
    /// `?` — zero or one.
    Opt,
    /// `{n}` / `{n,}` / `{n,m}`.
    Bound(Bound),
}

impl Atom {
    /// True if this atom can consume at least one character along some path.
    ///
    /// This is the question that matters for repetition. A loop whose body can
    /// never consume is pure ceremony: it cannot lengthen a match, and at run
    /// time it re-enters itself without consuming, minting an unbounded number
    /// of thread states at a single position. A loop whose body *can* consume,
    /// such as `(a?b?)+`, must keep its loop — the body also matches empty, but
    /// iterating it still extends matches.
    pub(crate) fn can_consume(&self) -> bool {
        match self {
            Atom::Bol | Atom::Eol => false,
            Atom::Any | Atom::Literal(_) | Atom::Class(_) => true,
            Atom::Group(alternatives) => alternatives.iter().any(branch_can_consume),
            Atom::Repeat(inner, _) => inner.can_consume(),
        }
    }
}

/// True if a concatenation can consume at least one character on some path.
fn branch_can_consume(branch: &Branch) -> bool {
    branch.iter().any(|a| a.can_consume())
}

impl Atom {
    /// Attach a repetition operator to `self`.
    pub(crate) fn repeat(self, kind: RepeatKind) -> Atom {
        Atom::Repeat(Box::new(self), kind)
    }
}