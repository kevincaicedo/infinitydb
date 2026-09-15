//! Maintenance pumps: namespace apply (flat and tiered), the shadow-slot
//! settle pump (ADR-0093), and the compaction pump — each a resumable
//! future that yields between bounded slices (L6).

use super::*;

/// One origin cell's namespace-apply pump (M4-S26; generalized by
/// M4.5-S27): applies arrive in fabric FIFO order and execute strictly
/// in that order — a suspended cold read (tiered) or a staging park
/// (flat under pressure, ADR-0083 D1) holds the queue behind it, which
/// is exactly what preserves the origin's per-connection command
/// ordering. Deactivates when its
/// queue drains (the flag and the emptiness check share one borrow, so
/// a concurrent enqueue always observes a live pump or respawns one).
pub(super) async fn ns_apply_pump<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: Rc<Shared<O, F>>,
    origin: u16,
) {
    loop {
        let next = {
            let mut queues = shared.ns_applies.borrow_mut();
            let item = queues[usize::from(origin)].pop_front();
            if item.is_none() {
                shared.ns_pump_active.borrow_mut()[usize::from(origin)] = false;
            }
            item
        };
        let Some(item) = next else { return };
        let argv: Vec<&[u8]> = item.args.iter().map(Vec::as_slice).collect();
        // A scattered `DBSIZE` leg (the origin's `Counted` shape): this
        // cell's exact count as a typed integer — the tiered drain runs
        // here, on the pump, exactly as a local `DBSIZE` would — or the
        // drain's error as bytes for the origin to relay.
        if argv.len() == 1 && argv[0].eq_ignore_ascii_case(b"DBSIZE") {
            let origin_cell = ExecOrigin::Fabric(CellId(origin));
            match ns_dbsize_local(&shared, origin_cell, item.ns, item.proto).await {
                Ok(n) => {
                    shared.fabric.borrow_mut().reply(CellId(origin), item.token, &Outcome::Int(n));
                }
                Err(reply) => {
                    shared.fabric.borrow_mut().reply(
                        CellId(origin),
                        item.token,
                        &Outcome::Bytes(&reply),
                    );
                    shared.recycle_reply_buf(reply);
                }
            }
            continue;
        }
        // Flat durable applies ride the same FIFO under staging pressure
        // (M4.5-S27, ADR-0083 D1); the tier is re-resolved here because
        // the owner stays authoritative and DDL can retier a namespace
        // while an apply is queued. A keyspace-level command on a tiered
        // namespace takes the ordinary path here exactly as it does on
        // the connection's cell (`keyspace_level` — ADR-0108).
        let ordinary = !shared.store.borrow().is_tiered(item.ns)
            || lookup(argv[0]).is_some_and(|meta| keyspace_level(meta, &argv));
        if ordinary {
            apply_flat_one(&shared, origin, item.ns, &argv, item.proto, item.token, item.program)
                .await;
            continue;
        }
        match apply_tiered_one(&shared, origin, item.ns, &argv, item.proto).await {
            tiered::TieredReply::Done(reply) => {
                shared.fabric.borrow_mut().reply(
                    CellId(origin),
                    item.token,
                    &Outcome::Bytes(&reply),
                );
                shared.recycle_reply_buf(reply);
            }
            // M4.5-S29: the durability wait leaves the pump. Holding the
            // FIFO across it serialized each origin to one `always` write
            // per fsync window — the flat-scaling defect. Staging already
            // happened (in FIFO order, no await since), so apply order is
            // intact; the reply ships from FABRIC-IN's deferred-reply
            // future once the watermark covers `seq`, and the origin
            // matches it by token — per-connection reply order is the
            // origin's pending-FIFO's job, not this queue's.
            tiered::TieredReply::Gated { reply, seq } => {
                shared
                    .durable
                    .borrow_mut()
                    .as_mut()
                    .expect("tiered implies the durable plane")
                    .note_gated_ack();
                shared.pump_gated.borrow_mut().push_back(GatedReply {
                    to: CellId(origin),
                    token: item.token,
                    seq,
                    reply,
                });
            }
        }
    }
}

