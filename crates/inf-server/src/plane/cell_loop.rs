//! `impl CellPlane for ServerPlane` — the reactor-facing loop: completion
//! handling, timers, PARSE+EXECUTE, FABRIC-IN, MAINTAIN, and the park/seal/
//! respond hooks. Nothing here holds a lease across a suspension (L6).

use super::*;

/// PARSE's staged fast-path batch (M2.5 Phase H, ADR-0029 lever 2) over
/// one take of the inbox: flushed at each barrier and at each buffer's end.
struct Staging<'s> {
    enabled: bool,
    stage: &'s mut Vec<StagedParse>,
    bytes: &'s mut Vec<u8>,
}

impl Staging<'_> {
    fn flush<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        shared: &Shared<O, F>,
        origin: ExecOrigin,
        conn_cx: &mut ConnCx,
        out: &mut ReplyStream,
    ) {
        flush_parse_stage(shared, self.stage, self.bytes, origin, conn_cx, out);
    }
}

/// What a parsing slot's replies are written into: an open slot's `out`, or
/// a closing or draining parser's recycled sink, discarded at the entry's
/// end.
enum Sink<'a> {
    Out(&'a mut ReplyStream),
    Dropped(&'a mut ReplyStream),
}

impl Sink<'_> {
    fn stream(&mut self) -> &mut ReplyStream {
        match self {
            Sink::Out(out) | Sink::Dropped(out) => out,
        }
    }
}

/// Where a reply PARSE writes goes: `Out`, the one borrow of the slot's
/// output PARSE takes, when the pump owes the connection nothing; else
/// `Owed`, behind the replies owed before it.
enum ReplyRoute<'a> {
    Out(&'a mut ReplyStream),
    Owed,
}

/// One entry's parse: the slot's sink, whether the pump owed the connection
/// a reply when the entry began, and the entries the buffer queues.
struct ParseInto<'a> {
    sink: Sink<'a>,
    pump_busy: bool,
    deferred: Vec<Queued>,
}

impl ParseInto<'_> {
    /// The reply route, read by the three writes PARSE makes (an inline
    /// command, `-LOADING`, a protocol error): `Owed` while the pump owes
    /// the connection a reply or a command of this buffer deferred, else
    /// the sink. Returns it beside the queue the entry fills.
    fn route(&mut self) -> (ReplyRoute<'_>, &mut Vec<Queued>) {
        let route = if self.pump_busy || !self.deferred.is_empty() {
            ReplyRoute::Owed
        } else {
            ReplyRoute::Out(self.sink.stream())
        };
        (route, &mut self.deferred)
    }
}

/// PARSE's one match on an entry's slot (ADR-0196 D1, D2, A1.1): the
/// parser and the sink of a slot that holds one, and whether it is a
/// draining slot's; `None` for a slot that releases the entry. An active or
/// half-closed slot writes into its `out`, and its received buffer stores
/// `timeout`'s clock; a closing or draining parser renders into a recycled
/// sink, made by `make_sink`.
fn parse_target<'s>(
    slot: &'s mut ConnSlot,
    dropped_out: &'s mut Option<ReplyStream>,
    now_ms: u64,
    make_sink: impl FnOnce() -> ReplyStream,
) -> Option<(&'s mut ConnParser, Sink<'s>, bool)> {
    // Canary: PARSE releases a `Peer` entry unparsed, flushing or closing.
    #[cfg(inf_canary_closing_inbox_released)]
    if let ConnSlot::Flushing { .. } | ConnSlot::Closing { .. } = slot {
        return None;
    }
    // Canary: PARSE releases a draining `Peer` entry unparsed.
    #[cfg(inf_canary_draining_releases_held_entry)]
    if let ConnSlot::Draining { .. } = slot {
        return None;
    }
    match slot {
        ConnSlot::Active { link, parser, .. }
        | ConnSlot::Flushing {
            link,
            stream: PeerStream::Ended { by: ClosedBy::Peer { parser, .. } },
        } => {
            link.last_active_ms = now_ms;
            Some((parser, Sink::Out(&mut link.out), false))
        }
        ConnSlot::Closing { by: ClosedBy::Peer { parser, .. } } => {
            Some((parser, Sink::Dropped(dropped_out.insert(make_sink())), false))
        }
        ConnSlot::Draining { by: ClosedBy::Peer { parser, .. } } => {
            Some((parser, Sink::Dropped(dropped_out.insert(make_sink())), true))
        }
        ConnSlot::Flushing {
            stream: PeerStream::Open | PeerStream::Ended { by: ClosedBy::Plane },
            ..
        }
        | ConnSlot::Closing { by: ClosedBy::Plane }
        | ConnSlot::Draining { by: ClosedBy::Plane } => None,
    }
}

/// What one buffer's parse did: its commands, and whether a `QUIT` or a
/// protocol error stopped it (the slot is then marked).
struct BufferParsed {
    commands: u32,
    stop: bool,
}

/// Whether a parsed command lets the buffer go on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    Next,
    Quit,
}

impl<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static> ServerPlane<O, F> {
    /// The everysec role's delivery: the durable plane's tick, then the
    /// successor a second on from the delivery's time (the tick schedules
    /// from the current injected time; no missed tick is caught up).
    fn everysec_tick<'n>(&self, cx: &mut LoopCx<'_>, notice: &'n TimerNotice) -> TimerOutcome<'n> {
        {
            let mut durable = self.shared.durable.borrow_mut();
            let Some(cell) = durable.as_mut() else { return TimerOutcome::Complete };
            cell.on_everysec_tick();
        }
        // Canary: the tick hands back no successor, so it runs once.
        if cfg!(inf_canary_everysec_no_successor) {
            return TimerOutcome::Complete;
        }
        let deadline = cx.now.saturating_add(Nanos::from_secs(1));
        let next = cx
            .timers()
            .successor(notice, deadline)
            .expect("the everysec successor arms inside the clock's range");
        TimerOutcome::Successor(next)
    }

    fn arm_initial_listener(&mut self, cx: &mut LoopCx<'_>) -> bool {
        use crate::cache_boot::CacheBootStatus;

        match self.shared.node.cache_boot.status() {
            CacheBootStatus::Pending => return false,
            CacheBootStatus::Failed => {
                self.boot_error.get_or_insert_with(|| {
                    std::io::Error::other("a peer failed cache construction before admission")
                });
                return false;
            }
            CacheBootStatus::Ready => {}
        }
        self.started = true;
        // The listener owns the reserved slot, outside the connection slab.
        cx.push(IoOp::AcceptArm {
            listener: self.listener,
            token: CompletionToken::new(TokenClass::Accept, CONN_SLOT_CAP, 0),
        });
        true
    }

    /// The turn's cold delivery (ADR-0181 R1, R2) from the iteration's
    /// allowance: the cold handle is cloned under a short tier borrow, which
    /// is released before any gate wake or orphan cleanup runs, and the
    /// allowance is lent only when the host holds ready delivery, so an
    /// iteration with nothing ready builds no turn. Out of line, so a cell
    /// with no cold engine carries none of it in its iteration
    /// (`inf-runtime`'s `cold_path` bench, its server-shaped reactor legs).
    /// Wakes each parked peer pump whose queue FABRIC-IN filled (ADR-0177
    /// D2): the flag clears in the step that wakes, and the pump reads
    /// its queue in the poll that follows, so no wake is lost. Owner-side
    /// `always` applies (ADR-0015 D6): the fabric reply is deferred, and the
    /// cell's one `gated` task publishes every reply the watermark covers
    /// (flushed by the next FABRIC-OUT). Tiered and paced applies run on
    /// each origin's FIFO pump; fabric-origin `PUBLISH`es fan out on the
    /// owner pump, one long-lived task over the origins' FIFOs.
    fn wake_peer_pumps(&self, cx: &mut LoopCx<'_>) {
        let Some(Some(pumps)) = self.shared.peer_pumps.get() else {
            return;
        };
        if self.shared.gated_pump_parked.get() && self.shared.gated.waits() {
            self.shared.gated_pump_parked.set(false);
            let _ = cx.executor.wake(pumps.gated);
        }
        {
            let queues = self.shared.ns_applies.borrow();
            if queues.waits() {
                let mut parked = self.shared.ns_pump_parked.borrow_mut();
                for origin in queues.origins_waiting() {
                    if core::mem::replace(&mut parked[usize::from(origin.0)], false) {
                        let _ = cx.executor.wake(pumps.ns_apply(self.shared.cell, origin));
                    }
                }
            }
        }
        if self.shared.pub_pump_parked.get() && self.shared.owner_pubs.borrow().waits() {
            self.shared.pub_pump_parked.set(false);
            let _ = cx.executor.wake(pumps.owner_pub);
        }
    }

