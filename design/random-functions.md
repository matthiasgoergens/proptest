# Random functions for proptest, shrinkable under the tape engine

Status: IMPLEMENTED 2026-07-16 on branch tape-fn (tape.rs stream
runtime, rng.rs shared handle, runner.rs lifecycle + stream passes,
src/func.rs strategy + tests). Port source: tapecheck's stream-keyed
tapes (tapecheck master ae46d85, design/stream-keyed-tapes.md there).

One mechanism was invented DURING this port and does not exist in the
OCaml engine yet: **orphan adoption**. Lowering a main-stream integer
that feeds a function argument changes the argument's hash, so the
per-argument stream key changes and the call would sample fresh; the
shrink's acceptance then flips a coin, and a seed sweep showed 19/60
runs of the data+predicate co-shrink getting stuck one edit short of
minimal. The fix: when a replay enters an unknown SALTED stream, it
adopts the input of an unclaimed sibling (same parent split, salt
leaf, key order) — exactly the orphan whose argument just changed —
so the function keeps its observed behaviour across the edit, and the
accepted output re-records everything under the new salt. After the
change the same sweep sticks 0/60. Worth back-porting to tapecheck.

The rest of the document is the original design; the implementation
follows it, with the test-before-finish lifecycle and the
live-replay-backed reported minimal both handled as planned.

## What exists today

Nothing. proptest has no CoArbitrary analogue: no strategy produces
function values at all. So this is two features in one:

1. A function strategy (new public API), useful even on the classic
   ValueTree engine, where the generated functions simply do not
   shrink (the QuickCheck-without-Fun state of the world).
2. Stream-keyed tapes in the Rust engine, so that under
   `ShrinkEngine::Tape` the functions DO shrink, purely and
   replay-stably.

Prior art: Haskell QuickCheck's `Fun` (shows a table of observed
calls; shrinks via an explicit function-table representation),
Hypothesis's `functions()` (draws lazily from the live linear choice
sequence, memoised per argument; shrinks, but per-argument identity is
call-order-dependent), tapecheck (keyed streams; shrinks with stable
per-argument identity).

## Feasibility findings (verified against the code)

- The tape lives in `TestRng.tape: TapeState` (rng.rs); the seam
  records raw `RngCore` draws and typed draws into one flat tape.
- `gen_and_run_case_tape` calls `take_recording()` immediately after
  `new_tree`, BEFORE `call_test` runs: same lifecycle obstacle the
  OCaml port hit. Function draws happen when the test calls the
  function, so the tape must stay live through the test.
- The test runs on the same thread as generation (timeout is enforced
  by the fork parent killing the child; no `thread::spawn` in the
  runner), so the escaped function value can share the tape via
  `Rc<RefCell<...>>` with no Send/Sync contamination.
- Fork (PR B's forkfile) serializes tapes, not values, so function
  values never cross a process boundary; streams ride along
  transparently once serialization understands them.

## Design

### The strategy (`proptest::func`)

```rust
pub struct RandomFn<I, O> { /* Rc handle to shared state */ }
impl<I: Hash, O: Clone> RandomFn<I, O> {
    pub fn call(&self, arg: &I) -> O { ... }
}
pub fn function<S: Strategy>(output: S) -> impl Strategy<Value = RandomFn<I, S::Value>>
```

(`.call()` rather than `impl Fn`: the `Fn` traits are unimplementable
on stable Rust. A `Deref`-to-closure sugar can come later.)

Per call: salt = hash of the argument under a FIXED-KEY hasher (not
`RandomState`: the salt is the argument's cross-run identity, exactly
the mistake-shaped detail to get right first). Then:

- Classic engine: derive a sub-RNG from (captured generation seed,
  salt), run the output strategy's `new_tree` against a lightweight
  runner, return `current()`. Deterministic per argument, pure, does
  not shrink. This is plain CoArbitrary and works everywhere.
- Tape engine: same, but the draws go through the shared tape handle
  under stream key `Salt(salt)` scoped by this function's `Split(n)`
  identity, so edits to the recorded entries change what the function
  returns.

`Debug` for `RandomFn` prints the table of observed calls
(`{0 => 100, 7 => 3, _ => fresh}`), which is what a failure report
needs and what QuickCheck's `Fun` got right.

### Stream identity without a splittable RNG

base_quickcheck forced tapecheck to intercept `split`/`perturb` inside
the PRNG (seam v2), because generators are welded to
`Splittable_random.t`. proptest has no splittable RNG and we are
writing the function strategy from scratch, so the strategy allocates
its own stream identity directly against the tape: `Split(n)` from a
per-run counter at generation time, `Salt(hash)` per call. No RNG-seam
revision at all; the Rust port is SIMPLER than the OCaml one on this
axis.

Mechanically: `TapeState` gains a current-stream field with
enter/leave semantics; draws record into the active stream. The
`RandomFn` closure enters its stream around the output strategy's
draws.

### Tape image, passes, lifecycle (straight port of ae46d85)

- `Tape` becomes an image: main stream plus `(key, choices)`
  sub-streams, sorted; per-stream replay cursors; a call boundary
  rewinds its stream's cursors (purity under edits); absent stream =
  sample fresh silently (whole-stream deletion + new salts); exhausted
  known stream = overrun (deletion guard preserved).
- Serialization v2 with keyed sections; main-only tapes keep emitting
  the v1 bytes, so existing persistence files and the PR B forkfile
  format stay valid.
- Shrink passes iterate per stream; new whole-stream deletion pass.
- Lifecycle: the tape path runs the test BEFORE taking the recording
  (`gen_and_run_case_tape` and `tape_attempt` both reorder); the
  reported minimal is regenerated on a tape left in replay mode. The
  OCaml port's 100-vs-298 gotcha (winning tape says 100, reported
  function draws fresh and says 298) is avoided up front.

## Hypothesis, for comparison (asked 2026-07-16)

Hypothesis already ships shrinkable random functions and has since
2019: `functions(..., pure=True)` draws the return value from the live
ConjectureData at call time, memoised per argument, and refuses to be
called outside the test. They never needed stream keys because their
whole model is one linear choice sequence and their shrinker is
misalignment-tolerant by construction; per-argument draws sit in the
sequence in call order, so shrink edits that change call order or
count scramble other arguments' draws, and the shrinker just powers
through the noise. A Hypothesis port of stream keys would be a
refinement (stabler fn shrinking, keyed persistence), not a new
capability, and would have to swim against their deliberately simple
linear-tape architecture. Not worth pursuing before the Rust port
proves the keyed design twice.

## Staging

A follow-up PR on top of tape-engine-main (independent of PR B), in
two commits: (1) stream-keyed tape core + serialization v2 + passes,
mirroring the OCaml commit; (2) the `proptest::func` strategy and its
tests. Test plan mirrors test_bq/test_fn_shrink.ml: boundary-exact
point shrink, purity under double call, multi-argument sum,
data+predicate co-shrink, v1/v2 serialization compat, plus a classic
-engine test that the strategy generates deterministic pure functions
even where it cannot shrink them.
