//-
// Copyright 2026 The proptest developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Strategies for generating random functions.
//!
//! [`function(output)`](function) generates values of [`RandomFn<I, S>`],
//! a pure pseudo-random function from `I` to the values of the `output`
//! strategy: calling it hashes the argument into a salt and materialises
//! the result from a PRNG stream derived from (captured seed, salt), so
//! the same argument always maps to the same result within a test case,
//! and different arguments get independent results. This is the
//! CoArbitrary analogue for proptest.
//!
//! Under the classic ValueTree engine the generated functions do not
//! shrink (like QuickCheck functions before `Fun`). Under the tape
//! engine (`Config::shrink_engine = ShrinkEngine::Tape`) each call's
//! draws are recorded as a keyed sub-stream of the test case's choice
//! tape (design/random-functions.md), so shrinking edits WHAT THE
//! FUNCTION RETURNS for the arguments the test actually used, and the
//! reported counterexample's function keeps its observed behaviour.
//!
//! The `Debug` form of a `RandomFn` prints the table of observed calls,
//! which is exactly what a failure report needs.

use crate::std_facade::{fmt, BTreeMap, Rc, String};
use core::cell::RefCell;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use crate::strategy::{NewTree, Strategy, ValueTree};
use crate::test_runner::TestRunner;
use crate::test_runner::{Config, RngAlgorithm, TestRng};

use crate::test_runner::tape::{KeyElt, StreamKey, TapeState};

/// FNV-1a with fixed keys: the salt is the argument's identity across
/// runs and across processes, so it must not depend on `RandomState` or
/// any per-process hasher seeding.
struct Fnv1a(u64);

impl Hasher for Fnv1a {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    }
}

fn salt_of<I: Hash>(arg: &I) -> u64 {
    let mut hasher = Fnv1a(0xcbf2_9ce4_8422_2325);
    arg.hash(&mut hasher);
    hasher.finish()
}

/// splitmix64: derives the per-argument PRNG seed from the function's
/// captured seed and the argument salt.
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

struct RandomFnShared<S: Strategy> {
    output: Rc<S>,
    /// Entropy captured at generation time (a recorded draw, so it
    /// replays deterministically under the tape engine).
    seed: u64,
    /// The function's stream identity when generated under a live tape.
    stream: Option<StreamKey>,
    /// Shared handle to the generating runner's tape state. `Off` (and
    /// unused) outside the tape engine's windows.
    tape: Rc<RefCell<TapeState>>,
    /// Observed calls, for `Debug`: argument's debug form to result's
    /// debug form, in argument order.
    observed: RefCell<BTreeMap<String, String>>,
}

/// A randomly generated pure function from `I` to the values of an
/// output strategy `S`. See the [module documentation](self).
pub struct RandomFn<I, S: Strategy> {
    shared: Rc<RandomFnShared<S>>,
    _marker: PhantomData<fn(&I)>,
}

impl<I, S: Strategy> Clone for RandomFn<I, S> {
    fn clone(&self) -> Self {
        RandomFn {
            shared: Rc::clone(&self.shared),
            _marker: PhantomData,
        }
    }
}

impl<I, S: Strategy> fmt::Debug for RandomFn<I, S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let observed = self.shared.observed.borrow();
        write!(f, "RandomFn {{")?;
        for (arg, out) in observed.iter() {
            write!(f, " {} => {},", arg, out)?;
        }
        write!(f, " _ => <fresh> }}")
    }
}