    /// One inbox entry, by PARSE's one match on its slot: the four states
    /// that hold a parser (Active, and Flushing, Closing and Draining
    /// `Peer`) parse it by one function, the rest release it (ADR-0196 D1,
    /// D2, A1.1). A closing or draining parser's inline replies render into
    /// a recycled buffer, discarded at the entry's end: the slot holds no
    /// `out`. Returns the commands parsed.
    fn parse_entry(
        &self,
        cx: &mut LoopCx<'_>,
        key: ConnKey,
        buf: BufferId,
        len: u32,
        staging: &mut Staging<'_>,
    ) -> u32 {
        // The parked pump to wake once the deferred commands are queued.
        let mut wake_pump: Option<TaskId> = None;
        let mut dropped_out: Option<ReplyStream> = None;
        let (commands, draining) = {
            let mut conns = self.shared.conns.borrow_mut();
            let Some(conn) = conns.get_mut(key) else { return 0 };
            // While the pump is not parked it owns the connection's
            // order: every command defers behind what it holds.
            let pump_busy = !conn.pump_parked;
            let now_ms = cx.now.as_millis();
            let Conn { cx: conn_cx, slot, .. } = &mut *conn;
            let make_sink = || ReplyStream::new(self.shared.take_reply_buf());
            let Some((parser, sink, draining)) =
                parse_target(slot, &mut dropped_out, now_ms, make_sink)
            else {
                return 0;
            };
            let mut into = ParseInto { sink, pump_busy, deferred: Vec::new() };
            let data = &cx.pool.bytes(buf)[..len as usize];
            let parsed = self.parse_buffer(key, parser, conn_cx, &mut into, data, staging);
            let ParseInto { deferred, .. } = into;
            if parsed.stop && conn.mark() {
                self.shared.mark_rearms.borrow_mut().push(key);
            }
            let queued = !deferred.is_empty();
            if let Some(fd) = conn.enqueue(deferred) {
                cx.push(IoOp::RecvDisarm { fd });
            }
            // A command queued for a parked pump wakes it; the flag
            // clears in this step, and the pump reads its queue in the
            // poll the wake queues.
            #[cfg(not(inf_canary_parked_pump_not_woken))]
            if queued {
                wake_pump = conn.take_parked_pump();
            }
            // Canary: the command is queued and its parked pump is never
            // woken, so the client waits for a reply nothing makes.
            #[cfg(inf_canary_parked_pump_not_woken)]
            let _ = (queued, &mut wake_pump);
            (parsed.commands, draining)
        };
        if let Some(out) = dropped_out {
            self.shared.recycle_reply_buf(out.into_vec());
        }
        if draining {
            let node = &self.shared.node;
            node.closed_reader_entries.set(node.closed_reader_entries.get() + 1);
        }
        if let Some(pump) = wake_pump {
            let _ = cx.executor.wake(pump);
        }
        commands
    }

    /// One received buffer through the slot's parser, the one function
    /// every parsing state runs (L13): inline commands execute into `out`,
    /// deferred ones queue behind the first that must defer, and a `QUIT`
    /// or a protocol error ends the buffer (Redis discards what follows
    /// either).
    fn parse_buffer(
        &self,
        key: ConnKey,
        parser: &mut ConnParser,
        conn_cx: &mut ConnCx,
        into: &mut ParseInto<'_>,
        data: &[u8],
        staging: &mut Staging<'_>,
    ) -> BufferParsed {
        let origin = ExecOrigin::Conn(key.slot, key.generation);
        let mut parsed = BufferParsed { commands: 0, stop: false };
        let mut iter = parser.feed(data);
        while let Some(next) = iter.next() {
            match next {
                Parsed::Command(argv) | Parsed::Inline(argv) => {
                    parsed.commands += 1;
                    if self.run_parsed(&argv, conn_cx, into, origin, staging) == Flow::Quit {
                        parsed.stop = true;
                        break;
                    }
                }
                Parsed::Incomplete => {}
                Parsed::ProtocolError(e) => {
                    // Display, not Debug: the Redis phrasing the compat
                    // harness diffs.
                    let error = format!("ERR Protocol error: {e}");
                    self.parse_reply(conn_cx, into, origin, staging, &error);
                    parsed.stop = true;
                    break;
                }
            }
        }
        // End of buffer: the staged batch executes with its record
        // lines prefetched across the whole batch.
        match into.route() {
            (ReplyRoute::Out(out), _) => staging.flush(&self.shared, origin, conn_cx, out),
            // Nothing is staged once the pump owes a reply: the first
            // deferral flushed the batch.
            (ReplyRoute::Owed, _) => {
                debug_assert!(staging.stage.is_empty(), "staged past a deferral")
            }
        }
        drop(iter);
        parsed
    }

    /// One parsed command: refused while loading, deferred to the pump,
    /// staged, or executed inline.
    fn run_parsed(
        &self,
        argv: &ArgvRef<'_>,
        conn_cx: &mut ConnCx,
        into: &mut ParseInto<'_>,
        origin: ExecOrigin,
        staging: &mut Staging<'_>,
    ) -> Flow {
        let meta = lookup(argv.arg(0));
        if self.loading_refuses(meta) {
            self.parse_reply(
                conn_cx,
                into,
                origin,
                staging,
                "LOADING Redis is loading the dataset in memory",
            );
            return Flow::Next;
        }
        let (route, deferred) = into.route();
        let out = match route {
            ReplyRoute::Out(out) => out,
            ReplyRoute::Owed => {
                deferred.push(Queued::Command(OwnedCmd::from_argv_into(
                    argv,
                    self.shared.take_cmd_buf(),
                )));
                return Flow::Next;
            }
        };
        // Named-namespace commands always ride the pump (M2-S08): durable
        // acks suspend, staging admission may park, and emission lives
        // there — one `Option` load on the memory fast path.
        if conn_cx.ns.named().is_some() || conn_cx.ns.unavailable() || self.needs_fabric(argv) {
            // Pump replies emit after the fast-path replies already in
            // `out` — flush so the staged batch keeps its pipeline position.
            staging.flush(&self.shared, origin, conn_cx, out);
            deferred
                .push(Queued::Command(OwnedCmd::from_argv_into(argv, self.shared.take_cmd_buf())));
            return Flow::Next;
        }
        if staging.enabled && parse_stageable(meta, argv) {
            self.stage_parsed(argv, conn_cx, staging);
            return Flow::Next;
        }
        staging.flush(&self.shared, origin, conn_cx, out);
        self.execute_inline(argv, conn_cx, out, origin)
    }

    /// A reply PARSE writes itself, `-LOADING` or a protocol error, by the
    /// route: into `out` after the staged batch when the pump owes the
    /// connection nothing, else rendered once into a recycled buffer and
    /// queued behind the replies owed before it, so it leaves in pipeline
    /// order. A closing or draining parser's reply is dropped: the slot
    /// writes no `out`, so the route is not read.
    fn parse_reply(
        &self,
        conn_cx: &mut ConnCx,
        into: &mut ParseInto<'_>,
        origin: ExecOrigin,
        staging: &mut Staging<'_>,
        error: &str,
    ) {
        if let Sink::Dropped(_) = into.sink {
            return;
        }
        // Canary: the reply is written into `out` whatever the route.
        #[cfg(inf_canary_parse_reply_around_queue)]
        let (route, deferred) = (ReplyRoute::Out(into.sink.stream()), &mut into.deferred);
        #[cfg(not(inf_canary_parse_reply_around_queue))]
        let (route, deferred) = into.route();
        match route {
            ReplyRoute::Out(out) => {
                staging.flush(&self.shared, origin, conn_cx, out);
                RespWriter::new(out.open(), conn_cx.proto).error(error);
            }
            ReplyRoute::Owed => {
                let mut reply = OpenReply::new(self.shared.take_reply_buf());
                RespWriter::new(reply.reply(), conn_cx.proto).error(error);
                deferred.push(Queued::Reply(reply.finish()));
                let node = &self.shared.node;
                node.parse_replies_queued.set(node.parse_replies_queued.get() + 1);
            }
        }
    }

    /// M2-S15 `-LOADING` gate: while the node loads, only LOADING-flagged
    /// commands run; the rest answer exactly Redis's error (oracle capture
    /// artifact). Unknown commands keep their normal error — Redis
    /// resolves the command first. The board re-check serves commands that
    /// arrive in the same iteration the last cell flips ready.
    fn loading_refuses(&self, meta: Option<&inf_wire::CommandMeta>) -> bool {
        if !self.shared.loading.get()
            || meta.is_none_or(|meta| meta.flags.contains(CmdFlags::LOADING))
        {
            return false;
        }
        if self.loading_board.as_deref().is_some_and(RecoveryBoard::all_ready) {
            self.shared.loading.set(false);
            return false;
        }
        true
    }

    /// M2.5 Phase H (ADR-0029 lever 2): stages the fast-path command — flat
    /// argv copy + key hash + phase-1 index-line prefetch; the batch
    /// executes at the next barrier or end of buffer with its record lines
    /// prefetched.
    fn stage_parsed(&self, argv: &ArgvRef<'_>, conn_cx: &ConnCx, staging: &mut Staging<'_>) {
        let (hash, has_key) = match argv.len() >= 2 {
            true => {
                let hash = self.shared.hasher.hash(argv.arg(1));
                if let Some(store) = self.shared.store.borrow().db(usize::from(conn_cx.db)) {
                    store.prefetch(hash);
                }
                (hash, true)
            }
            false => (0, false),
        };
        let off = stage_argv_block_argv(staging.bytes, argv);
        staging.stage.push(StagedParse { off, hash, has_key });
    }

    /// An inline command executed into `out`: `Flow::Quit` when it was
    /// `QUIT`, which stops the buffer (Redis discards anything pipelined
    /// after `QUIT`); the slot is marked and closes once the `+OK` flushed.
    fn execute_inline(
        &self,
        argv: &ArgvRef<'_>,
        conn_cx: &mut ConnCx,
        out: &mut ReplyStream,
        origin: ExecOrigin,
    ) -> Flow {
        let argv_slices: Vec<&[u8]> = argv.iter().collect();
        let now = self.shared.now.get();
        let scope = ExecScope::of(conn_cx);
        let reply = out.open();
        execute(argv, &mut self.shared.store.borrow_mut(), conn_cx, now, reply);
        self.shared.observer.borrow_mut().on_execute(
            self.shared.cell,
            origin,
            scope,
            &argv_slices,
            reply.bytes(),
            now,
        );
        if let Some(dur) = stall_request(&argv_slices) {
            self.shared.stall_until.set(now.saturating_add(dur));
        }
        if conn_cx.close_requested.get() {
            conn_cx.close_requested.set(false);
            return Flow::Quit;
        }
        Flow::Next
    }

