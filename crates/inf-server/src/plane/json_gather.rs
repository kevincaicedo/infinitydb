//! `JSON.MGET` across cells (ADR-0173 D4, A1): one leg per owning cell, the
//! command itself over that cell's keys, at most
//! [`JSON_GATHER_LEGS_IN_FLIGHT_MAX`] legs of one gather in flight, and one
//! account of the whole reply on the coordinating cell.
//!
//! The local leg runs first, at dispatch. The remote legs are sent in leg
//! order; with the window full the gather waits for its oldest leg before
//! it sends the next (pacing, the only wait inside dispatch). Once every leg
//! is sent the gather is one pending reply, and the legs still out are
//! settled at the reply's turn. Each leg is settled once, in leg order,
//! whatever order the replies arrived in: its outcome is read by type, a
//! `Success` reply decoded once through the plane's leg rule
//! ([`read_leg`]), and its bytes admitted with the store's [`ReplyLimit`]
//! predicate on the header and every element so far.
//!
//! | Leg outcome | Verdict |
//! |---|---|
//! | `Success`, decoded, the sum admitted | kept; its elements' ends written |
//! | `Success`, decoded, the sum over the budget | `ERR reply too large`, counted on this cell |
//! | `Error` | that reply is the whole reply, moved whole |
//! | `Success` its decoder refuses | `ERR cross-cell program reply malformed` |
//! | not a reply (`Err(code)`, a typed value) | `ERR cross-cell execution failed` |
//!
//! After a verdict no leg is sent, no outcome is decoded and no element is
//! served; the legs in flight are awaited and dropped before the reply is
//! emitted, so the connection's window count returns to what it was. Across
//! a suspension the gather keeps owned state only: its plan, its reply
//! (an `OpenReply` with its header written), the kept legs with their
//! element ends, which are offsets into buffers it owns alone, the account,
//! and its legs' waiters; never a store borrow, a writer, a slot or a mark.

use inf_foundation::bounded::{CappedDeque, Reserve, Reserved};
use inf_store::ReplyLimit;

use super::scatter::{ns_run_local, read_leg, scatter_send, send_whole};
use super::split::{GatherLeg, GatherPlan, OwnerSplit, split_by_owner};
use super::*;
use crate::json::{REPLY_TOO_LARGE, fold_reply_refusals};
use crate::limits::{
    JSON_GATHER_LEGS, JSON_GATHER_LEGS_IN_FLIGHT_MAX, JSON_LEG_FRAME_HEADER_BYTES_MAX,
};

// The window is a share of the connection's: one gather's legs in flight
// never fill it alone.
const _: () = assert!(
    JSON_GATHER_LEGS_IN_FLIGHT_MAX as usize <= REMOTE_WINDOW,
    "a gather's window fits the connection's"
);
// A leg is a subset of the client's argv, so the codec's argument cap holds
// every leg: the client parser's cap is not wider than the codec's.
const _: () = assert!(
    inf_wire::ParserLimits::for_bulk_cap(0).max_args <= MAX_APPLY_ARGS,
    "a leg's argv fits the codec's argument cap"
);

/// Where a gather's legs run: the client's origin, protocol and id, and the
/// keyspace its connection addresses.
#[derive(Copy, Clone)]
pub(super) struct GatherTarget {
    pub(super) origin: ExecOrigin,
    pub(super) proto: Protocol,
    pub(super) id: u64,
    pub(super) db: u16,
    pub(super) scope: ScatterScope,
}

/// The waiters of a gather's remote legs in flight, oldest first.
pub(super) type Flight = CappedDeque<GateWait<u64, OwnedOutcome>, Reserve>;

/// A `JSON.MGET` gather pending its turn in the pipeline.
pub(super) struct JsonGather {
    proto: Protocol,
    phase: Phase,
}

enum Phase {
    /// Every leg settled so far was admitted.
    Open(Open),
    /// A verdict fell: the reply is decided, and the legs still in flight
    /// are awaited and dropped before it is emitted.
    Draining { reply: OwnedReply, flight: Flight },
}

