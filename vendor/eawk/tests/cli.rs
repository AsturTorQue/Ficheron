//! End-to-end tests for the `eawk` command.
//!
//! These run the real binary, so they cover argument parsing and exit codes —
//! things the library tests cannot see. Each spawns one short-lived process.

use std::process::{Command, Stdio};

/// Path to the built binary, next to the integration test's own executable.
fn eawk() -> Command {
    // target/debug/deps/<test> -> target/debug/eawk
    let mut path = std::env::current_exe().expect("test executable path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("eawk");
    Command::new(path)
}

struct Output {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str], stdin: &str) -> Output {
    use std::io::Write;
    let mut child = eawk()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn eawk");
    child.stdin.as_mut().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn prints_fields_from_stdin() {
    let o = run(&["{ print $1 }"], "a b\nc d\n");
    assert_eq!(o.stdout, "a\nc\n");
    assert_eq!(o.status, 0);
}

#[test]
fn reads_program_from_a_file() {
    let dir = std::env::temp_dir().join("eawk-cli-test");
    std::fs::create_dir_all(&dir).unwrap();
    let prog = dir.join("p.awk");
    std::fs::write(&prog, "{ print $2 }").unwrap();
    let input = dir.join("in.txt");
    std::fs::write(&input, "a b\nc d\n").unwrap();
    let o = run(&["-f", prog.to_str().unwrap(), input.to_str().unwrap()], "");
    assert_eq!(o.stdout, "b\nd\n");
}

#[test]
fn field_separator_flag() {
    let o = run(&["-F,", "{ print $2 }"], "a,b\nc,d\n");
    assert_eq!(o.stdout, "b\nd\n");
}

#[test]
fn bundled_field_separator_flag() {
    let o = run(&["-F,", "-v", "OFS=-", "{ print $2, $1 }"], "a,b\n");
    assert_eq!(o.stdout, "b-a\n");
}

#[test]
fn assignment_flag() {
    let o = run(&["-v", "n=3", "-v", "label=x", "BEGIN { print n, label }"], "");
    assert_eq!(o.stdout, "3 x\n");
}

#[test]
fn dash_reads_stdin() {
    let o = run(&["{ print $1 }", "-"], "one\ntwo\n");
    assert_eq!(o.stdout, "one\ntwo\n");
}

#[test]
fn double_dash_ends_options() {
    let o = run(&["{ print }", "--"], "x\n");
    assert_eq!(o.stdout, "x\n");
}

/// `exit N` is a normal termination, not a failure: awk leaves quietly with
/// status N. Printing a diagnostic here is wrong, and the user sees it.
#[test]
fn exit_status_is_reported_without_a_diagnostic() {
    let o = run(&["BEGIN { exit 3 }"], "");
    assert_eq!(o.status, 3);
    assert_eq!(o.stdout, "");
    assert_eq!(o.stderr, "", "exit N must be silent");
}

#[test]
fn exit_zero_succeeds() {
    let o = run(&["BEGIN { exit 0 }"], "");
    assert_eq!(o.status, 0);
    assert_eq!(o.stderr, "");
}

#[test]
fn exit_status_still_runs_end() {
    let o = run(&["BEGIN { exit 0 } END { print \"done\" }"], "");
    assert_eq!(o.stdout, "done\n");
    assert_eq!(o.status, 0);
}

#[test]
fn a_runtime_error_reports_on_stderr_and_exits_one() {
    let o = run(&["BEGIN { x = 1/0 }"], "");
    assert_eq!(o.status, 1);
    assert!(o.stderr.contains("division by zero"));
}

#[test]
fn a_syntax_error_reports_a_position() {
    let o = run(&["BEGIN { print ( }"], "");
    assert_ne!(o.status, 0);
    assert!(o.stderr.contains("eawk:"));
}

#[test]
fn missing_program_is_a_usage_error() {
    let o = run(&[], "");
    assert_eq!(o.status, 2);
    assert!(o.stderr.contains("USAGE"));
}

#[test]
fn unknown_option_is_a_usage_error() {
    let o = run(&["--nope", "BEGIN{}"], "");
    assert_eq!(o.status, 2);
}

#[test]
fn version_and_help() {
    let v = run(&["--version"], "");
    assert!(v.stdout.starts_with("eawk "));
    assert_eq!(v.status, 0);
    let h = run(&["--help"], "");
    assert!(h.stdout.contains("USAGE"));
    assert_eq!(h.status, 0);
}

#[test]
fn multiple_files_continue_across_them() {
    let dir = std::env::temp_dir().join("eawk-cli-test");
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, "1\n2\n").unwrap();
    std::fs::write(&b, "3\n").unwrap();
    let o = run(&["{ print }", a.to_str().unwrap(), b.to_str().unwrap()], "");
    assert_eq!(o.stdout, "1\n2\n3\n");
}

#[test]
fn filename_and_record_number_are_set() {
    let dir = std::env::temp_dir().join("eawk-cli-test");
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("f1.txt");
    std::fs::write(&a, "x\ny\n").unwrap();
    let o = run(&["{ print FILENAME, NR, FNR }", a.to_str().unwrap()], "");
    assert!(o.stdout.contains("f1.txt 1 1"));
    assert!(o.stdout.contains("f1.txt 2 2"));
}