//! Pub/sub plane programs (M1-S10/S11): subscription deltas to owner
//! cells, the owner-side publish pump, local delivery, and the output cap.

use super::*;

// ---- pub/sub plane programs (M1-S10/S11) ----------------------------------------

/// Pump-side dispatch for the six public pub/sub commands. Subscribe-family
/// ops mutate the connection state, sync this cell's registries, and ship
/// the 0→1/1→0 transition deltas — **awaited before the confirmation frames
/// are emitted**, so once a client sees its confirmation, a PUBLISH from
/// anywhere reaches it. PUBLISH routes to the channel's owner; PUBSUB is an
/// introspection program over the owner views.
pub(super) async fn dispatch_pubsub<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    key: ConnKey,
    id: CommandId,
    argv: &[&[u8]],
    proto: Protocol,
    pending: &mut VecDeque<PendingReply>,
    inflight: &mut usize,
) -> bool {
    match id {
        CommandId::Subscribe
        | CommandId::Psubscribe
        | CommandId::Unsubscribe
        | CommandId::Punsubscribe => {
            let kind = if matches!(id, CommandId::Subscribe | CommandId::Unsubscribe) {
                SubKind::Channel
            } else {
                SubKind::Pattern
            };
            let adding = matches!(id, CommandId::Subscribe | CommandId::Psubscribe);
            let names: Vec<&[u8]> = argv[1..].to_vec();
            let mut frames = shared.take_reply_buf();
            let now = shared.now.get();
            let Some(changes) = shared.with_conn(key, |conn| {
                if adding {
                    pubsub::apply_subscribe(&names, kind, &mut conn.cx, now, &mut frames)
                } else {
                    let names = (!names.is_empty()).then_some(names.as_slice());
                    pubsub::apply_unsubscribe(names, kind, &mut conn.cx, now, &mut frames)
                }
            }) else {
                return false;
            };
            let mut notes: Vec<(SubKind, Vec<u8>, i32)> = Vec::new();
            {
                let mut ps = shared.pubsub.borrow_mut();
                for (name, changed) in &changes {
                    if !changed {
                        continue;
                    }
                    let transition = if adding {
                        ps.local_add(kind, name, key)
                    } else {
                        ps.local_remove(kind, name, key)
                    };
                    if transition {
                        notes.push((kind, name.clone(), if adding { 1 } else { -1 }));
                    }
                }
            }
            let mut waiters = Vec::new();
            for (kind, name, delta) in &notes {
                send_sub_delta(shared, *kind, name, *delta, &mut waiters).await;
            }
            for waiter in waiters {
                let _ = waiter.await;
            }
            pending.push_back(PendingReply::Done(frames));
        }
        CommandId::Publish => {
            let (channel, payload) = (argv[1], argv[2]);
            let owner = if shared.route_local_only {
                shared.cell
            } else {
                shared.router.cell_of(SlotRouter::slot_of(channel))
            };
            if owner.0 == shared.cell.0 {
                // This cell owns the channel: deliver locally, fan one
                // INF.PUBFAN per subscriber-bearing peer, sum the typed
                // per-cell delivery counts (the Counted shape). A publisher
                // subscribed to its own channel gets its frames *after* the
                // count reply (Redis order) via a trailing Done entry.
                let (acc, self_frames) = deliver_local(shared, channel, payload, Some(key));
                let targets = shared.pubsub.borrow().fan_targets(channel, shared.cell.0);
                let mut waiters = Vec::new();
                for cell in targets {
                    let fan = &[&b"INF.PUBFAN"[..], channel, payload];
                    if let Ok(waiter) = send_apply(
                        shared,
                        CellId(cell),
                        ApplyOrigin::Program,
                        Protocol::Resp2,
                        0,
                        fan,
                    )
                    .await
                    {
                        note_fan(&shared.node);
                        waiters.push(waiter);
                        *inflight += 1;
                    }
                }
                pending.push_back(PendingReply::Counted { waiters, acc, proto, refusal: None });
                if !self_frames.is_empty() {
                    pending.push_back(PendingReply::Done(self_frames));
                }
            } else {
                // ADR-0101 D1: a subscribed publisher tags the forward
                // with `(conn, seq)` so the owner's fan leg back to this
                // cell defers the publisher's own frames to the reply
                // (Redis: reply, then push). Subscription commands are
                // conn-state barriers, so the set cannot change while the
                // publish is in flight; unsubscribed publishers pay
                // nothing and keep the three-arg form.
                let seq = shared
                    .with_conn(key, |conn| {
                        let subscribed =
                            !conn.cx.sub_channels.is_empty() || !conn.cx.sub_patterns.is_empty();
                        subscribed.then(|| {
                            conn.publish_seq += 1;
                            conn.publish_seq
                        })
                    })
                    .flatten();
                let mut conn_buf = [0u8; 20];
                let mut seq_buf = [0u8; 20];
                let mut publ: [&[u8]; 5] = [&b"INF.PUB"[..], channel, payload, b"", b""];
                let argc = match seq {
                    Some(seq) => {
                        publ[3] = u64_decimal(&mut conn_buf, key.packed());
                        publ[4] = u64_decimal(&mut seq_buf, seq);
                        5
                    }
                    None => 3,
                };
                match send_apply(
                    shared,
                    owner,
                    ApplyOrigin::Program,
                    Protocol::Resp2,
                    0,
                    &publ[..argc],
                )
                .await
                {
                    Ok(waiter) => {
                        *inflight += 1;
                        pending.push_back(match seq {
                            Some(seq) => PendingReply::Publish { waiter, proto, seq },
                            None => PendingReply::Remote { waiter, proto },
                        });
                    }
                    Err(refusal) => pending.push_back(PendingReply::Done(refusal)),
                }
            }
        }
        CommandId::Pubsub => {
            let reply = program_pubsub(shared, proto, argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        _ => unreachable!("is_plane_pubsub covers exactly the arms above"),
    }
    true
}

/// Ships one subscription transition: channel deltas go to the owner cell,
/// pattern deltas replicate to every cell (plus this cell's slot directly).
async fn send_sub_delta<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    kind: SubKind,
    name: &[u8],
    delta: i32,
    waiters: &mut Vec<GateWait<u64, OwnedOutcome>>,
) {
    let delta_arg: &[u8] = if delta < 0 { b"-1" } else { b"1" };
    let subd = &[&b"INF.SUBD"[..], kind.wire_tag(), name, delta_arg];
    match kind {
        SubKind::Channel => {
            let owner = if shared.route_local_only {
                shared.cell
            } else {
                shared.router.cell_of(SlotRouter::slot_of(name))
            };
            if owner.0 == shared.cell.0 {
                shared.pubsub.borrow_mut().apply_delta(kind, name, shared.cell.0, delta);
            } else if let Ok(waiter) =
                send_apply(shared, owner, ApplyOrigin::Program, Protocol::Resp2, 0, subd).await
            {
                waiters.push(waiter);
            }
        }
        SubKind::Pattern => {
            shared.pubsub.borrow_mut().apply_delta(kind, name, shared.cell.0, delta);
            if !shared.route_local_only {
                for cell in peer_cells(shared) {
                    if let Ok(waiter) =
                        send_apply(shared, cell, ApplyOrigin::Program, Protocol::Resp2, 0, subd)
                            .await
                    {
                        waiters.push(waiter);
                    }
                }
            }
        }
    }
}

/// Close-path subscription cleanup: ships the 1→0 deltas and consumes the
/// acks (nothing awaits them, but every fabric op replies — credit and
/// orphan-tripwire hygiene).
pub(super) async fn flush_sub_deltas<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: Rc<Shared<O, F>>,
    notes: Vec<(SubKind, Vec<u8>, i32)>,
) {
    let mut waiters = Vec::new();
    for (kind, name, delta) in &notes {
        send_sub_delta(&shared, *kind, name, *delta, &mut waiters).await;
    }
    for waiter in waiters {
        let _ = waiter.await;
    }
}

