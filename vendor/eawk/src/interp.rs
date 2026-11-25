//! The tree-walking interpreter.
//!
//! Control flow never uses panics. `Flow` carries `next`, `break`, `continue`,
//! `return` and `exit` out to the statement that knows what to do with them, and
//! `Bail` carries a `Flow` or an `Error` so that a single `Result` can express
//! both. A host embedding eawk cannot be brought down by a bad program.

use crate::ast::*;
use crate::error::Error;
use crate::fields::{FieldSep, Record};
use crate::format::sprintf;
use crate::io::{FileHandle, Input, OutputFiles, RecordSep, Sink};
use crate::regex::Regex;
use crate::value::{compare, format_number, ArrayKey, Value, DEFAULT_FMT};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufReader, Write};
use std::rc::Rc;
use std::sync::Arc;

/// What happens on division or modulo by zero.
///
/// BWK awk and mawk both treat this as fatal; gawk produces `inf`. A host
/// running awk over a data pipeline needs to choose, so it is a policy rather
/// than a hardcoded behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivZeroPolicy {
    /// Raise an error. The default, matching BWK awk and mawk.
    Fatal,
    /// Produce `inf` or `nan`, matching gawk.
    Inf,
}

/// A non-local control transfer.
pub(crate) enum Flow {
    /// `next` — stop processing this record.
    Next,
    /// `break`
    Break,
    /// `continue`
    Continue,
    /// `return expr`
    Return(Value),
    /// `exit` — stop, but still run `END`.
    Exit,
}

/// The error type used inside the interpreter.
pub(crate) enum Bail {
    /// A control-flow signal.
    Flow(Flow),
    /// A real failure.
    Err(Error),
}

/// The interpreter's result type.
pub(crate) type Ctl<T> = Result<T, Bail>;

impl From<Error> for Bail {
    fn from(e: Error) -> Bail {
        Bail::Err(e)
    }
}

impl From<Flow> for Bail {
    fn from(f: Flow) -> Bail {
        Bail::Flow(f)
    }
}

impl Bail {
    /// The error, if this is not a control-flow signal.
    fn into_error(self) -> Option<Error> {
        match self {
            Bail::Err(e) => Some(e),
            Bail::Flow(_) => None,
        }
    }
}

/// One function call's parameters. Extra parameters beyond those supplied are
/// the function's locals, per the usual awk convention.
pub(crate) struct Frame {
    params: HashMap<Rc<str>, Value>,
}

/// A Rust function callable from awk.
pub type NativeFn = Rc<dyn Fn(&mut crate::Ctx, Vec<Value>) -> Result<Value, Error>>;

/// Execution options.
#[derive(Debug, Clone)]
pub struct Options {
    /// Initial value for `FS`.
    pub fs: Option<String>,
    /// Initial value for `OFS`.
    pub ofs: Option<String>,
    /// `BEGIN`-time assignments, as if from `-v name=value`.
    pub assignments: Vec<(String, String)>,
    /// Division-by-zero behaviour.
    pub div_zero: DivZeroPolicy,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            fs: None,
            ofs: None,
            assignments: Vec::new(),
            div_zero: DivZeroPolicy::Fatal,
        }
    }
}

/// A parsed program plus one run's worth of mutable state.
pub struct Engine {
    pub(crate) prog: Rc<Program>,
    pub(crate) opts: Options,

    pub(crate) globals: HashMap<Rc<str>, Value>,
    pub(crate) arrays: HashMap<Rc<str>, HashMap<ArrayKey, Value>>,
    pub(crate) frames: Vec<Frame>,

    pub(crate) rec: Record,
    pub(crate) inputs: Vec<Input>,
    /// Which input is currently being read; advances as files are exhausted.
    pub(crate) input_pos: usize,
    pub(crate) getfiles: HashMap<String, FileHandle>,
    pub(crate) outfiles: OutputFiles,
    pub(crate) sink: Sink,

    pub(crate) exit_code: Option<i32>,
    pub(crate) regexes: HashMap<Rc<str>, Rc<Regex>>,
    pub(crate) natives: HashMap<Rc<str>, NativeFn>,
    pub(crate) rng: u64,

    /// Per-rule state for `pat1, pat2` ranges.
    pub(crate) range_active: Vec<bool>,
    pub(crate) began: bool,
    pub(crate) ended: bool,
    pub(crate) call_depth: usize,

    /// Cached `FS` / `RS` interpretation, invalidated when they are assigned.
    pub(crate) fs_cache: Option<(String, FieldSep)>,
    pub(crate) rs_cache: Option<(String, RecordSep)>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("globals", &self.globals.len())
            .field("arrays", &self.arrays.len())
            .field("frames", &self.frames.len())
            .field("record", &self.rec.line)
            .field("inputs", &self.inputs.len())
            .finish()
    }
}

/// Guards against runaway recursion in user functions.
///
/// Without it, a function that calls itself forever overflows the stack, and a
/// stack overflow aborts the process — it cannot be caught by `Result`, and a
/// host embedding eawk has no way to survive one. Each awk call level costs
/// roughly ten interpreter frames carrying sizeable values. Debug builds use
/// markedly larger frames than release, so the value was measured rather than
/// guessed: a hundred levels overflows a default-sized worker thread in a debug
/// build, fifty does not. That is still far more than real awk recursion needs
/// — `fib(25)` uses twenty-five levels. A host that needs deeper recursion can
/// run eawk on a thread with a larger stack.
const MAX_CALL_DEPTH: usize = 50;