/// One fabric-origin flat-namespace apply on the pump (M4.5-S27,
/// ADR-0083 D1): admission parks on `drained` — pacing, the same shape
/// the local pump has always had — and on admission success execution
/// and staging run with no await between, so apply order is the pump's
/// FIFO. A gated `always` verdict queues for FABRIC-IN's deferred-reply
/// spawn exactly like the tiered arm (ADR-0082: never awaited in FIFO
/// custody). `execute_ns_owned` counts the gated ack itself.
async fn apply_flat_one<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: u16,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    token: FabricToken,
    program: bool,
) {
    loop {
        let mut buf = shared.take_reply_buf();
        match shared.execute_ns_owned(CellId(origin), argv, proto, ns, program, &mut buf) {
            NsApplyOutcome::Park => {
                shared.recycle_reply_buf(buf);
                let wait = {
                    let mut durable = shared.durable.borrow_mut();
                    let cell = durable.as_mut().expect("durable admission parked");
                    cell.note_parked();
                    cell.drained.wait(())
                };
                wait.await;
            }
            NsApplyOutcome::Reply => {
                shared.fabric.borrow_mut().reply(CellId(origin), token, &Outcome::Bytes(&buf));
                shared.recycle_reply_buf(buf);
                return;
            }
            NsApplyOutcome::Gated(seq) => {
                shared.pump_gated.borrow_mut().push_back(GatedReply {
                    to: CellId(origin),
                    token,
                    seq,
                    reply: buf,
                });
                return;
            }
        }
    }
}

/// One fabric-origin tiered apply: validate and execute through the
/// tiered arm. An `always` write returns its gated verdict — the caller
/// queues it for the deferred-reply future (§8.2: the client-visible ack
/// never precedes the owner's fsync), never awaiting it in FIFO custody.
async fn apply_tiered_one<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: u16,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
) -> tiered::TieredReply {
    let Some(meta) = lookup(argv[0]) else {
        return tiered::TieredReply::Done(error_reply(shared, proto, "ERR unknown command"));
    };
    if !arity_ok(meta, argv.len()) {
        return tiered::TieredReply::Done(error_reply(
            shared,
            proto,
            "ERR wrong number of arguments",
        ));
    }
    let class = shared.store.borrow().ns_fsync_class(ns);
    let origin_cell = ExecOrigin::Fabric(CellId(origin));
    tiered::dispatch_tiered(shared, origin_cell, ns, meta, argv, proto, class).await
}

/// One shadow reconciliation read (M4.5-S37, ADR-0093 D4): the ticket's
/// cold record through `ColdReads` (`Maintain`, or `Foreground` when
/// the pinned suffix asks), then — inside one borrow, after the
/// suspension — the store re-validates the ticket and applies the
/// verdict. A failed read leaves the ticket for the next round. No
/// borrow is held across the await (§3.3).
pub(super) async fn shadow_pump<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: Rc<Shared<O, F>>,
    ns: NsId,
    read: inf_store::ShadowRead,
) {
    let class = if read.foreground {
        inf_runtime::ReadClass::Foreground
    } else {
        inf_runtime::ReadClass::Maintain
    };
    // The reconciler's own device error (ADR-0093 D4.3), distinct from
    // `shadow_twin_read_fail` so a harness can hold every ticket open
    // while `DBSIZE` and `DEL` still read (F-L07-01's rebuilt shape).
    let image = if inf_foundation::fault::fire(crate::fault::SHADOW_RECONCILE_READ_FAIL) {
        Err("injected reconciler read failure (fault point shadow_reconcile_read_fail)")
    } else {
        tiered::read_cold_record(&shared, ns, read.ticket.cold, class).await
    };
    let mut ks = shared.store.borrow_mut();
    // A dropped namespace took its tickets with it.
    let Some(table) = ks.tiered_store_mut(ns) else { return };
    let ticket = read.ticket;
    match image {
        Ok(image) => {
            let _ = table.resolve_shadow(ticket.hash, ticket.cold, &image);
        }
        Err(_) => table.shadow_read_failed(ticket.cold),
    }
}

