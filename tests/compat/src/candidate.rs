//! The in-process diff candidate: encoded RESP command bytes → `ConnParser`
//! → `inf_server::execute` → reply bytes. Exercises the same parser, command
//! registry, and store the node will run — only the reactor/TCP plumbing is
//! absent (it arrives with the node assembly; the harness then also gains an
//! `INFINITYD_BIN` mode).

use inf_foundation::time::Nanos;
use inf_server::{ConnCx, execute};
use inf_store::{CellStore, StoreConfig};
use inf_wire::{ConnParser, Parsed, ParserLimits};

pub struct Candidate {
    store: CellStore,
    parser: ConnParser,
    cx: ConnCx,
    epoch: std::time::Instant,
}

impl Default for Candidate {
    fn default() -> Candidate {
        Candidate::new()
    }
}

impl Candidate {
    pub fn new() -> Candidate {
        let cx = ConnCx::default();
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
            store: CellStore::new(StoreConfig::default()),
            parser: ConnParser::new(ParserLimits::default()),
            cx,
            epoch: std::time::Instant::now(),
        }
    }

    /// Executes one encoded RESP command, returning the raw reply bytes.
    ///
    /// # Panics
    /// Panics if `wire` is not exactly one complete command — harness bug.
    pub fn execute_wire(&mut self, wire: &[u8]) -> Vec<u8> {
        let now = Nanos(self.epoch.elapsed().as_nanos() as u64 + 1);
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
