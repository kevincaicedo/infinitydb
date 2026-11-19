//! Command dispatch: the per-connection pump (pipeline order in, reply
//! order out, a remote op in flight per gate unit of its task), the local
//! fast path,
//! and `dispatch_one` — the command-family switch every remote command
//! enters through.

use core::convert::Infallible;

#[cfg(feature = "doc")]
use super::json_gather::{GatherTarget, JsonGather, json_gather};
use super::scatter::send_whole;
use super::*;

/// A reply slot awaiting its in-order turn on the wire. Each remote leg it
/// waits on holds one of its connection task's gate units (ADR-0177 D4).
pub(super) enum PendingReply<'u> {
    /// Executed (locally or refused) at dispatch; the reply waits its turn.
    Done(OwnedReply),
    /// One remote `Apply` in flight; the owner's raw RESP reply parks in
    /// the gate if it lands before its turn.
    Remote { waiter: GateWait<'u, OwnedOutcome>, proto: Protocol },
    /// A tagged remote `PUBLISH` (ADR-0101 D4): the count reply, then the
    /// publisher's own frames stashed under `seq` by the tagged fan leg
    /// — Redis's reply-then-push order across a remote owner.
    Publish { waiter: GateWait<'u, OwnedOutcome>, proto: Protocol, seq: u64 },
    /// Split DEL/UNLINK/EXISTS/TOUCH (and scattered DBSIZE): locally-counted
    /// contributions and the remote per-key ones. A leg that answered a
    /// typed error instead of a count, or a leg that could not be sent,
    /// makes the reply that error — never a partial sum.
    Counted(CountedLegs<'u>),
    /// Split `MGET`: per-key replies reassemble into one array in argv
    /// order.
    Gather(GatherLegs<'u>),
    /// A cross-cell `JSON.MGET` (ADR-0173 D4): its legs in flight under one
    /// account of the whole reply, settled at the reply's turn.
    #[cfg(feature = "doc")]
    JsonGather(Box<JsonGather<'u>>),
    /// Fanned MSET / scattered FLUSH: all legs must come back `+OK` (the
    /// first error leg wins the reply otherwise). A leg that could not be
    /// sent makes the reply that refusal — never a silent `+OK` over a
    /// dropped cell; already-sent legs still settle so nothing orphans in
    /// flight.
    AllOk(AllOkLegs<'u>),
    /// `always` durable write (M2-S08): the reply bytes are staged but the
    /// slot resolves only once the fsync watermark covers the record's
    /// durable seq (§8.2 — ack after fsync; FIFO per connection holds).
    Durable { waiter: inf_runtime::WatermarkWait, reply: OwnedReply },
}

/// A fan's legs out, oldest first (ADR-0177 D4): each holds one of its
/// task's units, so settling the oldest gives one back.
pub(super) trait LegWindow {
    /// What ends the fan at a settle, before its last leg is sent:
    /// `Infallible` where every outcome waits for its reply's turn.
    type Stop;

    /// Settles the oldest leg out, its outcome kept or folded; `Ok(false)`
    /// when no leg is out.
    async fn settle_oldest<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        shared: &Shared<O, F>,
    ) -> Result<bool, Self::Stop>;
}

/// The one windowed send (ADR-0177 D4): `send` runs, and while the task has
/// every unit out the window's oldest leg settles and `send` runs again, so
/// a fan wider than its task's units runs in windows and at most its units
/// of legs are ever out. What is left is the send's own answer: a waiter,
/// the codec's refusal, or `NoUnit` with no leg of the window out, derived
/// never (a pump dispatches a command only while a unit is free, and a leg
/// settled gives its unit back). Each caller answers a refusal by its own
/// type; `Err` is a settle that ended the fan.
pub(super) async fn send_windowed<W, T, O, F>(
    window: &mut W,
    shared: &Shared<O, F>,
    mut send: impl AsyncFnMut(&mut W) -> Result<T, SendRefused>,
) -> Result<Result<T, SendRefused>, W::Stop>
where
    W: LegWindow,
    O: PlaneObserver + 'static,
    F: SegmentFs + Clone + 'static,
{
    loop {
        match send(window).await {
            Err(SendRefused::NoUnit) => {
                if !window.settle_oldest(shared).await? {
                    return Ok(Err(SendRefused::NoUnit));
                }
            }
            sent => return Ok(sent),
        }
    }
}

/// A fan's legs in flight, oldest first (ADR-0177 D4), whose outcomes
/// nothing reads: settling one drops its outcome.
pub(super) struct Legs<'u> {
    waiters: Vec<GateWait<'u, OwnedOutcome>>,
}

impl<'u> Legs<'u> {
    pub(super) fn new() -> Legs<'u> {
        Legs { waiters: Vec::new() }
    }

    pub(super) fn push(&mut self, waiter: GateWait<'u, OwnedOutcome>) {
        self.waiters.push(waiter);
    }

    /// The oldest leg's outcome, awaited; `None` when no leg is out.
    pub(super) async fn oldest(&mut self) -> Option<OwnedOutcome> {
        if self.waiters.is_empty() {
            return None;
        }
        Some(self.waiters.remove(0).await)
    }
}

impl LegWindow for Legs<'_> {
    type Stop = Infallible;

    async fn settle_oldest<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        _shared: &Shared<O, F>,
    ) -> Result<bool, Infallible> {
        Ok(self.oldest().await.is_some())
    }
}

/// A program's legs whose outcomes it reads in leg order: the oldest leg
/// out is settled first when the task needs its unit, its outcome kept for
/// its turn (ADR-0177 D4).
pub(super) struct OrderedLegs<'u> {
    legs: Vec<OrderedLeg<'u>>,
    /// Every leg before this one is settled or read.
    settled: usize,
    /// Every leg before this one is read.
    read: usize,
}

enum OrderedLeg<'u> {
    Out(GateWait<'u, OwnedOutcome>),
    In(OwnedOutcome),
    Read,
}

impl<'u> OrderedLegs<'u> {
    pub(super) fn new() -> OrderedLegs<'u> {
        OrderedLegs { legs: Vec::new(), settled: 0, read: 0 }
    }

    pub(super) fn push(&mut self, waiter: GateWait<'u, OwnedOutcome>) {
        self.legs.push(OrderedLeg::Out(waiter));
    }

    /// The next leg's outcome in leg order; `None` past the last.
    pub(super) async fn next_outcome(&mut self) -> Option<OwnedOutcome> {
        let leg = self.legs.get_mut(self.read)?;
        self.read += 1;
        self.settled = self.settled.max(self.read);
        match core::mem::replace(leg, OrderedLeg::Read) {
            OrderedLeg::Out(waiter) => Some(waiter.await),
            OrderedLeg::In(outcome) => Some(outcome),
            OrderedLeg::Read => None,
        }
    }
}

impl LegWindow for OrderedLegs<'_> {
    type Stop = Infallible;

    /// Settles the oldest leg out, keeping its outcome for its turn.
    async fn settle_oldest<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        _shared: &Shared<O, F>,
    ) -> Result<bool, Infallible> {
        while let Some(leg) = self.legs.get_mut(self.settled) {
            self.settled += 1;
            let OrderedLeg::Out(waiter) = leg else { continue };
            *leg = OrderedLeg::In(waiter.await);
            return Ok(true);
        }
        Ok(false)
    }
}

