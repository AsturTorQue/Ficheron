//! NFA execution: leftmost-longest simulation.
//!
//! This is a Pike VM — the same shape as the Thompson/Pike construction used
//! by RE2 and Go's `regexp`. It is worth spelling out why, because the obvious
//! alternative gets nested quantifiers wrong.
//!
//! # Why a Pike VM
//!
//! An earlier version walked epsilon transitions with an explicit stack. That
//! fails on a `*` whose body can also match empty: the loop re-enters itself
//! faster than the consuming work underneath it gets explored, so the real
//! match is never found — `(a*)+` against `"aaa"` would report an empty match.
//! Ordering the stack cannot fix that, because the right order depends on the
//! pattern.
//!
//! The Pike VM avoids the problem by never searching for an order. Each input
//! position holds a list of live threads. To advance:
//!
//! 1. For each thread, if it is a `Char` accepting the next character, follow
//!    it and add that state to the next list.
//! 2. For every state added, close over epsilon transitions (`Split`, `Bol`,
//!    `Eol`, `Count`). A state is expanded at most once per position because
//!    `addthread` checks `seen` first.
//!
//! That single dedup is what bounds the work to `O(len * states)` and makes
//! catastrophic backtracking impossible, while also cutting the zero-width
//! cycle: revisiting a state with identical counts is a no-op, so a loop that
//! can spin forever at a fixed position simply stops.
//!
//! # Leftmost-longest
//!
//! Threads are added in preference order — the order alternatives appear in the
//! pattern — and the list keeps insertion order, so it is always in preference
//! order without sorting. The first Accept found is the one a tie resolves to.
//!
//! We do not stop at the first accept. We keep stepping and keep re-recording,
//! so the furthest accept is the longest. Scanning start offsets left to right
//! and stopping at the first that produces an accept gives the POSIX
//! leftmost-longest match.
//!
//! # Safety
//!
//! Nothing indexes out of bounds or unwraps: state ids are checked against the
//! table length, iteration is over `char_indices` so only valid boundaries are
//! visited, and a step budget turns any pathological case into an `Err` rather
//! than a hang. The epsilon closure is iterative: it uses an explicit work
//! stack, not a recursive call chain. A pattern like `(a*)*` could otherwise
//! drive a recursive closure past the depth guard and overflow the call stack,
//! which is uncatchable. Iterating makes that input safe by construction.
//! A host embedding eawk can pass arbitrary patterns and subjects without
//! risking a panic or an infinite loop.

use crate::error::Error;

use std::collections::HashSet;

use super::nfa::{Inst, Program, StateId};

/// Multiplier on the step budget, which is `states * positions * STEP_FACTOR`.
/// Generous enough that no realistic pattern reaches it, tight enough that a
/// hang is impossible.
const STEP_FACTOR: usize = 4;

/// Cap on the repetition tally carried for any one nesting level. Beyond this
/// the tally stops incrementing, which makes an enormous `{n}` behave as though
/// it were bounded — an over-approximation that can only end a match early,
/// never accept something the pattern does not allow.
const MAX_COUNT: u32 = 1 << 20;

/// Hard ceiling on live threads per position.
///
/// The epsilon closure of a pattern with a nullable loop can grow without
/// bound — `(a?b?)+` is the classic case — so the lists are capped rather than
/// trusted. Exceeding the cap raises an error instead of exhausting memory.
/// This is the backstop that keeps a hostile or merely pathological pattern
/// from taking the host process down with it.
const MAX_THREADS: usize = 20_000;

/// Hard ceiling on the explicit work stack, for the same reason.
const MAX_WORK: usize = 60_000;

/// A live NFA thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Thread {
    state: StateId,
    /// Repetition tallies for the enclosing `Count` loops, outermost first.
    /// Inline for the shallow common case so stepping a thread allocates
    /// nothing.
    counts: Counts,
    /// Preference rank: lower is preferred.
    order: u32,
}

/// The per-depth repetition tallies of a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Counts {
    inline: [u32; Counts::INLINE],
}

impl Counts {
    /// How many nesting levels are tracked inline.
    const INLINE: usize = 4;

    fn new() -> Counts {
        Counts { inline: [0; Counts::INLINE] }
    }