/// The gather while every leg settled so far was admitted.
pub(super) struct Open {
    plan: GatherPlan,
    /// The reply, its `*N` header written: the refusal lines are written
    /// into it after a `clear()`, and the served reply reuses its buffer.
    reply: OpenReply,
    /// H, the header's bytes.
    header_bytes: usize,
    /// E, the admitted elements' bytes.
    element_bytes: usize,
    /// Per entry of the split's key order, the end of that key's element frame
    /// in its leg's reply.
    ends: Vec<usize>,
    /// The admitted legs, in leg order.
    kept: Vec<JsonMgetLeg>,
    /// Legs settled so far: the next leg to settle is `legs()[settled]`.
    settled: usize,
    flight: Flight,
    limit: ReplyLimit,
}

/// A leg its reader accepted: the reply and where its elements begin. Only
/// [`read_json_mget_leg`] builds one.
#[derive(Debug)]
pub(super) struct JsonMgetLeg {
    reply: OwnedReply,
    body_at: usize,
}

/// The verdict that ended a gather before it was served.
enum End {
    /// The sum crossed the account at this many bytes.
    Refused { refused_bytes: usize },
    /// A leg's own reply: its `Error`, or the malformed line its reader wrote.
    Leg(OwnedReply),
    /// A leg whose outcome is not a reply, or a FIFO that could not be built.
    Failed,
}

/// `JSON.MGET` with a remote key, from either dispatch site: one owner gets
/// the whole argv; two or more are a gather (ADR-0173 D4).
pub(super) async fn json_gather<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    target: GatherTarget,
    meta: &'static inf_wire::CommandMeta,
    argv: &[&[u8]],
    inflight: &mut usize,
) -> PendingReply {
    let plan = match split_by_owner(&shared.router, shared.cell, meta, argv) {
        OwnerSplit::One(owner) if owner == shared.cell => {
            return PendingReply::Done(local_leg(shared, target, argv).await);
        }
        OwnerSplit::One(owner) => {
            let GatherTarget { proto, db, scope, .. } = target;
            return send_whole(shared, owner, proto, db, scope, argv, inflight).await;
        }
        OwnerSplit::Many(plan) => plan,
    };
    let open = match Open::new(shared, target, plan) {
        Ok(open) => open,
        Err(failed) => return PendingReply::Done(failed),
    };
    let gather = open.send_legs(shared, target, argv, inflight).await;
    match gather.phase {
        Phase::Draining { reply, flight } if flight.is_empty() => PendingReply::Done(reply),
        phase @ (Phase::Open(_) | Phase::Draining { .. }) => {
            PendingReply::JsonGather(Box::new(JsonGather { proto: gather.proto, phase }))
        }
    }
}

/// The leg on this cell: the namespace's own execution, the tiered arm for
/// a tiered namespace, which may suspend.
async fn local_leg<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    target: GatherTarget,
    leg: &[&[u8]],
) -> OwnedReply {
    let GatherTarget { origin, proto, id, db, scope } = target;
    match scope {
        ScatterScope::Db => run_local(shared, origin, proto, id, db, leg),
        ScatterScope::Ns(ns) => ns_run_local(shared, origin, proto, id, db, ns, leg).await,
    }
}

