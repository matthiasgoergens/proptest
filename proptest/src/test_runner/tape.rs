//-
// Copyright 2026 The proptest developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The choice tape: a Conjecture-style typed record of every decision made
//! while generating a test case, used by the experimental tape shrink engine
//! (`Config::shrink_engine = ShrinkEngine::Tape`).
//!
//! See `design/choice-tape-shrinking.md` in the repository root for the full
//! design. In short: generation records each random draw as a `Choice`;
//! shrinking edits the recorded tape and re-runs generation in `Replaying`
//! mode, accepting an edit iff the test still fails and the re-recorded
//! output tape is shortlex-smaller than the incumbent.

use crate::std_facade::{BTreeMap, Vec};
use core::cmp::Ordering;

#[cfg(not(feature = "std"))]
use num_traits::float::FloatCore;

/// One recorded decision.
///
/// `Integer` values (and their constraints) are stored in u128 "offset
/// space": an order-preserving embedding of the original integer type
/// (unsigned: identity; signed: offset binary, i.e. `x ^ (1 << 127)` of the
/// sign-extended value). This gives all integer widths a single canonical
/// unsigned ordering.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Choice {
    Integer {
        value: u128,
        min: u128,
        max: u128,
        /// The value shrinking moves toward; `encode(0)` clamped into
        /// `[min, max]` for numeric strategies.
        shrink_to: u128,
    },
    Float {
        value: f64,
        min: f64,
        max: f64,
        allow_nan: bool,
    },
    #[allow(dead_code)] // constructed starting with the collection encoding
    Bool {
        value: bool,
    },
    /// Untyped entropy recorded by the `RngCore` compat wrapper.
    RawU32 {
        value: u32,
    },
    RawU64 {
        value: u64,
    },
    RawBytes {
        value: Vec<u8>,
    },
}

impl Choice {
    /// Complexity of a scalar choice. Lower is "simpler"; the shrink
    /// target keys to 0. `None` for RawBytes, which order after all
    /// scalars by (len, bytes) in `cmp_complexity`.
    fn scalar_key(&self) -> Option<u128> {
        match self {
            Choice::Integer {
                value, shrink_to, ..
            } => Some(zigzag(*value, *shrink_to)),
            Choice::Float { value, .. } => Some(float_key(*value)),
            Choice::Bool { value } => Some(*value as u128),
            Choice::RawU32 { value } => Some(*value as u128),
            Choice::RawU64 { value } => Some(*value as u128),
            Choice::RawBytes { .. } => None,
        }
    }

    /// Allocation-free complexity comparison between two choices,
    /// consistent with the historical ordering (all scalar keys order
    /// before byte blobs; byte blobs order by length then contents).
    pub(crate) fn cmp_complexity(&self, other: &Choice) -> Ordering {
        match (self.scalar_key(), other.scalar_key()) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => match (self, other) {
                (
                    Choice::RawBytes { value: a },
                    Choice::RawBytes { value: b },
                ) => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
                _ => unreachable!(),
            },
        }
    }

    /// The same choice with its value replaced by its shrink target.
    pub(crate) fn trivial(&self) -> Choice {
        match self {
            Choice::Integer {
                min,
                max,
                shrink_to,
                ..
            } => Choice::Integer {
                value: *shrink_to,
                min: *min,
                max: *max,
                shrink_to: *shrink_to,
            },
            Choice::Float {
                min,
                max,
                allow_nan,
                ..
            } => Choice::Float {
                value: float_shrink_target(*min, *max),
                min: *min,
                max: *max,
                allow_nan: *allow_nan,
            },
            Choice::Bool { .. } => Choice::Bool { value: false },
            Choice::RawU32 { .. } => Choice::RawU32 { value: 0 },
            Choice::RawU64 { .. } => Choice::RawU64 { value: 0 },
            Choice::RawBytes { value } => Choice::RawBytes {
                value: vec![0; value.len()],
            },
        }
    }
}

/// Distance of `v` from the shrink target `t`, zigzag-encoded so that the
/// target keys to 0 and, at equal distance, the positive direction is
/// preferred (`t` < `t+1` < `t-1` < `t+2` < ...).
///
/// Saturates at the extremes of u128, which slightly coarsens the ordering
/// for astronomically distant values; that only weakens tie-breaking, never
/// soundness.
pub(crate) fn zigzag(v: u128, t: u128) -> u128 {
    if v >= t {
        (v - t).saturating_mul(2).saturating_sub(1)
    } else {
        (t - v).saturating_mul(2)
    }
}

/// Complexity key for floats: NaN worst, then by sign (non-negative
/// simpler), then by the lexicographic encoding of the magnitude.
pub(crate) fn float_key(v: f64) -> u128 {
    if v.is_nan() {
        return u128::MAX;
    }
    let sign = if v.is_sign_negative() { 1u128 << 64 } else { 0 };
    sign | float_to_lex(v.abs()) as u128
}

/// The shrink target of a float draw constrained to `[min, max]`: the
/// in-range value closest to +0.0.
pub(crate) fn float_shrink_target(min: f64, max: f64) -> f64 {
    min.max(0.0).min(max)
}

/// Port of Hypothesis's lexicographic float encoding
/// (`hypothesis.internal.conjecture.floats.float_to_lex`).
///
/// Maps non-negative floats to u64 such that lexicographically smaller
/// integers correspond to "simpler" floats:
///
/// - Integer-valued floats below 2^56 encode as themselves (tag bit 0), so
///   0 < 1 < 2 < ... in encoded space.
/// - Everything else gets the tag bit set, an exponent reordered so that
///   integer-like exponents come first, and the fractional mantissa bits
///   reversed so that shorter decimal fractions are smaller.
pub(crate) fn float_to_lex(f: f64) -> u64 {
    debug_assert!(
        !(f < 0.0),
        "float_to_lex requires a non-negative input, got {}",
        f
    );
    if f.is_finite() && f == f.trunc() && f < (1u64 << 56) as f64 {
        return f as u64;
    }
    base_float_to_lex(f)
}

const MANTISSA_MASK: u64 = (1u64 << 52) - 1;
const MAX_EXPONENT: u64 = 0x7FF;
const BIAS: i64 = 1023;

/// Rank of an 11-bit exponent under Hypothesis's ordering: non-negative
/// unbiased exponents first in increasing order, then negative unbiased
/// exponents in decreasing order, then the inf/NaN exponent last.
///
/// This is a closed form of Hypothesis's sorted `ENCODING_TABLE`.
fn exponent_rank(e: u64) -> u64 {
    debug_assert!(e <= MAX_EXPONENT);
    if e == MAX_EXPONENT {
        MAX_EXPONENT
    } else if e >= BIAS as u64 {
        e - BIAS as u64
    } else {
        // unbiased = e - 1023 in -1023..=-1; rank 1024..=2046 with the
        // least negative exponent first.
        2046 - e
    }
}

fn update_mantissa(unbiased_exponent: i64, mut mantissa: u64) -> u64 {
    if unbiased_exponent <= 0 {
        // Subnormals and values in [0, 2): all 52 bits are fractional;
        // reverse them all so that "fewer fraction bits" sorts lower.
        mantissa = mantissa.reverse_bits() >> (64 - 52);
    } else if unbiased_exponent <= 51 {
        let n_fractional = (52 - unbiased_exponent) as u32;
        let fractional = mantissa & ((1u64 << n_fractional) - 1);
        mantissa -= fractional;
        mantissa |= fractional.reverse_bits() >> (64 - n_fractional);
    }
    mantissa
}