/// A counted fan: the fold of the legs settled and the legs still out.
pub(super) struct CountedLegs<'u> {
    legs: Legs<'u>,
    fold: CountedFold,
    proto: Protocol,
}

impl<'u> CountedLegs<'u> {
    /// A fan whose local contribution is `acc`.
    pub(super) fn new(acc: i64, proto: Protocol) -> CountedLegs<'u> {
        CountedLegs { legs: Legs::new(), fold: CountedFold::new(acc, None), proto }
    }

    pub(super) fn push(&mut self, waiter: GateWait<'u, OwnedOutcome>) {
        self.legs.push(waiter);
    }

    /// Adds local contributions counted at dispatch.
    pub(super) fn add(&mut self, local: i64) {
        self.fold.add(local);
    }

    /// A leg that could not be sent: it is the reply, over any leg's error.
    pub(super) fn refuse<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        shared: &Shared<O, F>,
        refusal: OwnedReply,
    ) {
        if let Some(unused) = self.fold.refuse(refusal) {
            shared.recycle_reply_buf(unused.into_bytes());
        }
    }

    /// Sends one leg to `to`, the fan's oldest settled first while the task
    /// has every unit out. A leg that cannot be sent is the reply: `false`,
    /// and the fan sends no more.
    #[allow(clippy::too_many_arguments)] // the send funnel's one context
    pub(super) async fn send<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        shared: &'u Rc<Shared<O, F>>,
        units: &'u GateUnits,
        to: CellId,
        origin: ApplyOrigin,
        db: u16,
        argv: &[&[u8]],
    ) -> bool {
        let proto = self.proto;
        let Ok(sent) = send_windowed(self, shared, async |_| {
            send_apply(shared, units, to, origin, proto, db, argv).await
        })
        .await;
        match sent {
            Ok(waiter) => {
                self.push(waiter);
                true
            }
            Err(refused) => {
                let reply = refused.into_reply(shared, proto);
                self.refuse(shared, reply);
                false
            }
        }
    }

    /// Every leg settled, so nothing is orphaned in flight; the fold decides
    /// the reply (a count, or the first error — a leg that is not a count
    /// is an error, never a partial sum: ADR-0093 A3′).
    async fn finish<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        mut self,
        shared: &Shared<O, F>,
    ) -> OwnedReply {
        while let Ok(true) = self.settle_oldest(shared).await {}
        match self.fold.finish() {
            Err(error) => error,
            Ok(total) => int_reply(shared, self.proto, total),
        }
    }
}

impl LegWindow for CountedLegs<'_> {
    type Stop = Infallible;

    /// Folds the oldest leg.
    async fn settle_oldest<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        shared: &Shared<O, F>,
    ) -> Result<bool, Infallible> {
        let Some(outcome) = self.legs.oldest().await else { return Ok(false) };
        if let Some(unused) = self.fold.leg(outcome, self.proto) {
            shared.recycle_reply_buf(unused.into_bytes());
        }
        Ok(true)
    }
}

/// An all-or-first-error fan: the first failure settled, and the legs out.
pub(super) struct AllOkLegs<'u> {
    legs: Legs<'u>,
    failure: Option<OwnedReply>,
    proto: Protocol,
}

impl<'u> AllOkLegs<'u> {
    pub(super) fn new(proto: Protocol) -> AllOkLegs<'u> {
        AllOkLegs { legs: Legs::new(), failure: None, proto }
    }

    pub(super) fn push(&mut self, waiter: GateWait<'u, OwnedOutcome>) {
        self.legs.push(waiter);
    }

    /// A leg that could not be sent: the fan is known-incomplete, so this
    /// refusal is the reply over any leg's error, and `+OK` would be a lie.
    pub(super) fn refuse<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        shared: &Shared<O, F>,
        refusal: OwnedReply,
    ) {
        if let Some(unused) = self.failure.replace(refusal) {
            shared.recycle_reply_buf(unused.into_bytes());
        }
    }

    /// Sends one leg to `to`, the fan's oldest settled first while the task
    /// has every unit out. A leg that cannot be sent is the reply: `false`,
    /// and the fan sends no more.
    #[allow(clippy::too_many_arguments)] // the send funnel's one context
    pub(super) async fn send<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        shared: &'u Rc<Shared<O, F>>,
        units: &'u GateUnits,
        to: CellId,
        origin: ApplyOrigin,
        db: u16,
        argv: &[&[u8]],
    ) -> bool {
        let proto = self.proto;
        let Ok(sent) = send_windowed(self, shared, async |_| {
            send_apply(shared, units, to, origin, proto, db, argv).await
        })
        .await;
        match sent {
            Ok(waiter) => {
                self.push(waiter);
                true
            }
            Err(refused) => {
                let reply = refused.into_reply(shared, proto);
                self.refuse(shared, reply);
                false
            }
        }
    }

    /// Every leg settled, so nothing is orphaned in flight; the reply is
    /// the first failure or `+OK`.
    async fn finish<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        mut self,
        shared: &Shared<O, F>,
    ) -> OwnedReply {
        while let Ok(true) = self.settle_oldest(shared).await {}
        match self.failure {
            Some(error) => error,
            None => simple_reply(shared, self.proto, "OK"),
        }
    }
}

impl LegWindow for AllOkLegs<'_> {
    type Stop = Infallible;

    /// Settles the oldest leg, keeping the first failure.
    async fn settle_oldest<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        shared: &Shared<O, F>,
    ) -> Result<bool, Infallible> {
        let Some(outcome) = self.legs.oldest().await else { return Ok(false) };
        // A leg failed by its outcome, never by its bytes: an error reply,
        // or a fabric refusal (ADR-0190 D2).
        let failed = match outcome {
            OwnedOutcome::Reply(reply) if reply.is_error() => Some(reply),
            OwnedOutcome::Reply(reply) => {
                shared.recycle_reply_buf(reply.into_bytes());
                None
            }
            refused @ OwnedOutcome::Err(_) => Some(render_outcome(shared, refused, self.proto)),
            OwnedOutcome::Ok | OwnedOutcome::Int(_) | OwnedOutcome::Nil | OwnedOutcome::Bool(_) => {
                None
            }
        };
        match (self.failure.is_none(), failed) {
            (true, failed) => self.failure = failed,
            (false, Some(unused)) => shared.recycle_reply_buf(unused.into_bytes()),
            (false, None) => {}
        }
        Ok(true)
    }
}

/// One `MGET` position: a local reply, a remote leg, or a remote leg
/// settled early into its element (`None`: nil).
pub(super) enum GatherPart<'u> {
    Done(OwnedReply),
    Wait(GateWait<'u, OwnedOutcome>),
    Settled(Option<OwnedReply>),
}

/// The `MGET` gather: every position in argv order, its oldest remote leg
/// settled first when the task needs the unit.
pub(super) struct GatherLegs<'u> {
    parts: Vec<GatherPart<'u>>,
    /// No position before this one waits.
    next: usize,
    proto: Protocol,
}

