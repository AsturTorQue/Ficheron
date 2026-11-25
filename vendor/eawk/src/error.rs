//! Error types for eawk.
//!
//! Everything that can go wrong in an awk program surfaces as an [`Error`]. The
//! library never panics on bad input from a program or from a host — a host
//! embedding eawk cannot catch a panic without poisoning its own state.

use std::fmt;
use std::sync::Arc;

/// The category of a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Syntax error in the awk source.
    Parse,
    /// Semantically invalid program: unknown function, bad regex literal, etc.
    Compile,
    /// Error while running: division by zero, bad field index, etc.
    Runtime,
    /// A dynamic (non-literal) regex failed to compile at run time.
    Regex,
    /// I/O failure opening or reading a file.
    Io,
    /// The program called `exit` with a non-zero status.
    Exit,
}

/// An eawk failure, carrying enough context to point at the offending source.
#[derive(Debug, Clone)]
pub struct Error {
    /// What kind of failure this is.
    pub kind: ErrorKind,
    /// Human-readable description.
    pub message: String,
    /// Name of the file being executed, if known.
    pub file: Option<Arc<str>>,
    /// 1-based line number, if known.
    pub line: Option<usize>,
    /// 1-based column, if known.
    pub column: Option<usize>,
}

impl Error {
    /// Build an error of the given kind with no source position.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error { kind, message: message.into(), file: None, line: None, column: None }
    }

    /// Syntax error in the program text.
    pub fn parse(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Parse, message)
    }

    /// Semantically invalid program.
    pub fn compile(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Compile, message)
    }

    /// Error raised while executing the program.
    pub fn runtime(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Runtime, message)
    }

    /// A regex failed to compile.
    pub fn regex(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Regex, message)
    }

    /// The program requested a non-zero exit status.
    pub fn exit(status: i32) -> Error {
        Error::new(ErrorKind::Exit, format!("exit status {status}"))
    }

    /// Attach a 1-based source position.
    pub fn at(mut self, line: usize, column: usize) -> Error {
        self.line = Some(line);
        self.column = Some(column);
        self
    }

    /// Attach a source line, if one is known.
    pub fn at_line(mut self, line: usize) -> Error {
        self.line = Some(line);
        self
    }

    /// Attach a source file name.
    pub fn in_file(mut self, file: Option<Arc<str>>) -> Error {
        self.file = file;
        self
    }

    /// The exit status carried by an [`ErrorKind::Exit`] error.
    pub fn exit_status(&self) -> Option<i32> {
        if self.kind != ErrorKind::Exit {
            return None;
        }
        self.message
            .strip_prefix("exit status ")
            .and_then(|s| s.parse().ok())
    }

    /// True if this error is the program calling `exit`.
    pub fn is_exit(&self) -> bool {
        self.kind == ErrorKind::Exit
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "eawk: {}", self.message)?;
        if let Some(file) = &self.file {
            write!(f, " in {file}")?;
        }
        if let Some(line) = self.line {
            write!(f, " at line {line}")?;
            if let Some(col) = self.column {
                write!(f, ", column {col}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::new(ErrorKind::Io, e.to_string())
    }
}

impl From<std::string::FromUtf8Error> for Error {
    fn from(e: std::string::FromUtf8Error) -> Error {
        Error::new(ErrorKind::Io, format!("invalid UTF-8: {e}"))
    }
}