impl Open {
    /// The account read once from the addressed store, the reply opened with
    /// its header, the FIFO built: a FIFO that cannot be built answers the
    /// cross-cell failure line, with no leg run or sent. A namespace's
    /// account is read without making its store: a tiered namespace keeps
    /// none.
    fn new<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        shared: &Shared<O, F>,
        target: GatherTarget,
        plan: GatherPlan,
    ) -> Result<Open, OwnedReply> {
        let limit = match target.scope {
            ScatterScope::Db => {
                let mut store = shared.store.borrow_mut();
                Some(store.db_mut(usize::from(target.db)).doc_reply_limit())
            }
            ScatterScope::Ns(ns) => shared.store.borrow().ns_doc_reply_limit(ns),
        };
        match limit {
            Some(limit) => Open::with_limit(shared, target.proto, plan, limit),
            None => Err(error_reply(shared, target.proto, CROSS_CELL_EXECUTION_FAILED)),
        }
    }

    /// [`Open::new`] past the account's read.
    fn with_limit<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        shared: &Shared<O, F>,
        proto: Protocol,
        plan: GatherPlan,
        limit: ReplyLimit,
    ) -> Result<Open, OwnedReply> {
        let Ok(flight) = Flight::new(JSON_GATHER_LEGS, &shared.census) else {
            return Err(error_reply(shared, proto, CROSS_CELL_EXECUTION_FAILED));
        };
        let mut reply = OpenReply::new(shared.take_reply_buf());
        RespWriter::new(reply.reply(), proto).array_header(plan.key_count());
        let header_bytes = reply.bytes().len();
        let ends = vec![0; plan.key_count()];
        let kept = Vec::with_capacity(plan.legs().len());
        Ok(Open {
            plan,
            reply,
            header_bytes,
            element_bytes: 0,
            ends,
            kept,
            settled: 0,
            flight,
            limit,
        })
    }

    /// Sends every leg in leg order: the local leg run and settled first,
    /// each remote leg sent once the window has room. A verdict stops the
    /// sends; legs already in flight drain at the reply's turn.
    async fn send_legs<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        mut self,
        shared: &Rc<Shared<O, F>>,
        target: GatherTarget,
        argv: &[&[u8]],
        inflight: &mut usize,
    ) -> JsonGather {
        let proto = target.proto;
        let mut leg_argv: Vec<&[u8]> = Vec::with_capacity(argv.len());
        for index in 0..self.plan.legs().len() {
            let leg: GatherLeg = self.plan.legs()[index].clone();
            leg_argv.clear();
            leg_argv.push(argv[0]);
            leg_argv.extend(self.plan.positions(&leg).iter().map(|&position| argv[position]));
            leg_argv.push(argv[argv.len() - 1]);
            if leg.owner == shared.cell {
                let reply = local_leg(shared, target, &leg_argv).await;
                if let Err(end) = self.settle(shared, proto, OwnedOutcome::Reply(reply)) {
                    return JsonGather::draining(proto, self.end(shared, proto, end));
                }
                continue;
            }
            if let Err(end) = self.pace(shared, proto, inflight).await {
                return JsonGather::draining(proto, self.end(shared, proto, end));
            }
            let GatherTarget { db, scope, .. } = target;
            let waiter = match scatter_send(shared, leg.owner, proto, db, scope, &leg_argv).await {
                Ok(waiter) => waiter,
                Err(refusal) => {
                    return JsonGather::draining(proto, self.end(shared, proto, End::Leg(refusal)));
                }
            };
            match self.flight.reserve() {
                Reserved::Slot(slot) => {
                    slot.publish(waiter);
                    *inflight += 1;
                }
                // The gather alone fills its FIFO and `pace` made room, so
                // this is unreachable; the waiter drops, leaving the gate.
                Reserved::Full => {
                    drop(waiter);
                    return JsonGather::draining(proto, self.end(shared, proto, End::Failed));
                }
            }
        }
        JsonGather { proto, phase: Phase::Open(self) }
    }

    /// Room for one more leg in flight: with the window full, the oldest leg
    /// is awaited and settled (the census row counts the crossing). The slot
    /// a reservation lends is dropped at once, before any await.
    async fn pace<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        shared: &Shared<O, F>,
        proto: Protocol,
        inflight: &mut usize,
    ) -> Result<(), End> {
        loop {
            match self.flight.reserve() {
                Reserved::Slot(_) => return Ok(()),
                Reserved::Full => {}
            }
            let Some(oldest) = self.flight.pop_front() else { return Ok(()) };
            let outcome = oldest.await;
            *inflight -= 1;
            self.settle(shared, proto, outcome)?;
        }
    }

    /// Settles the next leg in leg order from its outcome: the one place a
    /// leg's outcome is read. `Ok` keeps it; `Err` is the verdict.
    fn settle<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        shared: &Shared<O, F>,
        proto: Protocol,
        outcome: OwnedOutcome,
    ) -> Result<(), End> {
        let Some(leg) = self.plan.legs().get(self.settled).cloned() else {
            debug_assert!(false, "a leg settles once");
            return Err(End::Failed);
        };
        self.settled += 1;
        let reply = match outcome {
            OwnedOutcome::Reply(reply) => reply,
            OwnedOutcome::Ok
            | OwnedOutcome::Int(_)
            | OwnedOutcome::Nil
            | OwnedOutcome::Bool(_)
            | OwnedOutcome::Err(_) => return Err(End::Failed),
        };
        let ends = &mut self.ends[leg.span.clone()];
        let kept = read_json_mget_leg(shared, proto, reply, ends).map_err(End::Leg)?;
        let body = kept.reply.bytes().len() - kept.body_at;
        let sum =
            self.header_bytes.checked_add(self.element_bytes).and_then(|s| s.checked_add(body));
        let admitted = match sum {
            Some(_) if cfg!(inf_canary_json_gather_unaccounted) => true,
            Some(sum) => self.limit.admits(sum).is_ok(),
            None => false,
        };
        if !admitted {
            shared.recycle_reply_buf(kept.reply.into_bytes());
            let refused_bytes = sum.unwrap_or(usize::MAX);
            let counted = u64::try_from(refused_bytes).unwrap_or(u64::MAX);
            fold_reply_refusals(&shared.node, 1, counted);
            return Err(End::Refused { refused_bytes });
        }
        self.element_bytes += body;
        self.kept.push(kept);
        Ok(())
    }

    /// The verdict's reply: a refusal the gather writes is its reply's first
    /// frame, after a `clear()` (so the outcome is `Error`); a leg's reply is
    /// the reply, and the gather's buffer returns to the pool. Every kept leg
    /// is recycled; the legs in flight drain at the reply's turn.
    fn end<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        mut self,
        shared: &Shared<O, F>,
        proto: Protocol,
        end: End,
    ) -> (OwnedReply, Flight) {
        for leg in self.kept.drain(..) {
            shared.recycle_reply_buf(leg.reply.into_bytes());
        }
        let line = match end {
            End::Refused { refused_bytes } => {
                debug_assert!(self.limit.admits(refused_bytes).is_err(), "the sum crossed");
                REPLY_TOO_LARGE
            }
            End::Failed => CROSS_CELL_EXECUTION_FAILED,
            End::Leg(reply) => {
                shared.recycle_reply_buf(self.reply.finish().into_bytes());
                return (reply, self.flight);
            }
        };
        self.reply.clear();
        RespWriter::new(self.reply.reply(), proto).error(line);
        (self.reply.finish(), self.flight)
    }

    /// Every leg admitted: the elements copied into argv order behind the
    /// header, into a buffer sized once to the account's sum, H + E.
    fn assemble<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        self,
        shared: &Shared<O, F>,
        proto: Protocol,
    ) -> OwnedReply {
        let Open { plan, reply, header_bytes, element_bytes, ends, kept, .. } = self;
        let mut buf = reply.finish().into_bytes();
        buf.clear();
        buf.reserve_exact(header_bytes + element_bytes);
        let mut reply = OpenReply::new(buf);
        RespWriter::new(reply.reply(), proto).array_header(plan.key_count());
        // Per leg, where its next element begins and its next entry of `ends`.
        let mut cursors: Vec<(usize, usize)> = plan
            .legs()
            .iter()
            .zip(&kept)
            .map(|(leg, kept)| (kept.body_at, leg.span.start))
            .collect();
        for &leg_index in plan.leg_of_position() {
            let (begin, entry) = cursors[leg_index];
            let end = ends[entry];
            RespWriter::new(reply.reply(), proto)
                .encoded(&kept[leg_index].reply.bytes()[begin..end]);
            cursors[leg_index] = (end, entry + 1);
        }
        debug_assert_eq!(
            reply.bytes().len(),
            header_bytes + element_bytes,
            "the reply's length is the account's sum"
        );
        for leg in kept {
            shared.recycle_reply_buf(leg.reply.into_bytes());
        }
        reply.finish()
    }
}

