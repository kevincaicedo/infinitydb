#![allow(
    clippy::disallowed_methods,
    reason = "harness crate: process deadlines and run stamps, not cell code"
)]
//! The in-process diff candidate: encoded RESP command bytes → `ConnParser`
//! → `inf_server::execute` → reply bytes. Exercises the same parser, command
//! registry, and store the node will run — only the reactor/TCP plumbing is
//! absent (it arrives with the node assembly; the harness then also gains an
//! `INFINITYD_BIN` mode).

use inf_foundation::time::Nanos;
use inf_server::{ConnCx, execute};
use inf_store::{Keyspace, StoreConfig};
use inf_wire::{ConnParser, Parsed, ParserLimits};

pub struct Candidate {
    store: Keyspace,
    parser: ConnParser,
    cx: ConnCx,
    clock: CandidateClock,
}

/// The `now` each command executes at.
enum CandidateClock {
    /// The process's monotonic clock from construction: the corpus runs
    /// beside a live redis-server, whose wall clock moves too.
    Monotonic(std::time::Instant),
    /// One instant for every command: a run whose replies cannot depend
    /// on how fast the corpus reaches a case.
    Held(Nanos),
}

impl Default for Candidate {
    fn default() -> Candidate {
        Candidate::new()
    }
}

impl Candidate {
    pub fn new() -> Candidate {
        Candidate::with_clock(CandidateClock::Monotonic(std::time::Instant::now()))
    }

    /// A candidate whose clock reads `now` for every command, with the
    /// same wall anchor as [`Candidate::new`] (internal 0 is the moment
    /// of construction).
    pub fn with_clock_held_at(now: Nanos) -> Candidate {
        Candidate::with_clock(CandidateClock::Held(now))
    }

    fn with_clock(clock: CandidateClock) -> Candidate {
        let cx = ConnCx::try_default().expect("fixture cache allocation");
        // Wall anchor at the candidate's epoch: EXPIREAT/EXAT/EXPIRETIME
        // convert through the same Unix instants the redis-server oracle
        // sees, so absolute-time cases diff within `IntWithin` tolerances.
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        cx.node.wall_anchor.set((0, unix_ms));
        cx.node.rng_state.set(0x1AF1_D8A5_0DB5_EED1);
        Candidate {
            store: Keyspace::new(StoreConfig::default()),
            parser: ConnParser::new(ParserLimits::default()),
            cx,
            clock,
        }
    }

    /// Executes one encoded RESP command, returning the raw reply bytes.
    ///
    /// # Panics
    /// Panics if `wire` is not exactly one complete command — harness bug.
    pub fn execute_wire(&mut self, wire: &[u8]) -> Vec<u8> {
        let now = match self.clock {
            CandidateClock::Monotonic(epoch) => Nanos(epoch.elapsed().as_nanos() as u64 + 1),
            CandidateClock::Held(now) => now,
        };
        let mut out = Vec::new();
        let mut iter = self.parser.feed(wire);
        let mut executed = 0;
        while let Some(parsed) = iter.next() {
            match parsed {
                Parsed::Command(argv) | Parsed::Inline(argv) => {
                    execute(&argv, &mut self.store, &mut self.cx, now, &mut out);
                    executed += 1;
                }
                Parsed::Incomplete => break,
                Parsed::ProtocolError(e) => panic!("harness sent a malformed command: {e:?}"),
            }
        }
        assert_eq!(executed, 1, "harness must send exactly one command per call");
        out
    }
}
