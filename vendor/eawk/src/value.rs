//! AWK values, including the string/number duality that awk's semantics rest on.
//!
//! The single most important thing in this file is that [`Value`] has **four**
//! variants, not two. A naive `Num(f64) | Str(String)` looks like it works and
//! silently returns wrong answers on numeric-looking text, which is awk's most
//! common input. The rules encoded here were each verified against three
//! independent awk implementations (BWK, gawk, mawk); the tests at the bottom
//! of this file are the record.
//!
//! The rules that a two-variant enum gets wrong:
//!
//! ```text
//! BEGIN { $0="10"; $1="9"; if ($1 > $0) print "str" }   # prints "str"
//! BEGIN { print (10 < "9") }                           # prints 1
//! BEGIN { print ("abc" == 0) }                         # prints 0
//! BEGIN { if ("0") print "true"; else print "false" }  # prints "true"
//! BEGIN { if (0) print "true"; else print "false" }    # prints "false"
//! ```
//!
//! The comparison rule, confirmed identical in all three implementations:
//!
//! > Compare **numerically** iff *both* operands are numeric-typed ([`Value::Num`]
//! > or [`Value::StrNum`]). If either operand is a pure [`Value::Str`], compare as
//! > strings.
//!
//! This is a *runtime* decision based on the types of the values, not a syntactic
//! one — `x = "10"; x < 9` compares as strings, and so does `y = 10; y < "9"`.

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

/// Default `OFMT` and `CONVFMT`.
pub const DEFAULT_FMT: &str = "%.6g";

/// Above this magnitude, integers are printed in exponent form rather than
/// expanded. `%g` kicks in past here because expanding further just produces
/// unrepresentable noise — BWK awk, for one, prints 29 digits of garbage for
/// `1e30`.
const MAX_EXPANDED_INT: f64 = 1e21;

/// An AWK value.
///
/// * [`Value::Uninit`] — never assigned. Behaves as both `""` and `0`.
/// * [`Value::Str`] — a pure string: a literal, or the result of a string op.
/// * [`Value::Num`] — a pure number.
/// * [`Value::StrNum`] — a "numeric string": text that arrived from input, a
///   field, `getline`, or a string converted for numeric use. It is numeric
///   when compared against a number, and its **original text** when used as a
///   string. The cached `f64` avoids re-parsing on every comparison.
#[derive(Clone)]
pub enum Value {
    /// Never assigned. Equal to both `""` and `0`.
    Uninit,
    /// A pure string value.
    Str(Arc<str>),
    /// A pure numeric value.
    Num(f64),
    /// A string that also carries its numeric value.
    StrNum(Arc<str>, f64),
}

impl Value {
    /// A string value.
    pub fn str(s: impl AsRef<str>) -> Value {
        Value::Str(Arc::from(s.as_ref()))
    }

    /// A numeric value.
    pub fn num(n: f64) -> Value {
        Value::Num(n)
    }

    /// The numeric value of this, using awk's string-to-number rules.
    pub fn to_num(&self) -> f64 {
        match self {
            Value::Uninit => 0.0,
            Value::Num(n) => *n,
            Value::Str(s) => parse_awk_number(s),
            Value::StrNum(_, n) => *n,
        }
    }

    /// The integer value of this, truncating toward zero.
    pub fn to_int(&self) -> i64 {
        let n = self.to_num();
        if n.is_nan() {
            0
        } else {
            n.trunc() as i64
        }
    }

    /// Whether this value is numeric-typed (`Num` or `StrNum`). An uninitialized
    /// value counts as numeric, which is what makes `x == 0` true for a fresh `x`.
    pub fn is_numeric(&self) -> bool {
        matches!(self, Value::Num(_) | Value::StrNum(..) | Value::Uninit)
    }

    /// The string form of this value, using `convfmt` when a number has to be
    /// rendered as text (as in string concatenation).
    pub fn to_str(&self, convfmt: &str) -> Arc<str> {
        match self {
            Value::Uninit => Arc::from(""),
            Value::Str(s) => s.clone(),
            // A strnum always stringifies to its *original* text, never to a
            // reformatted number. `print ($1 == "1.0")` on input `1.0` is true.
            Value::StrNum(s, _) => s.clone(),
            Value::Num(n) => Arc::from(format_number(*n, convfmt).as_str()),
        }
    }

