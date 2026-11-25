//! The `printf` / `sprintf` conversion engine.
//!
//! awk's printf is C's printf, with one awk-specific detail: `%s` renders a
//! number using `CONVFMT`.
//!
//! Arguments beyond the conversions in the format string are ignored. gawk
//! restarts the format for the leftovers, but BWK awk and mawk do not, and eawk
//! follows the majority.

use crate::error::Error;
use crate::value::{pad_with_flags, parse_fmt_spec, FmtSpec, Value};

/// Format `args` according to `fmt`.
///
/// A conversion with no argument left produces nothing, matching awk, as do
/// any surplus arguments.
pub fn sprintf(fmt: &str, args: &[Value], convfmt: &str) -> Result<String, Error> {
    let mut out = String::new();
    let mut arg = 0usize;
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i < chars.len() && chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        let (spec, next) = parse_spec_at(&chars, i, args, &mut arg)?;
        i = next;
        match args.get(arg) {
            Some(v) => {
                arg += 1;
                out.push_str(&render(&spec, v, convfmt)?);
            }
            // Nothing left to substitute.
            None => {}
        }
    }
    Ok(out)
}

/// Parse a conversion spec starting at `i`, advancing `arg` for `*`.
fn parse_spec_at(
    c: &[char],
    mut i: usize,
    args: &[Value],
    arg: &mut usize,
) -> Result<(FmtSpec, usize), Error> {
    let mut left = false;
    let mut plus = false;
    let mut space = false;
    let mut alt = false;
    let mut zero = false;

    // Flags.
    loop {
        match c.get(i) {
            Some('-') => left = true,
            Some('+') => plus = true,
            Some(' ') => space = true,
            Some('#') => alt = true,
            Some('0') => zero = true,
            _ => break,
        }
        i += 1;
    }

    // Width.
    let mut width = String::new();
    while matches!(c.get(i), Some(d) if d.is_ascii_digit()) {
        width.push(c[i]);
        i += 1;
    }
    let mut width_val: usize = width.parse().unwrap_or(0);
    if c.get(i) == Some(&'*') {
        i += 1;
        width_val = args.get(*arg).map_or(0, |v| v.to_num().abs() as usize);
        *arg += 1;
    }

    // Precision.
    let mut precision = usize::MAX;
    if c.get(i) == Some(&'.') {
        i += 1;
        let mut p = String::new();
        while matches!(c.get(i), Some(d) if d.is_ascii_digit()) {
            p.push(c[i]);
            i += 1;
        }
        if c.get(i) == Some(&'*') {
            i += 1;
            precision = args.get(*arg).map_or(0, |v| v.to_num().max(0.0) as usize);
            *arg += 1;
        } else {
            precision = if p.is_empty() { 0 } else { p.parse().unwrap_or(0) };
        }
    }

    // Length modifiers are accepted and ignored, as awk does.
    while matches!(c.get(i), Some('l') | Some('h') | Some('L') | Some('z') | Some('j') | Some('t')) {
        i += 1;
    }

    let conv = c.get(i).copied().unwrap_or('s');
    i += 1;

    Ok((
        FmtSpec { left, plus, space, alt, zero, width: width_val, precision, conv },
        i,
    ))
}

/// Render one argument according to a parsed spec.
fn render(spec: &FmtSpec, v: &Value, convfmt: &str) -> Result<String, Error> {
    let prec = if spec.precision == usize::MAX { usize::MAX } else { spec.precision };
    Ok(match spec.conv {
        // `%d` truncates toward zero. A float too large for i64 saturates
        // rather than wrapping the way C's undefined behaviour would.
        'd' | 'i' => {
            let n = v.to_num();
            let i = if n.is_nan() {
                0
            } else {
                n.trunc().clamp(i64::MIN as f64, i64::MAX as f64) as i64
            };
            let s = i.to_string();
            pad_with_flags(if spec.alt && i >= 0 { format!("{s}") } else { s }, spec)
        }
        'u' => {
            let n = v.to_num().trunc();
            let n = if n.is_nan() { 0.0 } else { n };
            pad_with_flags(format!("{}", n as u64), spec)
        }
        'o' => {
            let n = v.to_num().trunc();
            let n = if n.is_nan() { 0.0 } else { n } as i64 as u64;
            let s = format!("{n:o}");
            pad_with_flags(if spec.alt { format!("0{s}") } else { s }, spec)
        }
        'x' => {
            let n = v.to_num().trunc();
            let n = if n.is_nan() { 0.0 } else { n } as i64 as u64;
            let s = format!("{n:x}");
            pad_with_flags(if spec.alt { format!("0x{s}") } else { s }, spec)
        }
        'X' => {
            let n = v.to_num().trunc();
            let n = if n.is_nan() { 0.0 } else { n } as i64 as u64;
            let s = format!("{n:X}");
            pad_with_flags(if spec.alt { format!("0X{s}") } else { s }, spec)
        }
        'c' => {
            let s = match v {
                Value::Str(s) => s.chars().next().map(String::from).unwrap_or_default(),
                other => {
                    char::from_u32(other.to_num() as u32).map(String::from).unwrap_or_default()
                }
            };
            pad_with_flags(s, spec)
        }
        's' => {
            let s = v.to_str(convfmt);
            let s: &str = &s;
            let s = if prec == usize::MAX { s.to_string() } else { s.chars().take(prec).collect() };
            pad_with_flags(s, spec)
        }
        'e' | 'E' | 'f' | 'F' | 'g' | 'G' => render_float(spec, v.to_num()),
        // Unknown conversions are emitted literally, as many awks do.
        other => format!("%{other}"),
    })
}