    /// One `Send` from a slot's staged output, when a send buffer is free
    /// and no send is in flight.
    fn send_out(cx: &mut LoopCx<'_>, key: ConnKey, link: &mut Link) {
        if link.out.is_empty() || link.send_inflight {
            return;
        }
        let Some(buf) = cx.pool.try_lease(LeaseKind::Send) else { return };
        let n = link.out.len().min(cx.pool.buf_size());
        cx.pool.bytes_mut(buf)[..n].copy_from_slice(&link.out.as_bytes()[..n]);
        link.out.consume(n);
        link.send_inflight = true;
        cx.push(IoOp::Send {
            fd: link.fd,
            buf,
            len: n as u32,
            token: Self::token(TokenClass::Send, key),
        });
    }

    /// The marks' owed `RecvArm`s (ADR-0196 A1.3): one for each marked
    /// slot still `Flushing` `Open`, which has not read its end of stream.
    fn push_mark_rearms(&self, cx: &mut LoopCx<'_>) {
        let keys = core::mem::take(&mut *self.shared.mark_rearms.borrow_mut());
        for key in keys {
            let fd = self.shared.with_conn(key, |conn| match &conn.slot {
                ConnSlot::Flushing { link, stream: PeerStream::Open } => Some(link.fd),
                ConnSlot::Flushing { stream: PeerStream::Ended { .. }, .. }
                | ConnSlot::Active { .. }
                | ConnSlot::Closing { .. }
                | ConnSlot::Draining { .. } => None,
            });
            if let Some(Some(fd)) = fd {
                cx.push(IoOp::RecvArm { fd, token: Self::token(TokenClass::Recv, key) });
            }
        }
    }

    /// The peer's end of stream on `key` (ADR-0196 A1.1): a mark of the
    /// flush shape that keeps the parser, so the entries reaped before it
    /// parse and are answered; no `Close` is pushed. The slot's pump is
    /// woken, so its empty read runs the run-out.
    fn end_of_stream(&mut self, cx: &mut LoopCx<'_>, key: ConnKey) {
        // Canary: the end of stream initiates the close at once, as the
        // peer's departure: Active → Closing `Peer`.
        #[cfg(inf_canary_eof_closes_at_once)]
        return self.peer_gone(cx, key);
        #[cfg(not(inf_canary_eof_closes_at_once))]
        {
            let takes = self.shared.inbox_takes.get();
            let wake = self.shared.with_conn(key, |conn| conn.end_of_stream(takes)).flatten();
            // Canary: the end-of-stream arm re-arms the half-closed slot's
            // receive.
            #[cfg(inf_canary_rearm_after_eof)]
            if let Some(Some(fd)) = self.shared.with_conn(key, |conn| match &conn.slot {
                ConnSlot::Flushing { link, stream: PeerStream::Ended { .. } } => Some(link.fd),
                ConnSlot::Active { .. }
                | ConnSlot::Flushing { stream: PeerStream::Open, .. }
                | ConnSlot::Closing { .. }
                | ConnSlot::Draining { .. } => None,
            }) {
                cx.push(IoOp::RecvArm { fd, token: Self::token(TokenClass::Recv, key) });
            }
            if let Some(pump) = wake {
                let _ = cx.executor.wake(pump);
            }
        }
    }

    /// `Closed` on `key`, or a `Close`-class error: the closing slot moves
    /// into `Draining` with its queue and its `Peer` parser; the client
    /// leaves `CLIENT LIST` for good and `active`; the local registries are
    /// not touched (the drain's end drops them); the pump is woken.
    fn on_closed(&mut self, cx: &mut LoopCx<'_>, key: ConnKey) {
        let takes = self.shared.inbox_takes.get();
        let moved = self.shared.with_conn(key, |conn| {
            let moved = conn.closed(takes);
            // Canary: `Closed` drops the queue, as a removed slot did.
            #[cfg(inf_canary_closed_conn_queue_dropped)]
            if moved {
                for entry in conn.queue.drain(..) {
                    entry.recycle(&self.shared);
                }
            }
            // Canary: the registry removal and the 1→0 capture at `Closed`,
            // before the drain.
            #[cfg(inf_canary_registry_dropped_at_closed)]
            if moved {
                conn.closed_notes = Some(unsubscribe_closed_conn(&self.shared, key, &mut conn.cx));
            }
            moved.then(|| (conn.cx.id, conn.take_parked_pump()))
        });
        let Some(Some((id, pump))) = moved else { return };
        // Canary: the slot is vacated at `Closed`, before its pump returns.
        #[cfg(inf_canary_conn_slot_frees_at_closed)]
        let _ = self.shared.conns.borrow_mut().remove(key);
        self.shared.node.clients.borrow_mut().unregister(id);
        // Canary: the parked pump is not woken at `Closed`, so its draining
        // slot never retires.
        #[cfg(not(inf_canary_closed_pump_not_woken))]
        if let Some(pump) = pump {
            let _ = cx.executor.wake(pump);
        }
        #[cfg(inf_canary_closed_pump_not_woken)]
        let _ = pump;
    }

    #[inline(never)]
    fn deliver_cold(&self, cx: &mut LoopCx<'_>) {
        let cold = self.shared.tier.borrow().as_ref().and_then(|t| t.cold.clone());
        if let Some(cold) = cold
            && cold.has_ready_delivery()
        {
            let now_us = cx.now.as_micros();
            let _ = cx.with_cold_delivery_budget(|budget| cold.deliver_ready(budget, now_us));
        }
    }
}