impl Engine {
    /// Build an engine around a parsed program.
    pub(crate) fn from_program(prog: Rc<Program>, opts: Options) -> Result<Engine, Error> {
        let nrules = prog.items.len();
        let mut e = Engine {
            prog,
            opts,
            globals: HashMap::new(),
            arrays: HashMap::new(),
            frames: Vec::new(),
            rec: Record::default(),
            inputs: Vec::new(),
            input_pos: 0,
            getfiles: HashMap::new(),
            outfiles: HashMap::new(),
            sink: Sink::Stream(Box::new(std::io::stdout())),
            exit_code: None,
            regexes: HashMap::new(),
            natives: HashMap::new(),
            rng: 0x2545_F491_4F6C_DD1D,
            range_active: vec![false; nrules],
            began: false,
            ended: false,
            call_depth: 0,
            fs_cache: None,
            rs_cache: None,
        };

        e.globals.insert(Rc::from("FS"), Value::str(" "));
        e.globals.insert(Rc::from("OFS"), Value::str(" "));
        e.globals.insert(Rc::from("ORS"), Value::str("\n"));
        e.globals.insert(Rc::from("RS"), Value::str("\n"));
        e.globals.insert(Rc::from("SUBSEP"), Value::str("\u{1c}"));
        e.globals.insert(Rc::from("CONVFMT"), Value::str(DEFAULT_FMT));
        e.globals.insert(Rc::from("OFMT"), Value::str(DEFAULT_FMT));
        e.globals.insert(Rc::from("NR"), Value::Num(0.0));
        e.globals.insert(Rc::from("FNR"), Value::Num(0.0));
        e.globals.insert(Rc::from("RSTART"), Value::Num(0.0));
        e.globals.insert(Rc::from("RLENGTH"), Value::Num(-1.0));

        if let Some(fs) = e.opts.fs.clone() {
            e.globals.insert(Rc::from("FS"), Value::str(fs));
        }
        if let Some(ofs) = e.opts.ofs.clone() {
            e.globals.insert(Rc::from("OFS"), Value::str(ofs));
        }
        // `-v` values are literal text, never awk expressions. Verified against
        // all three reference implementations: `-v v=x` yields "x", not the
        // value of a variable called x (which would be empty), and `-v v=1+1`
        // yields the four characters "1+1", not 2.
        for (name, expr) in e.opts.assignments.clone() {
            let v = if expr.is_empty() {
                Value::Uninit
            } else if let Ok(n) = expr.parse::<f64>() {
                // A value that is entirely a number becomes that number.
                Value::Num(n)
            } else {
                Value::str(&expr)
            };
            e.globals.insert(Rc::from(name.as_str()), v);
        }
        Ok(e)
    }

    // ---- variable access -----------------------------------------------

    /// The `CONVFMT` string currently in effect.
    pub(crate) fn convfmt(&self) -> Arc<str> {
        self.globals
            .get("CONVFMT")
            .map(|v| v.to_str(DEFAULT_FMT))
            .unwrap_or_else(|| Arc::from(DEFAULT_FMT))
    }

    /// The `OFMT` string currently in effect.
    pub(crate) fn ofmt(&self) -> Arc<str> {
        self.globals.get("OFMT").map(|v| v.to_str(DEFAULT_FMT)).unwrap_or_else(|| Arc::from(DEFAULT_FMT))
    }

    /// The `OFS` separator.
    pub(crate) fn ofs(&self) -> Arc<str> {
        self.globals.get("OFS").map(|v| v.to_str(DEFAULT_FMT)).unwrap_or_else(|| Arc::from(" "))
    }

    /// The `ORS` terminator.
    pub(crate) fn ors(&self) -> Arc<str> {
        self.globals.get("ORS").map(|v| v.to_str(DEFAULT_FMT)).unwrap_or_else(|| Arc::from("\n"))
    }

    /// The current field separator, re-interpreting `FS` if it changed.
    pub(crate) fn field_sep(&mut self) -> Result<FieldSep, Error> {
        let fs = self.globals.get("FS").map(|v| v.to_str(DEFAULT_FMT)).unwrap_or_else(|| Arc::from(" "));
        let key = fs.to_string();
        if self.fs_cache.as_ref().map(|(k, _)| *k == key).unwrap_or(false) {
            return Ok(self.fs_cache.as_ref().unwrap().1.clone());
        }
        let sep = FieldSep::parse(&key)?;
        self.fs_cache = Some((key, sep.clone()));
        Ok(sep)
    }

    /// The current record separator, re-interpreting `RS` if it changed.
    pub(crate) fn record_sep(&mut self) -> Result<RecordSep, Error> {
        let rs = self.globals.get("RS").map(|v| v.to_str(DEFAULT_FMT)).unwrap_or_else(|| Arc::from("\n"));
        let key = rs.to_string();
        if self.rs_cache.as_ref().map(|(k, _)| *k == key).unwrap_or(false) {
            return Ok(self.rs_cache.as_ref().unwrap().1.clone());
        }
        let sep = RecordSep::parse(&key)?;
        self.rs_cache = Some((key, sep.clone()));
        Ok(sep)
    }