/// One compaction read chain (M4-S26 driving ADR-0059 D2): chunked cold
/// reads of the candidate through `ColdReads` (`ReadClass::Maintain`),
/// each chunk fed to `TieredTable::compaction_apply` at the exact scan
/// cursor. The chain ends on slice exhaustion, a tail stall, scan
/// completion, a pinned walk (relocating mid-walk is the D9-1 duplicate
/// hazard), or a read refusal/error — the cursor persists and the next
/// MAINTAIN resumes it. No borrow is held across an await (§3.3).
pub(super) async fn compact_pump<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: Rc<Shared<O, F>>,
    read: crate::tier_cell::CompactRead,
) {
    let mut cursor = read.addr.to_raw();
    let mut budget = read.len;
    // Oversized-record assembly target (0 = one pool window).
    let mut need: usize = 0;
    'chain: while budget > 0 {
        let mut chunk: Vec<u8> = Vec::new();
        loop {
            let (wait, frames, skip) = {
                let tier = shared.tier.borrow();
                let Some(t) = tier.as_ref().and_then(|t| t.ns(read.ns)) else { break 'chain };
                let Some(cold) = tier.as_ref().and_then(|t| t.cold.clone()) else { break 'chain };
                let at = cursor + chunk.len() as u64;
                let want = if need > 0 { need - chunk.len() } else { 1 };
                let Some(addr) = inf_store::LogicalAddr::from_raw(at) else { break 'chain };
                // A retired-file miss re-resolves next round, never errors.
                let Some((fd, file, offset, frames, skip)) = t.plan_cold_read(addr, want) else {
                    break 'chain;
                };
                let len = frames as usize * inf_log::TIER_FRAME_BYTES;
                // Same-clock stamp as `on_completion` (the
                // `cold_read_p99_us` pair).
                let now_us = shared.now.get().as_micros();
                match cold.enqueue(fd, file, offset, len, inf_runtime::ReadClass::Maintain, now_us)
                {
                    Ok(wait) => (wait, frames, skip),
                    Err(_) => break 'chain, // queue full: back off to the next round
                }
            };
            let done = wait.await;
            if done.outcome().is_err() {
                break 'chain; // typed read failure: cursor persists, retried
            }
            let extracted = done.bytes(|window| {
                let window_data = frames as usize * inf_log::TIER_FRAME_DATA - skip;
                let take = if need > 0 { window_data.min(need - chunk.len()) } else { window_data };
                let mut piece = Vec::new();
                inf_log::tier_extract(window, skip, take, &mut piece).ok().map(|()| piece)
            });
            drop(done);
            match extracted {
                Some(piece) => chunk.extend_from_slice(&piece),
                None => break 'chain, // frame CRC failure: foreground reads surface it typed
            }
            if need == 0 || chunk.len() >= need {
                break;
            }
        }
        let applied = {
            let mut ks = shared.store.borrow_mut();
            let Some(table) = ks.tiered_store_mut(read.ns) else { break 'chain };
            // A walk pinned itself between chunks: relocating now would
            // let one walk emit a ref and an image for the same key
            // (ADR-0059 D9-1) — pause; the cursor resumes post-walk.
            if table.space().walk_watermark().is_some() {
                break 'chain;
            }
            let Some(addr) = inf_store::LogicalAddr::from_raw(cursor) else { break 'chain };
            table.compaction_apply(read.file_id, addr, &chunk)
        };
        if applied.consumed > 0 {
            cursor += applied.consumed;
            budget = budget.saturating_sub(applied.consumed);
            need = 0;
        }
        if applied.file_scanned || applied.stalled {
            break 'chain;
        }
        if applied.need > 0 {
            // A record length from an on-disk header: a typed stop, not
            // an assert (the decoder-bounds rule; the cursor persists).
            let Ok(n) = usize::try_from(applied.need) else {
                debug_assert!(false, "record length {} exceeds usize", applied.need);
                break 'chain;
            };
            need = n;
            continue;
        }
        if applied.consumed == 0 {
            debug_assert!(false, "compaction_apply made no progress without a verdict");
            break 'chain;
        }
    }
    if let Some(tier) = shared.tier.borrow_mut().as_mut()
        && let Some(t) = tier.ns_mut(read.ns)
    {
        t.compact_inflight = false;
    }
}