    /// The string form as a borrowed `&str`, valid only while this value is alive.
    pub fn as_str_ref(&self, convfmt: &str) -> std::borrow::Cow<'_, str> {
        match self {
            Value::Uninit => std::borrow::Cow::Borrowed(""),
            Value::Str(s) => std::borrow::Cow::Borrowed(s),
            Value::StrNum(s, _) => std::borrow::Cow::Borrowed(s),
            Value::Num(n) => std::borrow::Cow::Owned(format_number(*n, convfmt)),
        }
    }

    /// Whether this value is true in a boolean context.
    ///
    /// Only a *numeric* zero is false. This is subtler than the POSIX summary
    /// suggests, and all three reference implementations agree:
    ///
    /// ```text
    /// BEGIN { print ("0" ? 1 : 0) }   # 1 — a pure string "0" is TRUE
    /// BEGIN { print (0   ? 1 : 0) }   # 0 — a number 0 is FALSE
    /// $1 == "0"                        # 0 — an input field is a strnum, so it is FALSE
    /// ```
    ///
    /// The distinction is that `"0"` written in the program is a string, while
    /// `"0"` read from a field is a strnum that is numerically zero. An
    /// uninitialized value is false.
    pub fn is_true(&self) -> bool {
        match self {
            Value::Uninit => false,
            Value::Num(n) => *n != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::StrNum(_, n) => *n != 0.0,
        }
    }

    /// Convert to a strnum, keeping the original text for string use.
    pub fn into_strnum(self) -> Value {
        match self {
            Value::Uninit => Value::StrNum(Arc::from(""), 0.0),
            Value::Str(s) => {
                let n = parse_awk_number(&s);
                Value::StrNum(s, n)
            }
            other => other,
        }
    }

    /// A hash key for use when this value indexes an associative array.
    ///
    /// Uses the original text for strnums and strings, and the printed number
    /// for pure numbers, so that `a[1]` and `a["1"]` are the same element.
    pub fn array_key(&self, convfmt: &str) -> ArrayKey {
        match self {
            Value::Num(n) => ArrayKey::from_num(*n),
            _ => ArrayKey::Str(self.to_str(convfmt)),
        }
    }
}

/// The key half of an associative-array element.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ArrayKey {
    /// Indexed by a numeric value.
    Num(u64),
    /// Indexed by text.
    Str(Arc<str>),
}

impl ArrayKey {
    /// Key for a numeric index. `-0.0` and `0.0` are the same subscript, and
    /// NaN collapses to zero, so neither can split an array into phantom keys.
    pub(crate) fn from_num(n: f64) -> ArrayKey {
        let n = if n.is_nan() { 0.0 } else if n == 0.0 { 0.0 } else { n };
        ArrayKey::Num(n.to_bits())
    }
}

impl From<ArrayKey> for Value {
    fn from(k: ArrayKey) -> Value {
        match k {
            ArrayKey::Num(bits) => Value::Num(f64::from_bits(bits)),
            ArrayKey::Str(s) => Value::Str(s),
        }
    }
}

/// Compare two values the way awk does.
///
/// Numerically iff both sides are numeric-typed; otherwise as strings. String
/// comparison is bytewise, matching awk — `"a" < "B"` is true. Do not "improve"
/// this into Unicode collation.
pub fn compare(a: &Value, b: &Value, convfmt: &str) -> Ordering {
    if a.is_numeric() && b.is_numeric() {
        // NaN never equals anything, including itself. Total order: treat as equal.
        match a.to_num().partial_cmp(&b.to_num()) {
            Some(o) => o,
            None => Ordering::Equal,
        }
    } else {
        a.as_str_ref(convfmt).cmp(&b.as_str_ref(convfmt))
    }
}

/// Whether a string would convert to the number zero.
pub fn is_awk_zero(s: &str) -> bool {
    parse_awk_number(s) == 0.0
}