    /// Read a variable: the innermost frame that has it, else a global.
    pub fn get_var(&self, name: &str) -> Value {
        for f in self.frames.iter().rev() {
            if let Some(v) = f.params.get(name) {
                return v.clone();
            }
        }
        if name == "NF" {
            return Value::Num(self.rec.fields.len() as f64);
        }
        self.globals.get(name).cloned().unwrap_or(Value::Uninit)
    }

    /// Write a variable. A function parameter is local; anything else is global.
    pub fn set_var(&mut self, name: &str, v: impl Into<Value>) {
        let v = v.into();
        if let Some(f) = self.frames.last_mut() {
            if f.params.contains_key(name) {
                f.params.insert(Rc::from(name), v);
                return;
            }
        }
        if name == "NF" {
            let ofs = self.ofs();
            let n = v.to_int();
            if n < 0 {
                return;
            }
            self.rec.set_nf(n as usize, &ofs);
            return;
        }
        if name == "FS" || name == "RS" {
            self.fs_cache = None;
            self.rs_cache = None;
        }
        self.globals.insert(Rc::from(name), v);
    }

    /// Whether `name` names an array.
    pub(crate) fn is_array(&self, name: &str) -> bool {
        self.arrays.contains_key(name)
    }

    /// Call a function defined by the awk program. The entry point behind
    /// `Engine::call_num` and `Engine::call_str`.
    pub fn call_fn(&mut self, name: &str, args: &[Value]) -> Result<Value, Error> {
        let fd = self
            .prog
            .funcs
            .get(name)
            .cloned()
            .ok_or_else(|| Error::compile(format!("no function named `{name}`")))?;
        match self.call_user(fd, args.to_vec()) {
            Ok(v) => Ok(v),
            Err(Bail::Err(e)) => Err(e),
            Err(_) => Err(Error::runtime("unexpected control flow from a function call")),
        }
    }

    /// Evaluate one awk expression against the current state. The entry point
    /// behind [`crate::eval`].
    pub fn eval_str(&mut self, expr: &str) -> Result<Value, Error> {
        let e = crate::parser::parse_expression(expr)?;
        match self.eval(&e) {
            Ok(v) => Ok(v),
            Err(Bail::Err(err)) => Err(err),
            Err(_) => Err(Error::runtime("unexpected control flow in expression")),
        }
    }

    // ---- regex ---------------------------------------------------------

    /// Compile a regex, reusing a cached one when possible.
    pub(crate) fn regex(&mut self, pattern: &str) -> Result<Rc<Regex>, Error> {
        if let Some(r) = self.regexes.get(pattern) {
            return Ok(r.clone());
        }
        let re = Rc::new(Regex::new(pattern)?);
        // A bounded cache: patterns come from the program, so this stays small.
        if self.regexes.len() > 256 {
            self.regexes.clear();
        }
        self.regexes.insert(Rc::from(pattern), re.clone());
        Ok(re)
    }

    // ---- execution -----------------------------------------------------

    /// Run `BEGIN`, the main loop, and `END`.
    pub fn run(&mut self) -> Result<(), Error> {
        self.run_begin()?;
        loop {
            if self.exit_code.is_some() {
                break;
            }
            match self.next_record()? {
                Some(line) => match self.process_record(line) {
                    Ok(()) => {}
                    // `next` finishes this record; `exit` stops the loop but
                    // still runs END.
                    Err(Bail::Flow(Flow::Next)) => {}
                    Err(Bail::Flow(Flow::Exit)) => break,
                    Err(Bail::Err(e)) => return Err(e),
                    // break/continue/return outside a loop or function.
                    Err(_) => break,
                },
                None => break,
            }
        }
        // `exit` still runs END.
        self.run_end()?;
        let _ = self.sink.flush();
        match self.exit_code.take() {
            Some(0) | None => Ok(()),
            Some(n) => Err(Error::exit(n)),
        }
    }

    /// Run the `BEGIN` blocks.
    pub(crate) fn run_begin(&mut self) -> Result<(), Error> {
        if self.began {
            return Ok(());
        }
        self.began = true;
        let blocks: Vec<Rc<Block>> =
            self.prog.items.iter().filter_map(|i| match i {
                Item::Begin(b) => Some(b.clone()),
                _ => None,
            }).collect();
        for b in blocks {
            match self.exec_block(&b) {
                Ok(()) | Err(Bail::Flow(Flow::Exit)) | Err(Bail::Flow(Flow::Next)) => {}
                Err(Bail::Flow(Flow::Return(_))) => {}
                Err(e) => return Err(bail_to_err(e)),
            }
        }
        Ok(())
    }

    /// Run the `END` blocks.
    pub(crate) fn run_end(&mut self) -> Result<(), Error> {
        if self.ended {
            return Ok(());
        }
        self.ended = true;
        let blocks: Vec<Rc<Block>> =
            self.prog.items.iter().filter_map(|i| match i {
                Item::End(b) => Some(b.clone()),
                _ => None,
            }).collect();
        for b in blocks {
            match self.exec_block(&b) {
                Ok(()) | Err(Bail::Flow(_)) => {}
                Err(e) => return Err(bail_to_err(e)),
            }
        }
        Ok(())
    }

