//! Connection state: the slab keyed `{slot:24, gen:32}` (the completion-
//! token model, so a stale completion never touches a reused slot), the
//! deferred-command carrier a pump owns, and the accept-refusal shapes.

use super::*;

/// Owned fabric outcome (decoded outcomes borrow ring slots; gate values
/// must own their bytes).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OwnedOutcome {
    Ok,
    Bytes(Vec<u8>),
    Int(i64),
    Nil,
    Bool(bool),
    Err(ErrCode),
}

impl OwnedOutcome {
    pub(super) fn own(outcome: &Outcome<'_>) -> OwnedOutcome {
        match outcome {
            Outcome::Ok => OwnedOutcome::Ok,
            Outcome::Bytes(b) => OwnedOutcome::Bytes(b.to_vec()),
            Outcome::Int(i) => OwnedOutcome::Int(*i),
            Outcome::Nil => OwnedOutcome::Nil,
            Outcome::Bool(b) => OwnedOutcome::Bool(*b),
            Outcome::Err(e) => OwnedOutcome::Err(*e),
        }
    }
}

// ---- deferred commands --------------------------------------------------------

/// One deferred command, flattened into a single allocation:
/// `[argc:u32][end_0:u32 … end_{argc-1}:u32][arg bytes …]` with absolute end
/// offsets. Replaces `Vec<Vec<u8>>` — 1+argc allocations per deferred
/// command was a top origin-side cost in the M0-R1 cross-cell profile.
pub(super) struct OwnedCmd {
    buf: Vec<u8>,
}

impl OwnedCmd {
    /// Flatten `argv` into `buf` (recycled through `Shared::cmd_pool` —
    /// M2.5 Phase H: `from_argv` was one malloc/free per deferred command,
    /// and on the natural-routing leg every remote command defers).
    pub(super) fn from_argv_into(argv: &ArgvRef<'_>, mut buf: Vec<u8>) -> OwnedCmd {
        let argc = argv.len();
        let head = 4 + 4 * argc;
        let total = head + (0..argc).map(|i| argv.arg(i).len()).sum::<usize>();
        buf.clear();
        buf.reserve(total);
        buf.extend_from_slice(&u32::try_from(argc).expect("argc fits u32").to_le_bytes());
        let mut end = head;
        for i in 0..argc {
            end += argv.arg(i).len();
            buf.extend_from_slice(&u32::try_from(end).expect("cmd fits u32").to_le_bytes());
        }
        for i in 0..argc {
            buf.extend_from_slice(argv.arg(i));
        }
        OwnedCmd { buf }
    }

    pub(super) fn argc(&self) -> usize {
        u32::from_le_bytes(self.buf[..4].try_into().expect("header")) as usize
    }

    fn end(&self, i: usize) -> usize {
        let at = 4 + 4 * i;
        u32::from_le_bytes(self.buf[at..at + 4].try_into().expect("ends table")) as usize
    }

    pub(super) fn arg(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 4 + 4 * self.argc() } else { self.end(i - 1) };
        &self.buf[start..self.end(i)]
    }

    /// Borrowed views over the flat buffer (`extract_keys`/`ApplyArgs`/
    /// observer want `&[&[u8]]`). Heap fallback for wide commands — the
    /// dispatch hot path uses the [`ARGV_INLINE`] stack array instead.
    pub(super) fn slices(&self) -> Vec<&[u8]> {
        (0..self.argc()).map(|i| self.arg(i)).collect()
    }

    fn mem(&self) -> usize {
        self.buf.capacity()
    }

    /// Surrender the flat buffer for recycling (`Shared::recycle_cmd_buf`).
    pub(super) fn into_buf(self) -> Vec<u8> {
        self.buf
    }
}

// ---- connection slab ---------------------------------------------------------

impl ConnKey {
    /// The key as one word — the publisher tag the fabric echoes
    /// (ADR-0101 D1). Meaningful on the issuing cell only.
    pub(super) fn packed(self) -> u64 {
        (u64::from(self.slot) << 32) | u64::from(self.generation)
    }

    pub(super) fn unpack(word: u64) -> ConnKey {
        ConnKey { slot: (word >> 32) as u32, generation: word as u32 }
    }
}

impl Conn {
    pub(super) fn state_bytes(&self) -> usize {
        size_of::<Conn>()
            + self.parser.buffered()
            + self.out.capacity()
            + self.queue.iter().map(OwnedCmd::mem).sum::<usize>()
            + self.cx.sub_channels.iter().map(|c| c.len() + 24).sum::<usize>()
            + self.cx.sub_patterns.iter().map(|p| p.len() + 24).sum::<usize>()
            + self.self_push.iter().map(|(_, f)| f.capacity() + 32).sum::<usize>()
    }

    /// Takes the stashed self-frames of remote publish `seq`, if the
    /// tagged fan leg delivered any (ADR-0101 D4).
    pub(super) fn take_self_push(&mut self, seq: u64) -> Option<Vec<u8>> {
        let at = self.self_push.iter().position(|(s, _)| *s == seq)?;
        Some(self.self_push.swap_remove(at).1)
    }
}

/// The slot every connection key is below: the completion token carries
/// the slot in 24 bits (`inf_runtime::MAX_SLOT`), and the top value is
/// reserved for the accept the slab refused (see `on_completion`'s
/// `Accepted` arm) so its `Close` completion routes to no connection.
pub(super) const CONN_SLOT_CAP: u32 = inf_runtime::MAX_SLOT;

