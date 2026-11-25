//! eawk — an embeddable AWK interpreter.
//!
//! eawk is two things at once: a zero-dependency Rust library for running awk
//! code inside your own program, and the standalone `eawk` command.
//!
//! # As a library
//!
//! Parse a program, set variables, run it, and read the results back:
//!
//! ```
//! use eawk::{Engine, Value};
//!
//! let mut e = Engine::new("END { print total }")?;
//! e.set_var("total", 42.0);
//! e.run()?;
//! assert_eq!(e.get_var("total").to_num(), 42.0);
//! # Ok::<(), eawk::Error>(())
//! ```
//!
//! Feed it input and capture what it prints, without touching the filesystem:
//!
//! ```
//! use eawk::Engine;
//!
//! let mut e = Engine::new("{ print $1 + $2 }")?;
//! let out = e.exec_captured("1 2\n3 4\n")?;
//! assert_eq!(out, "3\n7\n");
//! # Ok::<(), eawk::Error>(())
//! ```
//!
//! Or stream records one at a time, which is how you would drive it from a
//! socket or a database cursor:
//!
//! ```
//! use eawk::Runner;
//!
//! let mut r = Runner::new("{ total += $1 } END { print total }")?;
//! r.record("10\n")?;
//! r.record("20\n")?;
//! r.finish()?;
//! assert_eq!(r.output(), "30\n");
//! # Ok::<(), eawk::Error>(())
//! ```
//!
//! # Calling across the boundary
//!
//! An awk function can be called directly from Rust, and a Rust function can be
//! exposed to awk:
//!
//! ```
//! use eawk::{Engine, Value};
//!
//! let mut e = Engine::new("function twice(x) { return x * 2 }")?;
//! assert_eq!(e.call_num("twice", &[Value::from(21.0)])?, 42.0);
//!
//! let mut e = Engine::new("BEGIN { print hostdouble(4) }")?;
//! e.register_fn1("hostdouble", |v: Value| Ok(v.to_num() * 2.0))?;
//! assert_eq!(e.run_captured()?, "8\n");
//! # Ok::<(), eawk::Error>(())
//! ```
//!
//! # Compatibility
//!
//! eawk aims to match the POSIX awk that BWK, gawk and mawk agree on, and its
//! behaviour was checked against all three. Two deliberate differences are
//! worth knowing about:
//!
//! * Division by zero is an error by default, as in BWK awk and mawk. Set
//!   [`DivZeroPolicy::Inf`] to get gawk's `inf` instead.
//! * `\d`, `\w` and `\s` in regular expressions are not supported. They are
//!   gawk extensions; the POSIX behaviour, which BWK awk and mawk follow, is
//!   to treat `\w` as a literal `w`.

#![forbid(unsafe_code)]

pub mod ast;
pub mod builtins;
pub mod error;
pub mod fields;
pub mod format;
pub mod interp;
pub mod io;
pub mod lexer;
pub mod parser;
pub mod regex;
pub mod value;

use std::io::Write;
use std::rc::Rc;

pub use error::{Error, ErrorKind};
pub use interp::{DivZeroPolicy, Engine, Options};
pub use value::Value;

/// A compiled program, ready to be instantiated any number of times.
///
/// Parsing is the expensive part. Keep one `Interp` and call
/// [`Interp::instantiate`] for each run — that is what makes it cheap to run
/// the same awk program over many inputs.
#[derive(Clone)]
pub struct Interp {
    prog: Rc<ast::Program>,
}

impl Interp {
    /// Parse `src` without running it.
    pub fn new(src: &str) -> Result<Interp, Error> {
        Ok(Interp { prog: Rc::new(parser::parse_program(src)?) })
    }

    /// Create a fresh engine with its own variable and field state.
    pub fn instantiate(&self) -> Engine {
        Engine::from_program(self.prog.clone(), Options::default())
            .expect("a parsed program always builds an engine")
    }

    /// Create a fresh engine with options.
    pub fn instantiate_with(&self, opts: Options) -> Result<Engine, Error> {
        Engine::from_program(self.prog.clone(), opts)
    }
}

/// The handle a Rust function receives when awk calls it.
///
/// This is a narrow view of the engine rather than the whole thing, so a native
/// function cannot accidentally disturb control flow.
pub struct Ctx<'a> {
    pub(crate) engine: &'a mut Engine,
}