/// Removes a closed connection from the local registries, returning the
/// cell-level transitions to notify.
pub(super) fn unsubscribe_closed_conn<
    O: PlaneObserver + 'static,
    F: SegmentFs + Clone + 'static,
>(
    shared: &Rc<Shared<O, F>>,
    key: ConnKey,
    cx: &ConnCx,
) -> Vec<(SubKind, Vec<u8>, i32)> {
    let mut notes = Vec::new();
    let mut ps = shared.pubsub.borrow_mut();
    for channel in &cx.sub_channels {
        if ps.local_remove(SubKind::Channel, channel, key) {
            notes.push((SubKind::Channel, channel.clone(), -1));
        }
    }
    for pattern in &cx.sub_patterns {
        if ps.local_remove(SubKind::Pattern, pattern, key) {
            notes.push((SubKind::Pattern, pattern.clone(), -1));
        }
    }
    notes
}

/// The owner-side publish pump: fabric-origin PUBLISHes drain strictly in
/// arrival order — local delivery, one INF.PUBFAN per subscriber-bearing
/// peer, then the receiver-count reply to the publisher's cell. Sends leave
/// in queue order, so per-publisher delivery order holds end-to-end; reply
/// aggregation is awaited inline, making publish throughput per owner cell
/// RTT-bound — a recorded M1 simplification (no gate measures sustained
/// publish throughput; revisit with evidence per L4 if a workload demands
/// overlap).
pub(super) async fn owner_pub_pump<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: Rc<Shared<O, F>>,
) {
    loop {
        let Some(item) = shared.pub_queue.borrow_mut().pop_front() else {
            shared.pub_pump_active.set(false);
            return;
        };
        let (mut total, _) = deliver_local(&shared, &item.channel, &item.payload, None);
        let targets = shared.pubsub.borrow().fan_targets(&item.channel, shared.cell.0);
        let mut waiters = Vec::new();
        for cell in targets {
            // ADR-0101 D2: the origin cell's leg echoes the publisher
            // tag; every other leg keeps the three-arg form.
            let tagged = item.tag.as_ref().filter(|_| cell == item.origin.0);
            let fan: &[&[u8]] = match tagged {
                Some((conn, seq)) => &[&b"INF.PUBFAN"[..], &item.channel, &item.payload, conn, seq],
                None => &[&b"INF.PUBFAN"[..], &item.channel, &item.payload],
            };
            if let Ok(waiter) =
                send_apply(&shared, CellId(cell), ApplyOrigin::Program, Protocol::Resp2, 0, fan)
                    .await
            {
                note_fan(&shared.node);
                waiters.push(waiter);
            }
        }
        for waiter in waiters {
            if let OwnedOutcome::Int(n) = waiter.await {
                total += n;
            }
        }
        // Publish the reply now — the origin's pump is suspended on it.
        let mut fabric = shared.fabric.borrow_mut();
        fabric.reply(item.origin, item.token, &Outcome::Int(total));
        fabric.flush();
    }
}

