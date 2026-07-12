# What's ad hoc in proptest, and what a principled engine replaces

Working notes from a full-codebase review (2026-07-12), branch
`tape-engine-main`. Grounded in four module surveys (strategy/, numerics,
collections/string/arbitrary, test_runner/state-machine); file:line
references verified against this branch.

## The frame: three models of shrinking

- **Jack / Hedgehog (Haskell):** a generator produces a lazy rose tree of
  values; combinators (`map`, `filter`, applicative composition) operate on
  trees, so shrinking is *integrated* and compositional. Weakness: monadic
  bind still discards the inner tree when the outer shrinks.
- **proptest's ValueTree:** the same rose tree, but *manually
  defunctionalized*. Every strategy hand-implements the tree walk as a
  mutable state machine (`simplify()`/`complicate()` with bespoke fields).
  All the compositionality of Hedgehog's trees is lost; every combinator
  reinvents its own zipper, almost always with exactly one step of undo
  memory (`prev_shrink`, `prev_pick`, `prev_shrinker`, `last_shrinker`).
- **Hypothesis (Conjecture):** shrink the recorded *choices* and replay
  generation. Shrinking is a property of the engine; strategies only
  describe generation. Compositional through everything, including bind.

Nearly every ad-hoc mechanism below is a predictable consequence of model
2, and disappears (not "gets fixed" — disappears) under model 3.

## Recurring ad-hoc patterns (the systematic cruft)

### P1. The one-step-undo zipper (5+ hand-rolled copies)
Every composite ValueTree tracks "which child am I shrinking" plus at most
one step of history:
- `VecValueTree`: `Shrink::{DeleteElement,ShrinkElement}` + `prev_shrink` +
  a `VarBitSet` of hidden elements (collection.rs:532-705, ~70 LOC).
  Deletion is a bit-clear because the materialized tree can't change shape.
- `UnionValueTree`/`TupleUnionValueTree`: `pick`/`min_pick`/`prev_pick`
  duplicated for Vec and per-arity tuples (unions.rs:104-445, ~200 LOC).
- `TupleValueTree`/`ArrayValueTree`: `shrinker` index + `prev_shrinker`,
  12 macro-expanded arities (tuple.rs, array.rs, ~175 LOC).
- `SequentialValueTree` (state-machine): a full second shrinker with
  `Shrink::{InitialState,DeleteTransition,Transition}` +
  `TransitionState` + `seen_transitions_counter` (~330 LOC).
Under the tape: the engine holds best-so-far and proposals; none of this
state exists. Tape passes (span deletion, per-choice minimization) already
subsume all four.

### P2. "Walk back until valid, else panic" (5 copies)
Rejection of an invalid shrink result is implemented as complicate-until-
acceptable with a panic fallback:
- filter.rs:76-85, filter_map.rs:114-127, statics.rs:95-105 (the Filter
  duplicate), num.rs ensure_acceptable (floats vs FloatTypes classes,
  num.rs:1014-1023), char.rs CharValueTree::reposition (surrogate hole,
  char.rs:366-373, "Converged to non-char value").
All exist because a scalar binary search can't represent a non-contiguous
or predicate-constrained domain. The tape's answer is generate-and-revet:
proposals replay through generation, filters and conform hooks re-vet, no
contract and no panic. The test suite carries a dedicated escape hatch
(`strict_complicate_after_simplify: false`) for exactly these files.