impl<I: Hash + fmt::Debug, S: Strategy> RandomFn<I, S> {
    /// Apply the function. Pure within a test case: equal arguments
    /// (under `Hash`) map to equal results.
    pub fn call(&self, arg: &I) -> S::Value {
        let salt = salt_of(arg);

        // Enter the per-argument stream when generated under a live
        // tape: entering rewinds the stream's cursors, so repeated
        // same-argument calls replay identically even on edited tapes.
        let entered = match &self.shared.stream {
            Some(stream) if self.shared.tape.borrow().is_on() => {
                let mut key = stream.clone();
                key.push(KeyElt::Salt(salt));
                Some(self.shared.tape.borrow_mut().enter_stream(key))
            }
            _ => None,
        };

        // A scratch runner over a PRNG derived from (seed, salt): the
        // fresh-sampling behaviour, and the source for any draw the tape
        // does not answer. Sharing the tape handle routes the output
        // strategy's draws to the entered stream.
        let derived = splitmix64(self.shared.seed ^ salt);
        let mut seed_bytes = [0u8; 16];
        seed_bytes[..8].copy_from_slice(&derived.to_le_bytes());
        seed_bytes[8..].copy_from_slice(&splitmix64(derived).to_le_bytes());
        let mut rng = TestRng::from_seed(RngAlgorithm::XorShift, &seed_bytes);
        rng.tape = Rc::clone(&self.shared.tape);
        let mut runner = TestRunner::new_with_rng(Config::default(), rng);

        let result = self.shared.output.new_tree(&mut runner);

        if let Some(prev) = entered {
            self.shared.tape.borrow_mut().exit_stream(prev);
        }

        let value = match result {
            Ok(tree) => tree.current(),
            Err(reason) => panic!(
                "RandomFn: output strategy failed to generate a value \
                 ({}); strategies whose filters reject most inputs are \
                 not usable as RandomFn outputs",
                reason
            ),
        };

        self.shared
            .observed
            .borrow_mut()
            .insert(format!("{:?}", arg), format!("{:?}", value));
        value
    }
}

/// The strategy returned by [`function`].
#[must_use = "strategies do nothing unless used"]
pub struct Function<I, S> {
    output: Rc<S>,
    _marker: PhantomData<fn(&I)>,
}

impl<I, S: fmt::Debug> fmt::Debug for Function<I, S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Function")
            .field("output", &self.output)
            .finish()
    }
}

/// The value tree for [`Function`]. The classic shrinker cannot walk a
/// function value, so `simplify`/`complicate` are inert; the tape
/// engine shrinks the function through its recorded streams instead.
pub struct FunctionValueTree<I, S: Strategy> {
    value: RandomFn<I, S>,
}

impl<I: Hash + fmt::Debug, S: Strategy> ValueTree for FunctionValueTree<I, S> {
    type Value = RandomFn<I, S>;

    fn current(&self) -> RandomFn<I, S> {
        self.value.clone()
    }

    fn simplify(&mut self) -> bool {
        false
    }

    fn complicate(&mut self) -> bool {
        false
    }
}

impl<I: Hash + fmt::Debug, S: Strategy> Strategy for Function<I, S> {
    type Tree = FunctionValueTree<I, S>;
    type Value = RandomFn<I, S>;

    fn new_tree(&self, runner: &mut TestRunner) -> NewTree<Self> {
        // The captured seed is an ordinary recorded draw, so a tape
        // replay reproduces it; the stream identity is a per-run split
        // ordinal, deterministic for the same reason.
        let seed: u64 = {
            use rand::RngExt;
            runner.rng().random()
        };
        let tape = runner.rng().tape_handle();
        let stream = {
            let mut t = tape.borrow_mut();
            if t.is_on() {
                Some(t.alloc_split())
            } else {
                None
            }
        };
        Ok(FunctionValueTree {
            value: RandomFn {
                shared: Rc::new(RandomFnShared {
                    output: Rc::clone(&self.output),
                    seed,
                    stream,
                    tape,
                    observed: RefCell::new(BTreeMap::new()),
                }),
                _marker: PhantomData,
            },
        })
    }
}