fn base_float_to_lex(f: f64) -> u64 {
    let bits = f.to_bits() & !(1u64 << 63);
    let exponent = bits >> 52;
    let mantissa =
        update_mantissa(exponent as i64 - BIAS, bits & MANTISSA_MASK);
    (1u64 << 63) | (exponent_rank(exponent) << 52) | mantissa
}

/// A contiguous run of choices forming one logical unit of generation
/// (e.g. one collection element together with its continuation flag).
/// Spans are metadata for the deletion pass: replay ignores them and
/// re-records them from the actual generation structure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Span {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// One element of a stream key (see `StreamKey`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum KeyElt {
    /// The n-th sub-stream allocated under the parent this run
    /// (a generated function's identity within its test case).
    Split(u32),
    /// A per-call salt (the hash of a generated function's argument).
    Salt(u64),
}

/// Identity of a sub-stream (design: stream-keyed tapes, ported from
/// tapecheck). The main generation stream is the empty key; a generated
/// function's stream is `[Split(n)]`, and its per-argument streams are
/// `[Split(n), Salt(hash)]`. Keys are deterministic across record and
/// replay because splits happen at generation-driven points and salts
/// are stable argument hashes.
pub(crate) type StreamKey = Vec<KeyElt>;

/// A recorded generation run: the main stream's choices and spans, plus
/// keyed sub-streams (sorted by key) for split-off draws, i.e. what
/// generated functions returned. Sub-streams have no spans: their
/// deletable unit is the whole stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Tape {
    pub(crate) choices: Vec<Choice>,
    pub(crate) spans: Vec<Span>,
    pub(crate) streams: Vec<(StreamKey, Vec<Choice>)>,
}

fn cmp_choices_shortlex(a: &[Choice], b: &[Choice]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| {
        for (a, b) in a.iter().zip(b) {
            match a.cmp_complexity(b) {
                Ordering::Equal => continue,
                unequal => return unequal,
            }
        }
        Ordering::Equal
    })
}

impl Tape {
    /// Total number of choices across the main stream and all
    /// sub-streams.
    pub(crate) fn total_len(&self) -> usize {
        self.choices.len()
            + self
                .streams
                .iter()
                .map(|(_, choices)| choices.len())
                .sum::<usize>()
    }

    /// Shortlex comparison: fewer choices first (totalled across all
    /// streams, so deleting a whole sub-stream is an improvement), then
    /// the main stream elementwise by complexity key, then fewer
    /// sub-streams, then the sorted sub-streams pairwise. `Less` means
    /// `self` is simpler than `other`. A total order, so shrink
    /// acceptance stays a strict descent.
    pub(crate) fn cmp_key(&self, other: &Tape) -> Ordering {
        self.total_len()
            .cmp(&other.total_len())
            .then_with(|| cmp_choices_shortlex(&self.choices, &other.choices))
            .then_with(|| self.streams.len().cmp(&other.streams.len()))
            .then_with(|| {
                for ((ka, ca), (kb, cb)) in
                    self.streams.iter().zip(&other.streams)
                {
                    match ka
                        .cmp(kb)
                        .then_with(|| cmp_choices_shortlex(ca, cb))
                    {
                        Ordering::Equal => continue,
                        unequal => return unequal,
                    }
                }
                Ordering::Equal
            })
    }

    /// The tape with every choice, in every stream, at its shrink
    /// target.
    pub(crate) fn trivial(&self) -> Tape {
        Tape {
            choices: self.choices.iter().map(Choice::trivial).collect(),
            spans: self.spans.clone(),
            streams: self
                .streams
                .iter()
                .map(|(k, choices)| {
                    (k.clone(), choices.iter().map(Choice::trivial).collect())
                })
                .collect(),
        }
    }

    /// A copy of the tape with the main-stream choice at `idx` replaced.
    /// Like `with_span_deleted`, the copy's span list is dropped: replay
    /// ignores input spans, and an accepted proposal re-records fresh
    /// ones on its output tape.
    pub(crate) fn with_choice(&self, idx: usize, choice: Choice) -> Tape {
        let mut choices = self.choices.clone();
        choices[idx] = choice;
        Tape {
            choices,
            spans: Vec::new(),
            streams: self.streams.clone(),
        }
    }

    /// A copy of the tape with the choices of `span` removed from the
    /// main stream. The copy's span list is dropped (it would be stale);
    /// an accepted proposal gets fresh spans from the replay's output
    /// tape anyway.
    pub(crate) fn with_span_deleted(&self, span: Span) -> Tape {
        let mut choices =
            Vec::with_capacity(self.choices.len() - (span.end - span.start));
        choices.extend_from_slice(&self.choices[..span.start]);
        choices.extend_from_slice(&self.choices[span.end..]);
        Tape {
            choices,
            spans: Vec::new(),
            streams: self.streams.clone(),
        }
    }

    /// A copy of the tape with the choice at `idx` of sub-stream
    /// `stream_idx` replaced.
    pub(crate) fn with_stream_choice(
        &self,
        stream_idx: usize,
        idx: usize,
        choice: Choice,
    ) -> Tape {
        let mut streams = self.streams.clone();
        streams[stream_idx].1[idx] = choice;
        Tape {
            choices: self.choices.clone(),
            spans: Vec::new(),
            streams,
        }
    }

    /// A copy of the tape with sub-stream `stream_idx` removed entirely:
    /// its draws resample fresh on replay (an absent stream is not an
    /// overrun), which pushes generated functions toward constant
    /// observed behaviour.
    pub(crate) fn with_stream_deleted(&self, stream_idx: usize) -> Tape {
        let mut streams = self.streams.clone();
        streams.remove(stream_idx);
        Tape {
            choices: self.choices.clone(),
            spans: Vec::new(),
            streams,
        }
    }
}

/// Recording/replaying mode.
#[derive(Clone, Copy, Debug, PartialEq)]
enum TapeMode {
    Off,
    Recording,
    Replaying,
}

/// Per-stream runtime state. The write side has rewrite-over semantics:
/// `wpos` rewinds to 0 at a call boundary (`enter_stream`), so a second
/// same-argument call re-records over the same entries instead of
/// appending duplicates; a divergent value truncates and overwrites.
/// The read side is a per-stream replay cursor; `known` distinguishes a
/// stream present in the replay input (exhausting it is an overrun)
/// from a brand-new stream (all draws fresh, silently: new salts appear
/// whenever an edit changes an argument's hash, and whole-stream
/// deletion relies on absent streams sampling fresh).
#[derive(Clone, Debug, Default)]
struct StreamRt {
    written: Vec<Choice>,
    wpos: usize,
    input: Vec<Choice>,
    rpos: usize,
    known: bool,
    /// Set once this stream has been entered (or donated) during a
    /// replay, so orphan adoption never reuses a donor.
    claimed: bool,
}

impl StreamRt {
    fn record(&mut self, choice: Choice) {
        if self.wpos < self.written.len() && self.written[self.wpos] == choice
        {
            self.wpos += 1;
        } else {
            self.written.truncate(self.wpos);
            self.written.push(choice);
            self.wpos += 1;
        }
    }
}