impl JsonGather {
    fn draining(proto: Protocol, (reply, flight): (OwnedReply, Flight)) -> JsonGather {
        JsonGather { proto, phase: Phase::Draining { reply, flight } }
    }

    /// The reply at its turn: the legs still in flight settled in leg order
    /// (or, after a verdict, drained), then the served array or the verdict.
    /// Every leg sent is awaited once, so `inflight` is back to its value
    /// before the gather.
    pub(super) async fn finish<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        self,
        shared: &Shared<O, F>,
        inflight: &mut usize,
    ) -> OwnedReply {
        let proto = self.proto;
        let (reply, mut flight) = match self.phase {
            Phase::Open(mut open) => loop {
                let Some(next) = open.flight.pop_front() else {
                    return open.assemble(shared, proto);
                };
                let outcome = next.await;
                *inflight -= 1;
                if let Err(end) = open.settle(shared, proto, outcome) {
                    break open.end(shared, proto, end);
                }
            },
            Phase::Draining { reply, flight } => (reply, flight),
        };
        while let Some(waiter) = flight.pop_front() {
            if let OwnedOutcome::Reply(dropped) = waiter.await {
                shared.recycle_reply_buf(dropped.into_bytes());
            }
            *inflight -= 1;
        }
        debug_assert!(flight.is_empty(), "every leg sent was awaited");
        reply
    }
}