impl<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static> CellPlane for ServerPlane<O, F> {
    fn on_completion(&mut self, cx: &mut LoopCx<'_>, c: Completion) {
        match c.result {
            CompletionResult::Accepted { fd } => {
                // ADR-0128: a tier whose kernel does not spread the
                // listener group rotates accepts across the fabric.
                if !self.try_handoff(fd) {
                    self.admit_accepted(cx, fd);
                }
            }
            CompletionResult::Recv { buf, len } => {
                let key = Self::key_of(c.token);
                if len == 0 {
                    cx.pool.release(buf);
                    self.end_of_stream(cx, key);
                } else {
                    self.inbox.push((key, buf, len));
                }
            }
            CompletionResult::RecvDropped => {
                self.shared.recv_dropped.set(self.shared.recv_dropped.get() + 1);
            }
            CompletionResult::Sent { buf } => {
                cx.pool.release(buf);
                if c.token.slot() == CONN_SLOT_CAP {
                    // The refusal frame left: close the refused socket.
                    self.close_refused(cx, c.token);
                    return;
                }
                // A send in flight when its slot closed returns its buffer
                // alone: a closing or draining slot holds no send.
                let key = Self::key_of(c.token);
                self.shared.with_conn(key, |conn| {
                    if let Some(link) = conn.link_mut() {
                        link.send_inflight = false;
                    }
                });
                // Canary: once a reply has left an active slot whose pump is
                // parked on an empty queue, the pump is woken to drop both
                // owners, the socket left open.
                #[cfg(inf_canary_conn_owner_lost)]
                if let Some(Some(pump)) = self.shared.with_conn(key, |conn| {
                    let sent = match &conn.slot {
                        ConnSlot::Active { link, .. } => link.out.is_empty(),
                        ConnSlot::Flushing { .. }
                        | ConnSlot::Closing { .. }
                        | ConnSlot::Draining { .. } => false,
                    };
                    let idle = sent && conn.queue.is_empty() && !conn.owner_lost;
                    conn.owner_lost |= idle;
                    if idle { conn.take_parked_pump() } else { None }
                }) {
                    let _ = cx.executor.wake(pump);
                }
            }
            CompletionResult::Closed => self.on_closed(cx, Self::key_of(c.token)),
            CompletionResult::Error { buf, errno } => {
                if let Some(buf) = buf {
                    cx.pool.release(buf);
                }
                // Tier-flush op failures (M4.5-S31, ADR-0084 D4) are the
                // round's to classify at the next MAINTAIN — ENOSPC
                // latches admission and retries; a failed barrier is
                // fatal there (fail-stop). Never connection housekeeping.
                if matches!(c.token.class(), TokenClass::TierFlushWrite | TokenClass::TierFlushSync)
                {
                    let mut tier = self.shared.tier.borrow_mut();
                    let tier = tier.as_mut().expect("tier-flush completion implies tier state");
                    tier.on_flush_completion(c.token, Some(errno));
                    return;
                }
                // Log-op failures are fail-stop territory, never
                // connection housekeeping. A zero-fill write failing is
                // the same class: the device refused a log write.
                if matches!(
                    c.token.class(),
                    TokenClass::LogWrite | TokenClass::Fsync | TokenClass::ZeroFillWrite
                ) {
                    let mut durable = self.shared.durable.borrow_mut();
                    let cell = durable.as_mut().expect("log completion without durable plane");
                    // `-> !`: the empty match pins the divergence here, so
                    // the housekeeping tail below is unreachable by
                    // construction (L13 style row) — a log token never
                    // reaches the connection arms.
                    match cell.on_log_error(c.token, errno) {}
                }
                // Checkpoint-op failures abort the checkpoint, never the
                // process (ADR-0016: the old checkpoint + log stay valid);
                // the token is not a connection.
                if matches!(c.token.class(), TokenClass::CkptWrite | TokenClass::CkptSync) {
                    let mut durable = self.shared.durable.borrow_mut();
                    let cell = durable.as_mut().expect("ckpt completion without durable plane");
                    cell.on_ckpt_error(errno);
                    return;
                }
                // MANIFEST-swap barrier failure: old recovery unit kept
                // (M2-S11, the checkpoint-abort class — ADR-0017).
                if c.token.class() == TokenClass::ManifestSync {
                    let mut durable = self.shared.durable.borrow_mut();
                    let cell = durable.as_mut().expect("manifest completion without durable plane");
                    cell.on_manifest_error(errno);
                    return;
                }
                // Cold-read failure (M4-S26): custody must release —
                // waiters observe the typed errno and the command layer
                // answers a typed error, never a crash (operating error).
                // The receipt is recorded here; the next turn's
                // `before_execute` delivers its members (ADR-0181 R1).
                if c.token.class() == TokenClass::TierRead {
                    let tier = self.shared.tier.borrow();
                    let cold = tier
                        .as_ref()
                        .and_then(|t| t.cold.as_ref())
                        .expect("TierRead completion implies a live cold-read engine");
                    record_cold_receipt(cold, c.token, Err(errno), cx.now.as_micros());
                    return;
                }
                // Accept-path failures belong to the listener, never to a
                // connection: count them. The driver delivers one only when
                // it parks the arm, on an exhaustion or a broken-listener
                // errno (`classify_accept_errno`); a transient one never
                // surfaces. The accept-retry owner re-arms the listener a
                // retry window later, the only way back for a broken one
                // (ADR-0118 D2). Armed only from Idle: failures inside a
                // window coalesce into its one arm.
                if c.token.class() == TokenClass::Accept {
                    self.shared.accept_errors.set(self.shared.accept_errors.get() + 1);
                    // Canary: the failure re-arms the listener at once, the
                    // spin into a persistent failure the window prevents.
                    #[cfg(inf_canary_accept_retry_at_once)]
                    cx.push(IoOp::AcceptArm {
                        listener: self.listener,
                        token: CompletionToken::new(TokenClass::Accept, CONN_SLOT_CAP, 0),
                    });
                    let owner = &self.timers.accept_retry;
                    #[cfg(not(inf_canary_accept_retry_late))]
                    let deadline = cx.now.saturating_add(ACCEPT_RETRY);
                    // Canary: the retry falls due 2 ms past its window.
                    #[cfg(inf_canary_accept_retry_late)]
                    let deadline =
                        cx.now.saturating_add(ACCEPT_RETRY).saturating_add(Nanos::from_millis(2));
                    let mut timers = cx.timers();
                    if timers.phase(owner) == OwnerPhase::Idle {
                        timers
                            .arm(owner, deadline)
                            .expect("the accept retry arms from Idle inside the clock's range");
                    }
                    return;
                }
                // The reserved slot is a refused accept (ADR-0123 D1): a
                // failed refusal send still closes its fd; nothing else
                // rides that slot.
                if c.token.slot() == CONN_SLOT_CAP {
                    if c.token.class() == TokenClass::Send {
                        self.close_refused(cx, c.token);
                    }
                    return;
                }
                // Only connection classes reach housekeeping — a class
                // this arm does not know must not tear down whichever
                // connection shares its slot/generation.
                if !matches!(
                    c.token.class(),
                    TokenClass::Recv | TokenClass::Send | TokenClass::Close
                ) {
                    return;
                }
                // `close(2)` releases the descriptor whatever it returns, so
                // a `Close`-class error is the op's terminal completion, as
                // `Closed` is; a receive or send error is the peer gone.
                let key = Self::key_of(c.token);
                if c.token.class() == TokenClass::Close {
                    self.on_closed(cx, key);
                } else {
                    self.peer_gone(cx, key);
                }
            }
            // Log + checkpoint file ops (M2-S05/S08/S10): routed by token
            // class into the durable cell — lease release on a write's
            // terminal completion; watermark advance + gated-ack wakes on
            // a log Synced; checkpoint progress on the ckpt classes.
            CompletionResult::LogWritten => {
                // Tier-flush writes (M4.5-S31): a round-counter update in
                // the tier plane — never the WAL frame lease's custody.
                if c.token.class() == TokenClass::TierFlushWrite {
                    let mut tier = self.shared.tier.borrow_mut();
                    let tier = tier.as_mut().expect("tier-flush completion implies tier state");
                    tier.on_flush_completion(c.token, None);
                    return;
                }
                let mut durable = self.shared.durable.borrow_mut();
                let cell = durable.as_mut().expect("LogWritten without durable plane");
                match c.token.class() {
                    TokenClass::CkptWrite => cell.on_ckpt_written(),
                    // ADR-0086 D4: a zero slice landed on the next
                    // segment — the rotor's cursor, never the frame lease.
                    TokenClass::ZeroFillWrite => cell.on_zero_fill_written(),
                    TokenClass::Accept
                    | TokenClass::Recv
                    | TokenClass::Send
                    | TokenClass::Close
                    | TokenClass::Wake
                    | TokenClass::LogWrite
                    | TokenClass::Fsync
                    | TokenClass::CkptSync
                    | TokenClass::ManifestSync
                    | TokenClass::TierRead
                    | TokenClass::TierFlushWrite
                    | TokenClass::TierFlushSync => cell.on_log_written(cx, c.token),
                }
            }
            CompletionResult::Synced => {
                // Tier-flush barriers (M4.5-S31): recorded here, applied
                // at MAINTAIN — flush watermarks never ride the WAL
                // commit ledger (ADR-0084 D1).
                if c.token.class() == TokenClass::TierFlushSync {
                    let mut tier = self.shared.tier.borrow_mut();
                    let tier = tier.as_mut().expect("tier-flush completion implies tier state");
                    tier.on_flush_completion(c.token, None);
                    return;
                }
                let mut durable = self.shared.durable.borrow_mut();
                let cell = durable.as_mut().expect("Synced without durable plane");
                if c.token.class() == TokenClass::CkptSync {
                    cell.on_ckpt_synced();
                } else if c.token.class() == TokenClass::ManifestSync {
                    cell.on_manifest_synced();
                } else {
                    cell.on_synced(cx, c.token);
                }
            }
            CompletionResult::TierRead => {
                // M4-S26: route into the custody table — this iteration's
                // `before_execute` delivers its members, so their futures
                // run in this iteration's EXECUTE slice (ADR-0181 R1, R2).
                let tier = self.shared.tier.borrow();
                let cold = tier
                    .as_ref()
                    .and_then(|t| t.cold.as_ref())
                    .expect("TierRead completion implies a live cold-read engine");
                record_cold_receipt(cold, c.token, Ok(()), cx.now.as_micros());
            }
        }
    }

    fn on_timer<'n>(&mut self, cx: &mut LoopCx<'_>, notice: &'n TimerNotice) -> TimerOutcome<'n> {
        match TimerRole::from_route(notice.route()) {
            Some(TimerRole::AcceptRetry) => {
                // Canary: the retry pushes no accept arm, so a listener the
                // driver parked as broken never accepts again.
                #[cfg(not(inf_canary_accept_retry_no_rearm))]
                if self.started {
                    // Idempotent on an armed listener; resumes a parked
                    // one of either class. An exhausted or broken listener
                    // whose failure persists fails again: one more count
                    // and one more window, one `accept(2)` a window.
                    cx.push(IoOp::AcceptArm {
                        listener: self.listener,
                        token: CompletionToken::new(TokenClass::Accept, CONN_SLOT_CAP, 0),
                    });
                }
                TimerOutcome::Complete
            }
            Some(TimerRole::Everysec) => self.everysec_tick(cx, notice),
            // The holds need no handler: the wake is the effect, and this
            // iteration's LOG step seals the held frame.
            Some(TimerRole::FillHold | TimerRole::GroupHold) => TimerOutcome::Complete,
            None => {
                debug_assert!(false, "a notice routed {} names no timer role", notice.route());
                TimerOutcome::Complete
            }
        }
    }

    fn before_execute(&mut self, cx: &mut LoopCx<'_>) {
        // ADR-0181 R2: the turn's cold delivery, before either executor
        // pass and outside `parse_execute`'s listener-startup and DEBUG
        // SLEEP returns. A cell with no cold engine pays the engine's
        // presence test under a short tier borrow; the delivery itself is
        // out of the iteration's line.
        let engine = self.shared.tier.borrow().as_ref().is_some_and(|t| t.cold.is_some());
        if engine {
            self.deliver_cold(cx);
        }
    }