    /// Read the next input record, advancing across files.
    pub(crate) fn next_record(&mut self) -> Result<Option<String>, Error> {
        loop {
            let sep = self.record_sep()?;
            // Files are read in the order they were added, so `eawk '{...}'
            // a.txt b.txt` processes a.txt first.
            let Some(input) = self.inputs.get_mut(self.input_pos) else {
                return Ok(None);
            };
            let name = input.name.clone();
            match input.next_record(&sep)? {
                Some(line) => {
                    self.globals.insert(Rc::from("FILENAME"), Value::str(name));
                    return Ok(Some(line));
                }
                None => self.input_pos += 1,
            }
        }
    }

    /// Process one record: bump counters, set `$0`, run the rules.
    pub(crate) fn process_record(&mut self, line: String) -> Ctl<()> {
        let nr = self.get_var("NR").to_num() + 1.0;
        let fnr = self.get_var("FNR").to_num() + 1.0;
        self.globals.insert(Rc::from("NR"), Value::Num(nr));
        self.globals.insert(Rc::from("FNR"), Value::Num(fnr));

        let sep = self.field_sep().map_err(Bail::Err)?;
        self.rec.set_line(Arc::from(line.as_str()), &sep);

        for i in 0..self.prog.items.len() {
            // Clone the pattern and action out of the program first; the block scopes
            // the borrow of `self.prog` so the engine is free to be borrowed
            // mutably below.
            let (pattern, action) = {
                match &self.prog.items[i] {
                    Item::Rule { pattern, action } => (pattern.clone(), action.clone()),
                    _ => continue,
                }
            };
            let selected = self.pattern_matches(&pattern, i)?;
            if !selected {
                continue;
            }
            match action {
                Some(b) => match self.exec_block(&b)? {
                    () => {}
                },
                // A rule with no action prints the record.
                None => {
                    let line = self.rec.line.to_string();
                    self.emit_line(&line, &PrintDest::Stdout).map_err(Bail::Err)?;
                }
            }
        }
        Ok(())
    }

    /// Evaluate the value of a rule pattern.
    ///
    /// A top-level regex literal means "does `$0` match this", which is why
    /// `/^c/ { }` selects only records beginning with `c`. Evaluated as an
    /// ordinary value the same expression is just the text `^c`, which is
    /// non-empty and therefore true for every record.
    fn pattern_value(&mut self, e: &Expr) -> Ctl<Value> {
        let regex = match e {
            Expr::Regex(r) => Some(r.clone()),
            Expr::Grouping(g) => match &**g {
                Expr::Regex(r) => Some(r.clone()),
                _ => None,
            },
            _ => None,
        };
        match regex {
            Some(pat) => {
                let record = self.rec.line.clone();
                let re = self.regex(&pat).map_err(Bail::Err)?;
                Ok(Value::Num(if re.find(&record, 0).is_some() { 1.0 } else { 0.0 }))
            }
            None => self.eval(e),
        }
    }

    /// Evaluate a rule's left-hand side, updating range state.
    fn pattern_matches(&mut self, p: &Pattern, item: usize) -> Ctl<bool> {
        match p {
            Pattern::Always => Ok(true),
            // A bare `/re/` as a rule pattern means `$0 ~ /re/`, not "a
            // non-empty string", which would select every record.
            Pattern::Expr(e) => Ok(self.pattern_value(e)?.is_true()),
            Pattern::Range(start, end) => {
                let active = self.range_active[item];
                if active {
                    if self.eval(end)?.is_true() {
                        self.range_active[item] = false;
                    }
                    return Ok(true);
                }
                if self.eval(start)?.is_true() {
                    self.range_active[item] = true;
                    return Ok(true);
                }
                Ok(false)
            }
        }
    }

    // ---- statements ----------------------------------------------------

    pub(crate) fn exec_block(&mut self, b: &Block) -> Ctl<()> {
        for s in &b.stmts {
            self.exec_stmt(s)?;
        }
        Ok(())
    }

