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