/// PUBSUB introspection over the cell registries: CHANNELS merges the owner
/// views (KEYS-style header arithmetic), NUMSUB asks each channel's owner,
/// NUMPAT answers locally (the pattern index is replicated).
async fn program_pubsub<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    argv: &[&[u8]],
) -> Vec<u8> {
    let sub = argv[1];
    if sub.eq_ignore_ascii_case(b"CHANNELS") && argv.len() <= 3 {
        let pattern = argv.get(2).copied();
        let local = shared.pubsub.borrow().live_owned_channels(pattern);
        let mut waiters = Vec::new();
        if shared.cells > 1 && !shared.route_local_only {
            let mut request: Vec<&[u8]> = vec![b"INF.PUBSUB", b"CHANNELS"];
            if let Some(p) = pattern {
                request.push(p);
            }
            for cell in peer_cells(shared) {
                match send_apply(shared, cell, ApplyOrigin::Program, Protocol::Resp2, 0, &request)
                    .await
                {
                    Ok(waiter) => waiters.push(waiter),
                    Err(refusal) => return refusal,
                }
            }
        }
        let mut total = local.len();
        let mut bodies: Vec<(Vec<u8>, usize)> = Vec::new();
        for waiter in waiters {
            match waiter.await {
                OwnedOutcome::Bytes(bytes) => {
                    let Some((n, off)) = parse_array_header(&bytes) else {
                        return bytes; // peer error passthrough
                    };
                    total += n;
                    bodies.push((bytes, off));
                }
                _ => return error_reply(shared, proto, "ERR cross-cell execution failed"),
            }
        }
        let mut reply = shared.take_reply_buf();
        {
            let mut w = RespWriter::new(&mut reply, proto);
            w.array_header(total);
            for name in &local {
                w.bulk(name);
            }
        }
        for (bytes, off) in bodies {
            reply.extend_from_slice(&bytes[off..]);
            shared.recycle_reply_buf(bytes);
        }
        reply
    } else if sub.eq_ignore_ascii_case(b"NUMSUB") {
        enum Count {
            Local(i64),
            Wait(GateWait<u64, OwnedOutcome>),
        }
        let mut parts: Vec<(&[u8], Count)> = Vec::with_capacity(argv.len() - 2);
        for name in &argv[2..] {
            let owner = if shared.route_local_only {
                shared.cell
            } else {
                shared.router.cell_of(SlotRouter::slot_of(name))
            };
            if owner.0 == shared.cell.0 {
                parts.push((name, Count::Local(shared.pubsub.borrow().owned_count(name))));
            } else {
                let numsub = &[&b"INF.PUBSUB"[..], b"NUMSUB", name];
                match send_apply(shared, owner, ApplyOrigin::Program, Protocol::Resp2, 0, numsub)
                    .await
                {
                    Ok(waiter) => parts.push((name, Count::Wait(waiter))),
                    Err(refusal) => return refusal,
                }
            }
        }
        let mut reply = shared.take_reply_buf();
        RespWriter::new(&mut reply, proto).array_header(parts.len() * 2);
        for (name, count) in parts {
            let count = match count {
                Count::Local(n) => n,
                Count::Wait(waiter) => match waiter.await {
                    OwnedOutcome::Int(n) => n,
                    _ => 0,
                },
            };
            let mut w = RespWriter::new(&mut reply, proto);
            w.bulk(name);
            w.int(count);
        }
        reply
    } else if sub.eq_ignore_ascii_case(b"NUMPAT") && argv.len() == 2 {
        int_reply(shared, proto, shared.pubsub.borrow().live_pattern_count() as i64)
    } else {
        let mut reply = shared.take_reply_buf();
        pubsub::pubsub_subcommand_error(sub, &mut RespWriter::new(&mut reply, proto));
        reply
    }
}

