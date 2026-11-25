//! Compiles the parsed AST into a Thompson NFA.
//!
//! The program is a flat `Vec<State>` of instructions with integer jumps, so
//! the executor can carry thread sets as `Vec<Thread>` without pointer chasing.
//!
//! # How repetition is encoded
//!
//! Every quantified atom compiles to a `Count` instruction: a loop whose
//! running repetition count lives in the *thread*, not in the program. A
//! `Count` instruction declares the admissible count range for the atom that
//! follows it:
//!
//! ```text
//! Count { min, max, body, next }
//! ```
//!
//! The executor re-enters `body` only while `count` stays in range, so
//! `a{1000}` is *two* states regardless of the bound — the bound is never
//! expanded, which is what keeps `a{1000}{1000}` from compiling to a million
//! instructions. `{n,}` uses `max = None` (unbounded).

use crate::error::Error;

use super::parse::{Atom, Branch, CharClass, RepeatKind};

/// Index into a [`Program`]'s state table.
pub(crate) type StateId = usize;

/// Maximum number of NFA states we will build. A pathological pattern must
/// produce an error, not an enormous allocation.
pub(crate) const MAX_STATES: usize = 20_000;

/// A single NFA instruction.
#[derive(Debug, Clone)]
pub(crate) enum Inst {
    /// Consume one character accepted by `set`, then go to `next`.
    Char { set: CharSet, next: StateId },
    /// Assert start-of-string, then go to `next`.
    Bol { next: StateId },
    /// Assert end-of-string, then go to `next`.
    Eol { next: StateId },
    /// Try `a`, else `b`. Both are live simultaneously under NFA simulation;
    /// the order encodes the preference used to break ties.
    Split { a: StateId, b: StateId },
    /// A repetition loop over `body`.
    ///
    /// On entry the executor has not yet counted a repetition of `body`. The
    /// instruction is a split: `body` is taken while the count is in
    /// `[min, max)`, and `next` is taken once it is exhausted. `body` loops
    /// back to this instruction.
    Count {
        /// Repetitions required before `next` may be taken.
        min: u32,
        /// True when the body can also match empty.
        ///
        /// Such a loop can be re-entered without consuming, so the executor
        /// Repetitions permitted, or `None` for unbounded.
        max: Option<u32>,
        /// The repeated fragment.
        body: StateId,
        /// Where to go once the count range is exhausted.
        next: StateId,
        /// Nesting level, so the executor can keep one repetition tally per
        /// enclosing loop. Nested quantifiers like `(a*)+` have two live loops
        /// at once and must not share a counter.
        depth: u8,
    },
    /// Accept. `order` is the preference rank of the alternative that reaches
    /// this state; lower wins ties of equal length.
    Accept { order: u32 },
}

/// What a `Char` instruction is willing to consume.
#[derive(Debug, Clone)]
pub(crate) enum CharSet {
    /// Any character except `\n`.
    Any,
    /// One of a set of literal characters.
    Set(Vec<char>),
    /// A bracketed class.
    Class(CharClass),
}

impl CharSet {
    /// True if this instruction consumes `c`.
    pub(crate) fn matches(&self, c: char) -> bool {
        match self {
            CharSet::Any => c != '\n',
            CharSet::Set(v) => v.contains(&c),
            CharSet::Class(cl) => cl.contains(c),
        }
    }
}

/// A compiled regular expression program.
#[derive(Debug, Clone)]
pub(crate) struct Program {
    /// The instruction table.
    pub(crate) states: Vec<Inst>,
    /// State to begin execution at.
    pub(crate) start: StateId,
    /// Minimum bytes any match can consume. 0 if it can match empty.
    pub(crate) min_len: usize,
    /// True if the pattern begins with `^`.
    pub(crate) anchored_start: bool,
    /// True if the pattern ends with `$`.
    pub(crate) anchored_end: bool,
}

