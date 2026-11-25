//! Tests for the embedding API — the reason to use eawk as a library rather
//! than as a command.

use eawk::{Ctx, DivZeroPolicy, Engine, Error, ErrorKind, Options, Runner, Value};

#[test]
fn run_a_program_and_read_variables_back() {
    let mut e = Engine::new("END { print total }").unwrap();
    e.set_var("total", 42.0);
    e.run().unwrap();
    assert_eq!(e.get_var("total").to_num(), 42.0);
}

#[test]
fn input_and_output_never_touch_the_filesystem() {
    let mut e = Engine::new("{ print $1 + $2 }").unwrap();
    assert_eq!(e.exec_captured("1 2\n3 4\n").unwrap(), "3\n7\n");
}

#[test]
fn strings_flow_through_unconverted() {
    let mut e = Engine::new("{ print $1 }").unwrap();
    assert_eq!(e.exec_captured("hello world\n").unwrap(), "hello\n");
}

#[test]
fn runner_streams_records_one_at_a_time() {
    let mut r = Runner::new("{ total += $1 } END { print total }").unwrap();
    r.record("10\n").unwrap();
    r.record("20\n").unwrap();
    r.record("12\n").unwrap();
    r.finish().unwrap();
    assert_eq!(r.output(), "42\n");
}

#[test]
fn runner_accepts_a_batch() {
    let mut r = Runner::new("{ n++ } END { print n }").unwrap();
    r.records(["a\n", "b\n", "c\n"]).unwrap();
    r.finish().unwrap();
    assert_eq!(r.output(), "3\n");
}

#[test]
fn call_an_awk_function_from_rust() {
    let mut e = Engine::new("function twice(x) { return x * 2 }").unwrap();
    assert_eq!(e.call_num("twice", &[Value::from(21.0)]).unwrap(), 42.0);
    // A string times a number is numeric, so this is 0 rather than "abab".
    assert_eq!(e.call_str("twice", &[Value::from("ab")]).unwrap(), "0");
}

#[test]
fn awk_functions_can_recurse_and_use_locals() {
    let mut e = Engine::new("function fact(n,   acc) { return n <= 1 ? 1 : n * fact(n - 1) }").unwrap();
    assert_eq!(e.call_num("fact", &[Value::from(10.0)]).unwrap(), 3628800.0);
}

#[test]
fn expose_a_rust_function_to_awk() {
    let mut e = Engine::new("BEGIN { print hostdouble(21) }").unwrap();
    e.register_fn1("hostdouble", |v: Value| Ok(v.to_num() * 2.0)).unwrap();
    assert_eq!(e.run_captured().unwrap(), "42\n");
}

#[test]
fn native_functions_receive_context_and_arguments() {
    let mut e = Engine::new("BEGIN { print shout(\"hi\") }").unwrap();
    e.register_fn("shout", |_ctx: &mut Ctx, args: Vec<Value>| {
        let mut out = String::new();
        for a in &args {
            out.push_str(&a.to_str("%.6g"));
            out.push(' ');
        }
        Ok(Value::str(out.trim_end()))
    })
    .unwrap();
    assert_eq!(e.run_captured().unwrap(), "hi\n");
}

#[test]
fn a_native_can_read_and_write_awk_variables() {
    let mut e = Engine::new("BEGIN { bump(); print counter }").unwrap();
    e.register_fn("bump", |ctx: &mut Ctx, _args: Vec<Value>| {
        let n = ctx.get_var("counter").to_num() + 10.0;
        ctx.set_var("counter", n);
        Ok(Value::Num(0.0))
    })
    .unwrap();
    assert_eq!(e.run_captured().unwrap(), "10\n");
}

#[test]
fn a_native_can_read_the_current_record() {
    let mut e = Engine::new("{ print recordlen() }").unwrap();
    e.register_fn("recordlen", |ctx: &mut Ctx, _a: Vec<Value>| {
        Ok(Value::Num(ctx.record().len() as f64))
    })
    .unwrap();
    assert_eq!(e.exec_captured("abc\nabcd\n").unwrap(), "3\n4\n");
}

#[test]
fn a_failing_native_propagates_its_error() {
    let mut e = Engine::new("BEGIN { boom() }").unwrap();
    e.register_fn("boom", |_c: &mut Ctx, _a: Vec<Value>| Err(Error::runtime("deliberate failure")))
        .unwrap();
    let err = e.run().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Runtime);
    assert!(err.message.contains("deliberate failure"));
}

#[test]
fn arrays_are_reachable_from_rust() {
    let mut e = Engine::new("END { counts[\"a\"] = 1; counts[\"b\"] = 2 }").unwrap();
    e.run().unwrap();
    assert_eq!(e.len_array("counts"), 2);
    assert!(e.has_elem("counts", "a"));
    assert!(!e.has_elem("counts", "z"));
    assert_eq!(e.get_elem("counts", "b").to_num(), 2.0);

    e.set_elem("counts", "c", 3.0);
    assert_eq!(e.len_array("counts"), 3);
    assert_eq!(e.get_elem("counts", "c").to_num(), 3.0);
}