/// Redis's reply past `maxclients` (networking.c, measured 8.0.5).
pub(super) const MAXCLIENTS_REFUSAL: &[u8] = b"-ERR max number of clients reached\r\n";

/// The accept-retry wheel key (F-L11-02): after the driver parks the
/// accept arm on an exhaustion/broken failure, the plane re-arms it at this
/// cadence — one `accept(2)` per window while the condition persists, never
/// a spin — so descriptors freed outside the driver (segment files,
/// checkpoint handles) let queued clients in without waiting for a
/// connection to close.
pub(super) const ACCEPT_RETRY_TIMER_KEY: u64 = 0xACCE_0001;
pub(super) const ACCEPT_RETRY: Nanos = Nanos::from_millis(100);

impl<T> Default for ConnSlab<T> {
    fn default() -> ConnSlab<T> {
        ConnSlab {
            slots: Vec::new(),
            gens: Vec::new(),
            free: Vec::new(),
            live: 0,
            cap: CONN_SLOT_CAP,
        }
    }
}

impl<T> ConnSlab<T> {
    /// Admits `conn` into a free or fresh slot; `None` when every slot
    /// below the cap is live — the admission bound (batch 12 of the
    /// 2026-08-30 review): before it, the 2^24-th concurrent connection
    /// on a cell was a release assert, i.e. a client-driven node kill.
    pub(super) fn insert(&mut self, conn: T) -> Option<ConnKey> {
        if let Some(slot) = self.free.pop() {
            self.live += 1;
            self.slots[slot as usize] = Some(conn);
            return Some(ConnKey { slot, generation: self.gens[slot as usize] });
        }
        let slot = u32::try_from(self.slots.len()).ok().filter(|&slot| slot < self.cap)?;
        self.live += 1;
        self.slots.push(Some(conn));
        self.gens.push(0);
        Some(ConnKey { slot, generation: 0 })
    }

    pub(super) fn get_mut(&mut self, key: ConnKey) -> Option<&mut T> {
        if self.gens.get(key.slot as usize) != Some(&key.generation) {
            return None;
        }
        self.slots.get_mut(key.slot as usize).and_then(Option::as_mut)
    }

    pub(super) fn remove(&mut self, key: ConnKey) -> Option<T> {
        if self.gens.get(key.slot as usize) != Some(&key.generation) {
            return None;
        }
        let conn = self.slots.get_mut(key.slot as usize).and_then(Option::take);
        if conn.is_some() {
            self.gens[key.slot as usize] = self.gens[key.slot as usize].wrapping_add(1);
            self.free.push(key.slot);
            self.live -= 1;
        }
        conn
    }

    pub(super) fn keys(&self) -> Vec<ConnKey> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_some())
            .map(|(slot, _)| ConnKey { slot: slot as u32, generation: self.gens[slot] })
            .collect()
    }

    /// Every live connection, in slot order (the config sweep).
    pub(super) fn for_each_mut(&mut self, mut f: impl FnMut(&mut T)) {
        for conn in self.slots.iter_mut().flatten() {
            f(conn);
        }
    }
}

#[cfg(test)]
mod conn_slab_tests {
    use super::{CONN_SLOT_CAP, ConnKey, ConnSlab};

    /// Batch 12 of the 2026-08-30 review: the slab refuses the accept
    /// past its cap instead of asserting (the 2^24-th connection was a
    /// release `assert!` — a client-driven node kill). A tiny cap stands
    /// in for `CONN_SLOT_CAP`; the arithmetic is the same.
    #[test]
    fn slab_refuses_past_the_cap_and_readmits_on_release() {
        let mut slab: ConnSlab<u8> = ConnSlab { cap: 3, ..ConnSlab::default() };
        let a = slab.insert(1).expect("slot 0");
        let b = slab.insert(2).expect("slot 1");
        let c = slab.insert(3).expect("slot 2");
        assert_eq!((a.slot, b.slot, c.slot), (0, 1, 2));
        assert_eq!(slab.live, 3);
        assert!(slab.insert(4).is_none(), "the cap refuses, never panics");
        assert_eq!(slab.live, 3, "a refused accept is not live");
        assert_eq!(slab.remove(b), Some(2));
        let reused = slab.insert(5).expect("released slot readmits");
        assert_eq!(reused.slot, 1);
        assert_eq!(reused.generation, 1, "the generation moved so the old key is dead");
        assert!(slab.get_mut(b).is_none());
        assert!(slab.insert(6).is_none());
    }

    #[test]
    fn production_cap_is_below_the_token_slot_width() {
        assert_eq!(CONN_SLOT_CAP, inf_runtime::MAX_SLOT);
        let refused = ConnKey { slot: CONN_SLOT_CAP, generation: 0 };
        // The reserved slot round-trips through a token (the Close op the
        // refusal issues) and never names a slab entry.
        let token = super::ServerPlane::<super::NoopObserver>::token(
            inf_runtime::TokenClass::Close,
            refused,
        );
        assert_eq!(token.slot(), CONN_SLOT_CAP);
        let slab: ConnSlab<u8> = ConnSlab::default();
        assert_eq!(slab.cap, CONN_SLOT_CAP);
    }
}
