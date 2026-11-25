//! Golden tests: every expectation here was produced by a reference awk and
//! cross-checked against all three implementations on the machine
//! (`tests/diff_oracle.sh`). They run offline, with no oracle required.
//!
//! Regenerate with `python3 tests/gen_golden.py`.

use eawk::{Engine, Options};

fn check(program: &str, input: &str, expected: &str) {
    let mut e = Engine::new(program).unwrap_or_else(|err| panic!("{program}: {err}"));
    let got = e.exec_captured(input).unwrap_or_else(|err| panic!("{program}: {err}"));
    assert_eq!(got, expected, "program: {program}\ninput: {input:?}");
}

fn check_with(program: &str, input: &str, expected: &str, opts: Options) {
    let mut e = Engine::with_options(program, opts).unwrap();
    let got = e.exec_captured(input).unwrap();
    assert_eq!(got, expected, "program: {program}");
}

#[test]
fn fields() {
    check("{ print $1, $2 }", "1 2 3\n4 5 6\n", "1 2\n4 5\n");
}

#[test]
fn default_print() {
    check("{ print }", "1 2 3\n4 5 6\n", "1 2 3\n4 5 6\n");
}

#[test]
fn pattern_only() {
    check("$2 == 5", "1 2 3\n4 5 6\n", "4 5 6\n");
}

#[test]
fn range() {
    check("NR == 2, NR == 3 { print \"r\", $0 }", "a\nb\nc\nd\n", "r b\nr c\n");
}

#[test]
fn regex_anchor() {
    check("/^a/ { print }", "apple\nbanana\napricot\n", "apple\napricot\n");
}

#[test]
fn regex_alt() {
    check("/alpha|gamma/ { print }", "alpha\nbeta\ngamma\n", "alpha\ngamma\n");
}

#[test]
fn regex_class() {
    check("/[0-9]+/ { n++ } END { print n+0 }", "a1\n22b\nc\n", "2\n");
}

#[test]
fn regex_quant() {
    check("/^[0-9]{1,3}$/ { print }", "1\n123\n1234\n", "1\n123\n");
}

#[test]
fn regex_neg_class() {
    check("/[^0-9]/ { print }", "1a\nbb\n33\n", "1a\nbb\n");
}

#[test]
fn regex_leftmost_longest() {
    check("BEGIN { print match(\"ab\", /a|ab/) }", "", "1\n");
}

#[test]
fn sum_fields() {
    check("{ for (i=1;i<=NF;i++) s+=$i } END { print s+0 }", "1 2 3\n4 5 6\n", "21\n");
}

#[test]
fn arith() {
    check("BEGIN { print 1/3, 1000, 2^31, -2^2, 2^3^2, 7%3, -7%3 }", "", "0.333333 1000 2147483648 -4 512 1 -1\n");
}

#[test]
fn concat() {
    check("BEGIN { print \"a\" 1, 1 \" \" 2, \"x\" \"\" \"y\" }", "", "a1 1 2 xy\n");
}

#[test]
fn numeric_format() {
    check("BEGIN { print 0.1+0.2, 1e6, 0.000001, 1000000 }", "", "0.3 1000000 1e-06 1000000\n");
}

#[test]
fn ofmt() {
    check("BEGIN { OFMT=\"%.2f\"; print 1/3 }", "", "0.33\n");
}

#[test]
fn convfmt() {
    check("BEGIN { CONVFMT=\"%.3g\"; print (1/3) \"\" }", "", "0.333\n");
}

#[test]
fn printf_basic() {
    check("BEGIN { printf \"%d|%5.2f|%s|%c|%x\\n\", 42, 3.14159, \"hi\", 65, 255 }", "", "42| 3.14|hi|A|ff\n");
}

#[test]
fn printf_flags() {
    check("BEGIN { printf \"[%-6s][%06d][%+d]\\n\", \"ab\", 42, 42 }", "", "[ab    ][000042][+42]\n");
}

#[test]
fn printf_precision() {
    check("BEGIN { printf \"%10.3s|\\n\", \"abcdefgh\" }", "", "       abc|\n");
}

