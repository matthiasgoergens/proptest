//-
// Copyright 2026 The proptest developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Forkfile protocol for the choice-tape shrink engine under `fork`
//! and `timeout`.
//!
//! The classic [`replay`](super::replay) protocol records one status
//! byte per test case and reconstructs the shrunken value by replaying
//! that deterministic walk. The tape engine shrinks by re-running
//! generation with *edited* tapes, which a status-only log cannot
//! express, so under fork it historically fell back to the ValueTree
//! engine. This protocol records the tapes themselves, so a child that
//! dies mid-shrink leaves behind exactly the input that killed it, and
//! a fresh child resumes by loading the current best tape and
//! continuing — no deterministic replay of the case sequence needed,
//! because the tape makes every state explicit.
//!
//! ## File format
//!
//! A distinct sentinel (so the classic parser rejects this file and
//! vice versa), then the seed line, then a sequence of newline-
//! terminated records. Each record is appended in a single write so a
//! concurrent reader never sees a torn line:
//!
//! ```text
//! proptest-tape-forkfile
//! <seed>
//! C <hex tape>      a generation case, written BEFORE its test runs
//! = -               the preceding case/attempt's status (+ - !)
//! B <hex tape>      a new accepted best (written when shrinking improves)
//! A <hex tape>      a shrink attempt, written BEFORE it runs
//! = +
//! .                 the run terminated normally
//! ```
//!
//! A record written *before* running (`C`/`A`) with no following `=`
//! is the input the child died on. A `C` crash is the failing case
//! itself; an `A` crash is a proposal that kills the process, which is
//! a failure worth keeping (it becomes the new best), matching how the
//! classic protocol treats child death.

use std::io::{self, BufRead, Read, Seek, Write};
use std::string::String;
use std::vec::Vec;

use crate::test_runner::rng::{from_base16, to_base16};
use crate::test_runner::tape::{deserialize_tape, serialize_tape, Tape};
use crate::test_runner::Seed;

const SENTINEL: &str = "proptest-tape-forkfile";

/// One outcome of running a case or attempt in the child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Pass,
    Fail,
    Reject,
}

impl Status {
    fn to_char(self) -> char {
        match self {
            Status::Pass => '+',
            Status::Fail => '-',
            Status::Reject => '!',
        }
    }

    fn from_char(c: char) -> Option<Status> {
        match c {
            '+' => Some(Status::Pass),
            '-' => Some(Status::Fail),
            '!' => Some(Status::Reject),
            _ => None,
        }
    }
}

/// A record appended to the tape forkfile.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Record {
    /// A generation case's recorded tape, written before its test runs.
    Case(Tape),
    /// A shrink attempt's proposal tape, written before it runs.
    Attempt(Tape),
    /// A newly accepted best (simpler failing) tape.
    Best(Tape),
    /// The status of the preceding `Case`/`Attempt`.
    Status(Status),
    /// The run terminated normally.
    Terminate,
}

/// Write the file header (sentinel + seed). Call once, before any
/// records, when creating the forkfile.
pub(crate) fn init_file(
    mut file: impl Write,
    seed: &Seed,
) -> io::Result<()> {
    writeln!(file, "{}", SENTINEL)?;
    writeln!(file, "{}", seed.to_persistence())
}

fn hex_of_tape(tape: &Tape) -> String {
    let mut s = String::new();
    to_base16(&mut s, &serialize_tape(tape));
    s
}

/// Append one record in a single write (newline-terminated so a
/// concurrent read never sees a partial line).
pub(crate) fn append(
    mut file: impl Write,
    record: &Record,
) -> io::Result<()> {
    let line = match record {
        Record::Case(t) => format!("C {}\n", hex_of_tape(t)),
        Record::Attempt(t) => format!("A {}\n", hex_of_tape(t)),
        Record::Best(t) => format!("B {}\n", hex_of_tape(t)),
        Record::Status(s) => format!("= {}\n", s.to_char()),
        Record::Terminate => String::from(".\n"),
    };
    file.write_all(line.as_bytes())
}

