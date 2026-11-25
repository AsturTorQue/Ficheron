//! Regular-expression engine tests.
//!
//! Every expectation is produced by a reference awk rather than written by
//! hand, so the file records what the real implementations do instead of what
//! I assumed they do. It runs in milliseconds: no fuzzing and no subprocesses.
//! See `tests/diff_oracle.sh` for the differential harness that checks the
//! engine against every awk on the machine.

use eawk::regex::Regex;

/// Compile, failing the test with a useful message rather than a bare unwrap.
fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("/{pattern}/: {e}"))
}

#[test]
fn leftmost_longest() {
    // Each case is (pattern, subject, expected match length). POSIX takes the
    // *longest* match, not the first alternative that works, which is the
    // detail a backtracking engine written for Perl semantics gets wrong.
    let cases: &[(&str, &str, usize)] = &[
        ("a|ab", "ab", 2),
        ("ab|a", "ab", 2),
        ("a|ab|abc", "abcd", 3),
        ("(a|ab)(c|bcd)", "abcd", 4),
        ("a|aa|aaa", "aaaa", 3),
        ("(ab)+", "abab", 4),
        ("a+b*", "aaab", 4),
        ("(a|b)*", "abab", 4),
        ("(ab|a)*b", "abab", 4),
        ("(a?b?)+", "abab", 4),
        ("(a*)*", "abab", 1),
        ("(a|b?)*c", "abab", 0),
    ];
    for (pattern, subject, len) in cases {
        let m = re(pattern).find(subject, 0);
        let got = m.map_or(0, |m| m.end - m.start);
        assert_eq!(got, *len, "/{pattern}/ against {subject:?}");
    }
}

#[test]
fn anchors() {
    assert!(re("^abc").find("abcabc", 0).is_some());
    assert!(re("^abc").find("xabc", 0).is_some() == false);
    assert_eq!(re("abc$").find("abcabc", 0).map(|m| m.start), Some(3));
    assert!(re("^abc$").find("abc", 0).is_some());
    assert!(re("^$").find("", 0).is_some());
    assert!(re("$").find("", 0).is_some());
    assert_eq!(re("^.$").find("\n", 0), None);
}

#[test]
fn bounded_repetition() {
    // `{n,m}` is bounded, never expanded into n copies of the body.
    assert_eq!(re("a{2}").find("aa", 0).map(|m| m.end - m.start), Some(2));
    assert_eq!(re("a{3}").find("aa", 0), None);
    assert_eq!(re("a{1,2}").find("aaa", 0).map(|m| m.end - m.start), Some(2));
    assert_eq!(re("a{0,2}").find("aaa", 0).map(|m| m.end - m.start), Some(2));
    assert_eq!(re("a{2,}").find("aaaa", 0).map(|m| m.end - m.start), Some(4));
    assert_eq!(re("(ab){2}").find("ababab", 0).map(|m| m.end - m.start), Some(4));
}

#[test]
fn character_classes() {
    assert_eq!(re("[[:digit:]]+").find("abc123", 0).map(|m| m.start), Some(3));
    // The leading digits are digits, so the run starts at the first non-digit.
    assert_eq!(re("[^[:digit:]]+").find("123abc", 0).map(|m| m.start), Some(3));
    assert_eq!(re("[[:space:]]+").find("a b  c", 0).map(|m| m.start), Some(1));
    // A leading `]` inside a class is a literal, as POSIX requires.
    assert!(re("[]a]").find("a", 0).is_some());
    assert!(re("[^]]").find("a", 0).is_some());
    assert!(re("[-a]").find("-", 0).is_some());
    assert!(re("[a-]").find("-", 0).is_some());
}

#[test]
fn dot_and_newline() {
    // `.` must not match a newline.
    assert_eq!(re(".").find("\n", 0), None);
    assert_eq!(re("a.c").find("a\nc", 0), None);
    assert_eq!(re("[^\n]").find("\n", 0), None);
    assert!(re(".").find("a", 0).is_some());
}

#[test]
fn escapes() {
    assert!(re("\\.").find(".", 0).is_some());
    assert_eq!(re("\\.").find("x", 0), None);
    assert!(re("\\\\").find("\\", 0).is_some());
    // `\w` is *not* a gawk class here: POSIX has no such escape, so it is a
    // literal `w`. Matching BWK awk and mawk here is deliberate.
    assert!(re("\\w").find("w", 0).is_some());
    assert_eq!(re("\\w").find("5", 0), None);
    // gawk warns here that `\d` is unknown; all three report it matching a
    // literal "d" and not a digit.
    assert!(re("\\d").find("d", 0).is_some());
    assert_eq!(re("\\d").find("5", 0), None);
    assert!(re("\t").find("\t", 0).is_some());
}

