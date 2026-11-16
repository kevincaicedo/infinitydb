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

    pub(super) fn mem(&self) -> usize {
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

// ---- the connection slot's states (ADR-0123 A2, ADR-0196 and its A1) ----------

/// A connection slot's state. `out` and the descriptor live only in
/// `Active` and `Flushing`, so a `Closing` or `Draining` slot has nothing
/// to send; a parser lives only in `Active` and in a [`ClosedBy::Peer`], so
/// a state that must not parse has nothing to feed. The queue, the context
/// and the pump are the slab entry's ([`Conn`]): every occupied state runs
/// the queue to its end.
pub(super) enum ConnSlot {
    /// Serving: received bytes parse, replies are sent.
    Active { link: Link, parser: ConnParser, recv: RecvArm },
    /// A close of the flush shape: still sending what the commands read
    /// before the plane's mark or the peer's end of stream owe.
    Flushing { link: Link, stream: PeerStream },
    /// The descriptor's `Close` is in flight: queued commands run, every
    /// reply is dropped.
    Closing { by: ClosedBy },
    /// The descriptor is closed: the queue runs to its end, then the local
    /// registries drop the connection and its pump ships the deltas.
    Draining { by: ClosedBy },
}

/// What a slot that can still send holds: the descriptor, the staged
/// replies and the inputs of the output cap and of `timeout`.
pub(super) struct Link {
    pub(super) fd: RawFd,
    /// Staged reply bytes awaiting RESPOND, one reply opened at a time.
    pub(super) out: ReplyStream,
    /// One in-flight Send max: two outstanding sends on one socket have no
    /// kernel ordering guarantee.
    pub(super) send_inflight: bool,
    /// Injected-clock ms since the staged output first exceeded the pub/sub
    /// soft cap (M1-S11); 0 = under the soft limit.
    pub(super) cob_soft_since_ms: u64,
    /// The output-cap kill was already requested (idempotent counter guard).
    pub(super) cob_kill_sent: bool,
    /// Injected-clock ms of the last received buffer (accept counts); the
    /// `timeout` reaper's input (ADR-0123 D2): one store per buffer PARSE
    /// takes, never per command.
    pub(super) last_active_ms: u64,
    /// The publisher's own frames a tagged fan leg delivered, keyed by
    /// sequence; the pump emits each right after that publish's count
    /// reply (ADR-0101 D3/D4). Bounded by the in-flight window and empty
    /// for every connection that is not a self-subscribed publisher.
    pub(super) self_push: Vec<(u64, Vec<u8>)>,
}

impl Link {
    pub(super) fn new(fd: RawFd, now_ms: u64) -> Link {
        Link {
            fd,
            out: ReplyStream::new(Vec::new()),
            send_inflight: false,
            cob_soft_since_ms: 0,
            cob_kill_sent: false,
            last_active_ms: now_ms,
            self_push: Vec::new(),
        }
    }

    fn bytes(&self) -> usize {
        self.out.capacity() + self.self_push.iter().map(|(_, f)| f.capacity() + 32).sum::<usize>()
    }
}

/// A flushing slot's stream: `Open` while the peer may still send (the
/// plane marked the slot), `Ended` once its end of stream was read. A
/// parser in `Flushing` implies an ended stream, and the end of stream
/// outlives the parser's end, so a delivery's count reads it.
pub(super) enum PeerStream {
    Open,
    Ended { by: ClosedBy },
}

/// Whether a closing slot still holds a parser (ADR-0196 D2, A1.1): `Peer`
/// keeps it for the bytes read before the end of stream or before
/// `Closed`; `Plane` parses nothing.
pub(super) enum ClosedBy {
    /// `taken_at` is the cell's inbox take count when the slot entered its
    /// `Peer` state: once a take has completed since, every entry a receive
    /// returned before then has been parsed. Compared with `!=` only: a
    /// wrap back to it makes one run-out wait one more take.
    Peer {
        parser: ConnParser,
        taken_at: u64,
    },
    Plane,
}

impl ClosedBy {
    /// The same selector with a `Peer`'s take count stamped anew: every
    /// entry into a `Peer` state stamps it.
    fn stamped(self, inbox_takes: u64) -> ClosedBy {
        match self {
            ClosedBy::Peer { parser, taken_at: _ } => {
                ClosedBy::Peer { parser, taken_at: inbox_takes }
            }
            ClosedBy::Plane => ClosedBy::Plane,
        }
    }

    fn buffered(&self) -> usize {
        match self {
            ClosedBy::Peer { parser, .. } => parser.buffered(),
            ClosedBy::Plane => 0,
        }
    }
}

/// The one way a `Peer` parser ends (ADR-0196 D3): its partial frame and
/// any unparsed bytes are dropped, and the selector parses nothing more.
pub(super) fn end_reader(by: &mut ClosedBy) {
    *by = ClosedBy::Plane;
}

/// An active slot's receive arm: `Disarmed` at the queue's high water,
/// `RearmDue` once the pump drains to the low water, re-armed by RESPOND.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RecvArm {
    Armed,
    Disarmed,
    RearmDue,
}