/// Result of parsing a tape forkfile.
#[derive(Clone, Debug)]
pub(crate) enum ParsedForkfile {
    /// Valid; the run is still in progress.
    InProgress(ForkState),
    /// Valid; the run terminated normally.
    Terminated(ForkState),
    /// Not parsable (wrong sentinel, bad seed, malformed record).
    Corrupt,
}

/// The reconstructed state of a tape-forkfile run.
#[derive(Clone, Debug)]
pub(crate) struct ForkState {
    pub(crate) seed: Seed,
    /// The best (simplest known failing) tape so far, if a failure has
    /// been found. A dangling `Attempt`/`Case` (the child died running
    /// it) is folded in here: a crashing input is a failure worth
    /// keeping.
    pub(crate) best: Option<Tape>,
    /// Whether the current best came from a child dying on it (as
    /// opposed to the test returning a normal failure). Kept for the
    /// reason string the parent reports.
    pub(crate) best_from_crash: bool,
    /// Number of `Case` records seen with a recorded status, i.e.
    /// generation cases the child already got through. Lets a resuming
    /// child skip re-running the passing prefix of the generation
    /// phase.
    pub(crate) cases_done: usize,
    /// Total `=` status records (case or attempt). Zero means no test
    /// ever completed without the child dying, i.e. the child cannot
    /// run even one case (as opposed to finding a real failure), which
    /// the parent treats as "failed to start".
    pub(crate) statuses_recorded: usize,
}

fn tape_of_hex(hex: &str) -> Option<Tape> {
    // 2 hex chars per byte.
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes: Vec<u8> = vec![0u8; hex.len() / 2];
    from_base16(&mut bytes, hex)?;
    deserialize_tape(&bytes)
}