impl<'u> GatherLegs<'u> {
    pub(super) fn new(proto: Protocol) -> GatherLegs<'u> {
        GatherLegs { parts: Vec::new(), next: 0, proto }
    }

    pub(super) fn push(&mut self, part: GatherPart<'u>) {
        self.parts.push(part);
    }

    /// Every position in argv order, each element pasted after the array
    /// header.
    async fn finish<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        mut self,
        shared: &Shared<O, F>,
    ) -> OwnedReply {
        while let Ok(true) = self.settle_oldest(shared).await {}
        let mut reply = OpenReply::new(shared.take_reply_buf());
        RespWriter::new(reply.reply(), self.proto).array_header(self.parts.len());
        for part in self.parts {
            let element = match part {
                GatherPart::Done(element) => Some(element),
                GatherPart::Settled(element) => element,
                GatherPart::Wait(_) => None,
            };
            let mut w = RespWriter::new(reply.reply(), self.proto);
            match element {
                Some(element) => {
                    w.encoded(element.bytes());
                    shared.recycle_reply_buf(element.into_bytes());
                }
                None => w.null(),
            }
        }
        reply.finish()
    }
}

impl LegWindow for GatherLegs<'_> {
    type Stop = Infallible;

    /// Settles the oldest remote position into its element.
    async fn settle_oldest<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        _shared: &Shared<O, F>,
    ) -> Result<bool, Infallible> {
        while let Some(part) = self.parts.get_mut(self.next) {
            self.next += 1;
            let GatherPart::Wait(waiter) = part else { continue };
            let element = match waiter.await {
                OwnedOutcome::Reply(element) => Some(element),
                OwnedOutcome::Ok
                | OwnedOutcome::Int(_)
                | OwnedOutcome::Nil
                | OwnedOutcome::Bool(_)
                | OwnedOutcome::Err(_) => None,
            };
            *part = GatherPart::Settled(element);
            return Ok(true);
        }
        Ok(false)
    }
}

/// What the pump found when it asked the connection for more work.
pub(super) enum Popped {
    Cmd(OwnedCmd),
    /// A reply PARSE queued behind the replies owed before it: it takes
    /// its place in the window as one executed at dispatch.
    Reply(OwnedReply),
    /// Queue empty but replies are still pending — keep emitting.
    Empty,
    /// Queue empty and nothing pending: the pump parked itself inside the
    /// conn borrow, so a command queued after it, or a move of its slot,
    /// wakes it by id.
    Park,
    /// A `Peer` run-out found no take of the inbox since its stamp: the
    /// pump waits for PARSE's next take, then reads the slot again.
    TakeWait,
    /// A draining slot read empty with no parser: the local registries
    /// dropped the connection in this step, and these are its 1→0 deltas.
    Retire(SubNotes),
    /// The slot is vacant or another generation's: the pump ends.
    Finished,
}

fn pop_or_quiesce<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    key: ConnKey,
    pending_empty: bool,
) -> Popped {
    let takes = shared.inbox_takes.get();
    let popped = shared.with_conn(key, |conn| {
        let next = conn.queue.pop_front();
        if let ConnSlot::Active { recv, .. } = &mut conn.slot
            && *recv == RecvArm::Disarmed
            && conn.queue.len() <= PENDING_LOW_WATER
        {
            *recv = RecvArm::RearmDue;
        }
        match next {
            Some(Queued::Command(cmd)) => {
                if let ConnSlot::Closing { .. } | ConnSlot::Draining { .. } = conn.slot {
                    let node = &shared.node;
                    node.closed_queue_commands.set(node.closed_queue_commands.get() + 1);
                }
                Popped::Cmd(cmd)
            }
            Some(Queued::Reply(reply)) => Popped::Reply(reply),
            None if pending_empty => quiesce(shared, key, conn, takes),
            None => Popped::Empty,
        }
    });
    popped.unwrap_or(Popped::Finished)
}

/// The pump's empty read with nothing pending, by the slot's state: a
/// `Peer` slot runs its run-out first (ADR-0196 D2, A1.1); a draining slot
/// with no parser drops the connection from the local registries, in this
/// borrow after its last dequeue, so a queued `SUBSCRIBE`'s 0→1 precedes
/// the close's 1→0 in one task (ADR-0177 D2); any other parks.
fn quiesce<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    key: ConnKey,
    conn: &mut Conn,
    takes: u64,
) -> Popped {
    match conn.run_out(takes) {
        Some(RunOut::Wait) => {
            let node = &shared.node;
            node.reader_take_waits.set(node.reader_take_waits.get() + 1);
            shared.reader_waiting.set(shared.reader_waiting.get() + 1);
            return Popped::TakeWait;
        }
        Some(RunOut::Ended) | None => {}
    }
    // Canary: a pump whose queue is empty after its first reply left returns
    // and its slot vacates with it, the socket left open, so both owners
    // lose a client that still holds its connection.
    #[cfg(inf_canary_conn_owner_lost)]
    if conn.owner_lost {
        return Popped::Retire(SubNotes::default());
    }
    // A draining slot's run-out ended its parser above, or it waits.
    if let ConnSlot::Draining { by: ClosedBy::Plane } = conn.slot {
        // Canary: the deltas captured at `Closed`, and no removal here.
        #[cfg(inf_canary_registry_dropped_at_closed)]
        {
            let _ = key;
            return Popped::Retire(conn.closed_notes.take().unwrap_or_default());
        }
        #[cfg(not(inf_canary_registry_dropped_at_closed))]
        return Popped::Retire(unsubscribe_closed_conn(shared, key, &mut conn.cx));
    }
    conn.pump_parked = true;
    Popped::Park
}

/// Commands that mutate connection execution state must observe — and be
/// observed by — their exact pipeline position (HELLO switches the protocol
/// every later reply serializes under; SELECT switches the database every
/// later command routes to — M1-S08).
#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column conn_state")]
fn is_conn_state(owned: &OwnedCmd) -> bool {
    lookup(owned.arg(0)).is_some_and(|m| match m.id {
        CommandId::Hello | CommandId::Select => true,
        // `INF.NS USE` switches the namespace every later command routes
        // to — the SELECT barrier class (M2-S08).
        CommandId::InfNs => owned.argc() > 1 && owned.arg(1).eq_ignore_ascii_case(b"USE"),
        _ => false,
    })
}

/// Outcome of the de-async dispatch fast path (ADR-0030 D4).
enum FastDispatch {
    /// Handled synchronously; `pending` updated.
    Handled,
    /// The connection is gone; the pump exits.
    ConnGone,
    /// Not a fast arm (rare shape) or no fabric credit on the first send
    /// attempt: run the async [`dispatch_one`] — the unchanged slow path.
    Fallback,
}