/// What a writer into `out` did with its bytes: a slot that can send takes
/// them, a closing or draining slot drops them, and a vacant slot or
/// another generation has no connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Emitted {
    Written,
    Dropped,
    Vacant,
}

/// What a `Peer` run-out found (ADR-0196 D2, A1.1).
pub(super) enum RunOut {
    /// A take completed since the slot entered its `Peer` state: the
    /// parser ended and the selector is `Plane`.
    Ended,
    /// No take since: the pump waits for PARSE's next.
    Wait,
}

impl Conn {
    pub(super) fn state_bytes(&self) -> usize {
        let slot = match &self.slot {
            ConnSlot::Active { link, parser, .. } => link.bytes() + parser.buffered(),
            ConnSlot::Flushing { link, stream: PeerStream::Ended { by } } => {
                link.bytes() + by.buffered()
            }
            ConnSlot::Flushing { link, stream: PeerStream::Open } => link.bytes(),
            ConnSlot::Closing { by } | ConnSlot::Draining { by } => by.buffered(),
        };
        size_of::<Conn>()
            + slot
            + self.queue.iter().map(Queued::mem).sum::<usize>()
            + self.cx.sub_channels.iter().map(|c| c.len() + 24).sum::<usize>()
            + self.cx.sub_patterns.iter().map(|p| p.len() + 24).sum::<usize>()
    }

    /// The descriptor and staged output of a slot that can still send.
    pub(super) fn link_mut(&mut self) -> Option<&mut Link> {
        match &mut self.slot {
            ConnSlot::Active { link, .. } | ConnSlot::Flushing { link, .. } => Some(link),
            ConnSlot::Closing { .. } | ConnSlot::Draining { .. } => None,
        }
    }

    /// The pump's emit: `Written` where `out` lives, else `Dropped`.
    pub(super) fn emit(&mut self, reply: &OwnedReply) -> Emitted {
        match self.link_mut() {
            Some(link) => {
                link.out.emit(reply);
                Emitted::Written
            }
            None => Emitted::Dropped,
        }
    }

    /// Takes the stashed self-frames of remote publish `seq`, if the
    /// tagged fan leg delivered any (ADR-0101 D4).
    pub(super) fn take_self_push(&mut self, seq: u64) -> Option<Vec<u8>> {
        let link = self.link_mut()?;
        let at = link.self_push.iter().position(|(s, _)| *s == seq)?;
        Some(link.self_push.swap_remove(at).1)
    }

    /// Queues the commands one buffer deferred. An active slot whose queue
    /// reached the high water disarms its receive: returns the descriptor
    /// whose `RecvDisarm` goes out. No other state holds a receive to
    /// disarm, so a `Peer` parser's queue grows only by the bytes read
    /// before its end.
    pub(super) fn enqueue(&mut self, deferred: Vec<Queued>) -> Option<RawFd> {
        self.queue.extend(deferred);
        match &mut self.slot {
            ConnSlot::Active { link, recv, .. }
                if *recv == RecvArm::Armed && self.queue.len() >= PENDING_HIGH_WATER =>
            {
                *recv = RecvArm::Disarmed;
                Some(link.fd)
            }
            ConnSlot::Active { .. }
            | ConnSlot::Flushing { .. }
            | ConnSlot::Closing { .. }
            | ConnSlot::Draining { .. } => None,
        }
    }

    /// The pump to wake, clearing its park flag in the same step.
    pub(super) fn take_parked_pump(&mut self) -> Option<TaskId> {
        core::mem::replace(&mut self.pump_parked, false).then_some(self.pump)
    }

    /// Moves the slot's state out for a transition, leaving a stateless
    /// placeholder that the caller overwrites in the same step.
    fn take_slot(&mut self) -> ConnSlot {
        core::mem::replace(&mut self.slot, ConnSlot::Closing { by: ClosedBy::Plane })
    }

