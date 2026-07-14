# Design: the choice tape under fork and timeout

Status: DRAFT, not implemented. This is the keystone of the ValueTree
retirement plan (see design/valuetree-cruft-inventory.md): fork/timeout
is the only configuration that still forces the ValueTree engine, so it
is the only thing standing between "the tape is the default" and "the
tape is the only engine anything depends on".

## Today

Fork mode survives test bodies that kill the process. The parent spawns
a child via rusty_fork; they share an append-only forkfile
(test_runner/replay.rs): 16 seed bytes, then one status byte per test
case. On child death the parent spawns a new child, which fast-forwards
through the recorded outcomes (re-running generation but not the tests)
and continues. Shrinking happens in the child through the same protocol,
one ValueTree simplify/complicate step per case. The parent finally
replays the outcome log in-process to reconstruct the shrunken value
without running the test.

Two assumptions bind this to the ValueTree engine:

1. Deterministic case sequence: replaying the same seed plus the same
   outcome log must reproduce the same generation states. The tape
   engine's shrink loop instead re-runs generation with edited tapes,
   which the outcome log cannot express.
2. Shrink steps are a walk on one materialized tree, so "the current
   case" is fully described by (seed, step outcomes). A tape attempt is
   described by the proposal tape itself.

## Proposed protocol

Keep the parent/child split and the crash-recovery model; change what
the forkfile records from "outcomes of a deterministic walk" to
"explicit work items". Two record types are appended by the child:

- `case <base16 tape> <status>`: a generation case. The tape is the
  recorded output tape of the case (written BEFORE running the test,
  status appended after). If the child dies between the two, the parent
  knows exactly which input killed it: the recorded tape replays it.
- `attempt <base16 proposal> <status>`: one shrink attempt. Written
  before running, status appended after. A dead child again leaves the
  killing proposal on file.

Recovery: the new child reads the log, replays the last known-best
failing tape (initial failure or last accepted attempt), and resumes
the shrink pass schedule from a pass/position cursor also recorded in
the log. Crash-during-attempt counts as "interesting" (the proposal
kills the process, which is a failure mode worth keeping), matching how
the ValueTree protocol treats child death today.

Cost analysis: forkfile grows by one serialized tape per case/attempt
instead of one byte. Tapes are typically tens to hundreds of bytes;
runs are hundreds of cases plus a bounded shrink budget
(max_shrink_iters, default 1024). Megabyte-scale worst case, temp-file
lifetime. Acceptable; can be truncated by rewriting the file at
accepted-attempt checkpoints if it ever matters.

Timeout: unchanged. The parent's watchdog (forkfile growth) and the
child's per-case wall clock both keep working, since the child still
appends before and after each test call.

## What this unlocks

- Tape shrinking (round floats, filters, flat_map, span deletion,
  cross-value passes) for fork and timeout users, who today silently
  get the weakest shrinker on the configurations where failures are
  most expensive to reproduce.
- ct1 persistence for fork failures (today they persist as seeds, with
  the seed-replay fragility documented in the CHANGELOG).
- Deleting the ValueTree fallback paths in runner.rs
  (gen_and_run_case, replay_persisted_tape) and eventually the
  per-strategy shrink machinery (stage C).

## Implementation sketch

1. Extend replay.rs with the two record types behind a version marker
   so old forkfiles are detected and discarded (they are temp files;
   no compat needed beyond not misparsing).
2. Child: in tape mode, wrap gen_and_run_case and tape_attempt with
   record-before/status-after appends. The tape serializer already
   exists (test_runner/tape.rs serialize_tape).
3. Parent: recovery reads the log; the "replay outcomes in-process"
   final step becomes "deserialize the winning tape and replay it",
   which replay_persisted_tape already implements.
4. Shrink-pass cursor: the tape shrink loop gains a resumable position
   (pass index, span/choice index), recorded on each accepted attempt.
   This is the largest piece; a coarse first cut can restart the pass
   schedule from the top after a crash (correct, mildly wasteful).
5. Delete the ShrinkEngine::Tape fork-mode guards.

## Open questions

- Should attempt records be batched (write every N) to cut syscall
  overhead, trading recovery granularity? Default no; measure first.
- rusty_fork gives one child per test; the shrink loop could instead
  fork per ATTEMPT for full isolation. Not needed for parity with
  today's behavior (child crashes are already handled), revisit only if
  crash-heavy shrinking proves slow.

