#!/usr/bin/env python3
"""Generate tests/golden.rs from a case list, taking expected output from a
reference awk. Regenerate with:  python3 tests/gen_golden.py"""

import subprocess, sys, os, json

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ORACLE = os.environ.get("ORACLE", "/usr/bin/awk")

CASES = [
    ("fields",             "{ print $1, $2 }",              "1 2 3\n4 5 6\n"),
    ("default_print",      "{ print }",                    "1 2 3\n4 5 6\n"),
    ("pattern_only",       "$2 == 5",                      "1 2 3\n4 5 6\n"),
    ("range",              "NR == 2, NR == 3 { print \"r\", $0 }", "a\nb\nc\nd\n"),
    ("regex_anchor",       "/^a/ { print }",               "apple\nbanana\napricot\n"),
    ("regex_alt",          "/alpha|gamma/ { print }",      "alpha\nbeta\ngamma\n"),
    ("regex_class",        "/[0-9]+/ { n++ } END { print n+0 }", "a1\n22b\nc\n"),
    ("regex_quant",        "/^[0-9]{1,3}$/ { print }",    "1\n123\n1234\n"),
    ("regex_neg_class",    "/[^0-9]/ { print }",           "1a\nbb\n33\n"),
    ("regex_leftmost_longest", 'BEGIN { print match("ab", /a|ab/) }', ""),
    ("sum_fields",         "{ for (i=1;i<=NF;i++) s+=$i } END { print s+0 }", "1 2 3\n4 5 6\n"),
    ("arith",              'BEGIN { print 1/3, 1000, 2^31, -2^2, 2^3^2, 7%3, -7%3 }', ""),
    ("concat",             'BEGIN { print "a" 1, 1 " " 2, "x" "" "y" }', ""),
    ("numeric_format",     'BEGIN { print 0.1+0.2, 1e6, 0.000001, 1000000 }', ""),
    ("ofmt",               'BEGIN { OFMT="%.2f"; print 1/3 }', ""),
    ("convfmt",            'BEGIN { CONVFMT="%.3g"; print (1/3) "" }', ""),
    ("printf_basic",       'BEGIN { printf "%d|%5.2f|%s|%c|%x\\n", 42, 3.14159, "hi", 65, 255 }', ""),
    ("printf_flags",       'BEGIN { printf "[%-6s][%06d][%+d]\\n", "ab", 42, 42 }', ""),
    ("printf_precision",   'BEGIN { printf "%10.3s|\\n", "abcdefgh" }', ""),
    ("substr",             'BEGIN { print substr("hello",2,3), substr("hello",2), "|", substr("hello",10,2) }', ""),
    ("index_fn",           'BEGIN { print index("hello","ll"), index("hello","z") }', ""),
    ("length_fn",          'BEGIN { print length("abc"), length(12345) }', ""),
    ("split_basic",        'BEGIN { n=split("a:b:c", p, ":"); print n, p[1], p[3] }', ""),
    ("split_default",      'BEGIN { n=split("a b  c", p); print n, p[2] }', ""),
    ("split_trailing",     'BEGIN { print split("a:b:", p, ":"), split("", q, ":"), split(",,", r, ",") }', ""),
    ("gsub_two_arg",       '{ gsub(/[0-9]/, ""); print }', "a1b2\nc3\n"),
    ("sub_three_arg",      'BEGIN { s="aaa"; print sub(/a/,"b",s), s }', ""),
    ("gsub_ampersand",     'BEGIN { s="cat"; gsub(/[ao]/,"[&]", s); print s }', ""),
    ("match_rstart",       'BEGIN { print match("foobar","o+"), RSTART, RLENGTH }', ""),
    ("match_no_match",     'BEGIN { print match("abc","z"), RSTART, RLENGTH }', ""),
    ("arrays",             'BEGIN { a["x"]=1; a["y"]=2; s=0; for(k in a) s+=a[k]; print s+0 }', ""),
    ("array_in",           'BEGIN { a[1]=1; print (1 in a), (2 in a) }', ""),
    ("array_delete",       'BEGIN { a[1]=1; a[2]=2; delete a[1]; n=0; for(k in a) n++; print n+0 }', ""),
    ("multidim",           'BEGIN { a[1,2]=7; for(k in a){ split(k,p,SUBSEP); print p[1], p[2] } }', ""),
    ("functions",          'function f(a,b){ return a+b } BEGIN { print f(1,2) }', ""),
    ("recursion",          'function fact(n){ return n<=1 ? 1 : n*fact(n-1) } BEGIN { print fact(6) }', ""),
    ("local_params",       'function f(a,   tmp){ tmp=99; return a } BEGIN { print f(1) }', ""),
    ("control_flow",       'BEGIN { for(i=0;i<5;i++){ if(i%2) continue; if(i>3) break; s+=i } print s+0 }', ""),
    ("while_do",           'BEGIN { i=0; while(i<3) i++; do i--; while(i>0); print i }', ""),
    ("ternary",            'BEGIN { print (1>2) ? "a" : "b" }', ""),
    ("incr_decr",          'BEGIN { i=5; print i++, i, ++i, i--, --i }', ""),
    ("assignments",        'BEGIN { x=5; x+=3; x-=1; x*=2; x/=7; print x }', ""),
    ("uninitialized",      'BEGIN { print "[" x "]", (x==""), (x==0), length(x) }', ""),
    ("truthiness",         'BEGIN { print ("0"?1:0), (0?1:0), (""?1:0), ("abc"?1:0) }', ""),
    ("strnum_compare",     'BEGIN { $0="10"; $1="9"; print ($1>$0), (10<"9"), ($1+0>$0+0) }', ""),
    ("next_stmt",          '{ if (NR==2) next; print }', "a\nb\nc\n"),
    ("begin_end",          'BEGIN { print "b" } { print "m" } END { print "e" }', "1\n2\n"),
    ("nf_assign",          'BEGIN { $0="a b c"; NF=1; print NF, $0 }', ""),
    ("field_grow",         'BEGIN { $0="a b c"; $5="e"; print NF, $0 }', ""),
    ("field_rebuild",      '{ $2 = "X"; print }', "a b c\n"),
    ("dollar0_set",        '{ $0 = toupper($0); print }', "hi there\n"),
    ("ofs_output",         'BEGIN { OFS="-"; $0="a b c"; print $1, $2 }', ""),
    ("ors_output",         'BEGIN { ORS="|"; print "a"; print "b" }', ""),
    ("case_functions",     'BEGIN { print toupper("aBc"), tolower("AbC") }', ""),
    ("math_functions",     'BEGIN { printf "%.4f %.4f %.4f\\n", sqrt(2), exp(1), log(10) }', ""),
    ("int_fn",             'BEGIN { print int(-3.7), int(3.7) }', ""),
    ("special_vars",       "{ print NR, NF, FNR }", "a b\nc d\n"),
    ("comparisons",        'BEGIN { print (1<2),(2<=2),(3>4),("a"<"b"),(1==1),(1!=2) }', ""),
    ("logical_not",        'BEGIN { print !0, !1, !"", !"x" }', ""),
    ("string_bytewise",    'BEGIN { print ("a" < "B") }', ""),
    ("unicode_passthrough","{ print toupper($1) }", "café\n"),
    ("sprintf_reuse",      'BEGIN { printf "%s-%s\\n", "a", "b", "c", "d" }', ""),
    ("fs_as_variable",     'BEGIN { FS=":" } { print $2 }', "a:b\nc:d\n"),
    ("empty_record",       "{ print NF, \"[\" $0 \"]\" }", "\n"),
    ("missing_fields",     '{ print $2 "[" $2 "]" }', "only\n"),
]