    fn before_park(&mut self) -> bool {
        // Boot replay pending: keep polling (M2-S15) — parking would gate
        // recovery throughput on the park timeout. Throttled (test-only)
        // boots do park; the park timeout bounds their step cadence.
        if let Some(boot) = &self.boot
            && boot.cfg.recover.throttle_bytes_per_sec.is_none()
        {
            return true;
        }
        // A registered checkpoint waiter keeps the cell unparked while its
        // sweep is part-way (ADR-0159 A1.4), and while its own slot
        // published since the start of its last completed sweep
        // (`interfaces-m2.md`, "Cells never fold the whole board"): the
        // cell publishes after its sweep step in a MAINTAIN, so the
        // observation that satisfies its own waiter is the next turn's. At
        // most `2 * ceil(N / 64)` unparked turns per own publication. The
        // two O(1) tests go first; the waitlist test stops at its first
        // live waiter.
        if self.ckpt_observation_owed() && self.shared.ckpt_waiters.any_waiting() {
            return true;
        }
        // A parked loop never holds ready cold work (ADR-0181 R2, R6):
        // members a turn's allowance left, or a drain that may issue now,
        // wait for the next turn, not for a park's timeout, park flags or
        // none.
        let tier = self.shared.tier.borrow();
        let cold = tier.as_ref().and_then(|t| t.cold.as_ref());
        if cold.is_some_and(|cold| cold.holds_ready_work()) {
            return true;
        }
        drop(tier);
        let Some(flags) = &self.park_flags else { return false };
        let me = usize::from(self.shared.cell.0);
        flags[me].store(true, Ordering::Relaxed);
        // Pairs with the producer's ring → fence → parked-flag load: either
        // this final check sees the doorbell, or the producer sees the flag
        // and wakes us. A doubly-missed wake degrades to the park timeout,
        // never a hang.
        fence(Ordering::SeqCst);
        if self.shared.fabric.borrow().doorbell_pending() {
            flags[me].store(false, Ordering::Relaxed);
            return true;
        }
        false
    }

    fn fabric_in(&mut self, cx: &mut LoopCx<'_>) {
        if let Some(flags) = &self.park_flags {
            flags[usize::from(self.shared.cell.0)].store(false, Ordering::Relaxed);
        }
        self.shared.now.set(cx.now);
        // Ops execute *during* the drain over their borrowed ring payloads —
        // the owner side of a remote `Apply` is zero-allocation (M0-E8: the
        // owned-staging copies dominated the cross-cell profile). Only the
        // replies wait: the fabric is mutably borrowed by `drain`, so their
        // bytes land in the reusable scratch and ship the moment it ends.
        // With `--fabric-apply-prefetch` (M2.5 Phase H), applies instead
        // stage (one small copy) so the whole pack's store lines prefetch
        // before any op executes — the ADR-0005 pipeline on the batch the
        // fabric naturally provides; order per source pair is unchanged.
        self.reply_scratch.discard();
        self.staged_replies.clear();
        let apply_prefetch = self.shared.apply_prefetch.get();
        let shared = &self.shared;
        let scratch = &mut self.reply_scratch;
        let staged = &mut self.staged_replies;
        let stage = &mut self.apply_stage;
        let stage_bytes = &mut self.apply_stage_bytes;
        let mut orphans: u64 = 0;
        let now = cx.now;
        let drained = shared.fabric.borrow_mut().drain(FABRIC_DRAIN_MAX, |from, op| {
            if apply_prefetch {
                stage_or_handle(
                    shared,
                    now,
                    from,
                    op,
                    stage,
                    stage_bytes,
                    scratch,
                    staged,
                    &mut orphans,
                );
            } else {
                handle_fabric_op(shared, now, from, op, scratch, staged, &mut orphans);
            }
        });
        flush_apply_stage(shared, stage, stage_bytes, scratch, staged);
        // ADR-0177 D3: a verdict waiting with the `gated` task parked wakes
        // the task, even on a fabric-quiet iteration: an apply pump fills
        // its verdict in run_ready, not in this drain, and a task cannot
        // wake another (the lag is this one iteration).
        let gated_due = self.shared.gated_pump_parked.get() && self.shared.gated.waits();
        if drained == 0 && !gated_due {
            return;
        }
        if drained > 0 {
            cx.note_fabric(drained as u64);
        }

        let mut fabric = self.shared.fabric.borrow_mut();
        for _ in 0..orphans {
            fabric.note_orphan_reply();
        }
        #[cfg(test)]
        if self.reverse_replies {
            self.staged_replies.reverse();
            self.reversed_batch_max = self.reversed_batch_max.max(self.staged_replies.len());
        }
        let mut had_replies = false;
        for (to, token, reply) in self.staged_replies.drain(..) {
            had_replies = true;
            fabric.reply(to, token, &staged_outcome(self.reply_scratch.as_bytes(), &reply));
        }
        // Publish replies NOW instead of at FABRIC-OUT: the origin is
        // blocked on them, and waiting for step 8 adds most of an iteration
        // to every hop RTT (M0-R1 latency finding — hops were
        // window-latency-bound, not just CPU-bound).
        if had_replies {
            let published = fabric.flush();
            if published > 0 {
                cx.note_fabric(published as u64);
            }
        }
        drop(fabric);
        self.admit_adopted(cx);
        self.wake_peer_pumps(cx);
    }

    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        if !self.started && !self.arm_initial_listener(cx) {
            return;
        }
        self.shared.now.set(cx.now);
        // DEBUG SLEEP stall: connection processing pauses (inbox buffers
        // hold; pool pressure degrades to RecvDropped, never blocks the
        // thread); FABRIC-IN keeps serving peers. A stalled PARSE takes
        // nothing, so the take count does not move.
        if cx.now < self.shared.stall_until.get() {
            return;
        }

        let mut stage = core::mem::take(&mut self.parse_stage);
        let mut stage_bytes = core::mem::take(&mut self.parse_stage_bytes);
        let mut staging = Staging {
            enabled: self.shared.parse_prefetch.get(),
            stage: &mut stage,
            bytes: &mut stage_bytes,
        };
        let inbox = core::mem::take(&mut self.inbox);
        for (key, buf, len) in inbox {
            let commands = self.parse_entry(cx, key, buf, len, &mut staging);
            cx.pool.release(buf);
            cx.charge(GroupClass::Foreground, commands);
        }
        debug_assert!(stage.is_empty(), "parse stage flushed before buffer release");
        self.parse_stage = stage;
        self.parse_stage_bytes = stage_bytes;
        self.shared.note_inbox_take();
    }

    fn maintain(&mut self, cx: &mut LoopCx<'_>) {
        self.shared.now.set(cx.now);
        self.drive_stop();
        // ---- checkpoint-board sweep (ADR-0159 D4, A1.4): MAINTAIN's first
        // budgeted step, so nothing charged before it can starve it.
        self.maintain_ckpt_sweep(cx);
        // ---- early fabric publish (M2.5-S21): remote ops staged during
        // EXECUTE become peer-visible NOW instead of at FABRIC-OUT (step
        // 8) — the peer drains them while this cell runs MAINTAIN/LOG/
        // RESPOND, so the hop RTT overlaps local work instead of
        // following it (the request-path sibling of the M0-R1 "publish
        // replies NOW" finding; remote throughput is window/RTT-bound at
        // a connection's gate units, so RTT saved converts directly). One extra
        // Release store + doorbell per destination per iteration; step 8
        // stays for late stagers.
        if self.early_fabric_flush {
            let mut fabric = self.shared.fabric.borrow_mut();
            let published = fabric.flush();
            if published > 0 {
                cx.note_fabric(published as u64);
            }
        }
        // ---- boot recovery (M2-S15): budgeted replay steps while the
        // cell answers -LOADING; enable_durable fires on completion.
        if self.boot.is_some() {
            self.drive_recovery(cx);
        }
        // ---- node memory board (M3-S25): publish this cell's gauges on
        // a coarse cadence so every cell's INFO renders fresh node-wide
        // totals (the serving cell re-publishes its own slot at render
        // time; this keeps the *peer* slots current).
        self.maintain_boards();
        // ---- expiry slice (M1-S05): wheel ticks under the Maintenance
        // deficit budget; the `expiry_debt` lag escalates the slice (×1..16,
        // hard-capped) so storms drain on idle headroom while foreground
        // latency stays protected by the deficit scheduler.
        let budget = cx.budget(GroupClass::Maintenance);
        if budget > 0 {
            let escalation = (self.expiry_lag / 1024).min(15) as u32 + 1;
            let max_fires = budget.saturating_mul(escalation).min(MAX_EXPIRY_FIRES_PER_SLICE);
            let stats = self.shared.store.borrow_mut().expire_tick(
                cx.now,
                ExpiryBudget {
                    max_fires,
                    max_steps: max_fires.saturating_mul(8).max(4096),
                    max_sweep_slots: inf_store::limits::EXPIRY_SWEEP_SLOTS_PER_SLICE,
                },
            );
            self.expiry_lag = stats.lag_ms;
            // Fires (each one bounded group walk, whatever it reaped — the
            // unit `max_fires` budgets) and sweep reaps are foreground-
            // visible work; cursor steps and sweep slots are cheap walking
            // (ADR-0008 A1 rule 6), charged per `EXPIRY_WALK_PER_UNIT`.
            let fired = stats.fires_charged();
            let walked =
                stats.steps / EXPIRY_WALK_PER_UNIT + stats.sweep_slots / EXPIRY_WALK_PER_UNIT;
            let units = fired.min(u64::from(u32::MAX)) as u32 + walked;
            if units > 0 {
                cx.charge(GroupClass::Maintenance, units);
            }
        }
        // ---- index backfill (M4.5-S05, ADR-0077): budgeted walk slices
        // on the Maintenance class, then readiness publication and the D6
        // catalog flip. Guarded on registry emptiness (one Vec-len load
        // when the feature is unused) and on recovery completion — replay
        // maintains nothing by default (ADR-0076 D7), so a walk over a
        // half-replayed store would go stale silently.
        self.maintain_backfill(cx);
        // ---- CLIENT KILL sweep: the registry carries each id's slab key
        // (ADR-0124 D6), so the mark maps back to the connection.
        self.maintain_kills_and_config(cx);
        // ---- eviction slice (M1-S06/S07): budgeted clock/CMS sweep toward
        // the low watermark + CMS decay. A no-op without a configured limit
        // (the cached-flag refresh keeps the write path one branch).
        let evict_budget = cx.budget(GroupClass::Maintenance);
        if evict_budget > 0 {
            let stats = self
                .shared
                .store
                .borrow_mut()
                .evict_tick(cx.now, EvictBudget { max_evictions: evict_budget });
            let units = (stats.evicted + stats.scanned_slots / 64).min(u64::from(u32::MAX)) as u32;
            if units > 0 {
                cx.charge(GroupClass::Maintenance, units);
            }
        }
        // ---- durable plane (M2-S08): segment prealloc rides MAINTAIN
        // (rotation stays a pointer swap — S02); durable counters flush
        // into NodeInfo for INFO persistence (S21 vocabulary).
        self.maintain_durable(cx);
        // ---- tiered plane half (M4-S26): namespace sync, the four
        // drivers, the cold-read drain (once per iteration — S10).
        self.tier_maintain(cx);
        // ---- DDL wakes (ADR-0015 D3): two loads per MAINTAIN, the
        // catalog word and the ticket's generation; parked DDL pumps wake
        // on either's edge.
        self.maintain_control_wakes();
        // ---- stats flush
        self.maintain_conn_sweep(cx);
    }

    fn seal_log(&mut self, cx: &mut LoopCx<'_>) {
        if let Some(cell) = self.shared.durable.borrow_mut().as_mut() {
            cell.seal_log(cx);
        }
    }

    fn respond(&mut self, cx: &mut LoopCx<'_>) {
        // Replies (including DEBUG SLEEP's own +OK) hold until a stall ends.
        if cx.now < self.shared.stall_until.get() {
            return;
        }
        self.push_mark_rearms(cx);
        let keys = self.shared.conns.borrow().keys();
        for key in keys {
            let close = self.shared.with_conn(key, |conn| {
                match &mut conn.slot {
                    ConnSlot::Active { link, recv, .. } => {
                        if *recv == RecvArm::RearmDue {
                            *recv = RecvArm::Armed;
                            cx.push(IoOp::RecvArm {
                                fd: link.fd,
                                token: Self::token(TokenClass::Recv, key),
                            });
                        }
                        Self::send_out(cx, key, link);
                    }
                    ConnSlot::Flushing { link, .. } => Self::send_out(cx, key, link),
                    ConnSlot::Closing { .. } | ConnSlot::Draining { .. } => {
                        // Canary: a closing slot is sent bytes from a copy of
                        // its descriptor.
                        #[cfg(inf_canary_closing_slot_sends)]
                        if let (ConnSlot::Closing { .. }, Some(fd)) = (&conn.slot, conn.closing_fd)
                        {
                            let mut link = Link::new(fd, 0);
                            RespWriter::new(link.out.open(), Protocol::Resp2).simple("OK");
                            Self::send_out(cx, key, &mut link);
                        }
                        return None;
                    }
                }
                // A flushing slot closes once every reply it owed is sent.
                conn.flush_end()
            });
            self.push_close(cx, key, close.flatten());
        }
    }

    fn fabric_out(&mut self, cx: &mut LoopCx<'_>) -> bool {
        let mut fabric = self.shared.fabric.borrow_mut();
        let published = fabric.flush();
        if published > 0 {
            cx.note_fabric(published as u64);
        }
        fabric.doorbell_pending() || fabric.staged_frames() > 0
    }
}