/// Recording/replaying state, owned by `TestRng` so that both the typed
/// draws on `TestRunner` and the raw `RngCore` calls write to the same
/// tape. Streams: all draws route to the current stream (the main
/// stream by default); generated functions enter their per-argument
/// stream around each call.
#[derive(Clone, Debug)]
pub(crate) struct TapeState {
    mode: TapeMode,
    streams: BTreeMap<StreamKey, StreamRt>,
    current: StreamKey,
    overrun: bool,
    /// While positive, raw RngCore draws bypass the tape entirely. Typed
    /// draws set this while running their sample closures so the closure's
    /// raw entropy is subsumed by the single typed choice.
    suppress_raw: u32,
    /// Start indices of currently-open spans (main-stream choice count at
    /// `start_span` time; `usize::MAX` when opened off the main stream,
    /// where spans do not apply).
    span_stack: Vec<usize>,
    /// Spans recorded on the main stream this run.
    spans: Vec<Span>,
    /// Sub-stream ordinal counters, per parent key, reset each run.
    split_counters: BTreeMap<StreamKey, u32>,
}

impl Default for TapeState {
    fn default() -> Self {
        TapeState {
            mode: TapeMode::Off,
            streams: BTreeMap::new(),
            current: StreamKey::new(),
            overrun: false,
            suppress_raw: 0,
            span_stack: Vec::new(),
            spans: Vec::new(),
            split_counters: BTreeMap::new(),
        }
    }
}

impl TapeState {
    pub(crate) fn is_on(&self) -> bool {
        !matches!(self.mode, TapeMode::Off)
    }

    pub(crate) fn is_replaying(&self) -> bool {
        matches!(self.mode, TapeMode::Replaying)
    }

    /// Whether raw RngCore draws should currently be recorded/replayed.
    pub(crate) fn raw_active(&self) -> bool {
        self.is_on() && 0 == self.suppress_raw
    }

    pub(crate) fn suppress_raw(&mut self) {
        self.suppress_raw += 1;
    }

    pub(crate) fn unsuppress_raw(&mut self) {
        debug_assert!(self.suppress_raw > 0);
        self.suppress_raw -= 1;
    }

    fn reset(&mut self) {
        self.streams.clear();
        self.current = StreamKey::new();
        self.overrun = false;
        self.span_stack.clear();
        self.spans.clear();
        self.split_counters.clear();
    }

    pub(crate) fn start_recording(&mut self) {
        self.reset();
        self.mode = TapeMode::Recording;
    }

    pub(crate) fn start_replay(&mut self, input: Tape) {
        self.reset();
        self.mode = TapeMode::Replaying;
        let root = StreamRt {
            input: input.choices,
            known: true,
            ..StreamRt::default()
        };
        self.streams.insert(StreamKey::new(), root);
        for (key, choices) in input.streams {
            self.streams.insert(
                key,
                StreamRt {
                    input: choices,
                    known: true,
                    ..StreamRt::default()
                },
            );
        }
    }

    /// Collect the run's output tape: the main stream's writes and spans
    /// plus every sub-stream that was actually written (streams the run
    /// never touched are dropped, which garbage-collects orphans).
    fn collect(&mut self) -> Tape {
        let mut main = Vec::new();
        let mut streams = Vec::new();
        for (key, rt) in core::mem::take(&mut self.streams) {
            if key.is_empty() {
                main = rt.written;
            } else if !rt.written.is_empty() {
                streams.push((key, rt.written));
            }
        }
        // BTreeMap iteration is already key-sorted.
        let spans = core::mem::take(&mut self.spans);
        Tape {
            choices: main,
            spans,
            streams,
        }
    }

    /// Stop recording and return the recorded tape. Returns an empty tape
    /// if not recording.
    pub(crate) fn take_recording(&mut self) -> Tape {
        let was_recording = matches!(self.mode, TapeMode::Recording);
        self.mode = TapeMode::Off;
        let tape = self.collect();
        self.reset();
        if was_recording {
            tape
        } else {
            Tape::default()
        }
    }

    /// Stop replaying and return the re-recorded output tape plus whether
    /// the input was overrun. Returns an empty tape if not replaying.
    pub(crate) fn finish_replay(&mut self) -> (Tape, bool) {
        let was_replaying = matches!(self.mode, TapeMode::Replaying);
        let overrun = self.overrun;
        self.mode = TapeMode::Off;
        let tape = self.collect();
        self.reset();
        if was_replaying {
            (tape, overrun)
        } else {
            (Tape::default(), false)
        }
    }

    /// Whether the replay input has already been overrun. Lets the
    /// engine reject a proposal before running the test.
    pub(crate) fn overrun_now(&self) -> bool {
        self.overrun
    }

    fn current_rt(&mut self) -> &mut StreamRt {
        // Entry API needs an owned key; avoid the clone on the hot path
        // (the current stream almost always exists already).
        if !self.streams.contains_key(&self.current) {
            self.streams
                .insert(self.current.clone(), StreamRt::default());
        }
        self.streams.get_mut(&self.current).expect("just inserted")
    }

    /// Switch all draws to `key`'s stream, rewinding that stream's read
    /// and write cursors (a call boundary: same-argument calls replay
    /// identically). Returns the previous stream for `exit_stream`.
    ///
    /// Orphan adoption: when a replay enters an UNKNOWN salted stream
    /// (a generated function called with an argument the input tape
    /// never saw, typically because a shrink edit changed the argument
    /// and with it the salt), the stream adopts the input of an
    /// unclaimed sibling (same parent, salt leaf, key order). That
    /// sibling is exactly the orphan whose argument just changed, so
    /// the function keeps its observed behaviour across the edit
    /// instead of flipping a fresh coin; the accepted output re-records
    /// under the new salt, realigning the tape for the next round.
    pub(crate) fn enter_stream(&mut self, key: StreamKey) -> StreamKey {
        if !self.streams.contains_key(&key) {
            self.streams.insert(key.clone(), StreamRt::default());
        }
        let known = self.streams[&key].known;
        if self.is_replaying()
            && !known
            && matches!(key.last(), Some(KeyElt::Salt(_)))
        {
            let parent = &key[..key.len() - 1];
            let donor_key = self
                .streams
                .iter()
                .find(|(k, rt)| {
                    rt.known
                        && !rt.claimed
                        && k.len() == key.len()
                        && k.starts_with(parent)
                        && matches!(k.last(), Some(KeyElt::Salt(_)))
                })
                .map(|(k, _)| k.clone());
            if let Some(donor_key) = donor_key {
                let donated = {
                    let donor =
                        self.streams.get_mut(&donor_key).expect("found");
                    donor.claimed = true;
                    donor.input.clone()
                };
                let rt = self.streams.get_mut(&key).expect("inserted");
                rt.input = donated;
                rt.known = true;
            }
        }
        let rt = self.streams.get_mut(&key).expect("just inserted");
        rt.rpos = 0;
        rt.wpos = 0;
        rt.claimed = true;
        core::mem::replace(&mut self.current, key)
    }

    /// Restore the stream returned by `enter_stream`.
    pub(crate) fn exit_stream(&mut self, prev: StreamKey) {
        self.current = prev;
    }

    /// Allocate the next sub-stream key under the current stream. The
    /// per-parent ordinal makes keys deterministic across record and
    /// replay: splits happen at generation-driven points.
    pub(crate) fn alloc_split(&mut self) -> StreamKey {
        let n = self
            .split_counters
            .entry(self.current.clone())
            .or_insert(0);
        let ordinal = *n;
        *n += 1;
        let mut key = self.current.clone();
        key.push(KeyElt::Split(ordinal));
        key
    }

