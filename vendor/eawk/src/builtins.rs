//! Built-in functions and variables.
//!
//! `sub`, `gsub` and `split` are handled before arguments are evaluated,
//! because they have to write back into a variable, a field, or a named array
//! and so need the argument expressions themselves rather than their values.

use crate::ast::Expr;
use crate::error::Error;
use crate::fields::{split, FieldSep};
use crate::format::sprintf;
use crate::interp::{Bail, Ctl, Engine};
use crate::regex::Regex;
use crate::value::{parse_awk_number, ArrayKey, Value};
use std::rc::Rc;
use std::sync::Arc;

/// How many arguments a builtin takes, or a range.
fn arity(name: &str) -> Option<(usize, usize)> {
    Some(match name {
        "length" => (0, 1),
        "substr" => (2, 3),
        "index" => (2, 2),
        "sub" | "gsub" => (2, 3),
        "match" => (2, 2),
        "split" => (2, 3),
        "sprintf" => (1, usize::MAX),
        "tolower" | "toupper" => (1, 1),
        "sin" | "cos" | "atan" | "exp" | "log" | "sqrt" | "int" | "rand" | "srand" => (0, 1),
        "atan2" | "fmod" => (2, 2),
        "system" => (0, 1),
        "close" | "fflush" => (0, 1),
        _ => return None,
    })
}

impl Engine {
    /// Call a built-in that operates purely on values. Returns `None` if
    /// `name` is not a builtin, so the caller can fall through to user code.
    pub(crate) fn builtin(&mut self, name: &str, args: &[Value]) -> Ctl<Option<Value>> {
        if arity(name).is_none() {
            return Ok(None);
        }
        let (lo, hi) = arity(name).unwrap();
        if args.len() < lo || args.len() > hi {
            return Err(Error::runtime(format!(
                "{name} called with {} argument{}",
                args.len(),
                if args.len() == 1 { "" } else { "s" }
            ))
            .into());
        }
        let convfmt = self.convfmt();
        let s = |v: &Value| v.to_str(&convfmt).to_string();
        let n = |v: &Value| v.to_num();

        let out = match name {
            "length" => {
                // length(array) is a common extension; the argument arrives as
                // the array's name when the caller wrote `length(a)`.
                match args.first() {
                    None => Value::Num(self.rec.line.chars().count() as f64),
                    Some(v) => match &v {
                        Value::Str(name) if self.is_array(name) => {
                            Value::Num(self.arrays.get(&**name).map_or(0, |a| a.len()) as f64)
                        }
                        other => {
                            // The numeric argument uses its string form, so
                            // length(12345) is 5.
                            Value::Num(other.to_str(&convfmt).chars().count() as f64)
                        }
                    },
                }
            }
            "substr" => {
                let text = s(&args[0]);
                let start = args[1].to_num().trunc();
                // awk rounds the start and clamps rather than erroring.
                let from = start.max(1.0).min(text.chars().count() as f64 + 1.0) as usize;
                let begin = char_offset(&text, from - 1);
                let end_byte = match args.get(2) {
                    Some(v) => {
                        let len = v.to_num().trunc();
                        if len < 0.0 {
                            begin
                        } else {
                            char_offset(&text, from - 1 + len as usize)
                        }
                    }
                    None => text.len(),
                };
                let end_byte = end_byte.max(begin).min(text.len());
                Value::str(&text[begin..end_byte])
            }
            "index" => {
                let hay = s(&args[0]);
                let needle = s(&args[1]);
                Value::Num(match hay.find(&needle) {
                    Some(b) => hay[..b].chars().count() as f64 + 1.0,
                    None => 0.0,
                })
            }
            "match" => {
                let text = s(&args[0]);
                let pat = s(&args[1]);
                let re = self.regex(&pat)?;
                match re.find(&text, 0) {
                    Some(m) => {
                        let rs = text[..m.start].chars().count() as f64 + 1.0;
                        let rl = text[m.start..m.end].chars().count() as f64;
                        self.globals.insert(Rc::from("RSTART"), Value::Num(rs));
                        self.globals.insert(Rc::from("RLENGTH"), Value::Num(rl));
                        Value::Num(rs)
                    }
                    None => {
                        self.globals.insert(Rc::from("RSTART"), Value::Num(0.0));
                        self.globals.insert(Rc::from("RLENGTH"), Value::Num(-1.0));
                        Value::Num(0.0)
                    }
                }
            }
            "sprintf" => {
                let fmt = s(&args[0]);
                Value::str(sprintf(&fmt, &args[1..], &convfmt)?)
            }
            "tolower" => Value::str(s(&args[0]).to_lowercase()),
            "toupper" => Value::str(s(&args[0]).to_uppercase()),
            "sin" => Value::Num(n(&args[0]).sin()),
            "cos" => Value::Num(n(&args[0]).cos()),
            "atan" => Value::Num(n(&args[0]).atan()),
            "atan2" => Value::Num(n(&args[0]).atan2(n(&args[1]))),
            "exp" => Value::Num(n(&args[0]).exp()),
            "log" => Value::Num(n(&args[0]).ln()),
            "sqrt" => Value::Num(n(&args[0]).sqrt()),
            "int" => Value::Num(n(&args[0]).trunc()),
            "fmod" => Value::Num(n(&args[0]) % n(&args[1])),
            "rand" => Value::Num(self.next_rand()),
            "srand" => {
                let old = self.rng;
                self.rng = match args.first() {
                    Some(v) => v.to_num() as u64 ^ 0x9E37_79B9_7F4A_7C15,
                    None => 0x2545_F491_4F6C_DD1D,
                };
                Value::Num((old & 0x7fff_ffff) as f64)
            }
            "system" => {
                let cmd = args.first().map_or_else(String::new, s);
                Value::Num(self.run_system(&cmd)?)
            }
            "close" | "fflush" => Value::Num(0.0),
            _ => return Ok(None),
        };
        Ok(Some(out))
    }

