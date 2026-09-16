//! `impl CellPlane for ServerPlane` — the reactor-facing loop: completion
//! handling, timers, PARSE+EXECUTE, FABRIC-IN, MAINTAIN, and the park/seal/
//! respond hooks. Nothing here holds a lease across a suspension (L6).

use super::*;

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
                    let live = self.shared.with_conn(key, |conn| !conn.closing).unwrap_or(false);
                    if live {
                        self.initiate_close(cx, key);
                    }
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
                let key = Self::key_of(c.token);
                self.shared.with_conn(key, |conn| conn.send_inflight = false);
            }
            CompletionResult::Closed => {
                let key = Self::key_of(c.token);
                let removed = self.shared.conns.borrow_mut().remove(key);
                if let Some(conn) = &removed {
                    self.shared.node.clients.borrow_mut().unregister(conn.cx.id);
                }
                // Pub/sub cleanup (M1-S10): drop the connection from the
                // local registries; 1→0 transitions notify the channel
                // owners / every cell (patterns) off the close path.
                if let Some(conn) = removed {
                    let notes = unsubscribe_closed_conn(&self.shared, key, &conn.cx);
                    if !notes.is_empty() {
                        let shared = Rc::clone(&self.shared);
                        let _ = cx.executor.poll_immediate(flush_sub_deltas(shared, notes));
                    }
                }
            }
            CompletionResult::Error { buf, errno } => {
                if let Some(buf) = buf {
                    cx.pool.release(buf);
                }
                // Tier-flush op failures (M4.5-S31, ADR-0084 D4) are the
                // round's to classify at the next MAINTAIN — ENOSPC
                // latches admission and retries; a failed barrier is the
                // §8.4 fatal class there. Never connection housekeeping.
                if matches!(c.token.class(), TokenClass::TierFlushWrite | TokenClass::TierFlushSync)
                {
                    let mut tier = self.shared.tier.borrow_mut();
                    let tier = tier.as_mut().expect("tier-flush completion implies tier state");
                    tier.on_flush_completion(c.token, Some(errno));
                    return;
                }
                // Log-op failures are fail-stop territory (§8.4), never
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
                if c.token.class() == TokenClass::TierRead {
                    let tier = self.shared.tier.borrow();
                    let cold = tier
                        .as_ref()
                        .and_then(|t| t.cold.as_ref())
                        .expect("TierRead completion implies a live cold-read engine");
                    cold.on_completion(
                        c.token,
                        CompletionResult::Error { buf: None, errno },
                        cx.now.as_micros(),
                    );
                    return;
                }
                // Accept-path failures (`EMFILE`/`ENFILE`) belong to the
                // listener, never to a connection (F-L11-05): count them.
                // The driver has PARKED the arm (F-L11-02); the retry
                // wheel re-arms it at a bounded cadence.
                if c.token.class() == TokenClass::Accept {
                    self.shared.accept_errors.set(self.shared.accept_errors.get() + 1);
                    if !self.accept_retry_armed {
                        self.accept_retry_armed = true;
                        cx.timers.insert(cx.now + ACCEPT_RETRY, ACCEPT_RETRY_TIMER_KEY);
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
                let key = Self::key_of(c.token);
                let live = self
                    .shared
                    .with_conn(key, |conn| {
                        conn.send_inflight = false;
                        !conn.closing
                    })
                    .unwrap_or(false);
                if live {
                    self.initiate_close(cx, key);
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
                    _ => cell.on_log_written(cx, c.token),
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
                // M4-S26: route into the custody table — waiters wake and
                // their futures run in this iteration's EXECUTE slice.
                let tier = self.shared.tier.borrow();
                let cold = tier
                    .as_ref()
                    .and_then(|t| t.cold.as_ref())
                    .expect("TierRead completion implies a live cold-read engine");
                cold.on_completion(c.token, CompletionResult::TierRead, cx.now.as_micros());
            }
        }
    }

    fn on_timer(&mut self, cx: &mut LoopCx<'_>, key: u64) {
        if key == EVERYSEC_TIMER_KEY
            && let Some(cell) = self.shared.durable.borrow_mut().as_mut()
        {
            cell.on_everysec_tick(cx);
        }
        if key == ACCEPT_RETRY_TIMER_KEY {
            // Idempotent on an armed listener; resumes a parked one. A
            // still-exhausted accept fails again → one more count and one
            // more window (bounded by wall time, F-L11-02).
            self.accept_retry_armed = false;
            cx.push(IoOp::AcceptArm {
                listener: self.listener,
                token: CompletionToken::new(TokenClass::Accept, CONN_SLOT_CAP, 0),
            });
        }
        // `FILL_TIMER_KEY` (M4.5-S39a) needs no handler: the wake is the
        // effect — this iteration's LOG step seals the held frame.
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
        self.reply_scratch.clear();
        self.staged_replies.clear();
        let apply_prefetch = self.shared.apply_prefetch.get();
        let shared = &self.shared;
        let scratch = &mut self.reply_scratch;
        let staged = &mut self.staged_replies;
        let stage = &mut self.apply_stage;
        let stage_bytes = &mut self.apply_stage_bytes;
        let mut orphans: u64 = 0;
        let mut pubs: Vec<OwnerPub> = Vec::new();
        let mut gated: Vec<GatedReply> = Vec::new();
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
                    &mut pubs,
                    &mut gated,
                    &mut orphans,
                );
            } else {
                handle_fabric_op(
                    shared,
                    now,
                    from,
                    op,
                    scratch,
                    staged,
                    &mut pubs,
                    &mut gated,
                    &mut orphans,
                );
            }
        });
        flush_apply_stage(shared, stage, stage_bytes, scratch, staged, &mut pubs);
        // M4.5-S29: gated verdicts the tiered apply pumps queued since the
        // last pass join this pass's deferred-reply spawns below. They must
        // drain even on a fabric-quiet iteration — their producers ran in
        // run_ready, not in this drain.
        gated.extend(self.shared.pump_gated.borrow_mut().drain(..));
        if drained == 0 && gated.is_empty() {
            return;
        }
        if drained > 0 {
            cx.note_fabric(drained as u64);
        }

        let mut fabric = self.shared.fabric.borrow_mut();
        for _ in 0..orphans {
            fabric.note_orphan_reply();
        }
        let mut had_replies = false;
        for (to, token, reply) in self.staged_replies.drain(..) {
            had_replies = true;
            match reply {
                StagedReply::Bytes(start, end) => {
                    fabric.reply(to, token, &Outcome::Bytes(&self.reply_scratch[start..end]));
                }
                StagedReply::Int(n) => fabric.reply(to, token, &Outcome::Int(n)),
                StagedReply::Nil => fabric.reply(to, token, &Outcome::Nil),
                StagedReply::Ok => fabric.reply(to, token, &Outcome::Ok),
                StagedReply::Refused => {
                    fabric.reply(to, token, &Outcome::Err(ErrCode::Unknown(0)));
                }
            }
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
        // Owner-side `always` applies (M2-S08, ADR-0015 D6): the fabric
        // reply itself is deferred — a future awaits this cell's ack gate,
        // then publishes the reply (flushed by the next FABRIC-OUT). The
        // client-visible ack never precedes the owning cell's fsync.
        for g in gated {
            let shared = Rc::clone(&self.shared);
            let _ = cx.executor.poll_immediate(async move {
                let waiter = {
                    let durable = shared.durable.borrow();
                    durable
                        .as_ref()
                        .expect("gated reply implies durable plane")
                        .ack_gate
                        .waiter(g.seq)
                };
                waiter.await;
                shared.fabric.borrow_mut().reply(g.to, g.token, &Outcome::Bytes(&g.reply));
            });
        }
        // Tiered fabric applies (M4-S26): wake each origin's FIFO pump.
        let tier_pending: Vec<u16> = {
            let queues = self.shared.ns_applies.borrow();
            let active = self.shared.ns_pump_active.borrow();
            (0..queues.len())
                .filter(|&i| !queues[i].is_empty() && !active[i])
                .map(|i| i as u16)
                .collect()
        };
        for origin in tier_pending {
            self.shared.ns_pump_active.borrow_mut()[usize::from(origin)] = true;
            let shared = Rc::clone(&self.shared);
            let _ = cx.executor.poll_immediate(ns_apply_pump(shared, origin));
        }
        // Fabric-origin PUBLISHes fan out on this cell's owner pump (one
        // long-lived FIFO future — arrival order is delivery order). The
        // reply to each publisher ships when its fan acks return.
        if !pubs.is_empty() {
            self.shared.pub_queue.borrow_mut().extend(pubs);
            if !self.shared.pub_pump_active.get() {
                self.shared.pub_pump_active.set(true);
                let shared = Rc::clone(&self.shared);
                let _ = cx.executor.poll_immediate(owner_pub_pump(shared));
            }
        }
    }

    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        if !self.started {
            self.started = true;
            // The listener rides the reserved top slot no connection ever
            // holds (F-L11-05): an accept-class completion can never share
            // `{slot, generation}` with a live connection.
            cx.push(IoOp::AcceptArm {
                listener: self.listener,
                token: CompletionToken::new(TokenClass::Accept, CONN_SLOT_CAP, 0),
            });
        }
        if !self.everysec_armed && self.shared.durable.borrow().is_some() {
            self.everysec_armed = true;
            cx.timers.insert(cx.now + Nanos::from_secs(1), EVERYSEC_TIMER_KEY);
        }
        self.shared.now.set(cx.now);
        // DEBUG SLEEP stall: connection processing pauses (inbox buffers
        // hold; pool pressure degrades to RecvDropped, never blocks the
        // thread); FABRIC-IN keeps serving peers.
        if cx.now < self.shared.stall_until.get() {
            return;
        }

        let stage_enabled = self.shared.parse_prefetch.get();
        let mut stage = core::mem::take(&mut self.parse_stage);
        let mut stage_bytes = core::mem::take(&mut self.parse_stage_bytes);
        let inbox = core::mem::take(&mut self.inbox);
        for (key, buf, len) in inbox {
            let mut commands: u32 = 0;
            // First command that must defer to a pump (everything after it
            // defers too — replies are ordered per connection).
            let mut deferred: Vec<OwnedCmd> = Vec::new();
            let mut spawn_first: Option<OwnedCmd> = None;
            let mut protocol_error = false;
            let mut quit = false;
            {
                let mut conns = self.shared.conns.borrow_mut();
                let Some(conn) = conns.get_mut(key) else {
                    cx.pool.release(buf);
                    continue;
                };
                if conn.closing || conn.close_after_flush {
                    cx.pool.release(buf);
                    continue;
                }
                conn.last_active_ms = cx.now.as_millis();
                let data = &cx.pool.bytes(buf)[..len as usize];
                let pump_was_active = conn.pump_active;
                // Field split: the parser iterator borrows `conn.parser`
                // while execution writes `conn.out`/`conn.cx`.
                let Conn { parser, cx: conn_cx, out, .. } = &mut *conn;
                let origin = ExecOrigin::Conn(key.slot, key.generation);
                let mut iter = parser.feed(data);
                while let Some(parsed) = iter.next() {
                    match parsed {
                        Parsed::Command(argv) | Parsed::Inline(argv) => {
                            commands += 1;
                            let meta = lookup(argv.arg(0));
                            // M2-S15 `-LOADING` gate: while the node loads,
                            // only LOADING-flagged commands run; the rest
                            // answer exactly Redis's error (oracle capture
                            // artifact). Unknown commands keep their normal
                            // error — Redis resolves the command first. The
                            // board re-check serves commands that arrive in
                            // the same iteration the last cell flips ready.
                            if self.shared.loading.get()
                                && meta.is_some_and(|meta| !meta.flags.contains(CmdFlags::LOADING))
                            {
                                if self
                                    .loading_board
                                    .as_deref()
                                    .is_some_and(RecoveryBoard::all_ready)
                                {
                                    self.shared.loading.set(false);
                                } else {
                                    // The error reply keeps pipeline order:
                                    // staged commands answer first.
                                    flush_parse_stage(
                                        &self.shared,
                                        &mut stage,
                                        &mut stage_bytes,
                                        origin,
                                        conn_cx,
                                        out,
                                    );
                                    let mut w = RespWriter::new(out, conn_cx.proto);
                                    w.error("LOADING Redis is loading the dataset in memory");
                                    continue;
                                }
                            }
                            // Named-namespace commands always ride the pump
                            // (M2-S08): durable acks suspend, staging
                            // admission may park, and emission lives there —
                            // one `Option` load on the memory fast path.
                            let defer = pump_was_active
                                || spawn_first.is_some()
                                || !deferred.is_empty()
                                || conn_cx.ns.named().is_some()
                                || conn_cx.ns.unavailable()
                                || self.needs_fabric(&argv);
                            if defer {
                                // Pump replies emit after the fast-path
                                // replies already in `out` — flush so the
                                // staged batch keeps its pipeline position.
                                flush_parse_stage(
                                    &self.shared,
                                    &mut stage,
                                    &mut stage_bytes,
                                    origin,
                                    conn_cx,
                                    out,
                                );
                                let owned =
                                    OwnedCmd::from_argv_into(&argv, self.shared.take_cmd_buf());
                                if pump_was_active || spawn_first.is_some() {
                                    deferred.push(owned);
                                } else {
                                    spawn_first = Some(owned);
                                }
                            } else if stage_enabled && parse_stageable(meta, &argv) {
                                // M2.5 Phase H (ADR-0029 lever 2): stage the
                                // fast-path command — flat argv copy + key
                                // hash + phase-1 index-line prefetch; the
                                // batch executes at the next barrier or end
                                // of buffer with its record lines prefetched.
                                let (hash, has_key) = match argv.len() >= 2 {
                                    true => {
                                        let hash = self.shared.hasher.hash(argv.arg(1));
                                        if let Some(store) =
                                            self.shared.store.borrow().db(usize::from(conn_cx.db))
                                        {
                                            store.prefetch(hash);
                                        }
                                        (hash, true)
                                    }
                                    false => (0, false),
                                };
                                let off = stage_argv_block_argv(&mut stage_bytes, &argv);
                                stage.push(StagedParse { off, hash, has_key });
                            } else {
                                flush_parse_stage(
                                    &self.shared,
                                    &mut stage,
                                    &mut stage_bytes,
                                    origin,
                                    conn_cx,
                                    out,
                                );
                                let argv_slices: Vec<&[u8]> = argv.iter().collect();
                                let before = out.len();
                                let now = self.shared.now.get();
                                let scope = ExecScope::of(conn_cx);
                                execute(
                                    &argv,
                                    &mut self.shared.store.borrow_mut(),
                                    conn_cx,
                                    now,
                                    out,
                                );
                                self.shared.observer.borrow_mut().on_execute(
                                    self.shared.cell,
                                    origin,
                                    scope,
                                    &argv_slices,
                                    &out[before..],
                                    now,
                                );
                                if let Some(dur) = stall_request(&argv_slices) {
                                    self.shared.stall_until.set(now.saturating_add(dur));
                                }
                                // QUIT: stop processing this buffer (Redis
                                // discards anything pipelined after QUIT) and
                                // close once the +OK reply has flushed.
                                if conn_cx.close_requested.get() {
                                    conn_cx.close_requested.set(false);
                                    quit = true;
                                    break;
                                }
                            }
                        }
                        Parsed::Incomplete => {}
                        Parsed::ProtocolError(e) => {
                            flush_parse_stage(
                                &self.shared,
                                &mut stage,
                                &mut stage_bytes,
                                origin,
                                conn_cx,
                                out,
                            );
                            let mut w = RespWriter::new(out, conn_cx.proto);
                            // Display, not Debug (batch 45): the Redis
                            // phrasing the compat harness diffs.
                            w.error(&format!("ERR Protocol error: {e}"));
                            protocol_error = true;
                            break;
                        }
                    }
                }
                // End of buffer: the staged batch executes with its record
                // lines prefetched (§7.3 phase 2 across the whole batch).
                flush_parse_stage(&self.shared, &mut stage, &mut stage_bytes, origin, conn_cx, out);
                drop(iter);
                let conn = conns.get_mut(key).expect("conn checked above");
                if protocol_error || quit {
                    conn.close_after_flush = true;
                }
                conn.queue.extend(deferred);
                if conn.queue.len() >= PENDING_HIGH_WATER && !conn.recv_disarmed {
                    conn.recv_disarmed = true;
                    cx.push(IoOp::RecvDisarm { fd: conn.fd });
                }
                if spawn_first.is_some() {
                    conn.pump_active = true;
                }
            }
            cx.pool.release(buf);
            cx.charge(GroupClass::Foreground, commands);
            if let Some(first) = spawn_first {
                self.spawn_pump(cx, key, first);
            }
        }
        debug_assert!(stage.is_empty(), "parse stage flushed before buffer release");
        self.parse_stage = stage;
        self.parse_stage_bytes = stage_bytes;
    }

    fn maintain(&mut self, cx: &mut LoopCx<'_>) {
        self.shared.now.set(cx.now);
        self.drive_stop();
        // ---- early fabric publish (M2.5-S21): remote ops staged during
        // EXECUTE become peer-visible NOW instead of at FABRIC-OUT (step
        // 8) — the peer drains them while this cell runs MAINTAIN/LOG/
        // RESPOND, so the hop RTT overlaps local work instead of
        // following it (the request-path sibling of the M0-R1 "publish
        // replies NOW" finding; remote throughput is window/RTT-bound at
        // REMOTE_WINDOW, so RTT saved converts directly). One extra
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
                ExpiryBudget { max_fires, max_steps: max_fires.saturating_mul(8).max(4096) },
            );
            self.expiry_lag = stats.lag_ms;
            let units =
                (stats.reaped + stats.stale).min(u64::from(u32::MAX)) as u32 + stats.steps / 64;
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
        // ---- DDL persist wakes (ADR-0015 D3): one relaxed load per
        // MAINTAIN; parked DDL pumps wake on epoch edges.
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
        let keys = self.shared.conns.borrow().keys();
        for key in keys {
            let mut close_now = false;
            self.shared.with_conn(key, |conn| {
                if conn.closing {
                    return;
                }
                if conn.rearm_recv {
                    conn.rearm_recv = false;
                    if conn.recv_disarmed {
                        conn.recv_disarmed = false;
                        cx.push(IoOp::RecvArm {
                            fd: conn.fd,
                            token: Self::token(TokenClass::Recv, key),
                        });
                    }
                }
                if !conn.out.is_empty()
                    && !conn.send_inflight
                    && let Some(buf) = cx.pool.try_lease(LeaseKind::Send)
                {
                    let n = conn.out.len().min(cx.pool.buf_size());
                    cx.pool.bytes_mut(buf)[..n].copy_from_slice(&conn.out[..n]);
                    conn.out.drain(..n);
                    conn.send_inflight = true;
                    cx.push(IoOp::Send {
                        fd: conn.fd,
                        buf,
                        len: n as u32,
                        token: Self::token(TokenClass::Send, key),
                    });
                }
                if conn.close_after_flush
                    && conn.out.is_empty()
                    && !conn.send_inflight
                    && !conn.pump_active
                {
                    close_now = true;
                }
            });
            if close_now {
                self.initiate_close(cx, key);
            }
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
            self.initiate_close(cx, key);
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
                self.shared.conns.borrow_mut().for_each_mut(|conn| conn.parser.set_limits(limits));
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
            // Manual checkpoint requests ride the control handle (one
            // relaxed load — the persisted-epoch pattern, ADR-0016 D7).
            if let Some(control) = self.shared.control.borrow().as_ref() {
                let epoch = control.ckpt_board().slot(self.shared.cell.0).req();
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

    /// One MAINTAIN concern of `maintain`, in phase order:
    ///  DDL persist wakes (ADR-0015 D3): one relaxed load per
    /// MAINTAIN; parked DDL pumps wake on epoch edges.
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
            // ---- checkpoint-publication wakes (M2-S20): any cell's
            // durable MANIFEST changes the board sum; parked INF.CKPT
            // WAIT pumps re-check their target. Also the LASTSAVE gauge.
            let board = control.ckpt_board();
            let sum = board.published_sum();
            if sum != self.shared.ckpt_pub_seen.get() {
                self.shared.ckpt_pub_seen.set(sum);
                self.shared.ckpt_waiters.wake_all(0);
            }
            self.shared.node.rdb_last_save_ms.set(board.max_unix_ms());
            self.shared.node.ns_drop_tombstones.set(control.drop_tombstones() as u64);
        }
    }

    /// One MAINTAIN concern of `maintain`, in phase order:
    ///  stats flush
    fn maintain_conn_sweep(&mut self, cx: &mut LoopCx<'_>) {
        let node = &self.shared.node;
        node.recv_dropped.set(self.shared.recv_dropped.get());
        node.accept_errors.set(self.shared.accept_errors.get());
        node.fabric_rtt_p50_ns.set(self.shared.rtt_ns.borrow().percentile(50.0));
        {
            let ps = self.shared.pubsub.borrow();
            node.pubsub_channels.set(ps.live_owned_channel_count());
            node.pubsub_patterns.set(ps.live_pattern_count());
            node.pubsub_state_bytes.set(ps.state_bytes() as u64);
        }
        let knobs = self.shared.knobs.get();
        let now_ms = cx.now.as_millis();
        let mut conns = self.shared.conns.borrow_mut();
        node.connections.set(conns.live as u64);
        let mut bytes = 0usize;
        let mut idle: Vec<ConnKey> = Vec::new();
        let ConnSlab { slots, gens, .. } = &mut *conns;
        for (slot, entry) in slots.iter_mut().enumerate() {
            let Some(conn) = entry.as_mut() else { continue };
            bytes += conn.state_bytes();
            // The output-buffer class follows the subscription state
            // (Redis's CLIENT_PUBSUB flag; ADR-0123 D4). Soft-cap aging
            // continues between deliveries (M1-S11): a stalled client
            // over the soft limit dies on schedule even when nothing
            // more is written to it.
            let subscribed = !conn.cx.sub_channels.is_empty() || !conn.cx.sub_patterns.is_empty();
            let caps = if subscribed { knobs.cob_pubsub } else { knobs.cob_normal };
            if conn.cob_soft_since_ms != 0 || caps != (0, 0, 0) {
                enforce_output_cap(node, conn, now_ms, caps);
            }
            // `timeout` (ADR-0123 D2): idle unsubscribed connections
            // close; subscribers are exempt, as in Redis.
            if knobs.timeout_ms != 0
                && !subscribed
                && !conn.closing
                && now_ms.saturating_sub(conn.last_active_ms) >= knobs.timeout_ms
            {
                idle.push(ConnKey { slot: slot as u32, generation: gens[slot] });
            }
        }
        node.conn_state_bytes.set(bytes as u64);
        drop(conns);
        // Recycle-pool residency (v0.4.0-alpha RSS-attribution gauges):
        // running sums maintained at the push/pop sites, flushed here.
        node.reply_pool_bytes.set(self.shared.reply_pool_bytes.get());
        node.cmd_pool_bytes.set(self.shared.cmd_pool_bytes.get());
        let shared = Rc::clone(&self.shared);
        for key in idle {
            shared.node.idle_disconnections.set(shared.node.idle_disconnections.get() + 1);
            self.initiate_close(cx, key);
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
    let mut io_budget = [0u64; 3 * IoClass::COUNT];
    for class in IoClass::ALL {
        let c = stats.io_budget[class.index()];
        io_budget[3 * class.index()] = c.spent_bytes;
        io_budget[3 * class.index() + 1] = c.spent_ops;
        io_budget[3 * class.index() + 2] = c.deferrals;
    }
    node.io_budget.set(io_budget);
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