/// Generate pure pseudo-random functions from `I` (anything `Hash +
/// Debug`) to the values of `output`. See the
/// [module documentation](self).
pub fn function<I: Hash + fmt::Debug, S: Strategy>(
    output: S,
) -> Function<I, S> {
    Function {
        output: Rc::new(output),
        _marker: PhantomData,
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::strategy::Just;
    use crate::test_runner::{Config, ShrinkEngine, TestError, TestRunner};

    fn tape_config() -> Config {
        Config {
            shrink_engine: ShrinkEngine::Tape,
            cases: 300,
            ..Config::default()
        }
    }

    #[test]
    fn deterministic_and_pure_without_tape() {
        let mut runner = TestRunner::deterministic();
        let f = function::<u32, _>(0i32..1000)
            .new_tree(&mut runner)
            .unwrap()
            .current();
        let a = f.call(&7);
        assert_eq!(a, f.call(&7), "same argument, same result");
        // Different arguments are (overwhelmingly) independent; probe a
        // few to catch a constant-function bug.
        let distinct: crate::std_facade::Vec<i32> =
            (0..16u32).map(|i| f.call(&i)).collect();
        assert!(distinct.iter().any(|v| *v != distinct[0]));
    }

    #[test]
    fn fn_point_shrinks_to_boundary() {
        // f(0) >= 100 fails; the minimal counterexample is a function
        // with f(0) == 100 exactly.
        let mut runner = TestRunner::new(tape_config());
        let result = runner.run(
            &function::<u32, _>(0i32..1000),
            |f: RandomFn<u32, _>| {
                prop_assert!(f.call(&0) < 100);
                Ok(())
            },
        );
        match result {
            Err(TestError::Fail(_, f)) => {
                assert_eq!(
                    100,
                    f.call(&0),
                    "function should shrink to the boundary; got {:?}",
                    f
                );
            }
            other => panic!("unexpected pass/abort: {:?}", other),
        }
    }

    #[test]
    fn fn_sum_shrinks_across_two_argument_streams() {
        let mut runner = TestRunner::new(tape_config());
        let result = runner.run(
            &function::<u32, _>(0i32..1000),
            |f: RandomFn<u32, _>| {
                prop_assert!(f.call(&1) + f.call(&2) < 100);
                Ok(())
            },
        );
        match result {
            Err(TestError::Fail(_, f)) => {
                assert_eq!(100, f.call(&1) + f.call(&2), "got {:?}", f);
            }
            other => panic!("unexpected pass/abort: {:?}", other),
        }
    }

    #[test]
    fn fn_purity_holds_on_the_minimal() {
        let mut runner = TestRunner::new(tape_config());
        let result = runner.run(
            &function::<u32, _>(0i32..1000),
            |f: RandomFn<u32, _>| {
                let a = f.call(&7);
                let b = f.call(&7);
                prop_assert!(a == b, "impure: {} vs {}", a, b);
                prop_assert!(a < 50);
                Ok(())
            },
        );
        match result {
            Err(TestError::Fail(_, f)) => {
                assert_eq!(f.call(&7), f.call(&7));
                assert_eq!(50, f.call(&7), "got {:?}", f);
            }
            other => panic!("unexpected pass/abort: {:?}", other),
        }
    }

    #[test]
    fn fn_and_data_co_shrink() {
        // A list and a predicate: the failing property wants some
        // element the predicate accepts; minimal is a single 0 element
        // with p(0) == true.
        let mut runner = TestRunner::new(tape_config());
        let strategy = (
            crate::collection::vec(0i32..1000, 1..20),
            function::<i32, _>(Just(false).prop_union(Just(true))),
        );
        let result = runner.run(&strategy, |(xs, p)| {
            prop_assert!(xs.iter().all(|x| !p.call(x)));
            Ok(())
        });
        match result {
            Err(TestError::Fail(_, (xs, p))) => {
                assert_eq!(1, xs.len(), "xs = {:?}", xs);
                assert_eq!(0, xs[0]);
                assert!(p.call(&xs[0]), "p = {:?}", p);
            }
            other => panic!("unexpected pass/abort: {:?}", other),
        }
    }

    #[test]
    fn classic_engine_generates_but_does_not_shrink() {
        // Sanity: on the ValueTree engine the strategy still produces
        // working functions (they just stay unshrunk).
        let mut runner = TestRunner::new(Config {
            cases: 50,
            ..Config::default()
        });
        let result = runner.run(
            &function::<u32, _>(0i32..1000),
            |f: RandomFn<u32, _>| {
                let _ = f.call(&0);
                prop_assert!(f.call(&1) == f.call(&1));
                Ok(())
            },
        );
        assert!(result.is_ok(), "{:?}", result);
    }
}
