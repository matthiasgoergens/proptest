//-
// Copyright 2026 The proptest developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Measures the three tape-shrinker realignment policies (freeze,
//! consume, both) on a shape-changing generator. A bool tag selects a
//! (i32, i32) branch (sum tested) or a (bool, i32) branch (int tested);
//! flipping the tag during shrinking changes the kinds that follow, so
//! replay misaligns. The canonical minimal lives in the second branch
//! (its tag choice is smaller), which the policies reach at different
//! rates.
//!
//! Run with: cargo run --release --example realign-quality

use std::cell::Cell;
use std::fmt::Debug;

use proptest::prelude::*;
use proptest::test_runner::{
    Config, RngAlgorithm, ShrinkEngine, ShrinkRealign, TestError, TestRng,
    TestRunner,
};

#[derive(Clone, Debug)]
enum Shape {
    A(i32, i32),
    B(bool, i32),
}

// Freeze-favouring: flipping the tag from (i32,i32) to (bool,i32)
// realigns better under Freeze (the first recorded int survives as the
// B int); Consume loses it. Canonical minimal: B(false, 100).
fn shape_strategy() -> impl Strategy<Value = Shape> {
    any::<bool>().prop_flat_map(|tag| {
        if tag {
            (0i32..1000, 0i32..1000)
                .prop_map(|(a, b)| Shape::A(a, b))
                .boxed()
        } else {
            (any::<bool>(), 0i32..1000)
                .prop_map(|(f, c)| Shape::B(f, c))
                .boxed()
        }
    })
}

fn fails(s: &Shape) -> bool {
    match s {
        Shape::A(a, b) => a + b >= 100,
        Shape::B(_, c) => *c >= 100,
    }
}

fn is_canonical(s: &Shape) -> bool {
    matches!(s, Shape::B(false, 100))
}

#[derive(Clone, Debug)]
enum Shape2 {
    C(bool, bool, i32, i32),
    D(i32, i32),
}

// Consume-favouring: flipping the tag from (bool,bool,i32,i32) to
// (i32,i32) realigns better under Consume (it skips the two stale bools
// and keeps BOTH recorded ints, carrying the a+b progress); Freeze
// fresh-samples both. Canonical minimal: D(0, 100).
fn shape2_strategy() -> impl Strategy<Value = Shape2> {
    any::<bool>().prop_flat_map(|tag| {
        if tag {
            (any::<bool>(), any::<bool>(), 0i32..1000, 0i32..1000)
                .prop_map(|(x, y, a, b)| Shape2::C(x, y, a, b))
                .boxed()
        } else {
            (0i32..1000, 0i32..1000)
                .prop_map(|(a, b)| Shape2::D(a, b))
                .boxed()
        }
    })
}

fn fails2(s: &Shape2) -> bool {
    match s {
        Shape2::C(_, _, a, b) => a + b >= 100,
        Shape2::D(a, b) => a + b >= 100,
    }
}

fn is_canonical2(s: &Shape2) -> bool {
    matches!(s, Shape2::D(0, 100))
}

fn run_policy<S: Strategy>(
    realign: ShrinkRealign,
    seeds: u64,
    make: &dyn Fn() -> S,
    fails: &dyn Fn(&S::Value) -> bool,
    canonical: &dyn Fn(&S::Value) -> bool,
) -> (u32, u32, u64)
where
    S::Value: Debug,
{
    let mut found = 0u32;
    let mut canon = 0u32;
    let mut calls_total = 0u64;
    for seed in 0..seeds {
        let calls = Cell::new(0u64);
        let mut bytes = [0u8; 32];
        bytes[..8]
            .copy_from_slice(&seed.wrapping_mul(0x9e3779b9).to_le_bytes());
        let mut runner = TestRunner::new_with_rng(
            Config {
                shrink_engine: ShrinkEngine::Tape,
                shrink_realign: realign,
                cases: 400,
                max_shrink_iters: 8000,
                failure_persistence: None,
                ..Config::default()
            },
            TestRng::from_seed(RngAlgorithm::ChaCha, &bytes),
        );
        let result = runner.run(&make(), |s| {
            calls.set(calls.get() + 1);
            if fails(&s) {
                Err(TestCaseError::fail("shape failure"))
            } else {
                Ok(())
            }
        });
        if let Err(TestError::Fail(_, minimal)) = result {
            found += 1;
            calls_total += calls.get();
            if canonical(&minimal) {
                canon += 1;
            }
        }
    }
    (found, canon, calls_total)
}

fn bench<S: Strategy>(
    name: &str,
    seeds: u64,
    make: &dyn Fn() -> S,
    fails: &dyn Fn(&S::Value) -> bool,
    canonical: &dyn Fn(&S::Value) -> bool,
) where
    S::Value: Debug,
{
    println!("{} ({} seeds)", name, seeds);
    println!("  policy    found  canonical   avg test calls");
    for (realign, label) in [
        (ShrinkRealign::Freeze, "freeze"),
        (ShrinkRealign::Consume, "consume"),
        (ShrinkRealign::Both, "both"),
    ] {
        let (found, canon, calls) =
            run_policy(realign, seeds, make, fails, canonical);
        let avg = if found == 0 {
            0.0
        } else {
            calls as f64 / found as f64
        };
        println!(
            "  {:<8}  {:3}/{}  {:3}/{:<3}  {:12.1}",
            label, found, seeds, canon, found, avg
        );
    }
    println!();
}

fn main() {
    let seeds = 200u64;
    bench(
        "freeze-favouring: tag selects (i32,i32) vs (bool,i32)",
        seeds,
        &shape_strategy,
        &fails,
        &is_canonical,
    );
    bench(
        "consume-favouring: tag selects (bool^2,i32,i32) vs (i32,i32)",
        seeds,
        &shape2_strategy,
        &fails2,
        &is_canonical2,
    );
}