/// The pump's synchronous dispatch fast path (M2.5 Phase H, ADR-0030 D4):
/// the arms that dominate the natural-routing mix — the single-owner
/// remote `Apply` and the local mirror — dispatch without constructing
/// the [`dispatch_one`] future, whose send path suspends only on
/// fabric-credit exhaustion. Guard order mirrors `dispatch_one`'s match
/// arms exactly; every other shape falls back to it
/// (`deasync_dispatch_matches_pump_semantics` pins the equivalence).
#[allow(clippy::disallowed_types, reason = "container: A")]
fn dispatch_one_fast<'u, O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &'u Rc<Shared<O, F>>,
    units: &'u GateUnits,
    key: ConnKey,
    owned: &OwnedCmd,
    pending: &mut VecDeque<PendingReply<'u>>,
) -> FastDispatch {
    let argc = owned.argc();
    if argc > ARGV_INLINE {
        // Wide argv (MSET…): rare — keep the heap-argv path async.
        return FastDispatch::Fallback;
    }
    let mut argv_inline: [&[u8]; ARGV_INLINE] = [b""; ARGV_INLINE];
    for (i, slot) in argv_inline[..argc].iter_mut().enumerate() {
        *slot = owned.arg(i);
    }
    let argv: &[&[u8]] = &argv_inline[..argc];
    let Some((proto, id, db, conn_ns, ns_unavailable, restricted)) = shared.with_conn(key, |c| {
        (
            c.cx.proto,
            c.cx.id,
            c.cx.db,
            c.cx.ns.named(),
            c.cx.ns.unavailable(),
            pubsub::subscriber_restricted(&c.cx),
        )
    }) else {
        return FastDispatch::ConnGone;
    };
    let origin = ExecOrigin::Conn(key.slot, key.generation);
    let meta = lookup(argv[0]);
    let well_formed = meta.is_some_and(|m| arity_ok(m, argv.len()));
    // ADR-0115: a fabric-program primitive typed by a client is unknown —
    // refused here, before any routing, so it never crosses the fabric.
    if let Some(m) = meta
        && m.flags.contains(CmdFlags::INTERNAL)
    {
        let mut reply = OpenReply::new(shared.take_reply_buf());
        crate::exec::unknown_command_reply(argv, proto, reply.reply());
        pending.push_back(PendingReply::Done(reply.finish()));
        return FastDispatch::Handled;
    }
    if let Some(meta) = meta
        && well_formed
        && ns_unavailable
        && !unavailable_default_allows(meta.id)
    {
        pending.push_back(PendingReply::Done(unavailable_default_reply(proto)));
        return FastDispatch::Handled;
    }
    if let Some(meta) = meta
        && well_formed
        && restricted
        && !pubsub::allowed_in_subscriber_mode(meta.id)
    {
        // The gate must use the Redis allowlist, not "commands the plane
        // routes to the pub/sub engine": `PUBSUB`/`PUBLISH` are plane
        // pub/sub yet restricted in RESP2 subscriber mode — the old
        // predicate let them through here while `execute`'s fast path
        // refused them (found 2026-09-01 by the INFINITYD_BIN lane).
        pending.push_back(PendingReply::Done(restricted_reply(shared, meta, argv, proto)));
        return FastDispatch::Handled;
    }
    if let Some(m) = meta
        && well_formed
    {
        // The program/rare arms, in dispatch_one's guard order: pub/sub,
        // NS DDL, ckpt surface, named-namespace dispatch, scatter.
        if pubsub::is_plane_pubsub(m.id)
            || (m.id == CommandId::InfNs && is_ns_ddl_sub(argv.get(1).copied()))
            || matches!(m.id, CommandId::InfCkpt | CommandId::Bgsave | CommandId::Lastsave)
            || (conn_ns.is_some() && !is_conn_state(owned))
            || (is_scatter(m.id, argv.get(1).copied())
                && shared.cells > 1
                && !shared.route_local_only)
        {
            return FastDispatch::Fallback;
        }
        // One routing pass per command, as in dispatch_one.
        let mut first_owner = shared.cell;
        let mut any_remote = false;
        if !shared.route_local_only {
            let mut first = true;
            for k in extract_keys_iter(m, argv) {
                let owner = shared.router.cell_of(SlotRouter::slot_of(k));
                if first {
                    first_owner = owner;
                    first = false;
                }
                if owner != shared.cell {
                    any_remote = true;
                    break;
                }
            }
        }
        if any_remote {
            let split = matches!(
                m.id,
                CommandId::Del
                    | CommandId::Exists
                    | CommandId::Unlink
                    | CommandId::Touch
                    | CommandId::Mget
                    | CommandId::JsonMget
                    | CommandId::Mset
                    | CommandId::Msetnx
            );
            let two_owner =
                matches!(m.id, CommandId::Rename | CommandId::Renamenx | CommandId::Copy)
                    && shared.router.cell_of(SlotRouter::slot_of(argv[1]))
                        != shared.router.cell_of(SlotRouter::slot_of(argv[2]));
            if split || two_owner {
                return FastDispatch::Fallback;
            }
            // Single-owner remote command: the hot arm.
            return match try_send_apply(
                shared,
                units,
                first_owner,
                ApplyOrigin::Client,
                proto,
                db,
                argv,
            ) {
                SendNow::Sent(waiter) => {
                    pending.push_back(PendingReply::Remote { waiter, proto });
                    FastDispatch::Handled
                }
                SendNow::Refused(refusal) => {
                    pending.push_back(PendingReply::Done(refusal));
                    FastDispatch::Handled
                }
                SendNow::NoCredit => FastDispatch::Fallback,
            };
        }
    }
    if dispatch_mirror(shared, key, owned, argv, origin, proto, id, db, conn_ns, pending) {
        FastDispatch::Handled
    } else {
        FastDispatch::ConnGone
    }
}

/// A connection's pump (ADR-0177 D2): made in the step that admits the
/// connection, it serves the connection in every occupied state, running
/// the queue to its end at a close (ADR-0123 A2); at a draining slot's
/// last empty read it ships the connection's 1→0 subscription deltas, so
/// one task sends the connection's subscribes and their undoing in order,
/// then vacates the slot and returns: the slot and its task free together.
pub(super) async fn pump<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: Rc<Shared<O, F>>,
    key: ConnKey,
    units: GateUnits,
) {
    let Some(notes) = serve_conn(&shared, key, &units).await else { return };
    // Canary: the slot vacates before its deltas are acknowledged, so the
    // stop's barrier stops counting it with its deltas in flight.
    #[cfg(inf_canary_draining_vacates_before_acks)]
    let _ = shared.conns.borrow_mut().remove(key);
    flush_sub_deltas(&shared, &units, notes).await;
    #[cfg(not(inf_canary_draining_vacates_before_acks))]
    let _ = shared.conns.borrow_mut().remove(key);
    // Canary: the pump keeps a clone of its own waker past its return, so
    // its header stays Retired and holds its class slot for good.
    #[cfg(inf_canary_conn_waker_kept)]
    core::future::poll_fn(|cx| {
        core::mem::forget(cx.waker().clone());
        core::task::Poll::Ready(())
    })
    .await;
}