    /// Open a span at the current position of the main stream. No-op
    /// when the tape is off; spans opened while a sub-stream is current
    /// are ignored (a sub-stream's deletable unit is the whole stream).
    pub(crate) fn start_span(&mut self) {
        if !self.is_on() {
            return;
        }
        let pos = if self.current.is_empty() {
            self.current_rt().written.len()
        } else {
            usize::MAX
        };
        self.span_stack.push(pos);
    }

    /// Close the innermost open span. Robust against unbalanced calls
    /// (e.g. when generation errors out mid-span): closing with no open
    /// span is a no-op.
    pub(crate) fn end_span(&mut self) {
        let start = match self.span_stack.pop() {
            Some(start) => start,
            None => return,
        };
        if start == usize::MAX || !self.is_on() || !self.current.is_empty() {
            return;
        }
        let end = self.current_rt().written.len();
        if end > start {
            self.spans.push(Span { start, end });
        }
    }

    /// Append the choice actually used to the current stream (the
    /// recording, or the output re-recording during replay). No-op when
    /// off.
    pub(crate) fn record(&mut self, choice: Choice) {
        if !self.is_on() {
            return;
        }
        self.current_rt().record(choice);
    }

    /// Record a choice whose value is forced by generation structure
    /// rather than drawn (e.g. the "stop" continuation flag of a
    /// maximum-length collection). During replay a matching next input
    /// choice is consumed so edits stay aligned, but its value is
    /// ignored, and running off the end of the input is NOT an overrun —
    /// no information is being read.
    pub(crate) fn record_forced_bool(&mut self, value: bool) {
        if !self.is_on() {
            return;
        }
        if self.is_replaying() {
            let rt = self.current_rt();
            if rt.known
                && rt.rpos < rt.input.len()
                && matches!(rt.input[rt.rpos], Choice::Bool { .. })
            {
                rt.rpos += 1;
            }
        }
        self.record(Choice::Bool { value });
    }

    /// Record a boolean that generation forces to `forced` (drawing no
    /// entropy), but that shrinking may edit: during replay the next
    /// input Bool's value is honored if present. Used for units that are
    /// structurally mandatory during generation yet deletable during
    /// shrinking, e.g. a state-machine sequence's transitions below the
    /// declared minimum length, which the classic shrinker deliberately
    /// deletes past.
    ///
    /// Contrast `record_forced_bool`, which ignores the replayed value
    /// (for markers whose value can never matter, like the stop flag of
    /// a maximum-length collection).
    pub(crate) fn draw_bool_forced(&mut self, forced: bool) -> bool {
        if !self.is_on() {
            return forced;
        }
        if let Some(Choice::Bool { value }) =
            self.pop_replay(|c| matches!(c, Choice::Bool { .. }))
        {
            self.record(Choice::Bool { value });
            return value;
        }
        // Not replaying, kind mismatch, or overrun: use the forced value.
        self.record(Choice::Bool { value: forced });
        forced
    }

    /// During replay, consume and return the current stream's next input
    /// choice if `matcher` accepts it. Returns `None` (and samples must
    /// go fresh) on kind mismatch, on overrun of a known stream (also
    /// setting the overrun flag), on an unknown stream (fresh by
    /// design), or when not replaying.
    pub(crate) fn pop_replay(
        &mut self,
        matcher: impl FnOnce(&Choice) -> bool,
    ) -> Option<Choice> {
        if !self.is_replaying() {
            return None;
        }
        let rt = self.current_rt();
        if !rt.known {
            return None;
        }
        if rt.rpos >= rt.input.len() {
            self.overrun = true;
            return None;
        }
        if matcher(&rt.input[rt.rpos]) {
            let choice = rt.input[rt.rpos].clone();
            rt.rpos += 1;
            return Some(choice);
        }
        None
    }
}

/// Serialize a tape for failure persistence (the payload of the "ct1"
/// persisted-failure format). Spans are shrinking metadata and are not
/// persisted; replay ignores them.
///
/// A tape without sub-streams serializes as the original flat record
/// list (choice tags 0..=5), so pre-stream files keep loading and
/// stream-free tapes keep writing the historical bytes. A tape with
/// sub-streams gets a leading `6` tag (no flat record starts with 6),
/// then a counted main section and counted keyed stream sections.
pub(crate) fn serialize_tape(tape: &Tape) -> Vec<u8> {
    let mut out = Vec::new();
    if !tape.streams.is_empty() {
        out.push(6);
        out.extend_from_slice(&(tape.choices.len() as u32).to_le_bytes());
        for choice in &tape.choices {
            serialize_choice(choice, &mut out);
        }
        out.extend_from_slice(&(tape.streams.len() as u32).to_le_bytes());
        for (key, choices) in &tape.streams {
            out.extend_from_slice(&(key.len() as u32).to_le_bytes());
            for elt in key {
                match elt {
                    KeyElt::Split(n) => {
                        out.push(0);
                        out.extend_from_slice(&n.to_le_bytes());
                    }
                    KeyElt::Salt(s) => {
                        out.push(1);
                        out.extend_from_slice(&s.to_le_bytes());
                    }
                }
            }
            out.extend_from_slice(&(choices.len() as u32).to_le_bytes());
            for choice in choices {
                serialize_choice(choice, &mut out);
            }
        }
        return out;
    }
    for choice in &tape.choices {
        serialize_choice(choice, &mut out);
    }
    out
}

fn serialize_choice(choice: &Choice, out: &mut Vec<u8>) {
    match choice {
            Choice::Integer {
                value,
                min,
                max,
                shrink_to,
            } => {
                out.push(0);
                out.extend_from_slice(&value.to_le_bytes());
                out.extend_from_slice(&min.to_le_bytes());
                out.extend_from_slice(&max.to_le_bytes());
                out.extend_from_slice(&shrink_to.to_le_bytes());
            }
            Choice::Float {
                value,
                min,
                max,
                allow_nan,
            } => {
                out.push(1);
                out.extend_from_slice(&value.to_le_bytes());
                out.extend_from_slice(&min.to_le_bytes());
                out.extend_from_slice(&max.to_le_bytes());
                out.push(*allow_nan as u8);
            }
            Choice::Bool { value } => {
                out.push(2);
                out.push(*value as u8);
            }
            Choice::RawU32 { value } => {
                out.push(3);
                out.extend_from_slice(&value.to_le_bytes());
            }
            Choice::RawU64 { value } => {
                out.push(4);
                out.extend_from_slice(&value.to_le_bytes());
            }
            Choice::RawBytes { value } => {
                out.push(5);
                out.extend_from_slice(&(value.len() as u32).to_le_bytes());
                out.extend_from_slice(value);
            }
    }
}

fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    if bytes.len() < n {
        return None;
    }
    let (head, tail) = bytes.split_at(n);
    *bytes = tail;
    Some(head)
}

fn take_u32(bytes: &mut &[u8]) -> Option<u32> {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(take(bytes, 4)?);
    Some(u32::from_le_bytes(buf))
}

fn take_u64(bytes: &mut &[u8]) -> Option<u64> {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(take(bytes, 8)?);
    Some(u64::from_le_bytes(buf))
}