    fn exec_stmt(&mut self, s: &Stmt) -> Ctl<()> {
        match s {
            Stmt::Expr(e) => {
                self.eval(e)?;
                Ok(())
            }
            Stmt::Print { args, dest } => self.exec_print(args, dest),
            Stmt::Printf { fmt, args, dest } => self.exec_printf(fmt, args, dest),
            Stmt::If { cond, then, otherwise } => {
                if self.eval(cond)?.is_true() {
                    self.exec_block(then)
                } else if let Some(o) = otherwise {
                    self.exec_block(o)
                } else {
                    Ok(())
                }
            }
            Stmt::While { cond, body } => loop {
                if !self.eval(cond)?.is_true() {
                    return Ok(());
                }
                match self.exec_block(body) {
                    Ok(()) | Err(Bail::Flow(Flow::Continue)) => {}
                    Err(Bail::Flow(Flow::Break)) => return Ok(()),
                    Err(e) => return Err(e),
                }
            },
            Stmt::DoWhile { body, cond } => loop {
                match self.exec_block(body) {
                    Ok(()) | Err(Bail::Flow(Flow::Continue)) => {}
                    Err(Bail::Flow(Flow::Break)) => return Ok(()),
                    Err(e) => return Err(e),
                }
                if !self.eval(cond)?.is_true() {
                    return Ok(());
                }
            },
            Stmt::For { init, cond, step, body } => {
                if let Some(i) = init {
                    self.exec_stmt(i)?;
                }
                loop {
                    if let Some(c) = cond {
                        if !self.eval(c)?.is_true() {
                            return Ok(());
                        }
                    }
                    match self.exec_block(body) {
                        Ok(()) | Err(Bail::Flow(Flow::Continue)) => {}
                        Err(Bail::Flow(Flow::Break)) => return Ok(()),
                        Err(e) => return Err(e),
                    }
                    if let Some(st) = step {
                        self.exec_stmt(st)?;
                    }
                }
            }
            Stmt::ForIn { var, array, body } => {
                let name = array.clone();
                // Snapshot the keys so the body may add or delete elements.
                let keys: Vec<ArrayKey> =
                    self.arrays.get(&name).map(|a| a.keys().cloned().collect()).unwrap_or_default();
                for k in keys {
                    self.assign_to(var, Value::from(k))?;
                    match self.exec_block(body) {
                        Ok(()) | Err(Bail::Flow(Flow::Continue)) => {}
                        Err(Bail::Flow(Flow::Break)) => return Ok(()),
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }
            Stmt::Block(b) => self.exec_block(b),
            Stmt::Next => Err(Flow::Next.into()),
            Stmt::Exit(e) => {
                let code = match e {
                    Some(x) => self.eval(x)?.to_int(),
                    None => 0,
                };
                self.exit_code = Some(code as i32);
                Err(Flow::Exit.into())
            }
            Stmt::Return(e) => {
                let v = match e {
                    Some(x) => self.eval(x)?,
                    None => Value::Uninit,
                };
                Err(Flow::Return(v).into())
            }
            Stmt::Break => Err(Flow::Break.into()),
            Stmt::Continue => Err(Flow::Continue.into()),
            Stmt::Delete { array, index } => {
                match index {
                    Some(ix) => {
                        let key = self.eval(ix)?.array_key(&self.convfmt());
                        if let Some(a) = self.arrays.get_mut(array) {
                            a.remove(&key);
                        }
                    }
                    None => {
                        self.arrays.remove(array);
                    }
                }
                Ok(())
            }
        }
    }

    fn exec_print(&mut self, args: &[Rc<Expr>], dest: &PrintDest) -> Ctl<()> {
        let mut rendered: Vec<String> = Vec::with_capacity(args.len());
        if args.is_empty() {
            rendered.push(self.rec.line.to_string());
        } else {
            let ofs = self.ofs().to_string();
            for a in args {
                let v = self.eval(a)?;
                // print uses OFMT; concatenation uses CONVFMT.
                let s = match &v {
                    Value::Num(n) => format_number(*n, &self.ofmt()),
                    other => other.to_str(DEFAULT_FMT).to_string(),
                };
                rendered.push(s);
            }
            rendered = vec![rendered.join(&ofs)];
        }
        let line = rendered.join("");
        self.emit_line(&line, dest).map_err(Bail::Err)
    }

    fn exec_printf(&mut self, fmt: &Rc<Expr>, args: &[Rc<Expr>], dest: &PrintDest) -> Ctl<()> {
        let f = self.eval(fmt)?;
        let f = f.to_str(&self.convfmt());
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(self.eval(a)?);
        }
        let convfmt = self.convfmt();
        let s = sprintf(&f, &vals, &convfmt).map_err(Bail::Err)?;
        self.write_dest(dest, &s).map_err(Bail::Err)
    }

    /// Write `parts` joined by `OFS` and terminated by `ORS`.
    fn emit_line(&mut self, line: &str, dest: &PrintDest) -> Result<(), Error> {
        let ors = self.ors().to_string();
        self.write_dest(dest, line)?;
        self.write_dest(dest, &ors)
    }

    /// Send a string to standard output or a redirected file.
    pub(crate) fn write_dest(&mut self, dest: &PrintDest, s: &str) -> Result<(), Error> {
        match dest {
            PrintDest::Stdout => self.sink.write_str(s),
            PrintDest::File { path, append } => {
                let p = self.eval(path).map_err(bail_to_err)?.to_str(&self.convfmt()).to_string();
                if !self.outfiles.contains_key(&p) {
                    let f = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .write(true)
                        .truncate(!*append)
                        .open(&p)
                        .map_err(|e| Error::new(crate::error::ErrorKind::Io, format!("can't write {p}: {e}")))?;
                    self.outfiles.insert(p.clone(), f);
                }
                if let Some(f) = self.outfiles.get_mut(&p) {
                    f.write_all(s.as_bytes())?;
                    f.flush()?;
                }
                Ok(())
            }
        }
    }

    // ---- expressions ----------------------------------------------------

    pub(crate) fn eval(&mut self, e: &Expr) -> Ctl<Value> {
        match e {
            Expr::Num(n) => Ok(Value::Num(*n)),
            Expr::Str(s) => Ok(Value::str(&**s)),
            Expr::Regex(r) => Ok(Value::str(&**r)),
            Expr::Var(name) => Ok(self.get_var(name)),
            Expr::Grouping(inner) => self.eval(inner),
            Expr::Field(target) => {
                let idx = self.eval(target)?.to_num();
                if idx < 0.0 || idx.is_nan() {
                    return Err(Error::runtime(format!("attempt to access field {idx}")).into());
                }
                // `$0` is the whole record, not field zero.
                Ok(self.rec.get_value(idx as usize))
            }
            Expr::Index(base, idx) => {
                let key = self.eval(idx)?.array_key(&self.convfmt());
                let name = match &**base {
                    Expr::Var(n) => n.clone(),
                    Expr::Grouping(g) => match &**g {
                        Expr::Var(n) => n.clone(),
                        _ => return Err(Error::runtime("only arrays may be subscripted").into()),
                    },
                    _ => return Err(Error::runtime("only arrays may be subscripted").into()),
                };
                Ok(self.arrays.get(&name).and_then(|a| a.get(&key)).cloned().unwrap_or(Value::Uninit))
            }
            Expr::Assign { target, op, value } => {
                let rhs = self.eval(value)?;
                match op {
                    AssignOp::Assign => self.assign_to(target, rhs)?,
                    _ => {
                        let cur = self.eval(target)?;
                        let aop = match op {
                            AssignOp::Add => ArithOp::Add,
                            AssignOp::Sub => ArithOp::Sub,
                            AssignOp::Mul => ArithOp::Mul,
                            AssignOp::Div => ArithOp::Div,
                            AssignOp::Mod => ArithOp::Mod,
                            AssignOp::Pow => ArithOp::Pow,
                            AssignOp::Assign => unreachable!(),
                        };
                        let newv = self.combine(aop, cur, rhs)?;
                        self.assign_to(target, newv)?;
                    }
                }
                Ok(self.eval(target)?)
            }
            Expr::Binary { op, left, right } => {
                let a = self.eval(left)?;
                let b = self.eval(right)?;
                self.combine(*op, a, b)
            }
            Expr::Concat(left, right) => {
                let convfmt = self.convfmt();
                let a = self.eval(left)?.to_str(&convfmt);
                let b = self.eval(right)?.to_str(&convfmt);
                Ok(Value::Str(Arc::from(format!("{a}{b}").as_str())))
            }
            Expr::And(left, right) => {
                if !self.eval(left)?.is_true() {
                    return Ok(Value::Num(0.0));
                }
                Ok(Value::Num(if self.eval(right)?.is_true() { 1.0 } else { 0.0 }))
            }
            Expr::Or(left, right) => {
                if self.eval(left)?.is_true() {
                    return Ok(Value::Num(1.0));
                }
                Ok(Value::Num(if self.eval(right)?.is_true() { 1.0 } else { 0.0 }))
            }
            Expr::Compare { op, left, right } => {
                let a = self.eval(left)?;
                let b = self.eval(right)?;
                let convfmt = self.convfmt();
                let ord = compare(&a, &b, &convfmt);
                use std::cmp::Ordering::*;
                let r = match op {
                    CmpOp::Lt => ord == Less,
                    CmpOp::Le => ord != Greater,
                    CmpOp::Gt => ord == Greater,
                    CmpOp::Ge => ord != Less,
                    CmpOp::Eq => ord == Equal,
                    CmpOp::Ne => ord != Equal,
                };
                Ok(Value::Num(if r { 1.0 } else { 0.0 }))
            }
            Expr::Unary { op, operand } => {
                let v = self.eval(operand)?;
                Ok(match op {
                    UnOp::Not => Value::Num(if v.is_true() { 0.0 } else { 1.0 }),
                    UnOp::Neg => Value::Num(-v.to_num()),
                    UnOp::Pos => Value::Num(v.to_num()),
                })
            }
            Expr::PostIncr { delta, operand } => {
                let cur = self.eval(operand)?;
                let newv = Value::Num(cur.to_num() + delta);
                self.assign_to(operand, newv)?;
                Ok(cur)
            }
            Expr::PreIncr { delta, operand } => {
                let cur = self.eval(operand)?.to_num();
                let newv = Value::Num(cur + delta);
                self.assign_to(operand, newv.clone())?;
                Ok(newv)
            }
            Expr::Ternary { cond, then, otherwise } => {
                if self.eval(cond)?.is_true() {
                    self.eval(then)
                } else {
                    self.eval(otherwise)
                }
            }
            Expr::In { key, array } => {
                let k = self.eval(key)?.array_key(&self.convfmt());
                Ok(Value::Num(if self.arrays.get(array).is_some_and(|a| a.contains_key(&k)) {
                    1.0
                } else {
                    0.0
                }))
            }
            Expr::Match { negated, left, right } => {
                let s = self.eval(left)?.to_str(&self.convfmt());
                let pat = self.eval(right)?;
                let pat = pat.to_str(&self.convfmt());
                let re = self.regex(&pat).map_err(Bail::Err)?;
                let hit = re.find(&s, 0).is_some();
                Ok(Value::Num(if hit != *negated { 1.0 } else { 0.0 }))
            }
            Expr::Call { name, args } => self.call_expr(name, args),
            Expr::Getline(g) => self.exec_getline(g),
        }
    }

    /// Apply an arithmetic operator.
    fn combine(&mut self, op: ArithOp, a: Value, b: Value) -> Ctl<Value> {
        let (x, y) = (a.to_num(), b.to_num());
        Ok(Value::Num(match op {
            ArithOp::Add => x + y,
            ArithOp::Sub => x - y,
            ArithOp::Mul => x * y,
            ArithOp::Div => self.divide(x, y)?,
            ArithOp::Mod => self.modulo(x, y)?,
            ArithOp::Pow => {
                // Both integral operands give an exact integer power, which is
                // why `print 2^31` shows 2147483648 rather than 2.14748e+09.
                if x.fract() == 0.0 && y.fract() == 0.0 && y >= 0.0 {
                    let mut r = 1.0f64;
                    for _ in 0..(y as u64).min(1024) {
                        r *= x;
                    }
                    if r.is_finite() {
                        r
                    } else {
                        x.powf(y)
                    }
                } else {
                    x.powf(y)
                }
            }
        }))
    }

    fn divide(&self, x: f64, y: f64) -> Result<f64, Bail> {
        if y == 0.0 {
            return match self.opts.div_zero {
                DivZeroPolicy::Fatal => {
                    Err(Error::runtime("division by zero").into())
                }
                DivZeroPolicy::Inf => Ok(x / y),
            };
        }
        Ok(x / y)
    }

    fn modulo(&self, x: f64, y: f64) -> Result<f64, Bail> {
        if y == 0.0 {
            return match self.opts.div_zero {
                DivZeroPolicy::Fatal => Err(Error::runtime("division by zero in %").into()),
                DivZeroPolicy::Inf => Ok(f64::NAN),
            };
        }
        // awk's `%` truncates toward zero, like C: -3 % 2 is -1.
        Ok(x % y)
    }

    /// Store a value into an assignable location.
    pub(crate) fn assign_to(&mut self, target: &Expr, v: Value) -> Ctl<()> {
        match target {
            Expr::Var(name) => {
                // Assigning a string to a variable makes it a string, but a
                // numeric assignment keeps it numeric.
                self.set_var(name, v);
                Ok(())
            }
            Expr::Field(inner) => {
                let idx = self.eval(inner)?.to_num();
                if idx < 0.0 || idx.is_nan() {
                    return Err(Error::runtime(format!("attempt to assign to field {idx}")).into());
                }
                let convfmt = self.convfmt();
                let ofs = self.ofs();
                if idx == 0.0 {
                    // `$0` is always text, and re-splits the record.
                    let sep = self.field_sep().map_err(Bail::Err)?;
                    let line: Arc<str> = v.to_str(&convfmt);
                    self.rec.set_line(line, &sep);
                } else {
                    let s: Arc<str> = v.to_str(&convfmt);
                    self.rec.set_field(idx as usize, s, &ofs);
                }
                Ok(())
            }
            Expr::Index(base, idx) => {
                let name = match &**base {
                    Expr::Var(n) => n.clone(),
                    Expr::Grouping(g) => match &**g {
                        Expr::Var(n) => n.clone(),
                        _ => return Err(Error::runtime("only arrays may be subscripted").into()),
                    },
                    _ => return Err(Error::runtime("only arrays may be subscripted").into()),
                };
                let key = self.eval(idx)?.array_key(&self.convfmt());
                self.arrays.entry(name).or_default().insert(key, v);
                Ok(())
            }
            Expr::Grouping(inner) => self.assign_to(inner, v),
            _ => Err(Error::runtime("invalid assignment target").into()),
        }
    }

    /// Call a function: user-defined, builtin, or native.
    fn call_expr(&mut self, name: &Rc<str>, args: &[Rc<Expr>]) -> Ctl<Value> {
        // These need the argument *expressions*, because they write back into
        // a variable, a field, or a named array — or because `length(a)` wants
        // the array's name rather than its (empty) value.
        match &**name {
            "sub" => return self.call_subst(false, args),
            "gsub" => return self.call_subst(true, args),
            "split" => return self.call_split(args),
            "length" => {
                if args.len() == 1 {
                    if let Expr::Var(n) = unwrap_grouping(&args[0]) {
                        if self.is_array(n) {
                            return Ok(Value::Num(self.arrays.get(n).map_or(0, |a| a.len()) as f64));
                        }
                    }
                }
            }
            _ => {}
        }

        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(self.eval(a)?);
        }

        if let Some(native) = self.natives.get(name).cloned() {
            let mut ctx = crate::Ctx { engine: self };
            return native(&mut ctx, vals).map_err(Bail::Err);
        }
        if let Some(v) = self.builtin(name, &vals)? {
            return Ok(v);
        }

        let fd = match self.prog.funcs.get(name) {
            Some(f) => f.clone(),
            None => return Err(Error::compile(format!("calling undefined function `{name}`")).into()),
        };
        self.call_user(fd, vals)
    }