impl Program {
    /// Compile parsed branches into a program.
    pub(crate) fn compile(branches: &[Branch]) -> Result<Program, Error> {
        // Preference rank: the index of the alternative in the top-level list.
        // Ties of equal length resolve to the lower rank.
        let mut b = Builder {
            states: Vec::new(),
            anchored_start: begins_with_bol(branches),
            anchored_end: ends_with_eol(branches),
            depth: 0,
        };

        let entry = b.push(Inst::Split { a: 0, b: 0 });

        // Compile each top-level alternative with its own accept, then chain
        // them into the entry split. Each alternative's accept records its
        // preference rank.
        let mut heads = Vec::with_capacity(branches.len());
        for (order, branch) in branches.iter().enumerate() {
            let accept = b.push(Inst::Accept { order: order as u32 });
            let head = b.compile_branch(branch, order as u32, accept)?;
            heads.push(head);
        }

        if heads.is_empty() {
            // The empty pattern: everything matched at length 0.
            let accept = b.push(Inst::Accept { order: 0 });
            if let Inst::Split { a, b: bb } = &mut b.states[entry] {
                *a = accept;
                *bb = accept;
            }
        } else {
            let mut tail = *heads.last().unwrap_or(&0);
            // Chain alternatives right-to-left so earlier ones are preferred.
            for head in heads[..heads.len() - 1].iter().rev() {
                let s = b.push(Inst::Split { a: *head, b: tail });
                tail = s;
            }
            if let Inst::Split { a, b: bb } = &mut b.states[entry] {
                *a = tail;
                *bb = tail;
            }
        }

        let mut program = Program {
            states: b.states,
            start: entry,
            min_len: 0,
            anchored_start: b.anchored_start,
            anchored_end: b.anchored_end,
        };
        program.min_len = min_len_of_branches(branches);
        Ok(program)
    }
}

/// Mutable NFA construction state.
struct Builder {
    states: Vec<Inst>,
    anchored_start: bool,
    anchored_end: bool,
    /// Current quantifier nesting depth, used to tag each `Count` so the
    /// executor can keep a separate tally per enclosing loop.
    depth: u8,
}

impl Builder {
    /// Append an instruction and return its id.
    fn push(&mut self, inst: Inst) -> StateId {
        self.states.push(inst);
        self.states.len() - 1
    }

    /// Append a placeholder instruction and return its id, so the caller can
    /// compile something that refers back to it and fill it in afterwards with
    /// [`Builder::set_body`]. This is how a repetition loop refers to itself
    /// without a separate back-patching pass.
    fn reserve(&mut self, inst: Inst) -> StateId {
        self.push(inst)
    }

    /// Fill in a reserved instruction's `body` field (a [`Inst::Count`]).
    fn set_body(&mut self, state: StateId, body: StateId) {
        if let Inst::Count { body: slot, .. } = &mut self.states[state] {
            *slot = body;
        }
    }

    /// Fail if we have grown past [`MAX_STATES`].
    fn check_size(&self) -> Result<(), Error> {
        if self.states.len() > MAX_STATES {
            return Err(Error::regex("regular expression is too large to compile"));
        }
        Ok(())
    }

    /// Compile a concatenation, returning the entry state. `accept` is where a
    /// successful branch continues.
    fn compile_branch(
        &mut self,
        branch: &Branch,
        order: u32,
        accept: StateId,
    ) -> Result<StateId, Error> {
        if branch.is_empty() {
            return Ok(accept);
        }
        // Link back-to-front so each piece knows its successor.
        let mut next = accept;
        for atom in branch.iter().rev() {
            next = self.compile_atom(atom, order, next)?;
            self.check_size()?;
        }
        Ok(next)
    }

    /// Compile one atom with its successor.
    fn compile_atom(
        &mut self,
        atom: &Atom,
        order: u32,
        next: StateId,
    ) -> Result<StateId, Error> {
        match atom {
            Atom::Any => Ok(self.push(Inst::Char { set: CharSet::Any, next })),
            Atom::Literal(c) => Ok(self.push(Inst::Char { set: CharSet::Set(vec![*c]), next })),
            Atom::Class(cl) => {
                Ok(self.push(Inst::Char { set: CharSet::Class(cl.clone()), next }))
            }
            Atom::Bol => Ok(self.push(Inst::Bol { next })),
            Atom::Eol => Ok(self.push(Inst::Eol { next })),
            Atom::Group(alternatives) => {
                if alternatives.len() == 1 {
                    return self.compile_branch(&alternatives[0], order, next);
                }
                let mut heads = Vec::with_capacity(alternatives.len());
                for branch in alternatives {
                    heads.push(self.compile_branch(branch, order, next)?);
                }
                if heads.is_empty() {
                    return Ok(next);
                }
                let mut tail = *heads.last().unwrap_or(&0);
                for head in heads[..heads.len() - 1].iter().rev() {
                    let s = self.push(Inst::Split { a: *head, b: tail });
                    tail = s;
                    self.check_size()?;
                }
                Ok(tail)
            }
            Atom::Repeat(inner, kind) => self.compile_repeat(inner, *kind, order, next),
        }
    }