#[test]
fn substr() {
    check("BEGIN { print substr(\"hello\",2,3), substr(\"hello\",2), \"|\", substr(\"hello\",10,2) }", "", "ell ello | \n");
}

#[test]
fn index_fn() {
    check("BEGIN { print index(\"hello\",\"ll\"), index(\"hello\",\"z\") }", "", "3 0\n");
}

#[test]
fn length_fn() {
    check("BEGIN { print length(\"abc\"), length(12345) }", "", "3 5\n");
}

#[test]
fn split_basic() {
    check("BEGIN { n=split(\"a:b:c\", p, \":\"); print n, p[1], p[3] }", "", "3 a c\n");
}

#[test]
fn split_default() {
    check("BEGIN { n=split(\"a b  c\", p); print n, p[2] }", "", "3 b\n");
}

#[test]
fn split_trailing() {
    check("BEGIN { print split(\"a:b:\", p, \":\"), split(\"\", q, \":\"), split(\",,\", r, \",\") }", "", "3 0 3\n");
}

#[test]
fn gsub_two_arg() {
    check("{ gsub(/[0-9]/, \"\"); print }", "a1b2\nc3\n", "ab\nc\n");
}

#[test]
fn sub_three_arg() {
    check("BEGIN { s=\"aaa\"; print sub(/a/,\"b\",s), s }", "", "1 baa\n");
}

#[test]
fn gsub_ampersand() {
    check("BEGIN { s=\"cat\"; gsub(/[ao]/,\"[&]\", s); print s }", "", "c[a]t\n");
}

#[test]
fn match_rstart() {
    check("BEGIN { print match(\"foobar\",\"o+\"), RSTART, RLENGTH }", "", "2 2 2\n");
}

#[test]
fn match_no_match() {
    check("BEGIN { print match(\"abc\",\"z\"), RSTART, RLENGTH }", "", "0 0 -1\n");
}

#[test]
fn arrays() {
    check("BEGIN { a[\"x\"]=1; a[\"y\"]=2; s=0; for(k in a) s+=a[k]; print s+0 }", "", "3\n");
}

#[test]
fn array_in() {
    check("BEGIN { a[1]=1; print (1 in a), (2 in a) }", "", "1 0\n");
}

#[test]
fn array_delete() {
    check("BEGIN { a[1]=1; a[2]=2; delete a[1]; n=0; for(k in a) n++; print n+0 }", "", "1\n");
}

#[test]
fn multidim() {
    check("BEGIN { a[1,2]=7; for(k in a){ split(k,p,SUBSEP); print p[1], p[2] } }", "", "1 2\n");
}

#[test]
fn functions() {
    check("function f(a,b){ return a+b } BEGIN { print f(1,2) }", "", "3\n");
}

#[test]
fn recursion() {
    check("function fact(n){ return n<=1 ? 1 : n*fact(n-1) } BEGIN { print fact(6) }", "", "720\n");
}

#[test]
fn local_params() {
    check("function f(a,   tmp){ tmp=99; return a } BEGIN { print f(1) }", "", "1\n");
}

#[test]
fn control_flow() {
    check("BEGIN { for(i=0;i<5;i++){ if(i%2) continue; if(i>3) break; s+=i } print s+0 }", "", "2\n");
}

#[test]
fn while_do() {
    check("BEGIN { i=0; while(i<3) i++; do i--; while(i>0); print i }", "", "0\n");
}

#[test]
fn ternary() {
    check("BEGIN { print (1>2) ? \"a\" : \"b\" }", "", "b\n");
}

#[test]
fn incr_decr() {
    check("BEGIN { i=5; print i++, i, ++i, i--, --i }", "", "5 6 7 7 5\n");
}

#[test]
fn assignments() {
    check("BEGIN { x=5; x+=3; x-=1; x*=2; x/=7; print x }", "", "2\n");
}

#[test]
fn uninitialized() {
    check("BEGIN { print \"[\" x \"]\", (x==\"\"), (x==0), length(x) }", "", "[] 1 1 0\n");
}