/// Inverse of `serialize_tape`. Strict: any malformed input yields `None`
/// (the persistence layer then ignores the entry).
pub(crate) fn deserialize_tape(bytes: &[u8]) -> Option<Tape> {
    let mut bytes = bytes;
    if bytes.first() == Some(&6) {
        // v2: counted sections with keyed sub-streams.
        take(&mut bytes, 1)?;
        let n_main = take_u32(&mut bytes)? as usize;
        let mut choices = Vec::new();
        for _ in 0..n_main {
            choices.push(deserialize_choice(&mut bytes)?);
        }
        let n_streams = take_u32(&mut bytes)? as usize;
        let mut streams = Vec::new();
        for _ in 0..n_streams {
            let n_key = take_u32(&mut bytes)? as usize;
            let mut key = StreamKey::new();
            for _ in 0..n_key {
                let tag = take(&mut bytes, 1)?[0];
                key.push(match tag {
                    0 => KeyElt::Split(take_u32(&mut bytes)?),
                    1 => KeyElt::Salt(take_u64(&mut bytes)?),
                    _ => return None,
                });
            }
            let n_choices = take_u32(&mut bytes)? as usize;
            let mut stream = Vec::new();
            for _ in 0..n_choices {
                stream.push(deserialize_choice(&mut bytes)?);
            }
            streams.push((key, stream));
        }
        if !bytes.is_empty() {
            return None;
        }
        return Some(Tape {
            choices,
            spans: Vec::new(),
            streams,
        });
    }
    let mut choices = Vec::new();
    while !bytes.is_empty() {
        choices.push(deserialize_choice(&mut bytes)?);
    }
    Some(Tape {
        choices,
        spans: Vec::new(),
        streams: Vec::new(),
    })
}

fn deserialize_choice(bytes: &mut &[u8]) -> Option<Choice> {
    fn take_u128(bytes: &mut &[u8]) -> Option<u128> {
        let mut buf = [0u8; 16];
        buf.copy_from_slice(take(bytes, 16)?);
        Some(u128::from_le_bytes(buf))
    }
    fn take_f64(bytes: &mut &[u8]) -> Option<f64> {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(take(bytes, 8)?);
        Some(f64::from_le_bytes(buf))
    }

    let tag = take(bytes, 1)?[0];
    Some(match tag {
        0 => Choice::Integer {
            value: take_u128(bytes)?,
            min: take_u128(bytes)?,
            max: take_u128(bytes)?,
            shrink_to: take_u128(bytes)?,
        },
        1 => Choice::Float {
            value: take_f64(bytes)?,
            min: take_f64(bytes)?,
            max: take_f64(bytes)?,
            allow_nan: 0 != take(bytes, 1)?[0],
        },
        2 => Choice::Bool {
            value: 0 != take(bytes, 1)?[0],
        },
        3 => Choice::RawU32 {
            value: take_u32(bytes)?,
        },
        4 => Choice::RawU64 {
            value: take_u64(bytes)?,
        },
        5 => {
            let len = take_u32(bytes)? as usize;
            Choice::RawBytes {
                value: take(bytes, len)?.to_vec(),
            }
        }
        _ => return None,
    })
}

/// Order-preserving embedding of primitive integers into u128 offset space.
pub(crate) trait TapeInt: Copy {
    fn encode(self) -> u128;
    fn decode(encoded: u128) -> Self;
    /// `encode(0)`, the natural shrink target before range clamping.
    fn encode_zero() -> u128;
}

macro_rules! tape_int_unsigned {
    ($($typ:ty),*) => {$(
        impl TapeInt for $typ {
            fn encode(self) -> u128 {
                self as u128
            }
            fn decode(encoded: u128) -> Self {
                encoded as $typ
            }
            fn encode_zero() -> u128 {
                0
            }
        }
    )*};
}

macro_rules! tape_int_signed {
    ($($typ:ty),*) => {$(
        impl TapeInt for $typ {
            fn encode(self) -> u128 {
                (self as i128 as u128) ^ (1u128 << 127)
            }
            fn decode(encoded: u128) -> Self {
                (encoded ^ (1u128 << 127)) as i128 as $typ
            }
            fn encode_zero() -> u128 {
                1u128 << 127
            }
        }
    )*};
}

tape_int_unsigned!(u8, u16, u32, u64, u128, usize);
tape_int_signed!(i8, i16, i32, i64, i128, isize);

/// `trunc` usable from both std and no_std builds.
pub(crate) fn float_trunc(v: f64) -> f64 {
    v.trunc()
}

