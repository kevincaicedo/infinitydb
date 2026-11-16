//! Connection state: the slab keyed `{slot:24, gen:32}` (the completion-
//! token model, so a stale completion never touches a reused slot), the
//! deferred-command carrier a pump owns, and the accept-refusal shapes.

use super::*;

/// Owned fabric outcome (decoded outcomes borrow ring slots; gate values
/// must own their bytes). A leg's reply is an [`OwnedReply`], its outcome
/// read from the wire's variant at the one decode (ADR-0190 D3).
#[derive(PartialEq, Eq, Debug)]
pub enum OwnedOutcome {
    Ok,
    Reply(OwnedReply),
    Int(i64),
    Nil,
    Bool(bool),
    Err(ErrCode),
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

    /// A flat command over `buf`, which holds one block `stage_argv_block`
    /// wrote at offset 0 (the same layout): one flat copy behind one
    /// pointer, as a parked fabric entry holds its request (ADR-0177 D6).
    pub(super) fn from_staged(buf: Vec<u8>) -> OwnedCmd {
        OwnedCmd { buf }
    }

    /// The flat copy's length in bytes, its header included: what it holds,
    /// not its buffer's capacity.
    pub(super) fn len_bytes(&self) -> u64 {
        self.buf.len() as u64
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
/// `Accepted` arm) so its `Close` completion routes to no connection. The
/// boot capacity's ceiling is the same bound (`limits::CONN_TASKS_MAX`).
pub(super) const CONN_SLOT_CAP: u32 = crate::limits::CONN_TASKS_MAX;

/// Redis's reply past `maxclients` (networking.c, measured 8.0.5).
// reply-outcome-allow: a connection frame copied to a send buffer, no reply (ADR-0123 D1)
pub(super) const MAXCLIENTS_REFUSAL: &[u8] = b"-ERR max number of clients reached\r\n";

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
    /// Admits the connection `make` builds from its own key, so whatever serves the key (its
    /// pump) is made in the step that installs it; `None` when every slot below the cap is
    /// live (the admission bound: a 2^24-th connection on a cell is refused, never a release
    /// assert), and `make` is not called.
    pub(super) fn insert_with(&mut self, make: impl FnOnce(ConnKey) -> T) -> Option<ConnKey> {
        let key = match self.free.last() {
            Some(&slot) => ConnKey { slot, generation: self.gens[slot as usize] },
            None => {
                let slot = u32::try_from(self.slots.len()).ok().filter(|&slot| slot < self.cap)?;
                ConnKey { slot, generation: 0 }
            }
        };
        let conn = make(key);
        if self.free.pop().is_some() {
            self.slots[key.slot as usize] = Some(conn);
        } else {
            self.slots.push(Some(conn));
            self.gens.push(0);
        }
        self.live += 1;
        Some(key)
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

    /// The slab refuses the accept past its cap instead of asserting, and
    /// `make` runs only for a slot it grants, with that slot's key. A tiny
    /// cap stands in for `CONN_SLOT_CAP`; the arithmetic is the same.
    #[test]
    fn slab_refuses_past_the_cap_and_readmits_on_release() {
        let mut slab: ConnSlab<u32> = ConnSlab { cap: 3, ..ConnSlab::default() };
        let keyed = |slab: &mut ConnSlab<u32>, value: u32| {
            slab.insert_with(|key| {
                assert_eq!(key.generation, if value == 5 { 1 } else { 0 }, "make sees its key");
                value
            })
        };
        let a = keyed(&mut slab, 1).expect("slot 0");
        let b = keyed(&mut slab, 2).expect("slot 1");
        let c = keyed(&mut slab, 3).expect("slot 2");
        assert_eq!((a.slot, b.slot, c.slot), (0, 1, 2));
        assert_eq!(slab.live, 3);
        let refused = slab.insert_with(|_| panic!("a refused accept builds nothing"));
        assert!(refused.is_none(), "the cap refuses, never panics");
        assert_eq!(slab.live, 3, "a refused accept is not live");
        assert_eq!(slab.remove(b), Some(2));
        let reused = keyed(&mut slab, 5).expect("released slot readmits");
        assert_eq!(reused.slot, 1);
        assert_eq!(reused.generation, 1, "the generation moved so the old key is dead");
        assert!(slab.get_mut(b).is_none());
        assert_eq!(slab.get_mut(reused).copied(), Some(5));
        assert!(keyed(&mut slab, 6).is_none());
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