### P3. Regeneration-with-retry for bind
flatten.rs:47-197 (~150 LOC): shrinking the outer value stashes the old
inner tree, regenerates fresh inner trees up to Config::cases times,
shared cross-combinator budget `flat_map_regens` (an Arc<AtomicUsize>) to
bound nested-flat_map blowup. Comments admit: "does unfortunately depart
from the direct interpretation of simplify/complicate". Fuse (~145 LOC)
exists largely as guard plumbing for this state juggling. Under the tape,
bind costs nothing (demo posted on PR #658); IndFlatten/IndFlattenMap exist
as user-facing escape hatches from Flatten's weakness and could be
deprecated once Flatten is free.

### P4. Hand-sequenced shrink dimensions
When two shrink axes exist, they are sequenced with a phase flag:
- ShuffleValueTree: `simplifying_inner` one-way boolean; shrink distance
  first, then the collection (shuffle.rs:109-207).
- VecValueTree: all deletions strictly before any element shrink.
- State-machine: delete-unseen-tail optimization before transition edits.
The engine's fixpoint over passes replaces the hand-sequencing; ordering
becomes a tuning choice in one place instead of a state machine per
strategy.

### P5. Three rejection budgets instead of one status concept
`local_rejects` (in-strategy filter retries), `global_rejects`
(prop_assume), `flat_map_regens` (bind retries) live on different objects
with different failure modes (runner.rs:78-92, 979-1021). Hypothesis has
one Status enum (Overrun/Invalid/Valid/Interesting) consumed uniformly by
engine, database, and shrinker. Concrete cost: the tape shrinker needs a
save/restore hack around local_rejects (runner.rs:1757-1765) because
"generation" and "generation during shrinking" share one counter.

### P6. Two replay systems, two persistence models
Fork mode's crash recovery is a bespoke append-only step-log file
(replay.rs, 189 LOC) that replays whole-case outcomes against a seed; it
assumes the ValueTree walk, so the tape engine disables itself under
fork/timeout (runner.rs:727-734). Persistence has both Seed and Tape
entries plus a deprecated v1/v2 double-dispatch shim in the trait
(failure_persistence/mod.rs:139-217). A choice tape is the strictly better
crash artifact (persist candidate tape before running the case; child
death loses nothing), which would let fork mode use the tape engine and
eventually retire the step-log.

### P7. RngAlgorithm zoo as a poor man's choice sequence
rng.rs (~960 LOC) dispatches 4 algorithms on every draw. Two of them are
approximations of "expose the choice sequence": PassThrough (drive
generation from fuzzer bytes) and Recorder (capture bytes for a corpus).
The typed tape is the principled version of both: a serialized tape is a
better fuzzer interface than raw bytes (typed, aligned, replayable), cf.
Hypothesis's PrimitiveProvider backends (CrossHair, Antithesis). XorShift
survives only for pre-0.9.1 persisted seeds.

### P8. Debug-string result cache
result_cache.rs keys on hash of `format!("{:?}", value)` (collision-prone,
allocation-heavy). With the tape, the canonical cache key is the
serialized choice tape.

## Ad hoc but justified (keep; don't confuse with cruft)

- float_samplers.rs (~500 LOC): hand-rolled uniform float sampling because
  rand's overflows on wide ranges and clusters. Generation-time concern,
  needed under any engine. Keep.
- char.rs curated special-chars/preferred-ranges tables: Hypothesis also
  curates ("\n", "\r", surrogates...). The *tables* are principled test
  design; only the binary-search-with-holes shrinking around them was
  cruft (already tape-migrated with conform hooks).
- statics.rs module duplication: RFC-1522/2071 workaround (named closure
  types), language limitation, self-documented for removal when Rust
  allows. Orthogonal to engines.
- tuple.rs 12-arity macros: language limitation (no variadic tuples).
- arbitrary/_std hand-rolled generators (sync.rs thread-spawning, ffi.rs
  extra-element+pop, string.rs UTF-8-error branch coverage ~240 LOC):
  generation-domain knowledge, not shrink cruft. The ffi.rs trick is even
  elegant (keeps length shrink-monotonic without rejection).
- SizeRange plumbing (~140 LOC): normalization boilerplate, fine.

## Quality gaps that are neither (fix by finishing the migration)

- bits.rs draws raw RNG (bits.rs:206-214, 288-291): under the tape it
  regresses to untyped byte-bisection, losing its bit-aware shrink.
  sample::Subsequence inherits this. Migrate to typed draws.
- sample::Selector: RESOLVED on this branch (commit 5228b14). It no
  longer smuggles a TestRng; it is a wrapper over Index (buffer the
  iterator, pick one stable position), shrinking precisely under both
  engines. Kept here for the record because the surrounding analysis
  referenced it.
- hash_map/hash_set MinSize dedup: generate-vec-then-reject loop; the
  principled fix (Hypothesis-style) is dedup-aware generation: retry only
  the colliding element (a local redraw) instead of rejecting the whole
  collection. Also removes a shrink-stall source.
- string.rs to_range: `a{4,}` silently capped at min*2; `*`/`+` at 32.
  Under biased generation this could instead use the collection machinery
  with an occasional heavy tail.
- recursive.rs: closed-form geometric decay per level approximating a size
  budget ("underestimates probabilities" per its own docs). Hypothesis
  threads an actual budget through generation; with the tape the budget
  could live on TestRunner.

## The stacked-PR plan

- **PR A (no API change): finish the migration.** bits.rs +
  sample::Subsequence typed draws; dedup-aware hash_map/set generation;
  possibly Vec<Strategy> fixed-length tape spans. Deprecation note on
  Selector. Closes the silent quality regressions.
- **PR B (keystone): tape under fork/timeout.** Persist the candidate
  tape before each case (it is the natural crash artifact), replay in the
  parent, retire the ValueTree fallback. After B, no configuration depends
  on per-strategy shrinking.
- **PR C (breaking, two-stage): retire the walkers.** C1 deprecate
  ShrinkEngine::ValueTree; C2 delete: Flatten regen machinery, the five
  P2 copies, the P1 zippers, Shuffle phases, state-machine second
  shrinker, Fuse, LazyValueTree, most of BinarySearch. Ballpark 2,000+
  LOC of shrink state machines plus their tests. ValueTree the *trait*
  can stay for API compat with `current()` only.

Cross-fork note: GitHub native stacked PRs (private preview 2026-04)
require base branches in the target repo, so while #658 lives on the
fork, "stacked" means fork-internal branches + PR descriptions that say
"builds on #658"; retarget as layers merge.
