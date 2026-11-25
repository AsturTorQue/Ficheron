//! Input records, output sinks, and the `getline` file machinery.

use crate::error::Error;
use crate::regex::Regex;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::rc::Rc;

/// How the input is cut into records.
#[derive(Debug, Clone)]
pub enum RecordSep {
    /// `RS` is a single character.
    Single(char),
    /// `RS` is unset — the default newline.
    Newline,
    /// `RS` is `""`: records are separated by blank lines (paragraph mode).
    Paragraph,
    /// `RS` is a regular expression.
    Pattern(Rc<Regex>),
}

impl RecordSep {
    /// Interpret an `RS` string.
    pub fn parse(rs: &str) -> Result<RecordSep, Error> {
        if rs.is_empty() {
            return Ok(RecordSep::Paragraph);
        }
        let mut chars = rs.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            // A lone newline is the default and gets a fast path.
            return Ok(if c == '\n' { RecordSep::Newline } else { RecordSep::Single(c) });
        }
        Ok(RecordSep::Pattern(Rc::new(Regex::new(rs)?)))
    }
}

/// One input stream.
pub struct Input {
    /// Name used for `FILENAME` and error messages.
    pub name: String,
    reader: Box<dyn BufRead>,
}

impl Input {
    /// Wrap a reader.
    pub fn new(name: impl Into<String>, reader: Box<dyn BufRead>) -> Input {
        Input { name: name.into(), reader }
    }

    /// Standard input.
    pub fn stdin() -> Input {
        Input::new("-", Box::new(BufReader::new(std::io::stdin())))
    }

    /// Open a file by name.
    pub fn open(path: &str) -> Result<Input, Error> {
        let f = File::open(path)
            .map_err(|e| Error::new(crate::error::ErrorKind::Io, format!("can't open {path}: {e}")))?;
        Ok(Input::new(path, Box::new(BufReader::new(f))))
    }

    /// Read the next record, honouring `sep`. Returns `None` at end of input.
    pub fn next_record(&mut self, sep: &RecordSep) -> Result<Option<String>, Error> {
        next_record(&mut self.reader, sep)
    }
}

/// Read one record from any buffered reader.
///
/// Shared by the main input and by files opened with `getline < file`, so both
/// honour `RS` identically.
pub(crate) fn next_record(r: &mut dyn BufRead, sep: &RecordSep) -> Result<Option<String>, Error> {
    match sep {
        RecordSep::Newline => read_until_char(r, b'\n', true),
        RecordSep::Single(c) => read_until_char(r, *c as u8, true),
        RecordSep::Paragraph => read_paragraph(r),
        RecordSep::Pattern(re) => read_pattern(r, re),
    }
}

/// Read up to and including `sep`, dropping the separator unless told not to.
fn read_until_char(r: &mut dyn BufRead, sep: u8, strip: bool) -> Result<Option<String>, Error> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let byte = r.fill_buf()?;
        if byte.is_empty() {
            return Ok(if out.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&out).into_owned())
            });
        }
        match byte.iter().position(|&b| b == sep) {
            Some(i) => {
                out.extend_from_slice(&byte[..i]);
                r.consume(i + 1);
                if !strip {
                    out.push(sep);
                }
                return Ok(Some(String::from_utf8_lossy(&out).into_owned()));
            }
            None => {
                let n = byte.len();
                out.extend_from_slice(byte);
                r.consume(n);
            }
        }
    }
}

/// Read a record terminated by a regular expression.
fn read_pattern(r: &mut dyn BufRead, re: &Regex) -> Result<Option<String>, Error> {
    let mut buf = String::new();
    loop {
        let byte = r.fill_buf()?;
        if byte.is_empty() {
            return Ok(if buf.is_empty() { None } else { Some(buf) });
        }
        // Append one character and test, so multi-byte characters stay intact.
        let len = utf8_seq_len(byte[0]);
        let take = len.min(byte.len());
        let s = String::from_utf8_lossy(&byte[..take]).into_owned();
        r.consume(take);
        let at = buf.len();
        buf.push_str(&s);
        if re.match_at(&buf, at) {
            return Ok(Some(buf));
        }
    }
}

/// Paragraph mode: a record ends at a blank line, which stays attached to it.
fn read_paragraph(r: &mut dyn BufRead) -> Result<Option<String>, Error> {
    let mut para = String::new();
    loop {
        let line = match read_until_char(r, b'\n', true)? {
            Some(l) => l,
            None => return Ok(if para.is_empty() { None } else { Some(para) }),
        };
        let blank = line.trim().is_empty();
        para.push_str(&line);
        if blank {
            return Ok(Some(std::mem::take(&mut para)));
        }
    }
}

/// Length of the UTF-8 sequence starting with `b`.
fn utf8_seq_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// A file opened by `getline < file`.
pub struct FileHandle {
    /// The buffered reader, so a partially-consumed file keeps its position
    /// across successive `getline` calls.
    pub reader: BufReader<File>,
}

/// Where a program's output goes.
pub enum Sink {
    /// Write to a real stream — usually stdout.
    Stream(Box<dyn Write>),
    /// Collect into a buffer, for output capture.
    Buffer(Vec<u8>),
}

impl Sink {
    /// Write a string.
    pub fn write_str(&mut self, s: &str) -> Result<(), Error> {
        match self {
            Sink::Stream(w) => w.write_all(s.as_bytes())?,
            Sink::Buffer(b) => b.extend_from_slice(s.as_bytes()),
        }
        Ok(())
    }

    /// Flush, if the sink is a real stream.
    pub fn flush(&mut self) -> Result<(), Error> {
        if let Sink::Stream(w) = self {
            w.flush()?;
        }
        Ok(())
    }

    /// Borrow the captured bytes so far.
    pub fn buffer(&self) -> Option<&str> {
        match self {
            Sink::Buffer(b) => Some(std::str::from_utf8(b).unwrap_or("")),
            Sink::Stream(_) => None,
        }
    }
}

/// Files opened for output by `print > "f"`.
pub type OutputFiles = HashMap<String, File>;