    /// `sub(re, repl[, target])` and `gsub(...)`.
    ///
    /// The two-argument form acts on `$0`, which is the idiomatic
    /// `{ gsub(/,/,"") }`.
    pub(crate) fn call_subst(&mut self, global: bool, args: &[Rc<Expr>]) -> Ctl<Value> {
        if args.len() < 2 || args.len() > 3 {
            return Err(Error::runtime(if global {
                "gsub called with wrong number of arguments"
            } else {
                "sub called with wrong number of arguments"
            })
            .into());
        }
        let convfmt = self.convfmt();
        let pat = self.eval(&args[0])?.to_str(&convfmt).to_string();
        let repl = self.eval(&args[1])?.to_str(&convfmt).to_string();
        let re = self.regex(&pat)?;

        let (text, had_target) = if args.len() == 3 {
            (self.eval(&args[2])?.to_str(&convfmt).to_string(), true)
        } else {
            (self.rec.line.to_string(), false)
        };

        let (result, count) = substitute(&re, &text, &repl, global);

        if count > 0 {
            if had_target {
                // Assigning through the expression keeps `$n = ...` rebuilding
                // `$0`, which the caller expects.
                self.assign_to(&args[2], Value::str(&result))?;
            } else {
                let sep = self.field_sep()?;
                self.rec.set_line(Arc::from(result.as_str()), &sep);
            }
        }
        Ok(Value::Num(count as f64))
    }