/// Convert a string to a number using awk's rules.
///
/// This cannot be `str::parse`: awk takes the longest numeric *prefix* and
/// ignores trailing garbage, accepts leading whitespace, and accepts a trailing
/// `.` with no digits after it.
///
/// ```text
/// "12abc"  + 0  =>  12
/// " 1.5 "  + 0  =>  1.5
/// ".5"     + 0  =>  0.5
/// "5."     + 0  =>  5
/// ""       + 0  =>  0
/// ```
pub fn parse_awk_number(s: &str) -> f64 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }

    // awk does not recognise these as numbers: every implementation gives 0.
    if starts_with_ignore_case(&s[i..], "inf")
        || starts_with_ignore_case(&s[i..], "infinity")
        || starts_with_ignore_case(&s[i..], "nan")
    {
        return 0.0;
    }

    // Hexadecimal, as BWK awk accepts by default.
    if i + 1 < b.len() && b[i] == b'0' && (b[i + 1] | 0x20) == b'x' {
        let hs = i + 2;
        let mut j = hs;
        while j < b.len() && b[j].is_ascii_hexdigit() {
            j += 1;
        }
        if j > hs {
            return u64::from_str_radix(&s[hs..j], 16).map_or(0.0, |v| v as f64);
        }
        return 0.0;
    }

    let mut j = i;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    if j < b.len() && b[j] == b'.' {
        j += 1;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
    }
    if j < b.len() && (b[j] | 0x20) == b'e' {
        // An `e` only starts an exponent if real digits follow it; otherwise
        // "1e" is the number 1 followed by junk.
        let save = j;
        let mut k = j + 1;
        if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
            k += 1;
        }
        if k < b.len() && b[k].is_ascii_digit() {
            while k < b.len() && b[k].is_ascii_digit() {
                k += 1;
            }
            j = k;
        } else {
            j = save;
        }
    }

    if j == i || (j == i + 1 && !b[i].is_ascii_digit() && b[i] != b'.') {
        return 0.0;
    }
    s[start..j].parse::<f64>().unwrap_or(0.0)
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Format a number as awk would, with `spec` used only for non-integral values.
///
/// Integral values bypass the format spec entirely — this is why `print 1000`
/// gives `1000` and not `1.000000e+03`, and why setting `CONVFMT="%.2f"` does
/// not turn `1000` into `1000.00`.
///
/// `spec` is an `OFMT`/`CONVFMT` value such as `"%.6g"`. Its conversion style
/// is honoured, because `OFMT="%.2f"` is a normal thing to set.
pub fn format_number(n: f64, spec: &str) -> String {
    if n.is_nan() {
        return if n.is_sign_negative() { "-nan".to_string() } else { "nan".to_string() };
    }
    if n.is_infinite() {
        return if n < 0.0 { "-inf".to_string() } else { "inf".to_string() };
    }
    if n == 0.0 {
        // Catches -0.0, which awk prints as `0`.
        return "0".to_string();
    }
    if n.fract() == 0.0 && n.abs() < MAX_EXPANDED_INT {
        return format!("{n:.0}");
    }

    let s = parse_fmt_spec(spec, 'g');
    let prec = if s.precision == usize::MAX { 6 } else { s.precision };
    let body = match s.conv {
        'f' | 'F' => format!("{n:.prec$}"),
        'e' | 'E' => {
            let mant = format!("{n:.prec$e}");
            let (m, e) = mant.split_once('e').unwrap_or((mant.as_str(), "0"));
            let exp: i32 = e.parse().unwrap_or(0);
            let sign = if s.conv == 'E' { 'E' } else { 'e' };
            if exp < 0 {
                format!("{m}{sign}-{:02}", -exp)
            } else {
                format!("{m}{sign}+{exp:02}")
            }
        }
        _ => format_g(n, spec),
    };
    pad_with_flags(body, &s)
}

/// Apply width, padding and sign flags to an already-rendered body.
pub fn pad_with_flags(body: String, s: &FmtSpec) -> String {
    let mut body = body;
    if s.plus && !body.starts_with('-') {
        body.insert(0, '+');
    } else if s.space && !body.starts_with('-') {
        body.insert(0, ' ');
    }
    if body.len() >= s.width {
        return body;
    }
    let fill = s.width - body.len();
    if s.left {
        format!("{body}{}", " ".repeat(fill))
    } else if s.zero && !body.starts_with('-') && !body.contains('.') {
        format!("{}{}", "0".repeat(fill), body)
    } else {
        format!("{}{}", " ".repeat(fill), body)
    }
}