/// The pump's serving loop: dispatch commands in pipeline order while one
/// of the task's gate `units` is free (a command's first leg takes it;
/// ADR-0177 D4), emit replies strictly in command order. Suspends on the
/// front reply's gate, on fabric credits, and parked when the queue is
/// empty with nothing pending; out-of-order completions
/// park in the gate until their turn. Returns a draining slot's deltas once
/// its queue is empty, or `None` when the slot is vacant.
#[allow(clippy::disallowed_types, reason = "container: A")]
async fn serve_conn<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    key: ConnKey,
    units: &GateUnits,
) -> Option<SubNotes> {
    let shared = Rc::clone(shared);
    let mut pending: VecDeque<PendingReply<'_>> = VecDeque::new();
    // A command held back by the conn-state barrier.
    let mut held: Option<OwnedCmd> = None;
    loop {
        // ---- dispatch: fill the window in pipeline order.
        while pending.len() < PENDING_REPLIES_MAX && units.free() > 0 {
            let cmd = match held.take() {
                Some(cmd) => cmd,
                None => match pop_or_quiesce(&shared, key, pending.is_empty()) {
                    Popped::Cmd(cmd) => cmd,
                    // No barrier: a conn-state command is read from commands.
                    Popped::Reply(reply) => {
                        pending.push_back(PendingReply::Done(reply));
                        continue;
                    }
                    Popped::Empty => break,
                    Popped::Park => {
                        park().await;
                        continue;
                    }
                    Popped::TakeWait => {
                        shared.reader_waiters.wait(()).await;
                        shared.reader_waiting.set(shared.reader_waiting.get() - 1);
                        continue;
                    }
                    Popped::Retire(notes) => return Some(notes),
                    Popped::Finished => return None,
                },
            };
            if is_conn_state(&cmd) && !pending.is_empty() {
                held = Some(cmd);
                break;
            }
            // De-async fast path (ADR-0030 D4): hot arms dispatch without
            // constructing the `dispatch_one` future; rare shapes and
            // credit exhaustion fall back to the async path unchanged.
            let handled = if shared.deasync_dispatch.get() {
                match dispatch_one_fast(&shared, units, key, &cmd, &mut pending) {
                    FastDispatch::Handled => true,
                    FastDispatch::ConnGone => return None,
                    FastDispatch::Fallback => false,
                }
            } else {
                false
            };
            if !handled && !dispatch_one(&shared, units, key, &cmd, &mut pending).await {
                return None; // the slot is vacant
            }
            // The command's flat buffer recycles once dispatched (waiters
            // own their inputs via `ApplyArgs`/reply slots — nothing
            // borrows `cmd` past this point).
            shared.recycle_cmd_buf(cmd.into_buf());
        }

        // ---- emit: resolve the front reply. Awaiting an already-parked
        // value completes on first poll; only a genuinely outstanding front
        // suspends the pump.
        let Some(front) = pending.pop_front() else {
            continue; // barrier held with pending drained: dispatch it now
        };
        // A closing or draining slot drops the reply and the pump goes on;
        // only a vacant slot ends it.
        let reply = settle_reply(&shared, key, front).await;
        let emitted = shared.with_conn(key, |conn| conn.emit(&reply)).unwrap_or(Emitted::Vacant);
        shared.recycle_reply_buf(reply.into_bytes());
        match emitted {
            Emitted::Written | Emitted::Dropped => {}
            Emitted::Vacant => return None,
        }
    }
}

/// Resolves one reply slot, in pipeline order, into the bytes the
/// connection writes next: a parked remote outcome is awaited here, so
/// only a genuinely outstanding front suspends the pump.
pub(super) async fn settle_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    key: ConnKey,
    front: PendingReply<'_>,
) -> OwnedReply {
    match front {
        PendingReply::Done(reply) => reply,
        PendingReply::Remote { waiter, proto } => render_outcome(shared, waiter.await, proto),
        PendingReply::Publish { waiter, proto, seq } => {
            let reply = render_outcome(shared, waiter.await, proto);
            // The owner replied only after this cell acked its fan
            // leg, so a self-frame stash for `seq` is either already
            // here or never existed (the publisher was not subscribed
            // at delivery) — ADR-0101 D4.
            match shared.with_conn(key, |conn| conn.take_self_push(seq)).flatten() {
                Some(frames) => {
                    let mut reply = reply.reopen();
                    RespWriter::new(reply.reply(), proto).encoded(&frames);
                    reply.finish()
                }
                None => reply,
            }
        }
        PendingReply::Counted(legs) => legs.finish(shared).await,
        PendingReply::Gather(legs) => legs.finish(shared).await,
        #[cfg(feature = "doc")]
        PendingReply::JsonGather(gather) => gather.finish(shared).await,
        PendingReply::Durable { waiter, reply } => {
            waiter.await;
            reply
        }
        PendingReply::AllOk(legs) => legs.finish(shared).await,
    }
}

/// Keyspace-wide commands that must scatter across all cells on a
/// multi-cell node (M1-S02). `sub` is argv[1] when present: CONFIG SET /
/// RESETSTAT and INF.NS CREATE / DROP mutate per-cell state (typed config,
/// namespace registries — M1-E3/E4) and fan out AllOk; their read forms
/// stay local.
#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column scatter")]
pub(super) fn is_scatter(id: CommandId, sub: Option<&[u8]>) -> bool {
    match id {
        CommandId::Dbsize
        | CommandId::Keys
        | CommandId::Scan
        | CommandId::Flushdb
        | CommandId::Flushall
        | CommandId::Randomkey => true,
        CommandId::Config => sub.is_some_and(|s| {
            s.eq_ignore_ascii_case(b"SET") || s.eq_ignore_ascii_case(b"RESETSTAT")
        }),
        // INF.NS DDL is a program (`program_ns_ddl`), routed ahead of the
        // scatter arm on every node shape — never a scatter (L13 style
        // row: the scatter arm's `InfNs` case was dead).
        _ => false,
    }
}

/// Peer legs for one scatter-mutator fan. A variadic `CONFIG SET` chunks
/// its pairs so every leg stays under the fabric codec's `MAX_APPLY_ARGS`
/// (review of 2026-08-30, H1 / F-L13-01: at ≥ 8 pairs every peer leg was
/// refused at the encoder and silently dropped — `+OK` with the peers
/// never told); every other mutator fans its argv whole (bounded arity).
/// Chunks land on each peer in ring-FIFO order and re-validate against
/// the same config table, so a locally-validated command cannot fail a
/// chunk; cross-chunk atomicity per peer matches the fan's existing
/// cross-cell eventual semantics (ADR-0098).
fn scatter_fan_legs<'a>(id: CommandId, argv: &[&'a [u8]]) -> Vec<Vec<&'a [u8]>> {
    const PAIRS_PER_LEG: usize = (MAX_APPLY_ARGS - 2) / 2;
    if id == CommandId::Config
        && argv.len() > MAX_APPLY_ARGS
        && argv.get(1).is_some_and(|s| s.eq_ignore_ascii_case(b"SET"))
    {
        return argv[2..]
            .chunks(2 * PAIRS_PER_LEG)
            .map(|pairs| {
                let mut leg = Vec::with_capacity(2 + pairs.len());
                leg.push(argv[0]);
                leg.push(argv[1]);
                leg.extend_from_slice(pairs);
                leg
            })
            .collect();
    }
    vec![argv.to_vec()]
}

/// `INF.NS` subcommands that ride the pump's DDL program: CREATE/DROP
/// since M2-S08; SET since M4-S19 (hot-reload mutates registries on
/// every cell and persists the catalog — DDL semantics exactly).
fn is_ns_ddl_sub(sub: Option<&[u8]>) -> bool {
    sub.is_some_and(|s| {
        s.eq_ignore_ascii_case(b"CREATE")
            || s.eq_ignore_ascii_case(b"DROP")
            || s.eq_ignore_ascii_case(b"SET")
    })
}

