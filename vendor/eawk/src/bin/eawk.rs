//! The `eawk` command.

use eawk::{Engine, Error, Options};
use std::process::ExitCode;

const USAGE: &str = "\
eawk — an AWK interpreter

USAGE:
    eawk [OPTIONS] 'PROGRAM' [FILE ...]
    eawk [OPTIONS] -f PROGFILE [FILE ...]

OPTIONS:
    -f FILE     Read the program text from FILE
    -F FS       Set the field separator
    -v VAR=VAL  Assign VAL to VAR before running
    -i          Produce `inf` on division by zero instead of erroring
    --version   Print version information
    -h, --help  Print this help

A FILE of `-` reads standard input. With no FILE, or when FILE is `-`, the
program reads standard input.

EXAMPLES:
    eawk '{ print $1 }' data.txt
    eawk -F, -v OFS='|' '{ print $2, $1 }' data.csv
    eawk 'NR>1 && $3>100 { n++ } END { print n+0 }' sales.txt
    eawk -f filter.awk < input.txt
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(Fail::Usage(msg)) => {
            eprintln!("eawk: {msg}");
            eprint!("\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Fail::Error(e)) => {
            // `exit N` from the program is not a failure: awk leaves quietly
            // with status N. Only a genuine error is reported.
            match e.exit_status() {
                Some(n) => ExitCode::from((n & 0xff) as u8),
                None => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

enum Fail {
    Usage(String),
    Error(Error),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        Fail::Error(e)
    }
}

fn run() -> Result<(), Fail> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut program: Option<String> = None;
    let mut files: Vec<String> = Vec::new();
    let mut fs: Option<String> = None;
    let mut assignments: Vec<(String, String)> = Vec::new();
    let mut opts = Options::default();
    let mut i = 0usize;
    let mut only_files = false;

    while i < args.len() {
        let a = args[i].clone();
        if only_files {
            files.push(a);
            i += 1;
            continue;
        }
        match a.as_str() {
            "--" => only_files = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--version" => {
                println!("eawk {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-i" => opts.div_zero = eawk::DivZeroPolicy::Inf,
            _ => {
                // A lone `-` and anything after `--` are file names.
                if a == "-" {
                    files.push(a);
                    i += 1;
                    continue;
                }
                if let Some(rest) = a.strip_prefix("--") {
                    return Err(Fail::Usage(format!("unknown option `--{rest}`")));
                }
                if a.len() > 1 && a.starts_with('-') {
                    // Short options may be bundled, and each may take a value.
                    let chars: Vec<char> = a[1..].chars().collect();
                    let mut c = 0usize;
                    while c < chars.len() {
                        match chars[c] {
                            'f' | 'F' | 'v' => {
                                let value: String = if c + 1 < chars.len() {
                                    chars[c + 1..].iter().collect()
                                } else {
                                    i += 1;
                                    match args.get(i) {
                                        Some(v) => v.clone(),
                                        None => {
                                            return Err(Fail::Usage(format!(
                                                "option `-{}` needs a value",
                                                chars[c]
                                            )))
                                        }
                                    }
                                };
                                match chars[c] {
                                    'f' => program = Some(read_program(&value)?),
                                    'F' => fs = Some(unescape_fs(&value)),
                                    _ => {
                                        let (name, val) = value.split_once('=').unwrap_or((&value, ""));
                                        assignments.push((name.to_string(), val.to_string()));
                                    }
                                }
                                c = chars.len();
                            }
                            other => {
                                return Err(Fail::Usage(format!("unknown option `-{other}`")));
                            }
                        }
                    }
                    i += 1;
                    continue;
                }
                // The first bare argument is the program; the rest are files.
                if program.is_none() {
                    program = Some(a);
                } else {
                    files.push(a);
                }
            }
        }
        i += 1;
    }

    let Some(program) = program else {
        return Err(Fail::Usage("no program given".to_string()));
    };

    opts.fs = fs;
    opts.assignments = assignments;

    let mut engine = Engine::with_options(&program, opts).map_err(Fail::Error)?;
    engine.set_var("ARGC", 1.0);

    let mut any = false;
    for f in &files {
        if f == "-" {
            engine.use_stdin();
        } else {
            engine.add_file(f).map_err(Fail::Error)?;
        }
        any = true;
    }
    if !any {
        engine.use_stdin();
    }
    // ARGV holds the remaining file names, as in POSIX awk.
    engine.set_elem("ARGV", 0.0, files.len() as f64);
    for (n, v) in files.iter().enumerate() {
        engine.set_elem("ARGV", (n + 1) as f64, v.as_str());
    }

    engine.run().map_err(Fail::Error)
}

fn read_program(path: &str) -> Result<String, Fail> {
    if path == "-" {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|e| Fail::Error(e.into()))?;
        return Ok(s);
    }
    std::fs::read_to_string(path).map_err(|e| Fail::Error(Error::new(eawk::ErrorKind::Io, format!("can't open {path}: {e}"))))
}

/// `-F\t` should mean a tab, not the two characters backslash-t.
fn unescape_fs(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}