/// A `JSON.MGET` leg's answers through the plane's one leg rule
/// ([`read_leg`]): an `Error` leg is the reply, a `Success` leg its decoder
/// refuses answers the malformed line, a decoded one is the leg, its
/// elements' ends in `ends`.
fn read_json_mget_leg<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    proto: Protocol,
    reply: OwnedReply,
    ends: &mut [usize],
) -> Result<JsonMgetLeg, OwnedReply> {
    let (reply, body_at) =
        read_leg(shared, proto, reply, |view| parse_json_mget_leg(view, proto, ends))?;
    Ok(JsonMgetLeg { reply, body_at })
}

/// A `JSON.MGET` leg's `Success` reply → where its elements begin, each
/// element's end written into `ends`: `*n\r\n` with `n` = `ends.len()`, then
/// exactly `n` frames to the reply's end, each a bulk `$<len>\r\n<len
/// bytes>\r\n` or the leg protocol's null (`$-1\r\n` under RESP2, `_\r\n`
/// under RESP3). A header's digits are ASCII digits only, its CRLF within
/// [`JSON_LEG_FRAME_HEADER_BYTES_MAX`]. Anything else is `None`, a malformed
/// success its reader answers. A value decoder over a `SuccessView`: its
/// caller read the leg's outcome by type (ADR-0190 A1.2), never over raw
/// bytes:
///
/// ```compile_fail,E0308
/// let _ = inf_server::parse_json_mget_leg(b"*0\r\n", inf_wire::Protocol::Resp2, &mut []);
/// ```
#[doc(hidden)]
#[cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]
pub fn parse_json_mget_leg(
    reply: SuccessView<'_>,
    proto: Protocol,
    ends: &mut [usize],
) -> Option<usize> {
    let raw = reply.bytes();
    let null: &[u8] = match proto {
        Protocol::Resp2 => b"$-1\r\n",
        Protocol::Resp3 => b"_\r\n",
    };
    // A header after its type byte: its decimal and the header's length,
    // the type byte and CRLF included.
    let header = |rest: &[u8]| -> Option<(usize, usize)> {
        let window = rest.get(..rest.len().min(JSON_LEG_FRAME_HEADER_BYTES_MAX.checked_sub(1)?))?;
        let digits = window.windows(2).position(|pair| pair == b"\r\n")?;
        let text = rest.get(..digits)?;
        if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let value: usize = core::str::from_utf8(text).ok()?.parse().ok()?;
        Some((value, digits.checked_add(3)?))
    };
    // reply-outcome-allow: a value decoder over a SuccessView, whose outcome its caller read
    let (count, body_at) = header(raw.strip_prefix(b"*")?)?;
    if count != ends.len() {
        return None;
    }
    let mut at = body_at;
    for end in ends.iter_mut() {
        let frame = raw.get(at..)?;
        let frame_bytes = if frame.starts_with(null) {
            null.len()
        } else {
            // reply-outcome-allow: a value decoder over a SuccessView, its outcome read before
            let (payload, head) = header(frame.strip_prefix(b"$")?)?;
            let close = head.checked_add(payload)?;
            if frame.get(close..close.checked_add(2)?)? != b"\r\n" {
                return None;
            }
            close.checked_add(2)?
        };
        at = at.checked_add(frame_bytes)?;
        *end = at;
    }
    (at == raw.len()).then_some(body_at)
}