def oracle(prog, inp):
    p = subprocess.run([ORACLE, prog], input=inp, capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"oracle failed for {prog!r}: {p.stderr}")
    return p.stdout


def rs(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n") + '"'


out = ['''//! Golden tests: every expectation here was produced by a reference awk and
//! cross-checked against all three implementations on the machine
//! (`tests/diff_oracle.sh`). They run offline, with no oracle required.
//!
//! Regenerate with `python3 tests/gen_golden.py`.

use eawk::{Engine, Options};

fn check(program: &str, input: &str, expected: &str) {
    let mut e = Engine::new(program).unwrap_or_else(|err| panic!("{program}: {err}"));
    let got = e.exec_captured(input).unwrap_or_else(|err| panic!("{program}: {err}"));
    assert_eq!(got, expected, "program: {program}\\ninput: {input:?}");
}

fn check_with(program: &str, input: &str, expected: &str, opts: Options) {
    let mut e = Engine::with_options(program, opts).unwrap();
    let got = e.exec_captured(input).unwrap();
    assert_eq!(got, expected, "program: {program}");
}
''']

for name, prog, inp in CASES:
    exp = oracle(prog, inp)
    out.append(f"#[test]\nfn {name}() {{\n    check({rs(prog)}, {rs(inp)}, {rs(exp)});\n}}\n")

# A few cases that need options rather than a bare program.
out.append('''#[test]
fn field_separator_option() {
    let opts = Options { fs: Some(",".into()), ..Default::default() };
    check_with("{ print $2, $1 }", "a,b\\n", "b a\\n", opts);
}

#[test]
fn output_separator_option() {
    let opts = Options { ofs: Some("|".into()), ..Default::default() };
    check_with("{ print $1, $2 }", "a b\\n", "a|b\\n", opts);
}

#[test]
fn command_line_assignment_option() {
    let opts = Options { assignments: vec![("n".into(), "3".into())], ..Default::default() };
    check_with("END { print n * 2 }", "", "6\\n", opts);
}
''')

path = os.path.join(ROOT, "tests", "golden.rs")
with open(path, "w") as f:
    f.write("\n".join(out))
print(f"wrote {path} with {len(CASES) + 3} cases")