/// The MAINTAIN phases, one method per concern (`maintain` reads as the
/// phase list).
impl<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static> ServerPlane<O, F> {
    /// One MAINTAIN concern of `maintain`, in phase order:
    ///  node memory board (M3-S25): publish this cell's gauges on
    /// a coarse cadence so every cell's INFO renders fresh node-wide
    /// totals (the serving cell re-publishes its own slot at render
    fn maintain_boards(&mut self) {
        self.memory_publish_in -= 1;
        if self.memory_publish_in == 0 {
            self.memory_publish_in = 64;
            let node = &self.shared.node;
            if let Some(board) = node.memory_board.borrow().as_ref() {
                let store = self.shared.store.borrow();
                #[cfg_attr(not(feature = "doc"), allow(unused_mut))]
                let mut report = store.report();
                #[cfg(feature = "doc")]
                node.add_cell_doc_memory(&mut report);
                board
                    .slot(self.shared.cell.0)
                    .publish(crate::exec::memory_gauges_of(&report, node, &store));
            }
        }
        if let Some(board) = &self.loading_board {
            let node = &self.shared.node;
            let (done, total) = board.bytes();
            node.loading_start_unix_ms.set(board.start_unix_ms());
            node.loading_loaded_bytes.set(done);
            node.loading_total_bytes.set(total);
            node.loading_cells_ready.set(board.ready_cells());
            if board.all_ready() {
                // ADR-0174 D6's node fold, once: every cell has published.
                node.recover_tier_replay.set(board.tier_replay_totals());
                node.recover_step_facts.set(board.step_facts_totals());
                node.loading.set(0);
                self.shared.loading.set(false);
                self.loading_board = None;
            } else {
                node.loading.set(1);
            }
        }
    }

    /// One MAINTAIN concern of `maintain`, in phase order:
    ///  index backfill (M4.5-S05, ADR-0077): budgeted walk slices
    /// on the Maintenance class, then readiness publication and the D6
    /// catalog flip. Guarded on registry emptiness (one Vec-len load
    #[cfg_attr(not(feature = "doc"), allow(unused_variables, reason = "the walk is doc-only"))]
    fn maintain_backfill(&mut self, cx: &mut LoopCx<'_>) {
        #[cfg(feature = "doc")]
        if self.boot.is_none() {
            let mut store = self.shared.store.borrow_mut();
            if !store.idx_registry().is_empty() {
                let budget = cx.budget(GroupClass::Maintenance);
                if budget > 0 {
                    let slice = inf_store::BackfillBudget {
                        max_docs: budget.saturating_mul(4).min(MAX_BACKFILL_DOCS_PER_TICK),
                        max_steps: budget.saturating_mul(32).min(MAX_BACKFILL_STEPS_PER_TICK),
                    };
                    let stats = store.idx_backfill_tick(cx.now, slice);
                    let units = (stats.docs_scanned + stats.reaped).min(u64::from(u32::MAX)) as u32
                        + stats.steps / 64;
                    if units > 0 {
                        cx.charge(GroupClass::Maintenance, units);
                    }
                }
                if let Some(control) = self.shared.control.borrow().as_ref() {
                    let board = control.index_board();
                    let cell = self.shared.cell.0;
                    // Republish every tick (ADR-0077 D4): ≤ 64 relaxed
                    // stores, and it makes the D5 rank rule self-healing.
                    for (slot, generation) in store.idx_ready_reports() {
                        board.publish_ready(cell, slot, generation);
                    }
                    let mut flipped = false;
                    for (id, slot, generation) in store.idx_fleet_candidates() {
                        if board.fleet_ready(slot, generation) {
                            // The ADR-0077 D6 flip: monotone, per cell,
                            // generation-exact.
                            store
                                .idx_registry_mut()
                                .set_catalog_state(id, inf_store::IndexState::Ready)
                                .expect("Backfilling → Ready is an ADR-0075 D3 edge");
                            flipped = true;
                        }
                    }
                    // One writer persists the flip (cell 0) — the durable
                    // `ready` is the ADR-0075 D4 rebuild-class hint S06's
                    // sidecar load reads at the next boot.
                    if flipped && cell == 0 {
                        control.request_persist(store.export_catalog(
                            control.next_ns_id(),
                            control.next_index_id(),
                            control.next_index_generation(),
                        ));
                    }
                }
            }
        }
    }

    /// One MAINTAIN concern of `maintain`, in phase order:
    ///  CLIENT KILL sweep: the registry carries each id's slab key
    /// (ADR-0124 D6), so the mark maps back to the connection.
    fn maintain_kills_and_config(&mut self, cx: &mut LoopCx<'_>) {
        let kills = {
            let mut clients = self.shared.node.clients.borrow_mut();
            let ids = clients.take_kill_requests();
            ids.into_iter().filter_map(|id| clients.conn_key(id)).collect::<Vec<u64>>()
        };
        for key in kills.into_iter().map(ConnKey::unpack) {
            self.close_at_once(cx, key);
        }
        // ---- pressure config push (M1-E3, hot-per-cell within one MAINTAIN
        // round): one u64 version compare per iteration; a real push only
        // when CONFIG SET (or boot wiring) touched the store.
        let config_version = self.shared.node.config.borrow().version();
        if config_version != self.config_pushed {
            self.config_pushed = config_version;
            crate::admin::push_pressure(&mut self.shared.store.borrow_mut(), &self.shared.node);
            // The connection knobs ride the same hot-per-cell sweep
            // (M1-S11 output caps; ADR-0123 `maxclients`/`timeout`/
            // `tcp-keepalive`).
            self.shared.knobs.set(crate::config::conn_knobs(
                &self.shared.node.config.borrow(),
                self.shared.cells,
            ));
            // `proto-max-bulk-len` (ADR-0122 D2): the accept-time limits,
            // and every live parser of this cell — bounded by the slab,
            // once per config change, never per command.
            let limits = crate::config::parser_limits(&self.shared.node.config.borrow());
            if limits != self.shared.parser_limits.get() {
                self.shared.parser_limits.set(limits);
                self.shared.conns.borrow_mut().for_each_mut(|conn| match &mut conn.slot {
                    ConnSlot::Active { parser, .. }
                    | ConnSlot::Flushing {
                        stream: PeerStream::Ended { by: ClosedBy::Peer { parser, .. } },
                        ..
                    }
                    | ConnSlot::Closing { by: ClosedBy::Peer { parser, .. } }
                    | ConnSlot::Draining { by: ClosedBy::Peer { parser, .. } } => {
                        parser.set_limits(limits);
                    }
                    ConnSlot::Flushing {
                        stream: PeerStream::Open | PeerStream::Ended { by: ClosedBy::Plane },
                        ..
                    }
                    | ConnSlot::Closing { by: ClosedBy::Plane }
                    | ConnSlot::Draining { by: ClosedBy::Plane } => {}
                });
            }
        }
    }

