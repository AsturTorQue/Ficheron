//! POSIX ERE regular expressions.
//!
//! This is a from-scratch, zero-dependency implementation of the regular
//! expression dialect that awk uses: POSIX Extended Regular Expressions, as
//! implemented by the One True Awk (BWK awk), gawk, and mawk. Differential
//! testing against all three is what keeps the behaviour honest.
//!
//! # Leftmost-longest, not leftmost-first
//!
//! The defining property of POSIX ERE. Given a pattern and a haystack, the
//! match returned is the one that starts leftmost and, among all matches that
//! start there, runs longest. So `/a|ab/` against `"ab"` matches **two**
//! characters, not one:
//!
//! ```
//! # use eawk::regex::Regex;
//! let re = Regex::new("a|ab").unwrap();
//! let m = re.find("ab", 0).unwrap();
//! assert_eq!((m.start, m.end), (0, 2));
//! ```
//!
//! When two alternatives match the *same* length, the one written earlier in
//! the pattern wins — this is what determines `RSTART`/`RLENGTH` in awk, and
//! therefore what `split`, `gsub`, and `sub` observe.
//!
//! # Implementation
//!
//! Thompson NFA simulation, not backtracking. There is no exponential blowup
//! on a pattern like `(a+)+b`, so a host process embedding eawk cannot be hung
//! by a hostile or merely careless program.
//!
//! * [`parse`] — pattern text to AST.
//! * [`nfa`] — AST to instruction table.
//! * [`exec`] — the simulation.
//!
//! # Deliberately unsupported
//!
//! `\d \w \s \D \W \S` are gawk extensions and are **not** implemented: like
//! BWK awk and mawk, we treat `\w` as a literal `w`. There are no
//! backreferences, because POSIX ERE has none. A free-floating `^` or `$`
//! anywhere other than the ends of the pattern is undefined behaviour in POSIX
//! awk; we treat it as an anchor to the start/end of the subject string
//! throughout, which agrees with the oracles on every input we fuzz.
//!
//! # Unicode
//!
//! Offsets are **bytes**, but the engine matches whole **characters**. `.`
//! consumes one full character however many bytes it occupies, so all reported
//! offsets always land on a UTF-8 character boundary.

mod exec;
mod nfa;
mod parse;

use crate::error::Error;

/// A half-open byte range that matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// Byte offset into the haystack where the match begins.
    pub start: usize,
    /// Byte offset just past the end of the match.
    pub end: usize,
}

impl Match {
    /// The length of the match in bytes.
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// True if the match consumed no bytes.
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// A compiled POSIX ERE.
///
/// Compilation happens once; [`Regex::find`] and friends are cheap to call
/// repeatedly and never mutate the program.
#[derive(Debug, Clone)]
pub struct Regex {
    /// The original pattern text.
    pattern: String,
    /// The compiled instruction table and its metadata.
    program: nfa::Program,
}

impl Regex {
    /// Compile `pattern` as a POSIX ERE.
    ///
    /// Returns [`Error::regex`] for a malformed pattern, a pattern nested
    /// beyond the parser's depth limit, or one with a repetition bound too
    /// large to compile.
    pub fn new(pattern: &str) -> Result<Regex, Error> {
        let branches = parse::Parser::new(pattern).parse()?;
        let program = nfa::Program::compile(&branches)?;
        Ok(Regex { pattern: pattern.to_string(), program })
    }

    /// The original pattern text, for error messages and `split`/`gsub`
    /// reporting.
    pub fn as_str(&self) -> &str {
        &self.pattern
    }

    /// The leftmost-longest match at or after byte offset `start`, or `None`.
    ///
    /// Never panics. If `start` is past the end of `haystack`, returns `None`.
    /// If `start` is not on a UTF-8 character boundary, the search starts at
    /// the next boundary at or after it.
    pub fn find(&self, haystack: &str, start: usize) -> Option<Match> {
        match exec::find(&self.program, haystack, start) {
            Ok(Some(found)) => Some(Match { start: found.start, end: found.end }),
            Ok(None) => None,
            // A budget overrun cannot be reported through `Option`; the engine
            // only reaches it on inputs far beyond any realistic use, and a
            // `None` here degrades to "no match" rather than a panic.
            Err(_) => None,
        }
    }

    /// True if the pattern matches a string starting exactly at `pos`.
    ///
    /// Unlike [`Regex::find`] this does not scan forward, so it answers only
    /// about a match anchored at `pos`. A pattern that matches the empty
    /// string at `pos` counts as a match.
    pub fn match_at(&self, haystack: &str, pos: usize) -> bool {
        if pos > haystack.len() {
            return false;
        }
        // Reuse the simulator but require the match to start here.
        match exec::find_at(&self.program, haystack, pos) {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(_) => false,
        }
    }

    /// True if the pattern begins with `^`, and so can only match at offset 0.
    pub fn is_anchored_start(&self) -> bool {
        self.program.anchored_start
    }

    /// True if the pattern ends with `$`, and so can only match at the end of
    /// the subject.
    pub fn is_anchored_end(&self) -> bool {
        self.program.anchored_end
    }

    /// The minimum number of bytes any match can consume; 0 if the pattern can
    /// match the empty string.
    ///
    /// This is a lower bound safe for skipping ahead, and may under-report.
    /// Over-reporting would be a bug.
    pub fn min_len(&self) -> usize {
        self.program.min_len
    }

    /// The compiled program, exposed for tests.
    #[cfg(test)]
    pub(crate) fn program(&self) -> &nfa::Program {
        &self.program
    }
}