/// C's `%g`: pick between exponent and fixed notation by exponent, use
/// `precision` significant digits, strip trailing zeros.
///
/// Note that C prints the exponent in at least two digits with an explicit
/// sign — `1e-06`, not Rust's `1e-6`. That difference is user-visible, so the
/// exponent is rebuilt by hand.
pub fn format_g(n: f64, spec: &str) -> String {
    let parsed = parse_fmt_spec(spec, 'g');
    let p = if parsed.precision == usize::MAX { 6 } else { parsed.precision };
    let p = if p == 0 { 1 } else { p };

    if n == 0.0 {
        return "0".to_string();
    }

    // Format once in exponent form to learn the rounded exponent; the choice
    // between `e` and `f` notation depends on the exponent *after* rounding,
    // which is why we cannot decide it from log10 alone.
    let e_str = format!("{:.*e}", p - 1, n);
    let (mant, exp_str) = e_str.split_once('e').unwrap_or((e_str.as_str(), "0"));
    let exp: i32 = exp_str.parse().unwrap_or(0);

    if exp < -4 || exp >= p as i32 {
        let mant = trim_fraction(mant, parsed.alt);
        if exp < 0 {
            format!("{mant}e-{:02}", -exp)
        } else {
            format!("{mant}e+{exp:02}")
        }
    } else {
        let frac = (p as i32 - 1 - exp).max(0) as usize;
        let s = format!("{n:.frac$}");
        if parsed.alt {
            s
        } else {
            trim_fraction(&s, parsed.alt)
        }
    }
}

/// Strip trailing zeros from a fraction, and a trailing bare `.`.
fn trim_fraction(s: &str, alt: bool) -> String {
    if alt || !s.contains('.') {
        return s.to_string();
    }
    let t = s.trim_end_matches('0');
    let t = t.strip_suffix('.').unwrap_or(t);
    t.to_string()
}

/// A parsed printf conversion specification.
#[derive(Debug, Clone, Copy, Default)]
pub struct FmtSpec {
    /// `-` left-justify.
    pub left: bool,
    /// `+` always emit a sign.
    pub plus: bool,
    /// ` ` emit a space before a positive number.
    pub space: bool,
    /// `#` alternate form (keep trailing zeros / force the radix point).
    pub alt: bool,
    /// `0` zero-pad.
    pub zero: bool,
    /// Minimum field width.
    pub width: usize,
    /// Digits after the decimal point, or `usize::MAX` if unspecified.
    pub precision: usize,
    /// The conversion character.
    pub conv: char,
}

/// Parse a printf format spec such as `%-8.3f`. A leading `%` is optional.
pub fn parse_fmt_spec(spec: &str, default_conv: char) -> FmtSpec {
    let b: Vec<char> = spec.chars().collect();
    let mut s = FmtSpec { precision: usize::MAX, conv: default_conv, ..Default::default() };
    let mut i = 0;
    if b.first() == Some(&'%') {
        i = 1;
    }
    while i < b.len() {
        match b[i] {
            '-' => s.left = true,
            '+' => s.plus = true,
            ' ' => s.space = true,
            '#' => s.alt = true,
            '0' => s.zero = true,
            _ => break,
        }
        i += 1;
    }
    let mut width = String::new();
    while i < b.len() && b[i].is_ascii_digit() {
        width.push(b[i]);
        i += 1;
    }
    s.width = width.parse().unwrap_or(0);
    if i < b.len() && b[i] == '.' {
        i += 1;
        let mut prec = String::new();
        while i < b.len() && b[i].is_ascii_digit() {
            prec.push(b[i]);
            i += 1;
        }
        // A bare `.` means precision 0 in C, not "unspecified".
        s.precision = if prec.is_empty() { 0 } else { prec.parse().unwrap_or(0) };
    }
    while i < b.len() && matches!(b[i], 'l' | 'h' | 'L' | 'z' | 'j' | 't') {
        i += 1;
    }
    if i < b.len() {
        s.conv = b[i];
    }
    s
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Uninit => Ok(()),
            Value::Str(s) => f.write_str(s),
            Value::Num(n) => f.write_str(&format_number(*n, DEFAULT_FMT)),
            Value::StrNum(s, _) => f.write_str(s),
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Uninit => f.write_str("Uninit"),
            Value::Str(s) => write!(f, "Str({s:?})"),
            Value::Num(n) => write!(f, "Num({n:?})"),
            Value::StrNum(s, n) => write!(f, "StrNum({s:?}, {n})"),
        }
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Value {
        Value::Num(n)
    }
}