#[test]
fn truthiness() {
    check("BEGIN { print (\"0\"?1:0), (0?1:0), (\"\"?1:0), (\"abc\"?1:0) }", "", "1 0 0 1\n");
}

#[test]
fn strnum_compare() {
    check("BEGIN { $0=\"10\"; $1=\"9\"; print ($1>$0), (10<\"9\"), ($1+0>$0+0) }", "", "0 1 0\n");
}

#[test]
fn next_stmt() {
    check("{ if (NR==2) next; print }", "a\nb\nc\n", "a\nc\n");
}

#[test]
fn begin_end() {
    check("BEGIN { print \"b\" } { print \"m\" } END { print \"e\" }", "1\n2\n", "b\nm\nm\ne\n");
}

#[test]
fn nf_assign() {
    check("BEGIN { $0=\"a b c\"; NF=1; print NF, $0 }", "", "1 a\n");
}

#[test]
fn field_grow() {
    check("BEGIN { $0=\"a b c\"; $5=\"e\"; print NF, $0 }", "", "5 a b c  e\n");
}

#[test]
fn field_rebuild() {
    check("{ $2 = \"X\"; print }", "a b c\n", "a X c\n");
}

#[test]
fn dollar0_set() {
    check("{ $0 = toupper($0); print }", "hi there\n", "HI THERE\n");
}

#[test]
fn ofs_output() {
    check("BEGIN { OFS=\"-\"; $0=\"a b c\"; print $1, $2 }", "", "a-b\n");
}

#[test]
fn ors_output() {
    check("BEGIN { ORS=\"|\"; print \"a\"; print \"b\" }", "", "a|b|");
}

#[test]
fn case_functions() {
    check("BEGIN { print toupper(\"aBc\"), tolower(\"AbC\") }", "", "ABC abc\n");
}

#[test]
fn math_functions() {
    check("BEGIN { printf \"%.4f %.4f %.4f\\n\", sqrt(2), exp(1), log(10) }", "", "1.4142 2.7183 2.3026\n");
}

#[test]
fn int_fn() {
    check("BEGIN { print int(-3.7), int(3.7) }", "", "-3 3\n");
}

#[test]
fn special_vars() {
    check("{ print NR, NF, FNR }", "a b\nc d\n", "1 2 1\n2 2 2\n");
}

#[test]
fn comparisons() {
    check("BEGIN { print (1<2),(2<=2),(3>4),(\"a\"<\"b\"),(1==1),(1!=2) }", "", "1 1 0 1 1 1\n");
}

#[test]
fn logical_not() {
    check("BEGIN { print !0, !1, !\"\", !\"x\" }", "", "1 0 1 0\n");
}

#[test]
fn string_bytewise() {
    check("BEGIN { print (\"a\" < \"B\") }", "", "0\n");
}

#[test]
fn unicode_passthrough() {
    check("{ print toupper($1) }", "café\n", "CAFÉ\n");
}

#[test]
fn sprintf_reuse() {
    check("BEGIN { printf \"%s-%s\\n\", \"a\", \"b\", \"c\", \"d\" }", "", "a-b\n");
}

#[test]
fn fs_as_variable() {
    check("BEGIN { FS=\":\" } { print $2 }", "a:b\nc:d\n", "b\nd\n");
}

#[test]
fn empty_record() {
    check("{ print NF, \"[\" $0 \"]\" }", "\n", "0 []\n");
}

#[test]
fn missing_fields() {
    check("{ print $2 \"[\" $2 \"]\" }", "only\n", "[]\n");
}

#[test]
fn field_separator_option() {
    let opts = Options { fs: Some(",".into()), ..Default::default() };
    check_with("{ print $2, $1 }", "a,b\n", "b a\n", opts);
}

#[test]
fn output_separator_option() {
    let opts = Options { ofs: Some("|".into()), ..Default::default() };
    check_with("{ print $1, $2 }", "a b\n", "a|b\n", opts);
}

#[test]
fn command_line_assignment_option() {
    let opts = Options { assignments: vec![("n".into(), "3".into())], ..Default::default() };
    check_with("END { print n * 2 }", "", "6\n", opts);
}