    /// Compile `inner` under a repetition bound.
    ///
    /// The mandatory prefix of a bound (`{3,5}`) is compiled structurally as
    /// literal copies, which is bounded by [`MAX_REPEAT_COPIES`]; the
    /// *unbounded tail* is a `Count` loop, so the number of states depends on
    /// the bound only up to that cap. That keeps `{1000}` affordable while
    /// guaranteeing we never silently mis-execute a huge bound.
    fn compile_repeat(
        &mut self,
        inner: &Atom,
        kind: RepeatKind,
        order: u32,
        next: StateId,
    ) -> Result<StateId, Error> {
        let (min, max) = match kind {
            RepeatKind::Star => (0u32, None),
            RepeatKind::Plus => (1u32, None),
            RepeatKind::Opt => (0u32, Some(1u32)),
            RepeatKind::Bound(b) => {
                if let Some(m) = b.max {
                    if m < b.min {
                        return Err(Error::regex(format!(
                            "repetition bound {{{},{}}} has max less than min",
                            b.min, m
                        )));
                    }
                }
                if b.min as u64 > MAX_REPEAT_COPIES as u64
                    || b.max.map_or(false, |m| m as u64 > MAX_REPEAT_COPIES as u64)
                {
                    return Err(Error::regex(format!(
                        "repetition bound exceeds the limit of {MAX_REPEAT_COPIES}"
                    )));
                }
                (b.min as u32, b.max.map(|m| m as u32))
            }
        };

        if min == 0 && max == Some(0) {
            return Ok(next);
        }

        // A loop whose body can *never consume* is pure ceremony: it cannot
        // lengthen a match, and at run time it would re-enter itself without
        // consuming, producing infinitely many distinct thread states at one
        // position. Collapse it to a single pass. This is the `(a*)+` / `()*`
        // family. `(a?b?)+` is deliberately spared: its body can consume, so
        if max.is_none() && !inner.can_consume() {
            if min == 0 {
                return Ok(next);
            }
            return self.compile_atom(inner, order, next);
        }

        match max {
            None => {
                // Unbounded: `{n,}`, `*`, `+`. Compile `n - 1` mandatory copies
                // structurally (none for `*`), then a Count loop that requires
                // at least one more repetition and may run forever. That gives
                // exactly `n` mandatory repetitions with an open tail, and the
                // state count does not grow with `n`.
                // The mandatory prefix must lead *into* the loop, not into
                // `next`: we build the Count first, then prepend the copies so
                // the last one flows into the loop head.
                // Reserve the loop instruction *before* compiling its body, so
                // the body can be compiled with the loop head already known as
                // its continuation. That avoids a back-patching pass entirely,
                // and it is what keeps nested loops like `(a*)+` correct: each
                // loop wires its own body to its own head.
                let count = self.reserve(Inst::Count {
                    min: u32::from(min != 0),
                    max: None,
                    body: 0,
                    next,
                    depth: self.depth,
                });
                self.depth = self.depth.saturating_add(1);
                let body = self.compile_atom(inner, order, count);
                self.depth = self.depth.saturating_sub(1);
                self.set_body(count, body?);

                let prefix = min.saturating_sub(1);
                let mut entry = count;
                for _ in 0..prefix {
                    entry = self.compile_atom(inner, order, entry)?;
                    self.check_size()?;
                }
                Ok(entry)
            }
            Some(max) => {
                // Bounded: `{n}`, `{n,m}`, and `?`.
                //
                // The NFA for `x{n,m}` is `n` mandatory copies followed by
                // `m - n` optional ones. Each optional copy is a split whose
                // taken branch runs the atom and then *re-offers the same
                // choice* is wrong — instead the taken branch must continue to
                // the next optional copy, and only the last one may exit.
                //
                // So we build it back-to-front: start at the tail (`next`),
                // and for each optional copy wrap that tail in a new
                // skip-or-take split. Then prepend the mandatory copies.
                let optional = max.saturating_sub(min);
                let mandatory = max.saturating_sub(optional);

                // Optional region, built tail-first: the innermost (last)
                // optional copy is created first and may exit to `next`.
                let mut cur = next;
                for _ in 0..optional {
                    // This copy's body continues to the *rest* of the
                    // optional region (`cur`), so taking successive copies
                    // walks forward through the chain.
                    let body = self.compile_atom(inner, order, cur)?;
                    cur = self.push(Inst::Split { a: body, b: next });
                    self.check_size()?;
                }
                let optional_entry = cur;

                // Mandatory copies, built tail-first onto the optional region.
                let mut entry = optional_entry;
                for _ in 0..mandatory {
                    entry = self.compile_atom(inner, order, entry)?;
                    self.check_size()?;
                }
                Ok(entry)
            }
        }
    }
}