#[test]
fn field_separator_and_output_separator_can_be_set() {
    let opts = Options {
        fs: Some(",".into()),
        ofs: Some("|".into()),
        ..Default::default()
    };
    let mut e = Engine::with_options("{ print $2, $1 }", opts).unwrap();
    assert_eq!(e.exec_captured("a,b\n").unwrap(), "b|a\n");
}

#[test]
fn assignments_run_before_begin() {
    let opts = Options { assignments: vec![("threshold".into(), "10".into())], ..Default::default() };
    let mut e = Engine::with_options("END { print threshold * 2 }", opts).unwrap();
    assert_eq!(e.run_captured().unwrap(), "20\n");
}

#[test]
fn compiled_once_run_many() {
    // The point of `Interp`: parsing happens once, state does not leak between
    // runs.
    let interp = eawk::Interp::new("{ count++ } END { print NR, count }").unwrap();

    let mut a = interp.instantiate();
    a.capture_output();
    a.push_input("x\ny\n").unwrap();
    a.run().unwrap();
    assert_eq!(a.captured().unwrap(), "2 2\n");

    let mut b = interp.instantiate();
    b.capture_output();
    b.push_input("z\n").unwrap();
    b.run().unwrap();
    assert_eq!(b.captured().unwrap(), "1 1\n");
}

#[test]
fn division_by_zero_is_an_error_by_default() {
    let mut e = Engine::new("BEGIN { print 1/0 }").unwrap();
    let err = e.run().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Runtime);
    assert!(err.message.contains("division by zero"));
}

#[test]
fn division_by_zero_can_be_made_to_produce_inf() {
    let opts = Options { div_zero: DivZeroPolicy::Inf, ..Default::default() };
    // Note the parentheses: a bare `>` after a print argument is redirection.
    let mut e = Engine::with_options("BEGIN { x = 1/0; print (x > 1e300 ? \"inf\" : \"no\") }", opts).unwrap();
    assert_eq!(e.run_captured().unwrap(), "inf\n");
}

#[test]
fn a_bad_program_is_a_parse_error_not_a_panic() {
    let err = Engine::new("BEGIN { print ( }").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Parse);
}

#[test]
fn infinite_recursion_is_an_error_not_a_stack_overflow() {
    // A host must survive this: the interpreter bounds its own call depth.
    let mut e = Engine::new("function f(n) { return f(n + 1) } BEGIN { f(1) }").unwrap();
    let err = e.run().unwrap_err();
    assert!(err.message.contains("too deeply") || err.message.contains("recursion"));
}

#[test]
fn exit_zero_succeeds_and_exit_nonzero_reports() {
    assert!(Engine::new("BEGIN { exit 0 }").unwrap().run().is_ok());
    let err = Engine::new("BEGIN { exit 7 }").unwrap().run().unwrap_err();
    assert!(err.is_exit());
    assert_eq!(err.exit_status(), Some(7));
}

#[test]
fn exit_still_runs_end() {
    let mut e = Engine::new("BEGIN { exit 0 } END { print \"end\" }").unwrap();
    assert_eq!(e.run_captured().unwrap(), "end\n");
}

#[test]
fn calling_an_undefined_function_is_an_error() {
    let mut e = Engine::new("BEGIN { x = 1 }").unwrap();
    assert_eq!(e.call_num("nope", &[]).unwrap_err().kind, ErrorKind::Compile);
}

#[test]
fn values_convert_the_way_a_rust_host_expects() {
    assert_eq!(Value::from(3.0).to_num(), 3.0);
    assert_eq!(Value::from(7i64).to_num(), 7.0);
    assert_eq!(Value::from(true).to_num(), 1.0);
    assert_eq!(Value::from("x").to_str("%.6g").as_ref(), "x");
    // A host reading a field gets the text, not a reformatted number.
    let mut e = Engine::new("BEGIN { print v }").unwrap();
    e.set_var("v", Value::from("1.50"));
    assert_eq!(e.run_captured().unwrap(), "1.50\n");
}

#[test]
fn errors_carry_a_source_position() {
    let err = Engine::new("BEGIN {\n  x = (1 +\n}").unwrap_err();
    assert_eq!(err.line, Some(3));
    assert!(err.to_string().contains("eawk:"));
}

#[test]
fn push_record_respects_next() {
    let mut r = Runner::new("{ if ($1 == \"skip\") next; print }").unwrap();
    r.record("keep\n").unwrap();
    r.record("skip\n").unwrap();
    r.record("also\n").unwrap();
    r.finish().unwrap();
    assert_eq!(r.output(), "keep\nalso\n");
}

#[test]
fn regexp_literals_in_patterns_select_records() {
    let mut e = Engine::new("/^b/ { print }").unwrap();
    assert_eq!(e.exec_captured("apple\nbanana\nblue\n").unwrap(), "banana\nblue\n");
}

#[test]
fn strnum_fields_compare_numerically_against_numbers() {
    // The subtle case that a two-variant value type gets wrong.
    let mut e = Engine::new("{ if ($1 + 0 > 9) print \"big\"; else print \"small\" }").unwrap();
    assert_eq!(e.exec_captured("10\n8\n").unwrap(), "big\nsmall\n");
}

#[test]
fn a_string_from_input_keeps_its_text() {
    let mut e = Engine::new("{ print ($1 == \"1.0\") }").unwrap();
    assert_eq!(e.exec_captured("1.0\n1\n").unwrap(), "1\n0\n");
}