    /// One MAINTAIN concern of `maintain`, in phase order:
    ///  durable plane (M2-S08): segment prealloc rides MAINTAIN
    /// (rotation stays a pointer swap — S02); durable counters flush
    /// into NodeInfo for INFO persistence (S21 vocabulary).
    fn maintain_durable(&mut self, cx: &mut LoopCx<'_>) {
        if let Some(cell) = self.shared.durable.borrow_mut().as_mut() {
            let write_through_wanted = self.shared.store.borrow().has_always_namespace();
            cell.maintain(cx, write_through_wanted);
            // Manual checkpoint requests: the larger of the all-cell word
            // and this cell's slot (two Acquire loads, ADR-0159 A1.3/A1.6),
            // edge-detected — the persisted-epoch pattern (ADR-0016 D7).
            if let Some(control) = self.shared.control.borrow().as_ref() {
                let epoch = control.ckpt_board().requested(self.shared.cell.0);
                if epoch != self.ckpt_epoch_seen {
                    self.ckpt_epoch_seen = epoch;
                    cell.request_ckpt(epoch);
                }
            }
            // ---- checkpoint slice (M2-S10, ADR-0016 D5): its own deficit
            // class — a 10 GB walk can't starve expiry, and vice versa.
            let ckpt_budget = cx.budget(GroupClass::Checkpoint);
            if ckpt_budget > 0 {
                let anchor = self.shared.wall_anchor();
                let mut tier = self.shared.tier.borrow_mut();
                let used = cell.ckpt_slice(
                    &mut self.shared.store.borrow_mut(),
                    tier.as_mut(),
                    cx,
                    ckpt_budget,
                    anchor,
                );
                drop(tier);
                if used > 0 {
                    cx.charge(GroupClass::Checkpoint, used);
                }
            }
            // ---- MANIFEST + truncation slice (M2-S11/S12, ADR-0017):
            // watermark-gated swap machine (barriers ride the driver),
            // bounded segment forgets (unlinks delegated to the control
            // thread), orphan GC — charged to Maintenance.
            let control = self.shared.control.borrow();
            let unix_now_ms = self.shared.wall_anchor().unix_from_internal(cx.now);
            let mut tier = self.shared.tier.borrow_mut();
            let manifest_units = cell.manifest_slice(
                cx,
                control.as_deref(),
                unix_now_ms,
                &mut self.shared.store.borrow_mut(),
                tier.as_mut(),
            );
            drop(tier);
            drop(control);
            if manifest_units > 0 {
                cx.charge(GroupClass::Maintenance, manifest_units);
            }
            publish_durable_stats(&self.shared.node, cell.stats());
            let node = &self.shared.node;
            let ckpt = cell.ckpt_stats();
            let unix_now_ms = self.shared.wall_anchor().unix_from_internal(cx.now);
            node.ckpt_age_s.set(if ckpt.last_unix_ms == 0 {
                0
            } else {
                unix_now_ms.saturating_sub(ckpt.last_unix_ms) / 1000
            });
            node.ckpts_completed.set(ckpt.completed);
            node.ckpt_walks_behind.set(ckpt.walks_behind);
            node.ckpt_bound_splits.set(ckpt.bound_splits);
            node.ckpts_aborted.set(ckpt.aborted);
            node.ckpt_last_unix_ms.set(ckpt.last_unix_ms);
            node.ckpt_last_begin_lsn.set(ckpt.last_begin_lsn);
            node.ckpt_buffer_bytes.set(ckpt.buffer_bytes);
        }
    }

    /// Whether this cell's next completed sweep can observe what its last
    /// one could not: the sweep is part-way through the board (ADR-0159
    /// A1.4), or the cell's own slot published since the start of the last
    /// completed sweep (`interfaces-m2.md`, "Cells never fold the whole
    /// board"). False on a cell with no control plane.
    fn ckpt_observation_owed(&self) -> bool {
        let sweep = self.shared.ckpt_sweep.borrow();
        // Canary: the guard without its cursor term.
        #[cfg(not(inf_canary_ckpt_park_guard_cursor_skipped))]
        if sweep.in_progress() {
            return true;
        }
        // Canary: the guard without its own-slot term.
        #[cfg(inf_canary_ckpt_park_guard_own_slot_skipped)]
        return false;
        #[cfg(not(inf_canary_ckpt_park_guard_own_slot_skipped))]
        self.shared
            .control
            .borrow()
            .as_ref()
            .is_some_and(|control| control.ckpt_board().own_unobserved(&sweep))
    }

    /// One MAINTAIN concern of `maintain`, in phase order: the
    /// checkpoint-board sweep (ADR-0159 D4, A1.4). One step of at most
    /// `CKPT_BOARD_VISITS_PER_TURN` slot visits, charged one Maintenance
    /// unit. A completed sweep publishes the observation `WAIT` and DROP
    /// pacing read, and with it the cell's `LastSave`; when any slot
    /// published since the last one, the parked `INF.CKPT WAIT` pumps
    /// re-check their target (the M2-S20 wake).
    fn maintain_ckpt_sweep(&mut self, cx: &mut LoopCx<'_>) {
        let control = self.shared.control.borrow();
        let Some(control) = control.as_ref() else { return };
        if cx.budget(GroupClass::Maintenance) == 0 {
            return;
        }
        let step = control
            .ckpt_board()
            .sweep_step(&mut self.shared.ckpt_sweep.borrow_mut(), &self.shared.node.lastsave);
        cx.charge(GroupClass::Maintenance, 1);
        match step {
            crate::control::SweepStep::InProgress | crate::control::SweepStep::Unchanged => {}
            crate::control::SweepStep::Progressed => {
                self.shared.ckpt_waiters.wake_all(0);
            }
        }
    }

    /// One MAINTAIN concern of `maintain`, in phase order:
    ///  DDL wakes (ADR-0015 D3): two Acquire loads per MAINTAIN, the
    /// catalog word and the ticket's generation; parked DDL pumps wake on
    /// either's edge. `INFO` reads the writer's gauges itself.
    fn maintain_control_wakes(&mut self) {
        if let Some(control) = self.shared.control.borrow().as_ref() {
            let epoch = control.persisted_epoch();
            if epoch != self.shared.ddl_epoch_seen.get() {
                self.shared.ddl_epoch_seen.set(epoch);
                self.shared.ddl_waiters.wake_all(0);
            }
            // ---- DDL-ticket releases (ADR-0108 D1): the same waitlist,
            // one more edge — a program parked on the ticket retries.
            let generation = control.ddl_generation();
            if generation != self.shared.ddl_gen_seen.get() {
                self.shared.ddl_gen_seen.set(generation);
                self.shared.ddl_waiters.wake_all(0);
            }
        }
    }

