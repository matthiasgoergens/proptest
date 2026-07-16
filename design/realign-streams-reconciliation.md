# Reconciling try-both realignment with stream-keyed tapes

Status: analysis, 2026-07-16. Concerns the two fork-preview branches
stacked on tape-engine-main: tape-realign (fork PR #4, the
Freeze/Consume/Both kind-mismatch policy) and tape-fn (fork PR #6,
stream-keyed tapes and random functions). They touch the same replay
machinery and will conflict textually when stacked; this note pins the
intended merged semantics so the conflict resolution is mechanical.

## The two mechanisms sit at different granularities

Kind-mismatch policy (Freeze/Consume/Both) acts at CHOICE granularity:
what to do when the next input entry has the wrong kind. Orphan
adoption (tape-fn) acts at STREAM granularity: which input stream a
re-keyed function call should read at all. They compose rather than
compete: adoption picks the stream, then the per-choice policy governs
mismatches inside it.

## Merged semantics (the OCaml engine is the reference)

tapecheck's tape.ml already implements the combination and passes both
feature suites: the policy field lives on the tape state and is
consulted inside the per-stream pop, so Freeze/Consume apply within
every stream independently; the `misaligned` flag stays global (any
stream misaligning marks the proposal, which is what triggers `Both`'s
second replay); adoption runs at stream entry, before any pop. Port
notes for the Rust merge:

- tape-realign's policy branch moves inside `StreamRt`-aware
  `pop_replay` (tape-fn's version), preserving tape-fn's unknown-stream
  rule: an unknown stream samples fresh silently, no misalignment, no
  overrun, regardless of policy.
- Adoption stays deterministic (donor chosen by key order), so `Both`'s
  two replays of the same proposal adopt identically and differ only by
  the per-choice policy, keeping the pick-the-shortlex-better
  comparison meaningful.
- `Both` doubles replays only on misaligned proposals, as before. Note
  that tape-fn removed the pre-test shortlex rejection (the output tape
  is complete only after the test runs), so each of the two replays now
  always includes a test execution; that is the same spend-on-failure
  trade the realign work already accepted.

## Sequencing

No need to merge preemptively. Whichever branch lands second rebases;
the conflict is confined to `pop_replay` and the attempt loop, and the
resolution target is the OCaml semantics above. If upstream review of
#658 asks for either feature to be folded in, fold tape-realign first
(it is the smaller diff) and stack tape-fn's streams on the result.