/// Owner-side handling of the internal pub/sub Apply vocabulary. Returns
/// false when `argv` is not pub/sub plumbing (the caller falls through to
/// normal Apply execution).
pub(super) fn handle_pubsub_apply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    from: CellId,
    token: FabricToken,
    argv: &[&[u8]],
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
    pubs: &mut Vec<OwnerPub>,
) -> bool {
    let name = argv[0];
    if name.eq_ignore_ascii_case(b"INF.PUBFAN") && matches!(argv.len(), 3 | 5) {
        // Subscriber-cell delivery leg: append frames locally, reply the
        // typed per-cell receiver count. A tagged leg (ADR-0101 D3 — the
        // publisher's own cell) writes the tagged connection's frames
        // into its stash instead of its output, still counted; the
        // pump emits them right after that publish's reply.
        let tag = (argv.len() == 5).then(|| parse_publisher_tag(argv[3], argv[4])).flatten();
        let (delivered, self_frames) =
            deliver_local(shared, argv[1], argv[2], tag.map(|(conn, _)| conn));
        if let Some((conn, seq)) = tag
            && !self_frames.is_empty()
        {
            shared.with_conn(conn, |c| c.self_push.push((seq, self_frames)));
        }
        staged.push((from, token, StagedReply::Int(delivered)));
        true
    } else if name.eq_ignore_ascii_case(b"INF.PUB") && matches!(argv.len(), 3 | 5) {
        // Owner leg of a remote PUBLISH: park for the owner pump (the
        // fabric is mutably borrowed by this drain; fan-out needs sends).
        pubs.push(OwnerPub {
            origin: from,
            token,
            channel: argv[1].to_vec(),
            payload: argv[2].to_vec(),
            tag: (argv.len() == 5).then(|| (argv[3].to_vec(), argv[4].to_vec())),
        });
        true
    } else if name.eq_ignore_ascii_case(b"INF.SUBD") && argv.len() == 4 {
        match SubKind::from_wire_tag(argv[1]) {
            Some(kind) => {
                let delta: i32 = if argv[3] == b"-1" { -1 } else { 1 };
                shared.pubsub.borrow_mut().apply_delta(kind, argv[2], from.0, delta);
                staged.push((from, token, StagedReply::Int(0)));
            }
            None => staged.push((from, token, StagedReply::Refused)),
        }
        true
    } else if name.eq_ignore_ascii_case(b"INF.PUBSUB") && argv.len() >= 2 {
        if argv[1].eq_ignore_ascii_case(b"NUMSUB") && argv.len() == 3 {
            let count = shared.pubsub.borrow().owned_count(argv[2]);
            staged.push((from, token, StagedReply::Int(count)));
        } else if argv[1].eq_ignore_ascii_case(b"CHANNELS") && argv.len() <= 3 {
            let names = shared.pubsub.borrow().live_owned_channels(argv.get(2).copied());
            let start = scratch.len();
            let mut w = RespWriter::new(scratch, Protocol::Resp2);
            w.array_header(names.len());
            for name in &names {
                w.bulk(name);
            }
            staged.push((from, token, StagedReply::Bytes(start, scratch.len())));
        } else {
            staged.push((from, token, StagedReply::Refused));
        }
        true
    } else {
        false
    }
}