/// Round `v` to `k` binary digits of fraction, for the precision-dropping
/// shrink pass.
pub(crate) fn float_round_to_precision(v: f64, k: i32) -> f64 {
    let scale = 2.0f64.powi(k);
    let scaled = v * scale;
    if !scaled.is_finite() {
        return v;
    }
    scaled.round() / scale
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::std_facade::Box;

    fn tape_of(choices: Vec<Choice>) -> Tape {
        Tape {
            choices,
            spans: Vec::new(),
            streams: Vec::new(),
        }
    }

    #[test]
    fn serialize_v2_roundtrip_and_v1_compat() {
        // Stream-free tapes keep the historical flat format.
        let flat = tape_of(vec![
            Choice::Integer {
                value: 5,
                min: 0,
                max: 10,
                shrink_to: 0,
            },
            Choice::Bool { value: true },
        ]);
        let bytes = serialize_tape(&flat);
        assert_eq!(0, bytes[0], "v1 bytes start with the first record tag");
        assert_eq!(Some(flat.clone()), deserialize_tape(&bytes));

        // Stream-carrying tapes round-trip through the v2 format.
        let mut with_streams = flat.clone();
        with_streams.streams = vec![
            (
                vec![KeyElt::Split(0), KeyElt::Salt(0xdead_beef_dead_beef)],
                vec![Choice::RawU32 { value: 7 }],
            ),
            (
                vec![KeyElt::Split(1)],
                vec![Choice::Bool { value: false }],
            ),
        ];
        let bytes = serialize_tape(&with_streams);
        assert_eq!(6, bytes[0], "v2 bytes start with the version tag");
        assert_eq!(Some(with_streams), deserialize_tape(&bytes));
    }

    #[test]
    fn stream_deletion_orders_smaller() {
        let mut a = tape_of(vec![Choice::Bool { value: false }]);
        a.streams =
            vec![(vec![KeyElt::Split(0)], vec![Choice::Bool { value: true }])];
        let b = a.with_stream_deleted(0);
        assert_eq!(Ordering::Less, b.cmp_key(&a));
    }

    #[test]
    fn zigzag_prefers_target_then_positive_side() {
        let t = 100u128;
        assert_eq!(0, zigzag(100, t));
        assert_eq!(1, zigzag(101, t));
        assert_eq!(2, zigzag(99, t));
        assert_eq!(3, zigzag(102, t));
        assert_eq!(4, zigzag(98, t));
    }

    #[test]
    fn tape_int_roundtrip_and_order() {
        fn check<T: TapeInt + PartialEq + core::fmt::Debug>(values: &[T]) {
            for &v in values {
                assert_eq!(v, T::decode(T::encode(v)));
            }
            for pair in values.windows(2) {
                assert!(pair[0].encode() < pair[1].encode());
            }
        }
        check(&[0u8, 1, 200, u8::MAX]);
        check(&[i8::MIN, -1, 0, 1, i8::MAX]);
        check(&[i64::MIN, -1, 0, 1, i64::MAX]);
        check(&[u128::MIN, 1, u128::MAX]);
        check(&[i128::MIN, -1, 0, i128::MAX]);
        assert_eq!(i64::encode(0), i64::encode_zero());
        assert_eq!(u64::encode(0), u64::encode_zero());
    }

    #[test]
    fn float_lex_simple_integers_encode_as_themselves() {
        // (Values must be exactly representable as f64, so no 2^56 - 1:
        // that rounds up to 2^56, which is no longer "simple".)
        for i in [0u64, 1, 2, 3, 10, 1000, 1 << 53, 1 << 55] {
            assert_eq!(i, float_to_lex(i as f64));
        }
    }

    #[test]
    fn float_lex_integers_are_simpler_than_fractions() {
        // Any integer below 2^56 keys below any non-integer.
        assert!(float_to_lex(1000000.0) < float_to_lex(1.5));
        assert!(float_to_lex(3.0) < float_to_lex(2.5));
        // Ties on the integer part: fewer fraction bits is simpler.
        assert!(float_to_lex(2.5) < float_to_lex(2.25));
        assert!(float_to_lex(2.25) < float_to_lex(2.125));
        // Infinity is worse than all finite values.
        assert!(float_to_lex(f64::MAX) < float_to_lex(f64::INFINITY));
        assert!(float_to_lex(1e300) < float_to_lex(f64::INFINITY));
    }

    #[test]
    fn float_key_sign_and_nan_ordering() {
        assert!(float_key(1.0) < float_key(-1.0));
        assert!(float_key(-1.0) < float_key(f64::NAN));
        assert!(float_key(f64::INFINITY) < float_key(f64::NAN));
        assert_eq!(0, float_key(0.0));
    }

    #[test]
    fn float_shrink_target_clamps_toward_zero() {
        assert_eq!(0.0, float_shrink_target(-10.0, 10.0));
        assert_eq!(1.5, float_shrink_target(1.5, 10.0));
        assert_eq!(-1.5, float_shrink_target(-10.0, -1.5));
        assert_eq!(0.0, float_shrink_target(f64::NEG_INFINITY, f64::INFINITY));
    }

    #[test]
    fn shortlex_shorter_tape_wins() {
        let long = tape_of(vec![
            Choice::RawU32 { value: 0 },
            Choice::RawU32 { value: 0 },
        ]);
        let short = tape_of(vec![Choice::RawU32 { value: u32::MAX }]);
        assert_eq!(Ordering::Less, short.cmp_key(&long));
    }

    #[test]
    fn shortlex_compares_keys_elementwise() {
        let a = tape_of(vec![
            Choice::RawU32 { value: 1 },
            Choice::RawU32 { value: 100 },
        ]);
        let b = tape_of(vec![
            Choice::RawU32 { value: 2 },
            Choice::RawU32 { value: 0 },
        ]);
        assert_eq!(Ordering::Less, a.cmp_key(&b));
    }

    #[test]
    fn trivial_tape_is_minimal() {
        let tape = tape_of(vec![
            Choice::Integer {
                value: i32::encode(57),
                min: i32::encode(-100),
                max: i32::encode(100),
                shrink_to: i32::encode_zero(),
            },
            Choice::Float {
                value: 3.7,
                min: 1.5,
                max: 10.0,
                allow_nan: false,
            },
            Choice::RawBytes {
                value: vec![1, 2, 3],
            },
        ]);
        let trivial = tape.trivial();
        assert_eq!(Ordering::Less, trivial.cmp_key(&tape));
        assert_eq!(Ordering::Equal, trivial.cmp_key(&trivial.trivial()));
        match &trivial.choices[1] {
            Choice::Float { value, .. } => assert_eq!(1.5, *value),
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn replay_engine_shrinks_float_range_to_round_value() {
        // The headline feature: a float threshold failure shrinks to the
        // smallest *round* failing value, not to an ugly boundary
        // approximation like 1.7000000000000002.
        let mut runner = engine_runner();
        let result = runner.run(&(0.0f64..10.0), |v| {
            if v >= 1.7 {
                Err(crate::test_runner::TestCaseError::fail("too big"))
            } else {
                Ok(())
            }
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(2.0, value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_shrinks_filtered_integers_to_boundary() {
        // The ValueTree shrinker can stall on filters because simplify()
        // has no way to skip over rejected values; the tape engine
        // re-vets every proposal through the filter.
        use crate::strategy::Strategy;
        let mut runner = engine_runner();
        let strategy = (0i32..1000).prop_filter("even", |v| 0 == v % 2);
        let result = runner.run(&strategy, |v| {
            if v >= 100 {
                Err(crate::test_runner::TestCaseError::fail("too big"))
            } else {
                Ok(())
            }
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(100, value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_shrinks_each_tuple_component() {
        // Per-choice minimization drives the pair onto the failure
        // boundary. (Redistribution *along* the boundary toward (0, 100)
        // is a phase-4 cross-value pass.)
        let mut runner = engine_runner();
        let result = runner.run(&(0i32..1000, 0i32..1000), |(a, b)| {
            if a + b >= 100 {
                Err(crate::test_runner::TestCaseError::fail("too big"))
            } else {
                Ok(())
            }
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, (a, b))) => {
                assert_eq!(100, a + b, "final pair: ({}, {})", a, b);
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_shrinks_through_flat_map() {
        // flat_map replays naturally: shrinking the outer choice reuses
        // the recorded inner choice, clamped into the new constraints.
        use crate::strategy::Strategy;
        let mut runner = engine_runner();
        let strategy = (1i32..=5).prop_flat_map(|n| 0i32..(n * 100));
        let result = runner.run(&strategy, |v| {
            if v >= 57 {
                Err(crate::test_runner::TestCaseError::fail("too big"))
            } else {
                Ok(())
            }
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(57, value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_shrinks_length_prefixed_vec_to_single_element() {
        // Through a bind, the collection length is an explicit earlier
        // choice, so getting from [.., 0, 100] to [100] needs the
        // lower-and-delete pass: neither deleting a zero span (the
        // length still demands the old count) nor lowering the length
        // (the tail element falls off) works alone.
        use crate::strategy::Strategy;
        let mut runner = engine_runner();
        let strategy = (1usize..=64)
            .prop_flat_map(|len| crate::collection::vec(0i32..1000, len..=len));
        let result = runner.run(&strategy, |v| {
            if v.iter().sum::<i32>() >= 100 {
                Err(crate::test_runner::TestCaseError::fail("sum too big"))
            } else {
                Ok(())
            }
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(vec![100], value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_shrinks_unmigrated_strategy_via_raw_choices() {
        // bool::ANY draws straight from the RNG; the compat wrapper
        // records that as a raw choice and the trivial pass zeroes it.
        let mut runner = engine_runner();
        let result = runner.run(&crate::bool::ANY, |_| {
            Err(crate::test_runner::TestCaseError::fail("always"))
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(false, value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_shrinks_any_float_to_round_value() {
        // f64::ANY (class-based generation) records its value as a typed
        // Float choice, so it gets round-value shrinking too.
        let mut runner = engine_runner();
        let result = runner.run(&crate::num::f64::ANY, |v| {
            if v.is_finite() && v >= 1.7 {
                Err(crate::test_runner::TestCaseError::fail("too big"))
            } else {
                Ok(())
            }
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(2.0, value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_keeps_class_restricted_floats_in_class() {
        // Shrink proposals (0.0, truncation, ...) fall outside the
        // allowed class set; the conform hook maps them back in, so the
        // minimal example is the smallest positive subnormal, not 0.0.
        let mut runner = engine_runner();
        let strategy = crate::num::f64::POSITIVE | crate::num::f64::SUBNORMAL;
        let result = runner.run(&strategy, |_| {
            Err(crate::test_runner::TestCaseError::fail("always"))
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert!(
                    value.is_sign_positive()
                        && value.classify() == core::num::FpCategory::Subnormal,
                    "value left the strategy's class set: {:?}",
                    value
                );
                assert_eq!(f64::from_bits(1), value);
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_shrinks_char_range_to_boundary() {
        let mut runner = engine_runner();
        let result = runner.run(&crate::char::range('a', 'z'), |c| {
            if c >= 'd' {
                Err(crate::test_runner::TestCaseError::fail("too big"))
            } else {
                Ok(())
            }
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!('d', value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_shrinks_any_char_to_convenient_bottom() {
        // char shrink targets are the hard-wired convenient characters
        // ('a', 'A', '0', ' ', '¡', or NUL), not necessarily NUL.
        let mut runner = engine_runner();
        let result = runner.run(&crate::char::any(), |_| {
            Err(crate::test_runner::TestCaseError::fail("always"))
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert!(
                    ['\0', ' ', '0', 'A', 'a', '¡'].contains(&value),
                    "expected a convenient bottom, got {:?}",
                    value
                );
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_deletes_vec_elements() {
        // Element deletion is a generic span-deletion pass under the tape
        // engine; remaining elements minimize to their targets.
        let mut runner = engine_runner();
        let result =
            runner.run(&crate::collection::vec(0i32..100, 0..10), |v| {
                if v.len() >= 3 {
                    Err(crate::test_runner::TestCaseError::fail("too long"))
                } else {
                    Ok(())
                }
            });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(vec![0, 0, 0], value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_deletes_long_vec_via_batched_deletion() {
        // Starts around 50 elements on average; batched span deletion
        // takes out long runs in O(log n) attempts.
        let mut runner = engine_runner();
        let result =
            runner.run(&crate::collection::vec(0i32..100, 0..100), |v| {
                if v.len() >= 5 {
                    Err(crate::test_runner::TestCaseError::fail("too long"))
                } else {
                    Ok(())
                }
            });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(vec![0, 0, 0, 0, 0], value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_deletes_elements_from_max_length_vecs() {
        // Regression test: maximum-length vecs draw no natural "stop"
        // flag, and without the forced stop marker every deletion edit
        // overran the tape and was rejected. Sweep seeds so some initial
        // failing cases are at maximum length.
        for seed_byte in 0..20u8 {
            let mut runner = crate::test_runner::TestRunner::new_with_rng(
                engine_config(),
                crate::test_runner::TestRng::from_seed(
                    crate::test_runner::RngAlgorithm::ChaCha,
                    &[seed_byte; 32],
                ),
            );
            let result =
                runner.run(&crate::collection::vec(0i32..100, 0..6), |v| {
                    if v.len() >= 2 {
                        Err(crate::test_runner::TestCaseError::fail("too long"))
                    } else {
                        Ok(())
                    }
                });
            match result {
                Err(crate::test_runner::TestError::Fail(_, value)) => {
                    assert_eq!(vec![0, 0], value, "seed byte {}", seed_byte)
                }
                other => panic!(
                    "unexpected result for seed byte {}: {:?}",
                    seed_byte, other
                ),
            }
        }
    }

    #[test]
    fn replay_engine_minimizes_vec_elements() {
        let mut runner = engine_runner();
        let result = runner.run(&crate::collection::vec(0i32..100, 3), |v| {
            if v.iter().any(|&e| e >= 7) {
                Err(crate::test_runner::TestCaseError::fail("big elem"))
            } else {
                Ok(())
            }
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, mut value)) => {
                value.sort();
                assert_eq!(vec![0, 0, 7], value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_shrinks_union_to_first_branch() {
        use crate::strategy::Strategy;
        let mut runner = engine_runner();
        let strategy =
            crate::prop_oneof![crate::strategy::Just(3i32), 10i32..20,];
        let result = runner.run(&strategy.boxed(), |_| {
            Err(crate::test_runner::TestCaseError::fail("always"))
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(3, value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_redistributes_vec_sum_to_single_element() {
        // Needs the cross-value redistribute pass: [27, 23] -> [0, 50],
        // then deletion of the zero -> [50].
        let mut runner = engine_runner();
        let result =
            runner.run(&crate::collection::vec(0i32..100, 0..20), |v| {
                if v.iter().sum::<i32>() >= 50 {
                    Err(crate::test_runner::TestCaseError::fail("big sum"))
                } else {
                    Ok(())
                }
            });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(vec![50], value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_redistributes_pair_within_constraints() {
        // Transfer clamps at the second component's upper bound.
        let mut runner = engine_runner();
        let result = runner.run(&(0i32..60, 0i32..60), |(a, b)| {
            if a + b >= 100 {
                Err(crate::test_runner::TestCaseError::fail("big sum"))
            } else {
                Ok(())
            }
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, (a, b))) => {
                assert_eq!((41, 59), (a, b))
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_lowers_duplicates_together() {
        // No single-choice edit preserves a == b; the duplicates pass
        // lowers both together. Equal pairs are rare (1/1000 per case),
        // so give the runner enough cases to find one.
        let mut config = engine_config();
        config.cases = 20_000;
        let mut runner = crate::test_runner::TestRunner::new_with_rng(
            config,
            crate::test_runner::TestRng::deterministic_rng(
                crate::test_runner::RngAlgorithm::default(),
            ),
        );
        let result = runner.run(&(0i32..1000, 0i32..1000), |(a, b)| {
            if a == b && a >= 10 {
                Err(crate::test_runner::TestCaseError::fail("equal"))
            } else {
                Ok(())
            }
        });
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!((10, 10), value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_respects_zero_shrink_budget() {
        let mut config = engine_config();
        config.max_shrink_iters = 0;
        let mut runner = crate::test_runner::TestRunner::new_with_rng(
            config,
            crate::test_runner::TestRng::deterministic_rng(
                crate::test_runner::RngAlgorithm::default(),
            ),
        );
        let result = runner.run(&(0.0f64..10.0), |v| {
            if v >= 1.7 {
                Err(crate::test_runner::TestCaseError::fail("too big"))
            } else {
                Ok(())
            }
        });
        // No shrinking happened, so we only know the value fails.
        match result {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert!(value >= 1.7)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    fn engine_config() -> crate::test_runner::Config {
        crate::test_runner::Config {
            shrink_engine: crate::test_runner::ShrinkEngine::Tape,
            failure_persistence: None,
            ..crate::test_runner::Config::default()
        }
    }

    fn engine_runner() -> crate::test_runner::TestRunner {
        crate::test_runner::TestRunner::new_with_rng(
            engine_config(),
            crate::test_runner::TestRng::deterministic_rng(
                crate::test_runner::RngAlgorithm::default(),
            ),
        )
    }

    #[test]
    fn tape_serialization_roundtrips() {
        let tape = tape_of(vec![
            Choice::Integer {
                value: i64::encode(-42),
                min: i64::encode(i64::MIN),
                max: i64::encode(i64::MAX),
                shrink_to: i64::encode_zero(),
            },
            Choice::Float {
                value: 2.5,
                min: f64::NEG_INFINITY,
                max: f64::INFINITY,
                allow_nan: true,
            },
            Choice::Bool { value: true },
            Choice::RawU32 { value: 0xDEAD },
            Choice::RawU64 { value: 0xBEEF_CAFE },
            Choice::RawBytes {
                value: vec![1, 2, 3, 4, 5],
            },
        ]);
        let bytes = serialize_tape(&tape);
        assert_eq!(Some(tape), deserialize_tape(&bytes));
        // Strictness: truncated input is rejected.
        assert_eq!(None, deserialize_tape(&bytes[..bytes.len() - 1]));
        assert_eq!(None, deserialize_tape(&[99]));
    }

    #[test]
    fn persisted_tape_form_roundtrips_as_string() {
        use core::str::FromStr;
        let seed = crate::test_runner::PersistedSeed(
            crate::test_runner::failure_persistence::PersistedFailure::Tape {
                bytes: vec![0xab, 0xcd, 0x01],
                seed: None,
            },
        );
        let string = format!("{}", seed);
        assert!(string.starts_with("ct1 "), "got: {}", string);
        assert_eq!(
            Ok(seed),
            crate::test_runner::PersistedSeed::from_str(&string)
        );
    }

    #[test]
    fn persisted_tape_replays_exact_shrunken_value() {
        // Run 1: fail on v >= 1.7, shrink to 2.0, persist.
        let mut config = engine_config();
        config.failure_persistence = Some(Box::new(
            crate::test_runner::MapFailurePersistence::default(),
        ));
        config.source_file = Some("tape_persistence_test");
        let mut runner = crate::test_runner::TestRunner::new_with_rng(
            config,
            crate::test_runner::TestRng::deterministic_rng(
                crate::test_runner::RngAlgorithm::default(),
            ),
        );
        let strategy = 0.0f64..10.0;
        match runner.run(&strategy, |v| {
            if v >= 1.7 {
                Err(crate::test_runner::TestCaseError::fail("too big"))
            } else {
                Ok(())
            }
        }) {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(2.0, value)
            }
            other => panic!("unexpected result: {:?}", other),
        }

        // The persisted entry is a tape, not a seed.
        let map = runner
            .config()
            .failure_persistence
            .as_ref()
            .unwrap()
            .as_any()
            .downcast_ref::<crate::test_runner::MapFailurePersistence>()
            .unwrap()
            .map
            .clone();
        let entries = &map[&"tape_persistence_test"];
        assert_eq!(1, entries.len());
        assert!(
            format!("{}", entries.iter().next().unwrap()).starts_with("ct1 ")
        );

        // Run 2: fresh runner and RNG; the test only fails at *exactly*
        // 2.0, which random generation will essentially never produce.
        // Only replaying the persisted tape can find it.
        let mut config = engine_config();
        config.cases = 10;
        config.failure_persistence =
            Some(Box::new(crate::test_runner::MapFailurePersistence { map }));
        config.source_file = Some("tape_persistence_test");
        let mut runner = crate::test_runner::TestRunner::new_with_rng(
            config,
            crate::test_runner::TestRng::from_seed(
                crate::test_runner::RngAlgorithm::ChaCha,
                &[7; 32],
            ),
        );
        match runner.run(&(0.0f64..10.0), |v| {
            if v == 2.0 {
                Err(crate::test_runner::TestCaseError::fail("replayed"))
            } else {
                Ok(())
            }
        }) {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(2.0, value)
            }
            other => {
                panic!("persisted tape did not replay the failure: {:?}", other)
            }
        }
        let _ = strategy;
    }

    #[test]
    fn replay_engine_shrinks_weighted_bool_to_false() {
        // bool::weighted draws through a typed Bool choice; the raw
        // fallback would shrink toward `true` (rand's Bernoulli maps a
        // zeroed u64 to true for any p > 0).
        let mut runner = engine_runner();
        match runner.run(&crate::bool::weighted(0.5), |_| {
            Err(crate::test_runner::TestCaseError::fail("always"))
        }) {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(false, value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn replay_engine_shrinking_does_not_exhaust_local_rejects() {
        // Shrink attempts re-vet proposals through filters; the local
        // rejects they incur must not drain the run-wide budget, or
        // shrinking silently stalls once it crosses max_local_rejects.
        let mut config = engine_config();
        config.max_local_rejects = 32;
        let mut runner = crate::test_runner::TestRunner::new_with_rng(
            config,
            crate::test_runner::TestRng::deterministic_rng(
                crate::test_runner::RngAlgorithm::default(),
            ),
        );
        use crate::strategy::Strategy;
        let strategy = (0i32..1000).prop_filter("even", |v| 0 == v % 2);
        match runner.run(&strategy, |v| {
            if v >= 100 {
                Err(crate::test_runner::TestCaseError::fail("too big"))
            } else {
                Ok(())
            }
        }) {
            Err(crate::test_runner::TestError::Fail(_, value)) => {
                assert_eq!(100, value)
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn corrupt_persisted_tape_aborts_loudly() {
        // A regression entry that cannot replay guards nothing; it must
        // fail the run like a dead persisted seed does, not continue as
        // a pass.
        use crate::std_facade::BTreeMap;
        use crate::std_facade::BTreeSet;
        let mut entries = BTreeSet::new();
        entries.insert(crate::test_runner::PersistedSeed(
            crate::test_runner::failure_persistence::PersistedFailure::Tape {
                bytes: vec![99],
                seed: None,
            },
        ));
        let mut map = BTreeMap::new();
        map.insert("corrupt_tape_test", entries);
        let mut config = engine_config();
        config.failure_persistence =
            Some(Box::new(crate::test_runner::MapFailurePersistence { map }));
        config.source_file = Some("corrupt_tape_test");
        let mut runner = crate::test_runner::TestRunner::new_with_rng(
            config,
            crate::test_runner::TestRng::deterministic_rng(
                crate::test_runner::RngAlgorithm::default(),
            ),
        );
        match runner.run(&(0i32..10), |_| Ok(())) {
            Err(crate::test_runner::TestError::Abort(_)) => (),
            other => panic!("expected loud abort, got: {:?}", other),
        }
    }

    #[test]
    fn replay_pops_matching_choices() {
        let mut state = TapeState::default();
        state.start_replay(tape_of(vec![
            Choice::RawU32 { value: 7 },
            Choice::RawU64 { value: 9 },
        ]));
        // Matching kind pops.
        let popped = state.pop_replay(|c| matches!(c, Choice::RawU32 { .. }));
        assert_eq!(Some(Choice::RawU32 { value: 7 }), popped);
        // Kind mismatch does not consume...
        assert_eq!(
            None,
            state.pop_replay(|c| matches!(c, Choice::RawU32 { .. }))
        );
        // ...so the u64 is still there.
        let popped = state.pop_replay(|c| matches!(c, Choice::RawU64 { .. }));
        assert_eq!(Some(Choice::RawU64 { value: 9 }), popped);
        // Overrun.
        assert_eq!(
            None,
            state.pop_replay(|c| matches!(c, Choice::RawU32 { .. }))
        );
        let (output, overrun) = state.finish_replay();
        assert!(overrun);
        assert!(output.choices.is_empty());
    }
}