/// The fold's own handles for the arrival-order property and its canary
/// (`reply_outcome_tests`): a gather whose remote legs wait on a gate of the
/// test's own, and the same fold fed outcomes in an order the test gives.
#[cfg(test)]
pub(super) mod test_support {
    use super::*;

    /// A gather over `plan` under `limit` whose remote legs are in flight
    /// on `waiters`, in leg order: what `send_legs` leaves when no leg is
    /// local.
    pub(in crate::plane) fn in_flight<
        O: PlaneObserver + 'static,
        F: SegmentFs + Clone + 'static,
    >(
        shared: &Shared<O, F>,
        proto: Protocol,
        plan: GatherPlan,
        limit: ReplyLimit,
        waiters: Vec<GateWait<u64, OwnedOutcome>>,
    ) -> JsonGather {
        let mut open =
            Open::with_limit(shared, proto, plan, limit).expect("the row fits the census");
        for waiter in waiters {
            match open.flight.reserve() {
                Reserved::Slot(slot) => slot.publish(waiter),
                Reserved::Full => panic!("more legs than the window"),
            }
        }
        JsonGather { proto, phase: Phase::Open(open) }
    }

    /// The canary's fold: `outcomes` settled in the order given, each taken
    /// for the next leg's, as a fold that settles in completion order would.
    pub(in crate::plane) fn settled_as_given<
        O: PlaneObserver + 'static,
        F: SegmentFs + Clone + 'static,
    >(
        shared: &Shared<O, F>,
        proto: Protocol,
        plan: GatherPlan,
        limit: ReplyLimit,
        outcomes: Vec<OwnedOutcome>,
    ) -> OwnedReply {
        let mut open =
            Open::with_limit(shared, proto, plan, limit).expect("the row fits the census");
        for outcome in outcomes {
            if let Err(end) = open.settle(shared, proto, outcome) {
                return open.end(shared, proto, end).0;
            }
        }
        open.assemble(shared, proto)
    }
}

/// The leg decoder's accept and refuse rows: each protocol reads its own
/// null and refuses the other's, and a count other than the leg's keys,
/// trailing bytes, a negative length other than the null's, a frame header
/// past its bound, a length past the integer and an element that is not a
/// bulk are each a malformed leg.
#[cfg(test)]
mod leg_decoder_rows {
    use super::*;

    const PROTOCOLS: [Protocol; 2] = [Protocol::Resp2, Protocol::Resp3];

    /// `raw` as a `Success` leg of `keys` keys under `proto`: where its body
    /// begins and each element's end, or `None` for a malformed leg.
    fn decode(raw: &[u8], proto: Protocol, keys: usize) -> Option<(usize, Vec<usize>)> {
        let reply =
            OwnedReply::received(raw.to_vec(), ReplyOutcome::Success).expect("a non-empty reply");
        let mut ends = vec![0; keys];
        let view = reply.success().expect("a Success reply has a view");
        let body_at = parse_json_mget_leg(view, proto, &mut ends)?;
        Some((body_at, ends))
    }

    #[test]
    fn each_protocol_reads_its_own_null_and_refuses_the_other() {
        let resp2 = b"*2\r\n$-1\r\n$1\r\na\r\n";
        let resp3 = b"*2\r\n_\r\n$1\r\na\r\n";
        assert_eq!(decode(resp2, Protocol::Resp2, 2), Some((4, vec![9, 16])));
        assert_eq!(decode(resp3, Protocol::Resp3, 2), Some((4, vec![7, 14])));
        assert_eq!(decode(resp3, Protocol::Resp2, 2), None, "RESP3's null in a RESP2 leg");
        assert_eq!(decode(resp2, Protocol::Resp3, 2), None, "RESP2's null in a RESP3 leg");
    }