/// Delivers one published message to this cell's local subscribers:
/// complete frames append to each subscriber connection's staged output
/// (per-connection protocol — RESP3 push, RESP2 array), channel
/// subscriptions before pattern subscriptions, then the output cap
/// (M1-S11) is enforced. Returns the receiver count and — when `defer`
/// names the publishing connection — its own frames, held back so they
/// follow the publish reply (the Redis self-delivery order).
fn deliver_local<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    channel: &[u8],
    payload: &[u8],
    defer: Option<ConnKey>,
) -> (i64, Vec<u8>) {
    let mut delivered: i64 = 0;
    let mut deferred = Vec::new();
    for key in shared.pubsub.borrow().channel_conns(channel) {
        delivered += i64::from(deliver_one(shared, key, defer, &mut deferred, |out, proto| {
            pubsub::write_message(out, proto, channel, payload);
        }));
    }
    for (pattern, conns) in shared.pubsub.borrow().matching_pattern_conns(channel) {
        for key in conns {
            delivered += i64::from(deliver_one(shared, key, defer, &mut deferred, |out, proto| {
                pubsub::write_pmessage(out, proto, &pattern, channel, payload);
            }));
        }
    }
    let node = &shared.node;
    node.pubsub_delivered.set(node.pubsub_delivered.get() + delivered.unsigned_abs());
    (delivered, deferred)
}

fn deliver_one<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    key: ConnKey,
    defer: Option<ConnKey>,
    deferred: &mut Vec<u8>,
    write: impl FnOnce(&mut Vec<u8>, Protocol),
) -> bool {
    let now_ms = shared.now.get().as_millis();
    let caps = shared.knobs.get().cob_pubsub;
    shared
        .with_conn(key, |conn| {
            if conn.closing {
                return false;
            }
            if defer == Some(key) {
                // The publisher's own frames ride the reply path instead
                // (emitted right after the receiver count).
                write(deferred, conn.cx.proto);
                return true;
            }
            write(&mut conn.out, conn.cx.proto);
            enforce_output_cap(&shared.node, conn, now_ms, caps);
            true
        })
        .unwrap_or(false)
}

/// `client-output-buffer-limit pubsub` (M1-S11): the hard cap kills at
/// once; the soft cap kills after `soft_ms` continuously over. The kill is
/// the CLIENT KILL handshake (registry mark + MAINTAIN sweep close) —
/// delivery never touches another connection's I/O state directly, and the
/// connection (with its buffered output) frees on close.
pub(super) fn enforce_output_cap(
    node: &NodeInfo,
    conn: &mut Conn,
    now_ms: u64,
    caps: (u64, u64, u64),
) {
    let (hard, soft, soft_ms) = caps;
    if conn.cob_kill_sent {
        return;
    }
    let used = conn.out.len() as u64;
    let over_hard = hard > 0 && used > hard;
    let soft_expired = if soft > 0 && soft_ms > 0 && used > soft {
        if conn.cob_soft_since_ms == 0 {
            conn.cob_soft_since_ms = now_ms.max(1);
        }
        now_ms.saturating_sub(conn.cob_soft_since_ms) >= soft_ms
    } else {
        conn.cob_soft_since_ms = 0;
        false
    };
    if (over_hard || soft_expired) && node.clients.borrow_mut().request_kill(conn.cx.id) {
        conn.cob_kill_sent = true;
        node.cob_disconnections.set(node.cob_disconnections.get() + 1);
    }
}

fn note_fan(node: &NodeInfo) {
    node.pubsub_fan_msgs.set(node.pubsub_fan_msgs.get() + 1);
}