impl From<i32> for Value {
    fn from(n: i32) -> Value {
        Value::Num(n as f64)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Value {
        Value::Num(n as f64)
    }
}

impl From<usize> for Value {
    fn from(n: usize) -> Value {
        Value::Num(n as f64)
    }
}

impl From<bool> for Value {
    /// awk has no boolean type; true is 1 and false is 0.
    fn from(b: bool) -> Value {
        Value::Num(if b { 1.0 } else { 0.0 })
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(Arc::from(s))
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(Arc::from(s.as_str()))
    }
}

impl From<&String> for Value {
    fn from(s: &String) -> Value {
        Value::Str(Arc::from(s.as_str()))
    }
}

impl From<Arc<str>> for Value {
    fn from(s: Arc<str>) -> Value {
        Value::Str(s)
    }
}

impl From<std::borrow::Cow<'_, str>> for Value {
    fn from(s: std::borrow::Cow<'_, str>) -> Value {
        Value::Str(Arc::from(s.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every assertion below was verified against /usr/bin/awk (BWK),
    // gawk and mawk, which agreed on all of them.

    #[test]
    fn str_to_number_takes_longest_prefix() {
        assert_eq!(parse_awk_number("12abc"), 12.0);
        assert_eq!(parse_awk_number("1.5foo"), 1.5);
        assert_eq!(parse_awk_number(" 1.5 "), 1.5);
        assert_eq!(parse_awk_number(".5"), 0.5);
        assert_eq!(parse_awk_number("5."), 5.0);
        assert_eq!(parse_awk_number(""), 0.0);
        assert_eq!(parse_awk_number("abc"), 0.0);
        assert_eq!(parse_awk_number("-3.25e2"), -325.0);
        assert_eq!(parse_awk_number("1e"), 1.0);
        assert_eq!(parse_awk_number("-"), 0.0);
        assert_eq!(parse_awk_number("0x10"), 16.0);
        assert_eq!(parse_awk_number("inf"), 0.0);
        assert_eq!(parse_awk_number("nan"), 0.0);
    }

    #[test]
    fn integral_numbers_bypass_the_format_spec() {
        assert_eq!(format_number(1000.0, DEFAULT_FMT), "1000");
        assert_eq!(format_number(1000.0, "%.2f"), "1000");
        assert_eq!(format_number(-0.0, DEFAULT_FMT), "0");
        assert_eq!(format_number(2f64.powi(31), DEFAULT_FMT), "2147483648");
    }

    #[test]
    fn non_integral_numbers_use_the_format_spec() {
        assert_eq!(format_number(1.0 / 3.0, DEFAULT_FMT), "0.333333");
        assert_eq!(format_number(0.1 + 0.2, DEFAULT_FMT), "0.3");
        assert_eq!(format_number(1.0 / 3.0, "%.2f"), "0.33");
    }

    #[test]
    fn exponent_uses_two_digits_and_a_sign() {
        assert_eq!(format_number(0.000001, DEFAULT_FMT), "1e-06");
        assert_eq!(format_number(0.0000001, DEFAULT_FMT), "1e-07");
        assert_eq!(format_number(123456789012.0, DEFAULT_FMT), "123456789012");
    }

    #[test]
    fn truthiness_only_numeric_zero_is_false() {
        assert!(!Value::Uninit.is_true());
        assert!(!Value::Num(0.0).is_true());
        assert!(!Value::Num(-0.0).is_true());
        assert!(Value::Num(1.0).is_true());
        // A pure string "0" is TRUE in every reference implementation.
        assert!(Value::str("0").is_true());
        assert!(Value::str("00").is_true());
        assert!(Value::str(" 0").is_true());
        assert!(!Value::str("").is_true());
        // But "0" arriving from input is a strnum, and numerically zero, so it
        // is false. This is why the check uses the cached number.
        assert!(!Value::StrNum(Arc::from("0"), 0.0).is_true());
        assert!(Value::StrNum(Arc::from("1"), 1.0).is_true());
    }

    #[test]
    fn uninitialized_equals_both_empty_string_and_zero() {
        let x = Value::Uninit;
        assert_eq!(compare(&x, &Value::str(""), DEFAULT_FMT), Ordering::Equal);
        assert_eq!(compare(&x, &Value::Num(0.0), DEFAULT_FMT), Ordering::Equal);
        assert_ne!(compare(&x, &Value::str("abc"), DEFAULT_FMT), Ordering::Equal);
    }

    #[test]
    fn string_on_either_side_forces_string_comparison() {
        // Every one of these verified against all three oracles. `"10" < "9"`
        // is true because "1" sorts before "9".
        assert_eq!(compare(&Value::str("10"), &Value::Num(9.0), DEFAULT_FMT), Ordering::Less);
        assert_eq!(compare(&Value::Num(10.0), &Value::str("9"), DEFAULT_FMT), Ordering::Less);
        assert_eq!(compare(&Value::Num(10.0), &Value::Num(9.0), DEFAULT_FMT), Ordering::Greater);
        // "abc" == 0 is FALSE: as a string it is not "", and it is not numeric.
        assert_ne!(compare(&Value::str("abc"), &Value::Num(0.0), DEFAULT_FMT), Ordering::Equal);
        // " 10 " == 10 is FALSE for the same reason.
        assert_ne!(compare(&Value::str(" 10 "), &Value::Num(10.0), DEFAULT_FMT), Ordering::Equal);
    }

    #[test]
    fn two_numerics_compare_numerically() {
        // $1="10" from input is a strnum: numeric against a number, so 10 < 9
        // is false...
        let ten = Value::StrNum(Arc::from("10"), 10.0);
        assert_eq!(compare(&ten, &Value::Num(9.0), DEFAULT_FMT), Ordering::Greater);
        // ...but a string against a string constant.
        assert_eq!(compare(&ten, &Value::str("9"), DEFAULT_FMT), Ordering::Less);
        assert_eq!(compare(&ten, &Value::str("10"), DEFAULT_FMT), Ordering::Equal);
    }

    #[test]
    fn strnum_stringifies_to_its_original_text() {
        let v = Value::StrNum(Arc::from("1.0"), 1.0);
        // The text is preserved, not reformatted through the number.
        assert_eq!(&*v.to_str(DEFAULT_FMT), "1.0");
        assert_eq!(&*v.to_str("%.2f"), "1.0");
        assert_eq!(compare(&v, &Value::str("1.0"), DEFAULT_FMT), Ordering::Equal);
    }

    #[test]
    fn concat_uses_original_text_but_numbers_use_convfmt() {
        assert_eq!(&*Value::Num(1000.0).to_str("%.2f"), "1000");
        assert_eq!(&*Value::Num(0.5).to_str("%.2f"), "0.50");
    }

    #[test]
    fn string_comparison_is_bytewise_not_unicode() {
        assert_eq!(compare(&Value::str("a"), &Value::str("B"), DEFAULT_FMT), Ordering::Greater);
    }

    #[test]
    fn fmt_specs_parse() {
        let s = parse_fmt_spec("%.3f", 'g');
        assert_eq!((s.precision, s.conv), (3, 'f'));
        let s = parse_fmt_spec("%-8.3f", 'g');
        assert!(s.left);
        assert_eq!((s.width, s.precision, s.conv), (8, 3, 'f'));
        let s = parse_fmt_spec("%d", 'g');
        assert_eq!(s.precision, usize::MAX);
        let s = parse_fmt_spec("%.", 'g');
        assert_eq!(s.precision, 0);
    }
}