/// Render a floating-point conversion.
fn render_float(spec: &FmtSpec, n: f64) -> String {
    let p = if spec.precision == usize::MAX { 6 } else { spec.precision };
    let body = match spec.conv {
        'f' | 'F' => format!("{n:.p$}"),
        'e' => exponent_form(format!("{n:.p$e}"), 'e'),
        'E' => exponent_form(format!("{n:.p$e}"), 'E'),
        // %g picks between exponent and fixed notation by exponent.
        _ => {
            let spec_str = format!("%{}.{}{}", if spec.alt { "#" } else { "" }, p, spec.conv);
            crate::value::format_g(n, &spec_str)
        }
    };
    pad_with_flags(body, spec)
}

fn exponent_form(s: String, sign_char: char) -> String {
    match s.split_once('e') {
        Some((m, e)) => match e.parse::<i32>() {
            Ok(v) if v < 0 => format!("{m}{sign_char}-{:02}", -v),
            Ok(v) => format!("{m}{sign_char}+{v:02}"),
            Err(_) => s,
        },
        None => s,
    }
}

/// Parse a plain `OFMT`/`CONVFMT` string.
pub fn parse_default_spec(spec: &str) -> FmtSpec {
    parse_fmt_spec(spec, 'g')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(fmt: &str, args: &[Value]) -> String {
        sprintf(fmt, args, "%.6g").unwrap()
    }

    #[test]
    fn integer_conversions_truncate_toward_zero() {
        assert_eq!(f("%d", &[Value::Num(3.9)]), "3");
        assert_eq!(f("%d", &[Value::Num(-3.9)]), "-3");
        assert_eq!(f("%i", &[Value::Num(42.0)]), "42");
    }

    #[test]
    fn string_and_float_conversions() {
        assert_eq!(f("%s", &[Value::str("hi")]), "hi");
        assert_eq!(f("%.2f", &[Value::Num(1.0 / 3.0)]), "0.33");
        assert_eq!(f("%e", &[Value::Num(12345.6789)]), "1.234568e+04");
        assert_eq!(f("%5.2f", &[Value::Num(3.5)]), " 3.50");
        assert_eq!(f("%-5d|", &[Value::Num(42.0)]), "42   |");
        assert_eq!(f("%05d", &[Value::Num(42.0)]), "00042");
    }

    #[test]
    fn literals_and_escapes() {
        assert_eq!(f("100%%", &[]), "100%");
        assert_eq!(f("a\nb", &[]), "a\nb");
    }

    #[test]
    fn missing_arguments_render_empty() {
        assert_eq!(f("[%s][%s]", &[Value::str("x")]), "[x][]");
    }

    #[test]
    fn surplus_arguments_are_ignored() {
        // gawk restarts the format for the leftovers; BWK awk and mawk do not,
        // and neither does eawk.
        assert_eq!(f("%s-%s\n", &[Value::str("a"), Value::str("b"), Value::str("c")]), "a-b\n");
    }

    #[test]
    fn star_takes_width_from_arguments() {
        assert_eq!(f("%*d", &[Value::Num(5.0), Value::Num(42.0)]), "   42");
    }

    #[test]
    fn char_conversion() {
        assert_eq!(f("%c", &[Value::Num(65.0)]), "A");
        assert_eq!(f("%c", &[Value::str("xy")]), "x");
    }
}