impl Ctx<'_> {
    /// Read an awk variable, respecting the caller's local parameters.
    pub fn get_var(&mut self, name: &str) -> Value {
        self.engine.get_var(name)
    }

    /// Write an awk variable.
    pub fn set_var(&mut self, name: &str, v: impl Into<Value>) {
        self.engine.set_var(name, v.into());
    }

    /// Read `$0`.
    pub fn record(&self) -> String {
        self.engine.rec.line.to_string()
    }

    /// Read a field. Index 0 is `$0`, 1 is `$1`.
    pub fn field(&self, i: usize) -> String {
        self.engine.rec.get(i).to_string()
    }

    /// Write text to the program's current output.
    pub fn emit(&mut self, s: &str) -> Result<(), Error> {
        self.engine.sink.write_str(s)
    }

    /// Flush buffered output.
    pub fn flush(&mut self) -> Result<(), Error> {
        self.engine.sink.flush()
    }

    /// Build an error with this engine's context.
    pub fn error(msg: impl Into<String>) -> Error {
        Error::runtime(msg)
    }
}

impl Engine {
    /// Parse `src` into a runnable engine. Nothing executes yet.
    pub fn new(src: &str) -> Result<Engine, Error> {
        Ok(Interp::new(src)?.instantiate())
    }

    /// Parse with options — field separator, output separator, and `-v`
    /// style assignments.
    pub fn with_options(src: &str, opts: Options) -> Result<Engine, Error> {
        Interp::new(src)?.instantiate_with(opts)
    }

    /// Read an element of an array.
    pub fn get_elem(&self, array: &str, key: impl Into<Value>) -> Value {
        let k = key.into().array_key(crate::value::DEFAULT_FMT);
        self.arrays.get(array).and_then(|a| a.get(&k)).cloned().unwrap_or(Value::Uninit)
    }

    /// Write an element of an array.
    pub fn set_elem(&mut self, array: &str, key: impl Into<Value>, v: impl Into<Value>) {
        let k = key.into().array_key(crate::value::DEFAULT_FMT);
        self.arrays.entry(Rc::from(array)).or_default().insert(k, v.into());
    }

    /// Number of elements in an array.
    pub fn len_array(&self, array: &str) -> usize {
        self.arrays.get(array).map_or(0, |a| a.len())
    }

    /// Whether an array contains a key.
    pub fn has_elem(&self, array: &str, key: impl Into<Value>) -> bool {
        let k = key.into().array_key(crate::value::DEFAULT_FMT);
        self.arrays.get(array).is_some_and(|a| a.contains_key(&k))
    }

    /// Call a function and read the result as a number.
    pub fn call_num(&mut self, name: &str, args: &[Value]) -> Result<f64, Error> {
        Ok(self.call_fn(name, args)?.to_num())
    }

    /// Call a function and read the result as a string.
    pub fn call_str(&mut self, name: &str, args: &[Value]) -> Result<String, Error> {
        let v = self.call_fn(name, args)?;
        Ok(v.to_str(crate::value::DEFAULT_FMT).to_string())
    }

    /// Expose a Rust function to the awk program.
    pub fn register_fn<F>(&mut self, name: &str, f: F) -> Result<(), Error>
    where
        F: 'static + Fn(&mut Ctx, Vec<Value>) -> Result<Value, Error>,
    {
        self.natives.insert(Rc::from(name), Rc::new(f));
        Ok(())
    }

    /// Expose a one-argument Rust function to the awk program.
    pub fn register_fn1<F, R>(&mut self, name: &str, f: F) -> Result<(), Error>
    where
        F: 'static + Fn(Value) -> Result<R, Error>,
        R: Into<Value>,
    {
        self.register_fn(name, move |_ctx, mut args| {
            let a = args.pop().unwrap_or(Value::Uninit);
            f(a).map(Into::into)
        })
    }

    /// Redirect output. By default an engine writes to standard output.
    pub fn set_output(&mut self, w: Box<dyn Write>) {
        self.sink = io::Sink::Stream(w);
    }

    /// Capture everything the program prints instead of writing it out.
    ///
    /// Must be called before [`Engine::run`].
    pub fn capture_output(&mut self) {
        self.sink = io::Sink::Buffer(Vec::new());
    }

    /// Whatever has been captured so far, if capturing is enabled.
    pub fn captured(&self) -> Option<String> {
        self.sink.buffer().map(|s| s.to_string())
    }

