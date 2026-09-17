//! Additional L7 evidence; the frozen apply trace remains unchanged (ADR-0137).
use std::cell::Cell;
use std::rc::Rc;

use inf_foundation::hash64;
use inf_foundation::time::Nanos;
use inf_log::fs::sim::SimDisk;
use inf_store::{Keyspace, StateDigest};

/// Version 1, length-framed incremental observation hash. No retained event buffer.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StateHash(u64);

impl Default for StateHash {
    fn default() -> Self {
        Self(0x5354_4154_4500_0001)
    }
}

impl StateHash {
    pub(crate) fn value(self) -> u64 {
        self.0
    }

    pub(crate) fn bytes(&mut self, tag: &[u8], bytes: &[u8]) {
        self.0 = hash64(&(tag.len() as u64).to_le_bytes(), self.0);
        self.0 = hash64(tag, self.0);
        self.0 = hash64(&(bytes.len() as u64).to_le_bytes(), self.0);
        self.0 = hash64(bytes, self.0);
    }

    pub(crate) fn number(&mut self, tag: &[u8], value: u64) {
        self.bytes(tag, &value.to_le_bytes());
    }

    pub(crate) fn disk(&mut self, disk: &SimDisk) {
        self.number(b"disk-image", disk.image_digest());
    }

    pub(crate) fn digest(&mut self, digest: StateDigest) {
        self.number(b"entries", digest.entries);
        self.number(b"contents", digest.digest);
    }

    pub(crate) fn keyspace(&mut self, keyspace: &Keyspace, now: Nanos) {
        self.digest(keyspace.state_digest(now));
        for (namespace, table) in keyspace.tiered_namespaces() {
            self.number(b"tiered-namespace", u64::from(namespace.0));
            self.digest(table.simulation_digest());
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct Recorder(Rc<Cell<StateHash>>);

impl Recorder {
    pub(crate) fn update(&self, observe: impl FnOnce(&mut StateHash)) {
        let mut state = self.0.get();
        observe(&mut state);
        self.0.set(state);
    }

    pub(crate) fn value(&self) -> u64 {
        self.0.get().value()
    }
}

/// Both independent evidence streams must match, including with equal apply traces.
pub fn verify(trace: u64, state: u64, twin_trace: u64, twin_state: u64) -> Result<(), String> {
    if trace != twin_trace {
        return Err(format!("trace_hash differs ({trace:#018x} vs {twin_trace:#018x})"));
    }
    if state != twin_state {
        return Err(format!("state_hash differs ({state:#018x} vs {twin_state:#018x})"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