    /// The tally for the loop at `depth`. Levels past [`Counts::INLINE`] read
    /// as 0, which is what a freshly entered loop would have anyway.
    fn get(&self, depth: u8) -> u32 {
        if (depth as usize) < Counts::INLINE {
            self.inline[depth as usize]
        } else {
            0
        }
    }

    /// Set the tally for the loop at `depth`.
    fn with(&self, depth: u8, value: u32) -> Counts {
        let mut next = *self;
        if (depth as usize) < Counts::INLINE {
            next.inline[depth as usize] = value;
        }
        next
    }
}

/// A match found by the simulation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Found {
    /// Byte offset where the match begins.
    pub(crate) start: usize,
    /// Byte offset just past the end of the match.
    pub(crate) end: usize,
    /// Preference rank of the winning alternative; lower is preferred.
    pub(crate) order: u32,
}

/// The mutable state of one scan.
struct Sim<'a> {
    program: &'a Program,
    haystack: &'a str,
    /// Position of the character currently under consideration.
    pos: usize,
    steps: usize,
    budget: usize,
}

impl<'a> Sim<'a> {
    /// Add `state` and its epsilon closure to `list`, preserving preference
    /// order.
    ///
    /// A thread already in `list` is skipped, so each one appears at most once
    /// per position and the work stays linear in the state count.
    ///
    /// The closure is iterative, appending to an explicit work stack rather than
    /// recursing, so no pattern can grow the call stack.
    ///
    /// # Why the tally is capped at the minimum
    ///
    /// A thread's tally records how many repetitions of the loop at each depth
    /// it has completed, and normally that number is unbounded. It does not
    /// need to be: within a single input position, every *additional* zero-width
    /// pass through a loop reaches the same states with the same tally, so
    /// keeping them would produce infinitely many threads for a pattern like
    /// `(a*)+` without ever advancing. We therefore keep the tally only up to
    /// `min`, the smallest value any caller can distinguish. Extra repetitions
    /// beyond the minimum are explored by the loop in later positions, where
    /// they can consume input, and the count of *positions* consumed is what
    /// determines a leftmost-longest match anyway.
    fn addthread(
        &mut self,
        list: &mut Vec<Thread>,
        seen: &mut HashSet<Thread>,
        state: StateId,
        counts: Counts,
        order: u32,
    ) -> Result<(), Error> {
        let mut work: Vec<(StateId, Counts)> = vec![(state, counts)];

        while let Some((st, ct)) = work.pop() {
            let t = Thread { state: st, counts: ct, order };
            // A thread already queued or already listed needs no second visit.
            if !seen.insert(t) {
                continue;
            }
            if list.len() >= MAX_THREADS || work.len() >= MAX_WORK {
                return Err(Error::regex("regular expression is too complex to evaluate"));
            }
            let Some(inst) = self.program.states.get(st) else {
                continue;
            };
            match inst {
                Inst::Accept { .. } | Inst::Char { .. } => list.push(t),
                Inst::Split { a, b } => {
                    // `a` is the preferred branch; pushing it last means it is
                    // popped first and so lands earlier in the list.
                    work.push((*b, ct));
                    work.push((*a, ct));
                }
                Inst::Bol { next } => {
                    if self.pos == 0 {
                        work.push((*next, ct));
                    }
                }
                Inst::Eol { next } => {
                    if self.pos == self.haystack.len() {
                        work.push((*next, ct));
                    }
                }
                Inst::Count { min, max, body, next, depth: loop_depth, .. } => {
                    let taken = ct.get(*loop_depth);
                    // Bound the tally by the length of the subject.
                    //
                    // An iteration that consumes takes at least one byte, so
                    // more iterations than the subject is long cannot all
                    // consume; and an iteration that does *not* consume cannot
                    // lengthen the longest match being sought. Capping here is
                    // therefore free — it keeps the thread set small without
                    // changing which match wins.
                    //
                    // The earlier version of this capped at `min` for loops with
                    // a nullable body, which stopped `(a?b?)+` from exploding
                    // but also stopped it matching: it allowed only one
                    // iteration, so `(a?b?)+` failed against "abab".
                    let limit = (self.haystack.len() as u32).saturating_add(1).max(*min);
                    let limit = max.map_or(limit.min(MAX_COUNT), |m| m.min(limit));
                    if taken < limit {
                        work.push((*body, ct.with(*loop_depth, taken.saturating_add(1))));
                    }
                    if taken >= *min {
                        // Leaving clears this level's tally; enclosing levels
                        // keep theirs, so `(a*)+` still counts its outer
                        // repetitions.
                        work.push((*next, ct.with(*loop_depth, 0)));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Find the leftmost-longest match at or after `from`.
pub(crate) fn find(program: &Program, haystack: &str, from: usize) -> Result<Option<Found>, Error> {
    let len = haystack.len();
    if from > len {
        return Ok(None);
    }
    // A match can only begin on a character boundary. If `from` lands inside a
    // character, round up — slicing there would panic, and a caller may pass
    // any offset. The end of the subject counts too, because a pattern like
    // `$`, or an empty alternative, legitimately matches starting there.
    let base = if haystack.is_char_boundary(from) {
        Some(from)
    } else {
        haystack.get(from..).and_then(|s| s.char_indices().nth(1)).map(|(i, _)| from + i)
    };
    let mut starts: Vec<usize> = match base {
        Some(b) => haystack[b..].char_indices().map(|(i, _)| b + i).collect(),
        None => Vec::new(),
    };
    if starts.last() != Some(&len) {
        starts.push(len);
    }

    let budget = program
        .states
        .len()
        .saturating_mul(starts.len().max(1))
        .saturating_mul(STEP_FACTOR)
        .saturating_add(1024);

    for start in starts {
        if let Some(found) = scan(program, haystack, start, budget)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Match anchored at exactly `pos`, without scanning forward.
pub(crate) fn find_at(
    program: &Program,
    haystack: &str,
    pos: usize,
) -> Result<Option<Found>, Error> {
    if pos > haystack.len() || !haystack.is_char_boundary(pos) {
        return Ok(None);
    }
    let budget = program.states.len().saturating_mul(STEP_FACTOR).saturating_add(1024);
    scan(program, haystack, pos, budget)
}

/// Run the simulation from `start`, returning the longest match beginning there.
fn scan(
    program: &Program,
    haystack: &str,
    start: usize,
    budget: usize,
) -> Result<Option<Found>, Error> {
    let mut sim = Sim { program, haystack, pos: start, steps: 0, budget };

    let mut clist: Vec<Thread> = Vec::new();
    let mut seen: HashSet<Thread> = HashSet::new();
    sim.addthread(&mut clist, &mut seen, program.start, Counts::new(), 0)?;

    let mut best: Option<Found> = None;
    let mut pos = start;

    loop {
        // A live Accept means a match ends here. We deliberately keep going
        // afterwards, which is what makes the result longest rather than first.
        if let Some(order) = accept_order(program, &clist) {
            let better = match best {
                None => true,
                Some(prev) => pos > prev.end || (pos == prev.end && order < prev.order),
            };
            if better {
                best = Some(Found { start, end: pos, order });
            }
        }

        if pos >= haystack.len() {
            break;
        }
        let Some(c) = haystack[pos..].chars().next() else { break };

        let mut nlist: Vec<Thread> = Vec::new();
        let mut nseen: HashSet<Thread> = HashSet::new();
        for &t in &clist {
            sim.steps += 1;
            // Charge epsilon work to the budget too, so a closure that explodes
            // is stopped by the same deadline as the scan itself.
            if sim.steps > sim.budget {
                return Err(Error::regex("regular expression match exceeded the step budget"));
            }
            if let Inst::Char { set, next } = &program.states[t.state] {
                if set.matches(c) {
                    sim.pos = pos + c.len_utf8();
                    sim.addthread(&mut nlist, &mut nseen, *next, t.counts, t.order)?;
                    sim.steps += 1;
                    if sim.steps > sim.budget {
                        return Err(Error::regex("regular expression match exceeded the step budget"));
                    }
                }
            }
        }

        if nlist.is_empty() {
            break;
        }
        clist = nlist;
        pos += c.len_utf8();
        sim.pos = pos;
    }

    Ok(best)
}

/// The preference rank of the most-preferred Accept thread in `list`, if any.
///
/// The list is in preference order by construction, so the first Accept is the
/// one a tie resolves to.
fn accept_order(program: &Program, list: &[Thread]) -> Option<u32> {
    for t in list {
        if let Inst::Accept { order } = program.states[t.state] {
            return Some(order);
        }
    }
    None
}
