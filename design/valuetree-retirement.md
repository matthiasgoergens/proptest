# Design: retiring the per-strategy shrinkers

Status: DRAFT, blocked on design/tape-under-fork.md landing. Inventory
and rationale in design/valuetree-cruft-inventory.md.

## Staging

Stage C1 (minor release): deprecate `ShrinkEngine::ValueTree` and
announce the retirement in the CHANGELOG. The engine keeps working for
one release cycle; `PROPTEST_SHRINK_ENGINE=valuetree` prints a
deprecation note. Collect field reports: any shrink regression where
the classic engine beats the tape is a bug to fix in the tape passes
during this window (the shrink-quality harness makes such reports
reproducible).

Stage C2 (major release): remove the engine and the machinery that
exists only to serve it.

## What C2 deletes

Shrink state machines (per the inventory, file:line refs there):

- strategy/flatten.rs: FlattenValueTree regeneration-with-retry,
  complicate_regen_remaining, and the max_flat_map_regens config knob.
- strategy/filter.rs, filter_map.rs, statics.rs: ensure_acceptable
  walk-back loops and their panics.
- strategy/unions.rs: pick/min_pick/prev_pick walk (both Union and
  TupleUnion copies); strategy/lazy.rs LazyValueTree (deferred branch
  init exists only for the shrink walk).
- strategy/fuse.rs: Fuse and the out-of-order-call guard plumbing.
- strategy/shuffle.rs: simplifying_inner two-phase state.
- collection.rs: Shrink enum, prev_shrink, included_elements bitset.
- tuple.rs/array.rs: shrinker/prev_shrinker index walks.
- num.rs: integer and float BinarySearch (24 macro expansions),
  ensure_acceptable, the ValueTree-only NaN-lenient class check.
- char.rs: CharValueTree, reposition, shrink_bottom (the tape conform
  hook keeps the shrink-target semantics).
- bits.rs: BitSetValueTree shrink cursor.
- proptest-state-machine: the whole second shrinker (Shrink and
  TransitionState enums, seen_transitions_counter, check_acceptable
  re-simulation), keeping only tape-path generation.
- test_runner/runner.rs: the classic shrink() loop, the ValueTree
  branches of gen_and_run_case/replay_persisted_tape, and the
  seed-based persistence write path (seed entries remain READABLE
  forever; they replay through classic generation which stays).

## What stays, deliberately

- The ValueTree TRAIT and Strategy::Tree associated type: removing them
  breaks every downstream Strategy impl for no gain. current() remains
  load-bearing (it materializes the value). simplify()/complicate()
  remain with default implementations returning false, documented as
  vestigial; third-party implementations keep compiling and their
  hand-written shrinkers become inert under the tape (they already are
  today when the tape is on).
- check_strategy_sanity: reduced to generation/current checks.
- float_samplers.rs, char tables, statics.rs Map (language-limitation
  workarounds unrelated to shrinking), SizeRange, the _std generators.
- Generation-time draws stay exactly as in PR A: typed choices with
  conform hooks are the single source of shrink semantics.

## API fallout (major-version notes)

- Config::shrink_engine and PROPTEST_SHRINK_ENGINE are removed (or kept
  as a no-op accepting only "tape", to be decided by upstream taste).
- max_flat_map_regens removed.
- Fuse, LazyValueTree, and the public BinarySearch types removed;
  their appearance in public signatures (e.g. num::i32::BinarySearch as
  Strategy::Tree) is the main compat cost and wants a survey of
  downstream usage first (crater-style grep of crates.io reverse
  dependencies).
- Estimated deletion: 2,000+ LOC of shrink machinery plus its tests,
  and the simplify/complicate halves of every strategy doc.

## Risks

- Downstream custom shrinkers: crates implementing ValueTree with
  meaningful simplify() lose shrinking unless migrated to typed draws.
  Mitigation: the TestRunner draw API (draw_bool, draw_integer_in,
  draw_f64_in, spans, element flags) is public since the state-machine
  migration; document a migration recipe with the state-machine crate
  as the worked example.
- Shrink-quality regressions hiding in rarely-used strategies:
  mitigated by the C1 deprecation window plus the harness.