    /// Add an input file.
    pub fn add_file(&mut self, path: &str) -> Result<(), Error> {
        self.inputs.push(io::Input::open(path)?);
        Ok(())
    }

    /// Read standard input as the program's input.
    pub fn use_stdin(&mut self) {
        self.inputs.push(io::Input::stdin());
    }

    /// Supply input text directly, without a file.
    pub fn push_input(&mut self, text: &str) -> Result<(), Error> {
        let owned = std::io::Cursor::new(text.as_bytes().to_vec());
        self.inputs.push(io::Input::new("-", Box::new(std::io::BufReader::new(owned))));
        Ok(())
    }

    /// Run `BEGIN` and `END` over the configured input, returning what was printed.
    ///
    /// The output sink is switched to a buffer for the duration of the run.
    pub fn run_captured(&mut self) -> Result<String, Error> {
        let previous = std::mem::replace(&mut self.sink, io::Sink::Buffer(Vec::new()));
        let result = self.run();
        let captured = self.sink.buffer().unwrap_or("").to_string();
        self.sink = previous;
        result?;
        Ok(captured)
    }

    /// Run with input text and return everything printed.
    pub fn exec_captured(&mut self, input: &str) -> Result<String, Error> {
        self.sink = io::Sink::Buffer(Vec::new());
        self.push_input(input)?;
        self.run()?;
        Ok(self.sink.buffer().unwrap_or("").to_string())
    }

    /// Run the `BEGIN` blocks only, so records can be pushed afterwards.
    pub fn begin(&mut self) -> Result<(), Error> {
        self.run_begin()
    }

    /// Process one record.
    ///
    /// A trailing newline is stripped if present. Callers naturally hand over
    /// whole lines from a file or socket, but an awk record does not include
    /// its terminator, so keeping it would double every separator.
    pub fn push_record(&mut self, record: &str) -> Result<(), Error> {
        let record = record.strip_suffix('\n').unwrap_or(record);
        let record = record.strip_suffix('\r').unwrap_or(record);
        match self.process_record(record.to_string()) {
            Ok(()) => Ok(()),
            Err(interp::Bail::Flow(interp::Flow::Next)) | Err(interp::Bail::Flow(interp::Flow::Exit)) => {
                Ok(())
            }
            Err(interp::Bail::Err(e)) => Err(e),
            Err(_) => Ok(()),
        }
    }

    /// Run the `END` blocks.
    pub fn end(&mut self) -> Result<(), Error> {
        self.run_end()
    }
}

/// A convenience wrapper for streaming records through a program.
///
/// ```no_run
/// # use eawk::Runner;
/// let mut r = Runner::new("END { print n }")?;
/// for line in std::io::stdin().lines() {
///     r.record(&line?)?;
/// }
/// r.finish()?;
/// print!("{}", r.output());
/// # Ok::<(), eawk::Error>(())
/// ```
pub struct Runner {
    engine: Engine,
}

impl Runner {
    /// Parse `src` and run its `BEGIN` blocks.
    pub fn new(src: &str) -> Result<Runner, Error> {
        let mut engine = Engine::new(src)?;
        engine.capture_output();
        engine.begin()?;
        Ok(Runner { engine })
    }

    /// Feed one record.
    pub fn record(&mut self, record: &str) -> Result<(), Error> {
        self.engine.push_record(record)
    }

    /// Feed many records.
    pub fn records<I, S>(&mut self, it: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for r in it {
            self.record(r.as_ref())?;
        }
        Ok(())
    }

    /// Run the `END` blocks.
    pub fn finish(&mut self) -> Result<(), Error> {
        self.engine.end()
    }

    /// Everything printed so far.
    pub fn output(&self) -> String {
        self.engine.captured().unwrap_or_default()
    }

    /// Borrow the engine, for anything the streaming API does not cover.
    pub fn engine(&mut self) -> &mut Engine {
        &mut self.engine
    }
}

/// Compile and run in one step. Mostly useful in tests.
pub fn run(src: &str, input: &str) -> Result<String, Error> {
    Engine::new(src)?.exec_captured(input)
}

/// Evaluate a single awk expression, with `vars` bound as globals.
pub fn eval(src: &str, vars: &[(&str, Value)]) -> Result<Value, Error> {
    let mut engine = Interp::new("BEGIN{}")?.instantiate();
    for (k, v) in vars {
        engine.set_var(k, v.clone());
    }
    engine.eval_str(src)
}