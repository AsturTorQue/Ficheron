//! Record splitting and the `$0`/`$1`/`NF` invariants.
//!
//! The fiddly parts, all of which show up constantly in real scripts:
//!
//! * Assigning to `$0` re-splits the record.
//! * Assigning to `$n` past the end extends the record, growing `NF`.
//! * Assigning `NF` truncates or extends and rebuilds `$0`.
//! * Rebuilding `$0` joins with `OFS`, so a record that grew a field keeps its
//!   separators.

use crate::error::Error;
use crate::regex::Regex;
use std::rc::Rc;
use std::sync::Arc;

/// How a record is split into fields.
#[derive(Debug, Clone)]
pub enum FieldSep {
    /// `FS` is `" "` or unset: runs of blanks and tabs, with leading and
    /// trailing whitespace discarded.
    Whitespace,
    /// `FS` is `""`: every character is its own field.
    Chars,
    /// `FS` is a single character other than space, so it matches literally —
    /// `FS="."` is a dot, not "any character".
    Single(char),
    /// Anything longer is a regular expression.
    Pattern(Rc<Regex>),
}

impl FieldSep {
    /// Interpret an `FS` string.
    pub fn parse(fs: &str) -> Result<FieldSep, Error> {
        if fs == " " {
            return Ok(FieldSep::Whitespace);
        }
        if fs.is_empty() {
            return Ok(FieldSep::Chars);
        }
        let mut chars = fs.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            return Ok(FieldSep::Single(c));
        }
        Ok(FieldSep::Pattern(Rc::new(Regex::new(fs)?)))
    }
}

/// Split a record into fields.
pub fn split(line: &str, sep: &FieldSep) -> Vec<Arc<str>> {
    match sep {
        FieldSep::Whitespace => line.split_whitespace().map(Arc::from).collect(),
        FieldSep::Chars => {
            line.chars().map(|c| Arc::from(c.to_string().as_str())).collect()
        }
        FieldSep::Single(c) => line.split(*c).map(Arc::from).collect(),
        FieldSep::Pattern(re) => split_pattern(line, re),
    }
}

/// Split on a regular expression. An empty match is skipped rather than
/// producing an unbounded run of empty fields.
fn split_pattern(line: &str, re: &Regex) -> Vec<Arc<str>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut pos = 0usize;
    while pos <= line.len() {
        let Some(m) = re.find(line, pos) else { break };
        if m.end > m.start {
            out.push(Arc::from(&line[start..m.start]));
            start = m.end;
            pos = m.end;
        } else {
            // Zero-width match: step over one character so we make progress.
            pos = next_char_boundary(line, m.end);
            if pos > line.len() {
                break;
            }
        }
    }
    out.push(Arc::from(&line[start..]));
    out
}

/// The next character boundary at or after `i`, for advancing past an
/// empty regex match without splitting a UTF-8 sequence.
fn next_char_boundary(s: &str, i: usize) -> usize {
    let mut j = i;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    (j + 1).min(s.len().max(i)).max(i.min(s.len()))
}

/// The current record: `$0` and the fields derived from it.
#[derive(Debug, Clone, Default)]
pub struct Record {
    /// `$0`.
    pub line: Arc<str>,
    /// `$1` through `$NF`.
    pub fields: Vec<Arc<str>>,
}

impl Record {
    /// Set `$0` and re-split.
    pub fn set_line(&mut self, line: Arc<str>, sep: &FieldSep) {
        self.fields = split(&line, sep);
        self.line = line;
    }

    /// Read a field. Index 0 is `$0`.
    pub fn get(&self, i: usize) -> Arc<str> {
        if i == 0 {
            self.line.clone()
        } else {
            self.fields.get(i - 1).cloned().unwrap_or_else(|| Arc::from(""))
        }
    }

    /// The value of `$i` as an awk value. Fields are strnums: they came from
    /// input, so they compare numerically but keep their text.
    pub fn get_value(&self, i: usize) -> crate::value::Value {
        let s = self.get(i);
        let n = crate::value::parse_awk_number(&s);
        crate::value::Value::StrNum(s, n)
    }

    /// Assign to `$i` and rebuild `$0`. Assigning past the end grows the
    /// record, which also grows `NF`.
    pub fn set_field(&mut self, i: usize, v: Arc<str>, ofs: &str) {
        if i == 0 {
            return; // handled by `set_line`
        }
        if self.fields.len() < i {
            self.fields.resize(i, Arc::from(""));
        }
        self.fields[i - 1] = v;
        self.rebuild(ofs);
    }

    /// Set `NF`, truncating or padding with empty fields, then rebuild `$0`.
    pub fn set_nf(&mut self, n: usize, ofs: &str) {
        self.fields.resize(n, Arc::from(""));
        self.rebuild(ofs);
    }

    /// Rebuild `$0` from the fields using `OFS`.
    pub fn rebuild(&mut self, ofs: &str) {
        let mut s = String::new();
        for (i, f) in self.fields.iter().enumerate() {
            if i > 0 {
                s.push_str(ofs);
            }
            s.push_str(f);
        }
        self.line = Arc::from(s.as_str());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_split_drops_empty_fields() {
        let sep = FieldSep::parse(" ").unwrap();
        let f = split("  a  b  c  ", &sep);
        assert_eq!(f.iter().map(|s| &**s).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(split("", &sep).is_empty());
    }

    #[test]
    fn single_char_is_literal() {
        // `.` must be a dot, not "any character".
        let sep = FieldSep::parse(".").unwrap();
        assert_eq!(split("a.b", &sep).len(), 2);
        let sep = FieldSep::parse(",").unwrap();
        assert_eq!(split("a,b,,c", &sep).iter().map(|s| &**s).collect::<Vec<_>>(), ["a", "b", "", "c"]);
    }

    #[test]
    fn regex_split_handles_leading_separator() {
        // A record beginning with the separator has an empty first field.
        let sep = FieldSep::parse(",,").unwrap();
        let f = split(",,a,,b", &sep);
        assert_eq!(f.iter().map(|s| &**s).collect::<Vec<_>>(), ["", "a", "b"]);
        // With no separator present the whole record is one field.
        let f = split(",a,b", &sep);
        assert_eq!(f.iter().map(|s| &**s).collect::<Vec<_>>(), [",a,b"]);
    }

    #[test]
    fn empty_fs_splits_by_character() {
        let sep = FieldSep::parse("").unwrap();
        assert_eq!(split("abc", &sep).len(), 3);
    }

    #[test]
    fn assigning_past_the_end_grows_nf() {
        let mut r = Record::default();
        r.set_line(Arc::from("a b c"), &FieldSep::Whitespace);
        r.set_field(5, Arc::from("e"), " ");
        assert_eq!(r.fields.len(), 5);
        assert_eq!(&*r.line, "a b c  e");
    }

    #[test]
    fn setting_nf_truncates_and_rebuilds() {
        let mut r = Record::default();
        r.set_line(Arc::from("a b c"), &FieldSep::Whitespace);
        r.set_nf(1, " ");
        assert_eq!(r.fields.len(), 1);
        assert_eq!(&*r.line, "a");
    }
}