    /// Call a function defined in the awk program.
    pub(crate) fn call_user(&mut self, fd: Rc<FuncDef>, args: Vec<Value>) -> Ctl<Value> {
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(Error::runtime("function call nested too deeply (possible infinite recursion)").into());
        }
        let mut params: HashMap<Rc<str>, Value> = HashMap::new();
        for (i, p) in fd.params.iter().enumerate() {
            // Parameters beyond the supplied arguments stay uninitialized,
            // which is how awk functions get locals.
            params.insert(p.clone(), args.get(i).cloned().unwrap_or(Value::Uninit));
        }
        self.frames.push(Frame { params });
        self.call_depth += 1;
        let result = self.exec_block(&fd.body);
        self.call_depth -= 1;
        self.frames.pop();
        match result {
            Ok(()) => Ok(Value::Uninit),
            Err(Bail::Flow(Flow::Return(v))) => Ok(v),
            Err(e) => Err(e),
        }
    }

    /// The `getline` family.
    fn exec_getline(&mut self, g: &Getline) -> Ctl<Value> {
        let res = match g {
            Getline::FromRecord => {
                let line = self.next_record().map_err(Bail::Err)?;
                match line {
                    Some(l) => {
                        let sep = self.field_sep().map_err(Bail::Err)?;
                        self.rec.set_line(Arc::from(l.as_str()), &sep);
                        1.0
                    }
                    None => 0.0,
                }
            }
            Getline::IntoVar(target) => {
                let line = self.next_record().map_err(Bail::Err)?;
                match line {
                    Some(l) => {
                        self.assign_to(target, Value::StrNum(Arc::from(l.as_str()), crate::value::parse_awk_number(&l)))?;
                        1.0
                    }
                    None => 0.0,
                }
            }
            Getline::FromFile(path) => {
                let p = self.eval(path)?.to_str(&self.convfmt()).to_string();
                match self.read_getline(&p, None)? {
                    Some(line) => {
                        let sep = self.field_sep().map_err(Bail::Err)?;
                        self.rec.set_line(line, &sep);
                        1.0
                    }
                    None => 0.0,
                }
            }
            Getline::FromFileIntoVar(target, path) => {
                let p = self.eval(path)?.to_str(&self.convfmt()).to_string();
                match self.read_getline(&p, Some(target))? {
                    Some(_) => 1.0,
                    None => 0.0,
                }
            }
            Getline::PipeGetline { cmd, var } => {
                let c = self.eval(cmd)?.to_str(&self.convfmt()).to_string();
                match self.pipe_getline(&c, var.as_deref())? {
                    Some(line) => {
                        match var {
                            Some(t) => {
                                self.assign_to(t, Value::StrNum(Arc::from(line.as_str()), crate::value::parse_awk_number(&line)))?
                            }
                            None => {
                                let sep = self.field_sep().map_err(Bail::Err)?;
                                self.rec.set_line(Arc::from(line.as_str()), &sep);
                            }
                        }
                        1.0
                    }
                    None => 0.0,
                }
            }
        };
        Ok(Value::Num(res))
    }

    /// Read one record from a file opened by `getline < file`.
    ///
    /// Returns `None` at end of file or if the file cannot be opened, which is
    /// what awk reports as a return value of 0 or -1 rather than an error.
    fn read_getline(&mut self, path: &str, into: Option<&Expr>) -> Ctl<Option<Arc<str>>> {
        let sep = self.record_sep().map_err(Bail::Err)?;
        if !self.getfiles.contains_key(path) {
            let Ok(f) = std::fs::File::open(path) else {
                return Ok(None);
            };
            self.getfiles.insert(path.to_string(), FileHandle { reader: BufReader::new(f) });
        }
        let handle = self.getfiles.get_mut(path).unwrap();
        let line = crate::io::next_record(&mut handle.reader, &sep).map_err(Bail::Err)?;
        match line {
            Some(l) => {
                let arc: Arc<str> = Arc::from(l.as_str());
                if let Some(t) = into {
                    self.assign_to(t, Value::StrNum(arc.clone(), crate::value::parse_awk_number(&l)))?;
                }
                Ok(Some(arc))
            }
            None => Ok(None),
        }
    }

    /// Run a command and read one line of its output.
    fn pipe_getline(&mut self, cmd: &str, _var: Option<&Expr>) -> Ctl<Option<String>> {
        use std::process::{Command, Stdio};
        let child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };
        // Read a single record through the same reader the main input uses.
        let stdout = child.stdout.take();
        let mut reader = stdout.map(BufReader::new);
        let line = match reader.as_mut() {
            Some(r) => crate::io::next_record(r, &RecordSep::Newline).map_err(Bail::Err)?,
            None => None,
        };
        let _ = child.wait();
        Ok(line)
    }
}

/// Strip redundant parentheses to reach the expression inside.
fn unwrap_grouping(e: &Expr) -> &Expr {
    let mut cur = e;
    while let Expr::Grouping(inner) = cur {
        cur = inner;
    }
    cur
}

/// Recover an [`Error`] from a [`Bail`], or synthesise one for a stray signal.
pub(crate) fn bail_to_err(b: Bail) -> Error {
    b.into_error().unwrap_or_else(|| Error::runtime("unexpected control flow"))
}