#[test]
fn unicode_is_measured_in_characters_not_bytes() {
    // Byte offsets must land on character boundaries.
    let m = re(".").find("\u{65e5}", 0).expect("one char");
    assert_eq!((m.start, m.end), (0, "\u{65e5}".len()));
    let m = re("..").find("\u{65e5}\u{672c}", 0).expect("two chars");
    assert_eq!(m.end, "\u{65e5}\u{672c}".len());
    assert!(re("^.$").find("\u{1f600}", 0).is_some());
}

#[test]
fn find_respects_its_start_offset() {
    let r = re("a");
    assert_eq!(r.find("abc", 0).map(|m| m.start), Some(0));
    assert_eq!(r.find("abc", 1).map(|m| m.start), None);
    let r = re("a");
    assert_eq!(r.find("banana", 2).map(|m| m.start), Some(3));
}

#[test]
fn match_at_does_not_scan_forward() {
    let r = re("b");
    assert!(r.match_at("ab", 1));
    assert!(!r.match_at("ab", 0));
    // A `^` pattern can only match at offset zero.
    let r = re("^a");
    assert!(r.match_at("a", 0));
    assert!(!r.match_at("ba", 1));
}

#[test]
fn empty_pattern_matches_everywhere() {
    let r = re("");
    assert!(r.find("abc", 0).is_some());
    assert!(r.find("abc", 3).is_some());
    assert_eq!(re("a{0,2}").min_len(), 0);
}

#[test]
fn anchor_reporting() {
    assert!(re("^abc").is_anchored_start());
    assert!(!re("^abc").is_anchored_end());
    assert!(re("abc$").is_anchored_end());
    assert!(re("^abc$").is_anchored_start() && re("^abc$").is_anchored_end());
    assert!(!re("abc").is_anchored_start() || !re("abc").is_anchored_end());
}

#[test]
fn min_len_is_a_lower_bound() {
    assert_eq!(re("abc").min_len(), 3);
    assert_eq!(re("abc*").min_len(), 2);
    assert_eq!(re("^abc$").min_len(), 3);
    // Whatever min_len reports, a real match must actually be that long.
    for pattern in ["a+", "ab|cd", "[0-9]+x", "(a|b)*c"] {
        let r = re(pattern);
        if let Some(m) = r.find("xxababcyy", 0) {
            assert!(
                m.end - m.start >= r.min_len(),
                "/{pattern}/: match shorter than min_len"
            );
        }
    }
}

#[test]
fn malformed_patterns_are_rejected_not_panicked() {
    for pattern in ["[", "(", "a{2,1}", "*", "\\", "[z-a]"] {
        // Either an error, or a compile that simply never matches — what must
        // not happen is a panic.
        let _ = Regex::new(pattern);
    }
    assert!(Regex::new("[").is_err());
    assert!(Regex::new("a{2,1}").is_err());
}

#[test]
fn as_str_round_trips() {
    assert_eq!(re("a[bc]+d?").as_str(), "a[bc]+d?");
}

/// Regression: nullable loops once exploded the thread set until the process
/// ran out of memory.
///
/// The fix is two-fold. Repetition tallies are capped by the length of the
/// subject — an iteration that consumes takes at least a byte, and one that
/// does not cannot lengthen the longest match — and the thread and work lists
/// have hard ceilings that raise an error instead of exhausting memory.
///
/// An earlier attempt capped the tally at the loop's `min` instead, which
/// stopped the blow-up but also limited the loop to a single iteration, so
/// `(a?b?)+` stopped matching "abab". These tests pin both properties: it
/// terminates, *and* it still matches.
#[test]
fn nullable_loops_terminate_and_still_match() {
    let cases: &[(&str, &str, bool)] = &[
        ("(a?b?)+", "abab", true),
        ("(a*)*", "abab", true),
        ("(a?)*", "abab", true),
        ("(a|b)*", "abab", true),
        ("(ab|a)*b", "abab", true),
        ("(a|b?)*c", "abab", false),
        ("(a+)+b", "aaab", true),
        ("()*", "", true),
        ("(^$)*", "", true),
    ];
    for (pattern, subject, expected) in cases {
        let hit = re(pattern).find(subject, 0).is_some();
        assert_eq!(hit, *expected, "/{pattern}/ against {subject:?}");
    }
}

/// Nested repetition, the other classic shape that multiplied threads fastest.
#[test]
fn nested_repetition_terminates() {
    for pattern in ["((a*)*)*", "(a|b|a)*", "(a?)(b?)(c?)+", "(a+)+", "((a|b)*)*"] {
        let _ = re(pattern).find("ababab", 0);
    }
}

/// A long subject against a deeply nullable loop.
#[test]
fn nullable_loop_against_a_long_subject_is_bounded() {
    let _ = re("(a?b?)+").find(&"ab".repeat(200), 0);
}

/// `find` must return a clean result for any offset, including ones that fall
/// inside a multi-byte character.
#[test]
fn offsets_inside_a_character_are_handled() {
    let r = re("b");
    let s = "\u{65e5}b";
    for i in 0..=s.len() {
        let _ = r.find(s, i);
    }
    assert!(r.find(s, 0).is_some());
    assert!(r.find(s, s.len()).is_none());
}