    /// One MAINTAIN concern of `maintain`, in phase order:
    ///  stats flush
    fn maintain_conn_sweep(&mut self, cx: &mut LoopCx<'_>) {
        let node = &self.shared.node;
        node.recv_dropped.set(self.shared.recv_dropped.get());
        node.waker_foreign_ops.set(cx.executor.waker_foreign_ops());
        node.accept_errors.set(self.shared.accept_errors.get());
        node.timers_armed_max.set(u64::from(cx.timers().armed_max()));
        {
            let gate = self.shared.gate.borrow();
            node.fabric_rtt_p50_ns.set(gate.round_trips().percentile(50.0));
            node.gate_full.set(gate.full());
            node.gate_stale.set(gate.stale());
        }
        {
            let ps = self.shared.pubsub.borrow();
            node.pubsub_channels.set(ps.live_owned_channel_count());
            node.pubsub_patterns.set(ps.live_pattern_count());
            node.pubsub_state_bytes.set(ps.state_bytes() as u64);
        }
        let knobs = self.shared.knobs.get();
        let now_ms = cx.now.as_millis();
        let mut conns = self.shared.conns.borrow_mut();
        let mut bytes = 0usize;
        let mut draining = 0u64;
        let mut idle: Vec<ConnKey> = Vec::new();
        let ConnSlab { slots, gens, .. } = &mut *conns;
        for (slot, entry) in slots.iter_mut().enumerate() {
            let Some(conn) = entry.as_mut() else { continue };
            bytes += conn.state_bytes();
            let Conn { cx: conn_cx, slot: state, .. } = conn;
            // `timeout` and the output cap read the two states that hold
            // the descriptor and `out`; a closing or draining slot is
            // skipped, so `idle_disconnections` counts each close once.
            let link = match state {
                ConnSlot::Active { link, .. } | ConnSlot::Flushing { link, .. } => link,
                ConnSlot::Closing { .. } => continue,
                ConnSlot::Draining { .. } => {
                    draining += 1;
                    continue;
                }
            };
            // The output-buffer class follows the subscription state
            // (Redis's CLIENT_PUBSUB flag; ADR-0123 D4). Soft-cap aging
            // continues between deliveries (M1-S11): a stalled client
            // over the soft limit dies on schedule even when nothing
            // more is written to it.
            let subscribed = !conn_cx.sub_channels.is_empty() || !conn_cx.sub_patterns.is_empty();
            let caps = if subscribed { knobs.cob_pubsub } else { knobs.cob_normal };
            if link.cob_soft_since_ms != 0 || caps != (0, 0, 0) {
                enforce_output_cap(node, link, conn_cx.id, now_ms, caps);
            }
            // `timeout` (ADR-0123 D2): idle unsubscribed connections
            // close, a flushing one as an active one (ADR-0193 D1);
            // subscribers are exempt, as in Redis.
            if knobs.timeout_ms != 0
                && !subscribed
                && now_ms.saturating_sub(link.last_active_ms) >= knobs.timeout_ms
            {
                idle.push(ConnKey { slot: slot as u32, generation: gens[slot] });
            }
        }
        // `active` (ADR-0123 A1): Active, Flushing and Closing slots; a
        // draining slot is the third population, read beside it.
        node.connections.set(conns.live as u64 - draining);
        node.draining_connections.set(draining);
        node.conn_state_bytes.set(bytes as u64);
        drop(conns);
        // Recycle-pool residency (v0.4.0-alpha RSS-attribution gauges):
        // running sums maintained at the push/pop sites, flushed here.
        node.reply_pool_bytes.set(self.shared.reply_pool_bytes.get());
        node.cmd_pool_bytes.set(self.shared.cmd_pool_bytes.get());
        let shared = Rc::clone(&self.shared);
        for key in idle {
            shared.node.idle_disconnections.set(shared.node.idle_disconnections.get() + 1);
            self.close_at_once(cx, key);
        }
    }
}

/// Flushes the durable cell's counters into `NodeInfo` for `INFO
/// persistence` (the S21 vocabulary), once per MAINTAIN.
fn publish_durable_stats(node: &NodeInfo, stats: crate::durable::DurableStats) {
    node.log_records_appended.set(stats.records_appended);
    node.log_pending_bytes.set(stats.pending_log_bytes);
    node.log_last_durable_lsn.set(stats.last_durable_lsn);
    node.log_watermark_lag.set(stats.watermark_lag_lsn);
    node.log_fsyncs_completed.set(stats.fsyncs_completed);
    node.log_acks_gated.set(stats.acks_gated);
    node.log_frames_queued.set(stats.frames_queued);
    node.log_staging_bytes.set(stats.staging_resident_bytes);
    node.manifests_published.set(stats.manifests_published);
    node.manifests_aborted.set(stats.manifests_aborted);
    node.ckpt_in_progress.set(stats.ckpt_in_progress);
    node.segments_truncated.set(stats.segments_truncated);
    node.fsyncs_per_sec.set(stats.fsyncs_per_sec);
    node.acks_per_sec.set(stats.acks_per_sec);
    node.fsync_p50_us.set(stats.fsync_p50_us);
    node.fsync_p99_us.set(stats.fsync_p99_us);
    node.fsync_p999_us.set(stats.fsync_p999_us);
    node.fsync_group_p50.set(stats.fsync_group_p50);
    node.fsync_group_p99.set(stats.fsync_group_p99);
    node.log_write_stall_p50_us.set(stats.write_stall_p50_us);
    node.log_write_stall_p99_us.set(stats.write_stall_p99_us);
    node.log_write_stall_p999_us.set(stats.write_stall_p999_us);
    node.log_staging_capacity.set(stats.staging_capacity_bytes);
    node.log_admission_parked.set(stats.admission_parked);
    node.log_admission_parked_total.set(stats.admission_parked_total);
    node.fsyncs_linked.set(stats.fsyncs_linked);
    node.fsyncs_seal.set(stats.fsyncs_seal);
    node.fsyncs_standalone.set(stats.fsyncs_standalone);
    node.barrier_class_fua.set(stats.barrier_class_fua);
    node.io_class_configured_fua.set(stats.io_class_configured_fua);
    node.fsyncs_fua.set(stats.fsyncs_fua);
    node.fua_p50_us.set(stats.fua_p50_us);
    node.fua_p99_us.set(stats.fua_p99_us);
    node.log_padding_bytes.set(stats.log_padding_bytes);
    node.zero_fill_bytes.set(stats.zero_fill_bytes);
    node.rotations_unzeroed.set(stats.rotations_unzeroed);
    node.rotations_upgrade.set(stats.rotations_upgrade);
    node.reopened_packed_tails.set(stats.reopened_packed_tails);
    node.barrier_class_degraded.set(stats.barrier_class_degraded);
    node.frames_in_flight.set(stats.frames_in_flight);
    node.frames_in_flight_max.set(stats.frames_in_flight_max);
    node.frame_waits_barrier.set(stats.frame_waits_barrier);
    node.frame_waits_rotation.set(stats.frame_waits_rotation);
    node.frame_waits_reorder.set(stats.frame_waits_reorder);
    node.frame_waits_ledger.set(stats.frame_waits_ledger);
    node.frame_waits_ack_map.set(stats.frame_waits_ack_map);
    node.ledger_prealloc_deferrals.set(stats.ledger_prealloc_deferrals);
    node.frame_waits_extent.set(stats.frame_waits_extent);
    node.extent_parks.set(stats.extent_parks);
    node.turn_refused.set(stats.turn_refused);
    node.frame_waits_fill.set(stats.frame_waits_fill);
    node.fill_window_us.set(stats.fill_window_us);
    node.fill_target_bytes.set(stats.fill_target_bytes);
    node.frame_waits_group.set(stats.frame_waits_group);
    node.flush_group_window_us.set(stats.flush_group_window_us);
    node.frame_records_last.set(stats.frame_records_last);
    node.group_round_target.set(stats.group_round_target);
    node.io_provenance.set(stats.io_provenance);
    node.fsyncs_completion.set(stats.fsyncs_completion);
    node.log_segments_live.set(stats.log_segments_live);
    // M4.5-S36 (ADR-0088 D7): the device budget's ledger, the
    // checkpoint domain's bytes, the derived trigger, the figure.
    node.io_budget.set(stats.io_budget);
    node.io_budget_model_absent.set(stats.io_budget_model_absent);
    node.io_budget_write_bytes_per_s.set(stats.io_budget_write_bytes_per_s);
    node.io_budget_read_bytes_per_s.set(stats.io_budget_read_bytes_per_s);
    node.frame_waits_pace.set(stats.frame_waits_pace);
    node.log_frame_bytes.set(stats.log_frame_bytes);
    node.ckpt_bytes_total.set(stats.ckpt_bytes_total);
    node.ckpt_bytes_last.set(stats.ckpt_bytes_last);
    node.ckpt_padding_bytes.set(stats.ckpt_padding_bytes);
    node.manifest_bytes_total.set(stats.manifest_bytes_total);
    node.ckpt_interval_bytes.set(stats.ckpt_interval_bytes);
    node.ckpt_replay_bytes_per_s.set(stats.ckpt_replay_bytes_per_s);
    node.ckpt_cap_bytes.set(stats.ckpt_cap_bytes);
    node.ckpt_records_since_begin.set(stats.ckpt_records_since_begin);
    node.ckpt_io_mode_buffered.set(stats.ckpt_io_mode_buffered);
    node.ckpt_io_mode_downgrades.set(stats.ckpt_io_mode_downgrades);
    node.write_amp_milli_log_checkpoint.set(stats.write_amp_milli_log_checkpoint);
    node.write_amp_log_checkpoint_undefined.set(stats.write_amp_log_checkpoint_undefined);
    node.accounted_host_write_bytes.set(stats.accounted_host_write_bytes);
    node.write_amp_milli_accounted_host.set(stats.write_amp_milli_accounted_host);
    node.segments_recycled.set(stats.segments_recycled);
    node.recycle_misses.set(stats.recycle_misses);
    node.recycle_fallbacks.set(stats.recycle_fallbacks);
    node.recycle_pool_bytes.set(stats.recycle_pool_bytes);
    node.recycle_pool_full.set(stats.recycle_pool_full);
    node.recycle_sentinels.set(stats.recycle_sentinels);
    node.segment_rotations.set(stats.segment_rotations);
    node.segment_preallocs.set(stats.segment_preallocs);
    node.segment_inline_preallocs.set(stats.segment_inline_preallocs);
    node.segment_prealloc_failures.set(stats.segment_prealloc_failures);
    node.recycle_waits_started.set(stats.recycle_waits_started);
    node.recycle_waits_satisfied.set(stats.recycle_waits_satisfied);
    node.recycle_waits_expired.set(stats.recycle_waits_expired);
    node.recycle_wait_active_bytes_max.set(stats.recycle_wait_active_bytes_max);
}

/// Records a cold-read receipt the dispatch decided (ADR-0181 R1). A foreign
/// receipt is the host's to count; the dispatch has nothing more to do with
/// either answer.
fn record_cold_receipt(
    cold: &inf_runtime::ColdReads,
    token: inf_runtime::CompletionToken,
    result: Result<(), i32>,
    now_us: u64,
) {
    match cold.record_completion(token, result, now_us) {
        inf_runtime::ColdReceipt::Recorded | inf_runtime::ColdReceipt::Foreign => {}
    }
}