    #[test]
    fn a_count_other_than_the_legs_keys_is_malformed() {
        let two = b"*2\r\n$1\r\na\r\n$1\r\nb\r\n";
        for proto in PROTOCOLS {
            assert_eq!(decode(two, proto, 2), Some((4, vec![11, 18])));
            assert_eq!(decode(two, proto, 1), None, "a count above the leg's keys");
            assert_eq!(decode(two, proto, 3), None, "a count below the leg's keys");
            assert_eq!(decode(b"*2\r\n$1\r\na\r\n", proto, 2), None, "fewer frames than its count");
            // Frames that tile the leg's two keys exactly, under a header whose count lies: the
            // count's own check is the one rule that refuses these.
            assert_eq!(decode(b"*3\r\n$1\r\na\r\n$1\r\nb\r\n", proto, 2), None, "a count of 3");
            assert_eq!(decode(b"*1\r\n$1\r\na\r\n$1\r\nb\r\n", proto, 2), None, "a count of 1");
        }
    }

    /// A length past the integer and an element that is not a bulk are each malformed: a count's
    /// and a bulk's 2^64, and an integer frame where a bulk or the null stands.
    #[test]
    fn a_length_past_the_integer_or_a_non_bulk_element_is_malformed() {
        let rows: [(&[u8], usize); 4] = [
            (b"*18446744073709551616\r\n$1\r\na\r\n", 1),
            (b"*1\r\n$18446744073709551616\r\na\r\n", 1),
            (b"*1\r\n:1\r\n", 1),
            (b"*2\r\n$1\r\na\r\n:1\r\n", 2),
        ];
        for proto in PROTOCOLS {
            for (raw, keys) in rows {
                let shown = String::from_utf8_lossy(raw);
                assert_eq!(decode(raw, proto, keys), None, "{proto:?} {shown:?}");
            }
        }
    }

    #[test]
    fn trailing_bytes_are_malformed() {
        for proto in PROTOCOLS {
            assert_eq!(decode(b"*1\r\n$1\r\na\r\n", proto, 1), Some((4, vec![11])));
            assert_eq!(decode(b"*1\r\n$1\r\na\r\nX", proto, 1), None, "a byte past the frames");
            assert_eq!(decode(b"*1\r\n$1\r\na\r\n$1\r\nb\r\n", proto, 1), None, "a frame past");
            assert_eq!(decode(b"*1\r\n$1\r\nab\r\n", proto, 1), None, "a payload past its length");
        }
    }

    #[test]
    fn a_negative_length_other_than_the_null_is_malformed() {
        for proto in PROTOCOLS {
            for frame in ["$-2\r\n", "$-0\r\n", "$-9223372036854775808\r\n"] {
                let raw = format!("*1\r\n{frame}");
                assert_eq!(decode(raw.as_bytes(), proto, 1), None, "{proto:?} {frame:?}");
            }
        }
    }

    /// A header is its type byte, its digits and CRLF. A one-byte payload
    /// whose length is written with leading zeros reaches the bound with a
    /// payload that exists, so the header's own length alone decides.
    #[test]
    fn a_frame_header_past_its_bound_is_malformed() {
        let bound = JSON_LEG_FRAME_HEADER_BYTES_MAX;
        for header_bytes in [bound - 1, bound, bound + 1] {
            let digits = header_bytes - 3;
            let element = format!("*1\r\n${:0digits$}\r\na\r\n", 1);
            let count = format!("*{:0digits$}\r\n$1\r\na\r\n", 1);
            let (want_element, want_count) = if header_bytes <= bound {
                (Some((4, vec![element.len()])), Some((header_bytes, vec![count.len()])))
            } else {
                (None, None)
            };
            for proto in PROTOCOLS {
                assert_eq!(decode(element.as_bytes(), proto, 1), want_element, "{header_bytes} B");
                assert_eq!(decode(count.as_bytes(), proto, 1), want_count, "{header_bytes} B");
            }
        }
    }
}