    /// `split(s, array[, fs])`. The array is named, not passed by value.
    pub(crate) fn call_split(&mut self, args: &[Rc<Expr>]) -> Ctl<Value> {
        if args.len() < 2 || args.len() > 3 {
            return Err(Error::runtime("split called with wrong number of arguments").into());
        }
        let convfmt = self.convfmt();
        let text = self.eval(&args[0])?.to_str(&convfmt).to_string();

        let arr_name = match &*args[1] {
            Expr::Var(n) => n.clone(),
            Expr::Grouping(g) => match &**g {
                Expr::Var(n) => n.clone(),
                _ => return Err(Error::runtime("split's second argument must be an array").into()),
            },
            _ => return Err(Error::runtime("split's second argument must be an array").into()),
        };

        let sep = match args.get(2) {
            Some(e) => FieldSep::parse(&self.eval(e)?.to_str(&convfmt))?,
            None => self.field_sep()?,
        };

        let mut parts = split(&text, &sep);
        // Trailing empty fields *are* produced — split("a:b:", p, ":") gives 3
        // in BWK, gawk and mawk alike. Only an empty string yields nothing.
        if text.is_empty() {
            parts.clear();
        }

        let mut out = std::collections::HashMap::new();
        // awk arrays are 1-based: `split(s, a)` fills a[1], a[2], ...
        for (i, p) in parts.iter().enumerate() {
            out.insert(ArrayKey::from_num(i as f64 + 1.0), Value::StrNum(p.clone(), parse_awk_number(p)));
        }
        let n = out.len();
        self.arrays.insert(arr_name, out);
        Ok(Value::Num(n as f64))
    }

    /// The next value from a small xorshift generator.
    pub(crate) fn next_rand(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 11) as f64 / ((1u64 << 53) as f64)
    }

    /// Run a shell command, returning its exit status.
    pub(crate) fn run_system(&mut self, cmd: &str) -> Result<f64, Bail> {
        use std::process::Command;
        // Patched for Windows (upstream only builds on Unix): `cmd /C` there, no signals.
        #[cfg(windows)]
        let status = Command::new("cmd").arg("/C").arg(cmd).status();
        #[cfg(not(windows))]
        let status = Command::new("sh").arg("-c").arg(cmd).status();
        match status {
            Ok(st) => {
                #[cfg(unix)]
                use std::os::unix::process::ExitStatusExt;
                let code: i32 = match st.code() {
                    Some(c) => c,
                    #[cfg(unix)]
                    None => st.signal().unwrap_or(0),
                    #[cfg(not(unix))]
                    None => 0,
                };
                Ok(code as f64)
            }
            Err(e) => Err(Error::new(crate::error::ErrorKind::Io, format!("system: {e}")).into()),
        }
    }
}

/// Byte offset of character index `i`, clamped to the string.
fn char_offset(s: &str, i: usize) -> usize {
    match s.char_indices().nth(i) {
        Some((b, _)) => b,
        None => s.len(),
    }
}

/// Apply a replacement to `text`, expanding `&` to the matched text and `\&`
/// to a literal ampersand.
///
/// The empty-match case advances by one character. Skipping that is how a naive
/// `gsub(/x*/, "-")` spins forever.
fn substitute(re: &Regex, text: &str, repl: &str, global: bool) -> (String, usize) {
    let mut out = String::new();
    let mut pos = 0usize;
    let mut count = 0usize;

    while pos <= text.len() {
        let Some(m) = re.find(text, pos) else { break };
        out.push_str(&text[pos..m.start]);
        out.push_str(&expand(repl, &text[m.start..m.end]));
        count += 1;

        if m.end > m.start {
            pos = m.end;
        } else {
            let Some(ch) = text[m.end..].chars().next() else { break };
            out.push(ch);
            pos = m.end + ch.len_utf8();
        }
        if !global {
            break;
        }
    }
    if pos < text.len() {
        out.push_str(&text[pos..]);
    }
    (out, count)
}

/// Expand `&` and `\&` in a replacement string.
fn expand(repl: &str, matched: &str) -> String {
    let mut out = String::new();
    let mut chars = repl.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('&') => {
                    chars.next();
                    out.push('&');
                }
                _ => out.push('\\'),
            }
        } else if c == '&' {
            out.push_str(matched);
        } else {
            out.push(c);
        }
    }
    out
}