---

## Worked-out protocol (2026-07-14, refined during implementation)

The format is implemented and unit-tested (`test_runner/tape_replay.rs`).
Working out the wiring surfaced one subtlety that shapes the record
stream, recorded here so the implementation is unambiguous.

### The B-reanchor rule (the subtlety)

Records are appended across the crash boundary: a child dies leaving a
dangling `C`/`A` (no `=`), then a *fresh* child appends more records
after it. If the fresh child just continued, the parser would see a
`C`/`A` with no `=` followed by unrelated later records, and the crash
input's tape would be lost (overwritten by the next `C`/`A`'s pending
slot) rather than promoted to best.

Rule: **a recovering child writes `B <best>` as its very first
record.** On startup it parses the forkfile, folds any dangling record
into `best` (a crashing input is a failure worth keeping), and
re-anchors by emitting that best as a `B`. This keeps `best` explicit
and monotone across every crash, and makes the parser's "last `B`
wins" the whole truth except for a truly-final dangling record (the
current child's own crash), which folds in at EOF as before.

### Child algorithm (`run_in_process` in tape+fork mode)

1. Parse forkfile → `ForkState { seed, best, cases_done, .. }`.
   `set_seed(seed)`.
2. If `best` is `None` (generation phase): fast-forward generation by
   `cases_done` cases (generate-without-test, advancing the rng
   identically), then run the normal generation loop from there,
   emitting `C <tape>` before each test and `= <status>` after. The
   first failing case becomes `best` (recorded as its `C`/`= -`); go
   to 3. If `cases` is exhausted with no failure, emit `.` and exit
   (the parent reports "no failure").
3. Shrink phase: emit `B <best>` (re-anchor). Replay `best` to a tree,
   run the tape shrink loop from it, emitting `A <proposal>` before
   each attempt, `= <status>` after, and `B <new best>` on each
   accepted improvement. On fixpoint/budget, emit `.` and exit.

Crash handling is implicit: a child that dies mid-test leaves its `C`
or `A` dangling at EOF.

### Parent algorithm (`run_in_fork` in tape mode)

1. Create forkfile; `tape_replay::init_file(seed)`.
2. Loop: fork a child (the child runs the algorithm above). On return,
   parse:
   - `Terminated(state)` → break with `state.best`.
   - `InProgress(state)` → the child died; `state.best` already folds
     the crash input. Re-fork (the next child re-anchors from `best`).
   - `Corrupt` → panic (child corrupted the file).
   Cap re-forks at 10000 as today.
3. Final reconstruction: if `best` is `Some`, replay it once (generate
   the value; no re-shrink — the children did the shrinking) and
   return `TestError::Fail(reason, value)`, persisting the `ct1` entry.
   `reason` notes crash-provenance when `best_from_crash`. If `best`
   is `None`, the run passed.

This deletes the `!fork()` guard on the tape path and lets fork/timeout
failures persist as replayable `ct1` tapes.

### ForkOutput seam

`ForkOutput` gains a format tag. In tape mode: `append` (the per-test
char, called by `call_test`) becomes a no-op (the tape path records
`C`/`A` + `=` explicitly, so the char stream must not corrupt the tape
file); new `record_case`/`record_attempt`/`record_best`/
`record_status` write tape records. In classic mode everything is as
today. The recording calls live in `gen_and_run_case_tape` (case + its
status) and `tape_attempt`/`tape_shrink` (attempt, status, best).

### Verification layers (agreed)

- L0 format: DONE, 7 unit tests, in-process.
- L1 child recording: run the tape engine in-process with a
  tempfile-backed tape `ForkOutput`; parse the file back; assert the
  record stream is well-formed, `best` == the shrink result, and every
  `B` is strictly shortlex-smaller than the previous.
- L2 parent recovery: synthesize forkfiles crashed at each point; drive
  the recovery + final-replay; assert the recovered best and value.
- L3 end-to-end fork: a `mod timeout_tests` test where the tape-engine
  test body `process::abort()`s (crash) or sleeps past the timeout on
  some inputs; assert convergence to the expected minimal across
  re-forks. Also eyeball a real child's record stream against L1's.
- Regression: classic ValueTree fork protocol untouched; existing
  fork/timeout tests green; tape non-fork behavior unchanged (1557/0).