    /// The peer's end of stream (ADR-0196 A1.1): an active slot becomes
    /// `Flushing` `Peer`, keeping its parser for the entries reaped before
    /// it and its `out` for their replies; nothing is pushed, and the
    /// receive has ended, so `recv` leaves with `Active`. A marked slot's
    /// stream ends (`Open` → `Ended`, no longer a receiver). Returns the
    /// pump to wake, so its empty read runs the run-out.
    pub(super) fn end_of_stream(&mut self, inbox_takes: u64) -> Option<TaskId> {
        self.slot = match self.take_slot() {
            ConnSlot::Active { link, parser, recv: _ } => {
                let by = ClosedBy::Peer { parser, taken_at: inbox_takes };
                self.slot = ConnSlot::Flushing { link, stream: PeerStream::Ended { by } };
                return self.take_parked_pump();
            }
            ConnSlot::Flushing { link, stream: PeerStream::Open } => {
                ConnSlot::Flushing { link, stream: PeerStream::Ended { by: ClosedBy::Plane } }
            }
            // A second zero-length receive (the receive has ended) or a
            // closing slot's: nothing changes.
            other @ (ConnSlot::Flushing { stream: PeerStream::Ended { .. }, .. }
            | ConnSlot::Closing { .. }
            | ConnSlot::Draining { .. }) => other,
        };
        None
    }

    /// The peer is gone (a receive or send error): an active or flushing
    /// slot becomes `Closing`, its unsent bytes dropped, a `Peer` parser
    /// moving with its take count stamped. Returns the descriptor whose
    /// one `Close` goes out; a closing or draining slot returns none.
    pub(super) fn peer_gone(&mut self, inbox_takes: u64) -> Option<RawFd> {
        let (fd, slot) = match self.take_slot() {
            ConnSlot::Active { link, parser, recv: _ } => {
                let by = ClosedBy::Peer { parser, taken_at: inbox_takes };
                (Some(link.fd), ConnSlot::Closing { by })
            }
            ConnSlot::Flushing { link, stream: PeerStream::Open } => {
                (Some(link.fd), ConnSlot::Closing { by: ClosedBy::Plane })
            }
            ConnSlot::Flushing { link, stream: PeerStream::Ended { by } } => {
                (Some(link.fd), ConnSlot::Closing { by: by.stamped(inbox_takes) })
            }
            other @ (ConnSlot::Closing { .. } | ConnSlot::Draining { .. }) => (None, other),
        };
        self.slot = slot;
        fd
    }

    /// The plane's immediate close (`timeout`, the `CLIENT KILL` sweep,
    /// which the output cap feeds): an active or flushing slot becomes
    /// `Closing` `Plane`, its parser and unsent bytes dropped. Returns the
    /// descriptor whose one `Close` goes out.
    pub(super) fn close_at_once(&mut self) -> Option<RawFd> {
        let (fd, slot) = match self.take_slot() {
            ConnSlot::Active { link, .. } => {
                (Some(link.fd), ConnSlot::Closing { by: ClosedBy::Plane })
            }
            ConnSlot::Flushing { link, stream } => {
                if let PeerStream::Ended { mut by } = stream {
                    end_reader(&mut by);
                }
                (Some(link.fd), ConnSlot::Closing { by: ClosedBy::Plane })
            }
            other @ (ConnSlot::Closing { .. } | ConnSlot::Draining { .. }) => (None, other),
        };
        self.slot = slot;
        fd
    }

    /// RESPOND's flush end: a flushing slot whose stream holds no parser,
    /// with `out` sent, no send in flight and its pump parked after reading
    /// its queue empty, becomes `Closing` `Plane`. Returns the descriptor
    /// whose one `Close` goes out.
    pub(super) fn flush_end(&mut self) -> Option<RawFd> {
        // Canary: the side-flag mark's flush end, on an active slot.
        #[cfg(inf_canary_mark_applied_late)]
        if self.late_mark
            && let ConnSlot::Active { link, .. } = &self.slot
            && link.out.is_empty()
            && !link.send_inflight
            && self.pump_parked
        {
            let fd = link.fd;
            self.slot = ConnSlot::Closing { by: ClosedBy::Plane };
            return Some(fd);
        }
        let ConnSlot::Flushing { link, stream } = &self.slot else { return None };
        let no_parser = match stream {
            PeerStream::Open | PeerStream::Ended { by: ClosedBy::Plane } => true,
            PeerStream::Ended { by: ClosedBy::Peer { .. } } => false,
        };
        if !no_parser || !link.out.is_empty() || link.send_inflight || !self.pump_parked {
            return None;
        }
        let fd = link.fd;
        self.slot = ConnSlot::Closing { by: ClosedBy::Plane };
        Some(fd)
    }