/// Dispatch one command: execute locally into a `Done` slot, or ship its
/// remote ops (suspending only on fabric credits — backpressure, never
/// unbounded queueing) and stage the reply waiter. Multi-key commands split
/// per key; RENAME/RENAMENX/COPY across two owners and keyspace-wide
/// commands run as inline fabric programs (M1-S02). Returns `false` when
/// the connection is gone.
#[allow(clippy::disallowed_types, reason = "container: A")]
async fn dispatch_one<'u, O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &'u Rc<Shared<O, F>>,
    units: &'u GateUnits,
    key: ConnKey,
    owned: &OwnedCmd,
    pending: &mut VecDeque<PendingReply<'u>>,
) -> bool {
    // Argv views live on the stack for the common arity; wide commands
    // (MSET…) fall back to `slices()` (M2.5 Phase H allocator lever).
    let argc = owned.argc();
    let mut argv_inline: [&[u8]; ARGV_INLINE] = [b""; ARGV_INLINE];
    let argv_heap: Vec<&[u8]>;
    let argv: &[&[u8]] = if argc <= ARGV_INLINE {
        for (i, slot) in argv_inline[..argc].iter_mut().enumerate() {
            *slot = owned.arg(i);
        }
        &argv_inline[..argc]
    } else {
        argv_heap = owned.slices();
        &argv_heap
    };
    // One slab lookup per command: execution context + subscriber
    // restriction together (was two separate `with_conn` walks).
    let Some((proto, id, db, conn_ns, ns_unavailable, restricted)) = shared.with_conn(key, |c| {
        (
            c.cx.proto,
            c.cx.id,
            c.cx.db,
            c.cx.ns.named(),
            c.cx.ns.unavailable(),
            pubsub::subscriber_restricted(&c.cx),
        )
    }) else {
        return false;
    };
    let origin = ExecOrigin::Conn(key.slot, key.generation);

    let meta = lookup(argv[0]);
    let well_formed = meta.is_some_and(|m| arity_ok(m, argv.len()));
    // ADR-0115: as on the fast path — unknown before any routing.
    if let Some(m) = meta
        && m.flags.contains(CmdFlags::INTERNAL)
    {
        let mut reply = OpenReply::new(shared.take_reply_buf());
        crate::exec::unknown_command_reply(argv, proto, reply.reply());
        pending.push_back(PendingReply::Done(reply.finish()));
        return true;
    }
    if let Some(meta) = meta
        && well_formed
        && ns_unavailable
        && !unavailable_default_allows(meta.id)
    {
        pending.push_back(PendingReply::Done(unavailable_default_reply(proto)));
        return true;
    }
    // M1-S10: RESP2 subscriber-mode restriction for pump-dispatched
    // commands — the fast path checks inside `execute`, but commands
    // landing here would otherwise run under a synthesized ConnCx without
    // the subscription state (remote Apply, scatter legs). The predicate
    // is the Redis allowlist, not `is_plane_pubsub`: `PUBSUB`/`PUBLISH`
    // are plane pub/sub yet restricted in RESP2 subscriber mode (found
    // 2026-09-01 by the INFINITYD_BIN lane, matrix case `PUBSUB
    // CHANNELS`).
    if let Some(meta) = meta
        && well_formed
        && restricted
        && !pubsub::allowed_in_subscriber_mode(meta.id)
    {
        pending.push_back(PendingReply::Done(restricted_reply(shared, meta, argv, proto)));
        return true;
    }
    // One routing pass per command (M2.5 Phase H: was one
    // `extract_keys_slices` Vec per match guard plus a second — and a
    // second `slot_of` — inside the single-owner arm): the first key, its
    // owner, and remote presence, computed together. The pass stops at the
    // first remote key; the first key is always index 0 of the spec, so
    // `first_owner` is captured before any early exit.
    let mut first_owner = shared.cell;
    let mut any_remote = false;
    if let Some(m) = meta
        && well_formed
        && !shared.route_local_only
    {
        let mut first = true;
        for k in extract_keys_iter(m, argv) {
            let owner = shared.router.cell_of(SlotRouter::slot_of(k));
            if first {
                first_owner = owner;
                first = false;
            }
            if owner != shared.cell {
                any_remote = true;
                break;
            }
        }
    }
    let has_remote_key = |_meta| any_remote;
    let owner_of = |k: &[u8]| shared.router.cell_of(SlotRouter::slot_of(k));
    match meta {
        Some(meta) if well_formed && pubsub::is_plane_pubsub(meta.id) => {
            return dispatch_pubsub(shared, units, key, meta.id, argv, proto, pending).await;
        }
        // Namespace DDL (M2-S08, ADR-0015 D2/D3): each subcommand in its
        // program's order (`program_ns_ddl`, `program_ns_drop`) — on every
        // node shape (1-cell included), which is why `needs_fabric` routes
        // all INF.NS here.
        Some(meta)
            if well_formed
                && meta.id == CommandId::InfNs
                && is_ns_ddl_sub(argv.get(1).copied()) =>
        {
            let reply = program_ns_ddl(shared, units, origin, proto, id, db, argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        // Checkpoint operator surface (M2-S20, ADR-0021 D6).
        Some(meta)
            if well_formed
                && matches!(
                    meta.id,
                    CommandId::InfCkpt | CommandId::Bgsave | CommandId::Lastsave
                ) =>
        {
            let reply = program_ckpt(shared, proto, meta.id, argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        // Named-namespace commands (M2-S08): single-owner shape — local
        // execution with durable admission/emission/gating, or a whole-argv
        // `ApplyNs` to the owning cell. Conn-state (SELECT/HELLO/USE) falls
        // through to the mirror arm below. `CONFIG SET`/`RESETSTAT` are
        // node-wide hot-per-cell keys whatever namespace the connection
        // selected — they belong to the scatter arm below (here, a
        // namespace-bound connection would apply them to its own cell
        // only). `FLUSHALL` is node-wide whatever the connection selected,
        // for the same reason (here, it would reply `+OK` having flushed
        // one cell of `cells`); `FLUSHDB` stays here — under a named
        // namespace it means "flush the namespace", which `execute` refuses
        // typed (ADR-0015). `SCAN`/`KEYS`/`RANDOMKEY` stay here too:
        // `dispatch_ns` scatters them namespace-aware.
        Some(meta)
            if well_formed
                && conn_ns.is_some()
                && !is_conn_state(owned)
                && !(matches!(meta.id, CommandId::Config | CommandId::Flushall)
                    && is_scatter(meta.id, argv.get(1).copied())) =>
        {
            let ns = conn_ns.expect("guarded");
            return dispatch_ns(shared, units, key, origin, meta, argv, proto, id, db, ns, pending)
                .await;
        }
        Some(meta)
            if well_formed
                && is_scatter(meta.id, argv.get(1).copied())
                && shared.cells > 1
                && !shared.route_local_only =>
        {
            pending.push_back(
                dispatch_scatter(shared, units, meta.id, origin, proto, id, db, argv).await,
            );
        }
        Some(meta)
            if well_formed
                && matches!(
                    meta.id,
                    CommandId::Del | CommandId::Exists | CommandId::Unlink | CommandId::Touch
                )
                && has_remote_key(meta) =>
        {
            // Per-key split: local keys count at dispatch, remote keys ride
            // typed Apply replies. Applies leave in argv order (per-key
            // order rides the destination ring FIFO).
            let name: &[u8] = argv[0];
            let mut legs = CountedLegs::new(0, proto);
            let mut acc: i64 = 0;
            for k in &argv[1..] {
                if shared.router.is_local(k, shared.cell) {
                    acc += shared.apply_counted(origin, name, k, db);
                    continue;
                }
                let leg: [&[u8]; 2] = [name, k];
                if !legs.send(shared, units, owner_of(k), ApplyOrigin::Client, db, &leg).await {
                    break;
                }
            }
            legs.add(acc);
            pending.push_back(PendingReply::Counted(legs));
        }
        Some(meta) if well_formed && meta.id == CommandId::Mget && has_remote_key(meta) => {
            pending.push_back(gather_mget(shared, units, origin, proto, id, db, argv).await);
        }
        #[cfg(feature = "doc")]
        Some(meta) if well_formed && meta.id == CommandId::JsonMget && has_remote_key(meta) => {
            let target = GatherTarget { origin, proto, id, db, scope: ScatterScope::Db };
            pending.push_back(json_gather(shared, units, target, meta, argv).await);
        }
        Some(meta) if well_formed && meta.id == CommandId::Mset && has_remote_key(meta) => {
            pending.push_back(program_mset(shared, units, origin, proto, id, db, argv).await);
        }
        Some(meta) if well_formed && meta.id == CommandId::Msetnx && has_remote_key(meta) => {
            let reply = program_msetnx(shared, units, origin, proto, id, db, argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        Some(meta)
            if well_formed
                && matches!(meta.id, CommandId::Rename | CommandId::Renamenx | CommandId::Copy)
                && has_remote_key(meta)
                && owner_of(argv[1]) != owner_of(argv[2]) =>
        {
            // Two owners: the read(+delete)/write fabric program. Same-owner
            // pairs fall through to the whole-argv Apply below (atomic at
            // that cell).
            let reply = program_move(shared, units, origin, proto, id, db, meta.id, argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        Some(meta) if well_formed && has_remote_key(meta) => {
            // Single-owner remote command: ship the whole argv; the owner
            // executes and returns its raw RESP reply. The destination is
            // the first key's owner, computed in the routing pass above.
            let scope = ScatterScope::Db;
            pending.push_back(send_whole(shared, units, first_owner, proto, db, scope, argv).await);
        }
        _ => {
            return dispatch_mirror(
                shared, key, owned, argv, origin, proto, id, db, conn_ns, pending,
            );
        }
    }
    true
}

/// The keyspace-wide scatter programs of `dispatch_one` (`is_scatter`):
/// a counted `DBSIZE`, the all-or-nothing per-cell mutators, and the
/// `KEYS`/`SCAN`/`RANDOMKEY` folds. Returns the reply slot to stage.
#[allow(clippy::too_many_arguments, reason = "the dispatch context, not an API surface")]
#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column scatter")]
pub(super) async fn dispatch_scatter<
    'u,
    O: PlaneObserver + 'static,
    F: SegmentFs + Clone + 'static,
>(
    shared: &'u Rc<Shared<O, F>>,
    units: &'u GateUnits,
    cmd: CommandId,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> PendingReply<'u> {
    match cmd {
        CommandId::Dbsize => {
            let mut legs = CountedLegs::new(shared.apply_dbsize(origin, db), proto);
            for cell in peer_cells(shared) {
                // A leg that cannot be sent becomes the reply —
                // never a partial sum (review of 2026-08-30, H1:
                // `if let Ok` silently dropped the cell).
                let leg: [&[u8]; 1] = [b"DBSIZE"];
                if !legs.send(shared, units, cell, ApplyOrigin::Client, db, &leg).await {
                    break;
                }
            }
            PendingReply::Counted(legs)
        }
        CommandId::Flushdb | CommandId::Flushall | CommandId::Config => {
            // Per-cell-state mutators (flush, CONFIG SET/RESETSTAT):
            // the local leg validates and applies; an error reply
            // short-circuits the fan-out.
            // Every command on this arm must therefore be
            // all-or-nothing locally (review of 2026-08-30, H1 /
            // F-L17-12: `CONFIG SET` was not — ADR-0098 split it).
            let local = run_local(shared, origin, proto, id, db, argv);
            if local.is_error() {
                PendingReply::Done(local)
            } else {
                shared.recycle_reply_buf(local.into_bytes());
                // A wide `CONFIG SET` fans in pair-preserving
                // chunks under the codec's argument cap; a leg
                // that still cannot be sent surfaces as the reply
                // instead of being swallowed (H1 / F-L13-01: 8+
                // pairs answered `+OK` while every peer leg was
                // silently refused).
                let fan = scatter_fan_legs(cmd, argv);
                let mut legs = AllOkLegs::new(proto);
                'fan: for cell in peer_cells(shared) {
                    for leg in &fan {
                        if !legs.send(shared, units, cell, ApplyOrigin::Client, db, leg).await {
                            break 'fan;
                        }
                    }
                }
                PendingReply::AllOk(legs)
            }
        }
        CommandId::Keys => {
            let scope = ScatterScope::Db;
            let reply = program_keys(shared, units, origin, proto, id, db, scope, argv).await;
            PendingReply::Done(reply)
        }
        CommandId::Scan => {
            let scope = ScatterScope::Db;
            let reply = program_scan(shared, units, origin, proto, id, db, scope, argv).await;
            PendingReply::Done(reply)
        }
        CommandId::Randomkey => {
            let scope = ScatterScope::Db;
            let reply = program_randomkey(shared, units, origin, proto, id, db, scope, argv).await;
            PendingReply::Done(reply)
        }
        _ => unreachable!("is_scatter covers exactly the arms above"),
    }
}

/// `MGET` with a remote key: every position resolves independently and
/// the replies reassemble into one array in argv order.
async fn gather_mget<'u, O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &'u Rc<Shared<O, F>>,
    units: &'u GateUnits,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> PendingReply<'u> {
    // Gather: every position resolves independently, replies
    // reassemble into one array in argv order (the §6.2 "pipelined,
    // not serialized" shape; per-destination BatchOp coalescing is
    // the M1-S17 story).
    let mut legs = GatherLegs::new(proto);
    for k in &argv[1..] {
        let get: [&[u8]; 2] = [b"GET", k];
        if shared.router.is_local(k, shared.cell) {
            let mut reply = OpenReply::new(shared.take_reply_buf());
            shared.execute_owned_into(origin, &get, proto, id, db, None, false, reply.reply());
            legs.push(GatherPart::Done(reply.finish()));
            continue;
        }
        let to = shared.router.cell_of(SlotRouter::slot_of(k));
        let Ok(sent) = send_windowed(&mut legs, shared, async |_| {
            send_apply(shared, units, to, ApplyOrigin::Client, proto, db, &get).await
        })
        .await;
        legs.push(match sent {
            Ok(waiter) => GatherPart::Wait(waiter),
            Err(refused) => GatherPart::Done(refused.into_reply(shared, proto)),
        });
    }
    PendingReply::Gather(legs)
}

/// `MSET` with a remote key: local pairs first (an OOM reply preempts the
/// fan), then one `SET` leg per remote pair under an all-OK fold.
pub(super) async fn program_mset<'u, O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &'u Rc<Shared<O, F>>,
    units: &'u GateUnits,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> PendingReply<'u> {
    if argv.len().is_multiple_of(2) {
        PendingReply::Done(error_reply(
            shared,
            proto,
            "ERR wrong number of arguments for 'mset' command",
        ))
    } else {
        // Local pairs first (an OOM error reply preempts the fan).
        let mut failure: Option<OwnedReply> = None;
        let mut i = 1;
        while i < argv.len() {
            if shared.router.is_local(argv[i], shared.cell) {
                let mut reply = OpenReply::new(shared.take_reply_buf());
                let pair: [&[u8]; 3] = [b"SET", argv[i], argv[i + 1]];
                shared.execute_owned_into(origin, &pair, proto, id, db, None, false, reply.reply());
                let reply = reply.finish();
                if reply.is_error() && failure.is_none() {
                    failure = Some(reply);
                } else {
                    shared.recycle_reply_buf(reply.into_bytes());
                }
            }
            i += 2;
        }
        if let Some(error) = failure {
            PendingReply::Done(error)
        } else {
            let mut legs = AllOkLegs::new(proto);
            let mut i = 1;
            while i < argv.len() {
                let pair: [&[u8]; 3] = [b"SET", argv[i], argv[i + 1]];
                i += 2;
                if shared.router.is_local(pair[1], shared.cell) {
                    continue;
                }
                let to = shared.router.cell_of(SlotRouter::slot_of(pair[1]));
                // A leg that cannot be sent is the reply, never a `+OK` over
                // an unwritten pair; the fan sends no more.
                if !legs.send(shared, units, to, ApplyOrigin::Client, db, &pair).await {
                    break;
                }
            }
            PendingReply::AllOk(legs)
        }
    }
}

/// The RESP2 subscriber-restriction reply for a pump-dispatched command
/// (M1-S10) — shared verbatim by [`dispatch_one`] and the de-async fast
/// path (ADR-0030 D4): both arms must produce identical bytes.
fn restricted_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    meta: &'static inf_wire::CommandMeta,
    argv: &[&[u8]],
    proto: Protocol,
) -> OwnedReply {
    let mut reply = OpenReply::new(shared.take_reply_buf());
    if meta.id == CommandId::Ping {
        if argv.len() > 2 {
            RespWriter::new(reply.reply(), proto)
                .error("ERR wrong number of arguments for 'ping' command");
        } else {
            pubsub::subscriber_ping(argv.get(1).copied(), proto, reply.reply());
        }
    } else {
        let sub = argv.get(1).copied();
        pubsub::restricted_error(
            meta.id,
            meta.name,
            sub,
            &mut RespWriter::new(reply.reply(), proto),
        );
    }
    reply.finish()
}

fn unavailable_default_reply(proto: Protocol) -> OwnedReply {
    let mut reply = OpenReply::new(Vec::new());
    RespWriter::new(reply.reply(), proto)
        .error("ERR configured default namespace is unavailable; use SELECT or INF.NS USE");
    reply.finish()
}

/// The pump's local mirror arm: conn-state commands execute under a cx
/// mirroring the live connection with the negotiated state written back
/// (HELLO's proto switch must land on the conn — the M0 temp-cx bug);
/// everything else executes locally. Shared verbatim by [`dispatch_one`]
/// and the de-async fast path (ADR-0030 D4). Returns `false` when the
/// connection is gone.
#[allow(clippy::too_many_arguments)] // internal dispatch funnel
#[allow(clippy::disallowed_types, reason = "container: A")]
fn dispatch_mirror<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    key: ConnKey,
    owned: &OwnedCmd,
    argv: &[&[u8]],
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    conn_ns: Option<NsId>,
    pending: &mut VecDeque<PendingReply>,
) -> bool {
    let mut reply = OpenReply::new(shared.take_reply_buf());
    if is_conn_state(owned) {
        // Execute under a cx mirroring the live connection, then
        // write the negotiated protocol back — the M0 pump dropped
        // HELLO's proto switch on queued pipelines (temp-cx bug,
        // found extending the surface; ledger entry).
        let Some(mut live) = shared.with_conn(key, |c| ConnCx {
            proto: c.cx.proto,
            id: c.cx.id,
            db: c.cx.db,
            ns: c.cx.ns,
            // Cold path: HELLO/SELECT execute under the live
            // subscription view (the RESP2 subscriber restriction
            // applies to HELLO exactly as in Redis).
            sub_channels: c.cx.sub_channels.clone(),
            sub_patterns: c.cx.sub_patterns.clone(),
            node: Rc::clone(&shared.node),
            close_requested: Cell::new(false),
            program: false,
        }) else {
            return false;
        };
        let now = shared.now.get();
        // The scope a conn-state command executed *under* — SELECT / USE
        // change it for every later command, not for themselves.
        let scope = ExecScope::of(&live);
        execute_slices(argv, &mut shared.store.borrow_mut(), &mut live, now, reply.reply());
        shared.observer.borrow_mut().on_execute(
            shared.cell,
            origin,
            scope,
            argv,
            reply.bytes(),
            now,
        );
        shared.with_conn(key, |c| {
            c.cx.proto = live.proto;
            c.cx.db = live.db;
            c.cx.ns = live.ns;
        });
    } else {
        let close =
            shared.execute_owned_into(origin, argv, proto, id, db, conn_ns, false, reply.reply());
        if let Some(dur) = stall_request(argv) {
            shared.stall_until.set(shared.now.get().saturating_add(dur));
        }
        if close {
            close_after_reply(shared, key);
        }
    }
    pending.push_back(PendingReply::Done(reply.finish()));
    true
}

/// `QUIT` through the pump (review of 2026-08-30, F-L13-08): the
/// connection closes once the replies ahead of it have flushed, and
/// nothing queued behind it runs, in every state — the inline path's
/// `break`, for the pump. The slot is marked: an active one flushes and
/// parses nothing more, a `Peer` parser ends (ADR-0196 D3); a receive the
/// mark owes is RESPOND's to push, as the pump has no loop context.
pub(super) fn close_after_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    key: ConnKey,
) {
    shared.with_conn(key, |conn| {
        // Canary: the pump's `QUIT` keeps the queue behind it.
        #[cfg(not(inf_canary_quit_keeps_queue))]
        for entry in conn.queue.drain(..) {
            entry.recycle(shared);
        }
        if conn.mark() {
            shared.mark_rearms.borrow_mut().push(key);
        }
    });
}

/// Render an owner's outcome as the reply for a whole-argv `Apply` (buffers
/// come from and return to the cell's reply pool): the one renderer of an
/// [`OwnedOutcome`]. A leg's reply passes through with its outcome; a fabric
/// refusal renders the one refusal text as an error; a typed value renders
/// a success (ADR-0190 D2).
pub(super) fn render_outcome<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    outcome: OwnedOutcome,
    proto: Protocol,
) -> OwnedReply {
    match outcome {
        OwnedOutcome::Reply(reply) => reply,
        OwnedOutcome::Err(_) => error_reply(shared, proto, CROSS_CELL_EXECUTION_FAILED),
        other @ (OwnedOutcome::Ok
        | OwnedOutcome::Int(_)
        | OwnedOutcome::Nil
        | OwnedOutcome::Bool(_)) => {
            // Defensive: typed outcomes from a future peer.
            let mut reply = OpenReply::new(shared.take_reply_buf());
            let mut w = RespWriter::new(reply.reply(), proto);
            match other {
                OwnedOutcome::Ok => w.simple("OK"),
                OwnedOutcome::Int(i) => w.int(i),
                OwnedOutcome::Nil => w.null(),
                OwnedOutcome::Bool(b) => w.bool(b),
                OwnedOutcome::Reply(_) | OwnedOutcome::Err(_) => {
                    unreachable!("the outer match took Reply and Err")
                }
            }
            reply.finish()
        }
    }
}