/// Upper bound on a `{n,m}` bound. Bounds larger than this are rejected rather
/// than expanded, so `a{100000}` fails fast instead of allocating 100k states.
const MAX_REPEAT_COPIES: u32 = 1000;



/// True if every top-level branch begins with `^`.
fn begins_with_bol(branches: &[Branch]) -> bool {
    !branches.is_empty() && branches.iter().all(|b| matches!(b.first(), Some(Atom::Bol)))
}

/// True if every top-level branch ends with `$`.
fn ends_with_eol(branches: &[Branch]) -> bool {
    !branches.is_empty() && branches.iter().all(|b| matches!(b.last(), Some(Atom::Eol)))
}

/// Compute the minimum byte length any match can consume: the fewest bytes
/// that *any* match of the pattern can cover. 0 means the pattern can match the
/// empty string.
///
/// This walks the AST rather than the instruction table, because the answer is
/// structural: concatenation adds, alternation takes the minimum, and a
/// quantifier multiplies by the fewest repetitions it permits (`{0}` and `*`
/// make the whole pattern optional, so the answer collapses to 0).
///
/// The result is a lower bound on a real match and may under-report, which is
/// the contract [`super::Regex::min_len`] promises. It never over-reports.
fn min_len_of_branches(branches: &[Branch]) -> usize {
    branches
        .iter()
        .map(branch_min_len)
        .min()
        .unwrap_or(0)
}

/// The fewest bytes a single concatenation can cover.
fn branch_min_len(branch: &Branch) -> usize {
    branch.iter().map(atom_min_len).sum()
}

/// The fewest bytes one atom (with its quantifier) can cover.
fn atom_min_len(atom: &Atom) -> usize {
    match atom {
        // Anchors consume nothing.
        Atom::Bol | Atom::Eol => 0,
        Atom::Any => 1,
        Atom::Literal(c) => c.len_utf8(),
        Atom::Class(cl) => class_min_bytes(cl),
        Atom::Group(alternatives) => min_len_of_branches(alternatives),
        Atom::Repeat(inner, kind) => {
            let (min, _) = match kind {
                RepeatKind::Star => (0, 0),
                RepeatKind::Plus => (1, 0),
                RepeatKind::Opt => (0, 1),
                RepeatKind::Bound(b) => (b.min, b.max.unwrap_or(b.min)),
            };
            atom_min_len(inner).saturating_mul(min)
        }
    }
}

/// The smallest number of bytes a character in `cl` can occupy.
fn class_min_bytes(cl: &CharClass) -> usize {
    if cl.named.is_some() {
        // Every POSIX named class is defined over ASCII, so a one-byte match is
        // always reachable.
        return 1;
    }
    if cl.negated {
        // A negated class can always match a single ASCII byte.
        return 1;
    }
    let singles = cl.singles.iter().map(|c| c.len_utf8()).min();
    let ranges = cl.ranges.iter().map(|&(lo, _)| lo.len_utf8()).min();
    match (singles, ranges) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => 0,
    }
}