    /// `Closed`, the `Close` op's terminal completion: `Closing` becomes
    /// `Draining`, a `Peer` parser moving with its take count stamped (an
    /// entry of the connection may wait in the inbox). Returns whether the
    /// slot moved.
    pub(super) fn closed(&mut self, inbox_takes: u64) -> bool {
        let (moved, slot) = match self.take_slot() {
            ConnSlot::Closing { by } => (true, ConnSlot::Draining { by: by.stamped(inbox_takes) }),
            other @ (ConnSlot::Active { .. }
            | ConnSlot::Flushing { .. }
            | ConnSlot::Draining { .. }) => (false, other),
        };
        self.slot = slot;
        moved
    }

    /// The plane's mark of the flush shape (`QUIT`, a protocol error, the
    /// stop's mark): an active slot becomes `Flushing` `Open`, its parser
    /// dropped with its partial frame; a `Peer` parser of any other state
    /// ends. Returns whether the receive is owed one `RecvArm` (ADR-0196
    /// A1.3: an active slot whose receive was not armed), so the marked
    /// slot reaps and releases what follows and reads its end of stream.
    pub(super) fn mark(&mut self) -> bool {
        // Canary: the mark is a side flag and the slot stays active, so its
        // parser reads what follows the mark.
        #[cfg(inf_canary_mark_applied_late)]
        if let ConnSlot::Active { .. } = self.slot {
            self.late_mark = true;
            return false;
        }
        let (owed, slot) = match self.take_slot() {
            ConnSlot::Active { link, parser: _, recv } => {
                (recv != RecvArm::Armed, ConnSlot::Flushing { link, stream: PeerStream::Open })
            }
            ConnSlot::Flushing { link, stream: PeerStream::Ended { mut by } } => {
                end_reader(&mut by);
                (false, ConnSlot::Flushing { link, stream: PeerStream::Ended { by } })
            }
            open @ ConnSlot::Flushing { stream: PeerStream::Open, .. } => (false, open),
            ConnSlot::Closing { mut by } => {
                end_reader(&mut by);
                (false, ConnSlot::Closing { by })
            }
            ConnSlot::Draining { mut by } => {
                end_reader(&mut by);
                (false, ConnSlot::Draining { by })
            }
        };
        self.slot = slot;
        // Canary: the mark moves a disarmed receive out of `Active` with no
        // `RecvArm`.
        owed && !cfg!(inf_canary_mark_keeps_disarmed)
    }

    /// A `Peer` run-out at the pump's empty read, `Flushing` or `Draining`
    /// (ADR-0196 D2, A1.1): once a take of the inbox has completed since
    /// `taken_at`, every entry reaped before the slot entered its `Peer`
    /// state has been parsed, so the parser ends. `None` for a slot that
    /// holds no `Peer` parser.
    pub(super) fn run_out(&mut self, inbox_takes: u64) -> Option<RunOut> {
        let by = match &mut self.slot {
            ConnSlot::Flushing { stream: PeerStream::Ended { by }, .. }
            | ConnSlot::Draining { by } => by,
            ConnSlot::Active { .. }
            | ConnSlot::Flushing { stream: PeerStream::Open, .. }
            | ConnSlot::Closing { .. } => return None,
        };
        let ClosedBy::Peer { taken_at, .. } = by else { return None };
        // Canary: the run-out's test reads false, so the parser never ends.
        if inbox_takes == *taken_at || cfg!(inf_canary_draining_reader_never_ends) {
            return Some(RunOut::Wait);
        }
        end_reader(by);
        Some(RunOut::Ended)
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
    /// pump) is made in the step that installs it; `None` when every slot
    /// below the cap is live — the admission bound, so no client can drive a cell to the
    /// slab's 2^24-th slot, a release assert and so a client-driven node kill. Then `make`
    /// is not called.
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

    /// The slab refuses the accept past its cap instead of asserting, and `make` runs only
    /// for a slot it grants, with that slot's key. A tiny cap stands in for `CONN_SLOT_CAP`;
    /// the arithmetic is the same.
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