/// Parse a tape forkfile. The reader is seeked to the start first.
pub(crate) fn parse_from(
    mut file: impl Read + Seek,
) -> io::Result<ParsedForkfile> {
    file.seek(io::SeekFrom::Start(0))?;
    let mut reader = io::BufReader::new(&mut file);

    let mut line = String::new();
    reader.read_line(&mut line)?;
    if SENTINEL != line.trim() {
        return Ok(ParsedForkfile::Corrupt);
    }

    line.clear();
    reader.read_line(&mut line)?;
    let seed = match Seed::from_persistence(&line) {
        Some(seed) => seed,
        None => return Ok(ParsedForkfile::Corrupt),
    };

    let mut best: Option<Tape> = None;
    let mut best_from_crash = false;
    let mut cases_done = 0usize;
    let mut statuses_recorded = 0usize;
    // The tape of a `Case`/`Attempt` record awaiting its `=` status.
    let mut pending: Option<(bool /* is_attempt */, Tape)> = None;
    let mut terminated = false;

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches('\n');
        if trimmed.is_empty() {
            continue;
        }
        let (tag, rest) = match trimmed.split_once(' ') {
            Some((tag, rest)) => (tag, rest),
            None => (trimmed, ""),
        };
        match tag {
            "C" => {
                let tape = match tape_of_hex(rest) {
                    Some(t) => t,
                    None => return Ok(ParsedForkfile::Corrupt),
                };
                pending = Some((false, tape));
            }
            "A" => {
                let tape = match tape_of_hex(rest) {
                    Some(t) => t,
                    None => return Ok(ParsedForkfile::Corrupt),
                };
                pending = Some((true, tape));
            }
            "B" => {
                let tape = match tape_of_hex(rest) {
                    Some(t) => t,
                    None => return Ok(ParsedForkfile::Corrupt),
                };
                best = Some(tape);
                best_from_crash = false;
            }
            "=" => {
                statuses_recorded += 1;
                let status =
                    rest.chars().next().and_then(Status::from_char);
                let (is_attempt, tape) = match pending.take() {
                    Some(p) => p,
                    None => return Ok(ParsedForkfile::Corrupt),
                };
                match status {
                    Some(Status::Fail) => {
                        // A failing case is the initial best; a failing
                        // attempt is only accepted via a `B` record, so
                        // don't promote it here.
                        if !is_attempt {
                            cases_done += 1;
                            if best.is_none() {
                                best = Some(tape);
                                best_from_crash = false;
                            }
                        }
                    }
                    Some(Status::Pass) | Some(Status::Reject) => {
                        if !is_attempt {
                            cases_done += 1;
                        }
                    }
                    None => return Ok(ParsedForkfile::Corrupt),
                }
            }
            "." => {
                terminated = true;
                break;
            }
            _ => return Ok(ParsedForkfile::Corrupt),
        }
    }

    // A record written before running with no status: the child died
    // on that input. A crashing input is a failure worth keeping.
    if let Some((_is_attempt, tape)) = pending {
        best = Some(tape);
        best_from_crash = true;
    }

    let state = ForkState {
        seed,
        best,
        best_from_crash,
        cases_done,
        statuses_recorded,
    };
    Ok(if terminated {
        ParsedForkfile::Terminated(state)
    } else {
        ParsedForkfile::InProgress(state)
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_runner::tape::Choice;

    fn seed() -> Seed {
        Seed::XorShift([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
    }

    fn tape(vals: &[u128]) -> Tape {
        Tape {
            choices: vals
                .iter()
                .map(|&v| Choice::Integer {
                    value: v,
                    min: 0,
                    max: 1000,
                    shrink_to: 0,
                })
                .collect(),
            spans: Vec::new(),
        }
    }

    fn write_all(seed: &Seed, records: &[Record]) -> Vec<u8> {
        let mut buf = Vec::new();
        init_file(&mut buf, seed).unwrap();
        for r in records {
            append(&mut buf, r).unwrap();
        }
        buf
    }

    fn parse(buf: Vec<u8>) -> ParsedForkfile {
        parse_from(io::Cursor::new(buf)).unwrap()
    }

    #[test]
    fn roundtrips_a_completed_shrink() {
        let buf = write_all(
            &seed(),
            &[
                Record::Case(tape(&[5])),
                Record::Status(Status::Pass),
                Record::Case(tape(&[500, 500])),
                Record::Status(Status::Fail),
                Record::Attempt(tape(&[0, 500])),
                Record::Status(Status::Fail),
                Record::Best(tape(&[0, 100])),
                Record::Terminate,
            ],
        );
        match parse(buf) {
            ParsedForkfile::Terminated(st) => {
                assert_eq!(Some(tape(&[0, 100])), st.best);
                assert!(!st.best_from_crash);
                assert_eq!(2, st.cases_done);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn dangling_attempt_is_a_crash_best() {
        // Child wrote an attempt then died before its status.
        let buf = write_all(
            &seed(),
            &[
                Record::Case(tape(&[500, 500])),
                Record::Status(Status::Fail),
                Record::Best(tape(&[0, 300])),
                Record::Attempt(tape(&[0, 200])),
            ],
        );
        match parse(buf) {
            ParsedForkfile::InProgress(st) => {
                assert_eq!(Some(tape(&[0, 200])), st.best);
                assert!(st.best_from_crash);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn dangling_case_is_the_failing_input() {
        let buf = write_all(
            &seed(),
            &[
                Record::Case(tape(&[5])),
                Record::Status(Status::Pass),
                Record::Case(tape(&[999, 1])),
            ],
        );
        match parse(buf) {
            ParsedForkfile::InProgress(st) => {
                assert_eq!(Some(tape(&[999, 1])), st.best);
                assert!(st.best_from_crash);
                assert_eq!(1, st.cases_done);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn no_failure_yet() {
        let buf = write_all(
            &seed(),
            &[
                Record::Case(tape(&[5])),
                Record::Status(Status::Pass),
                Record::Case(tape(&[6])),
                Record::Status(Status::Reject),
            ],
        );
        match parse(buf) {
            ParsedForkfile::InProgress(st) => {
                assert_eq!(None, st.best);
                assert_eq!(2, st.cases_done);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn wrong_sentinel_is_corrupt() {
        let mut buf = Vec::new();
        writeln!(buf, "proptest-forkfile").unwrap();
        writeln!(buf, "{}", seed().to_persistence()).unwrap();
        assert!(matches!(parse(buf), ParsedForkfile::Corrupt));
    }

    #[test]
    fn bad_hex_is_corrupt() {
        let mut buf = write_all(&seed(), &[]);
        buf.extend_from_slice(b"C zzzz\n");
        assert!(matches!(parse(buf), ParsedForkfile::Corrupt));
    }
}
