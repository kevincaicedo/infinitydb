//! Pub/sub plane programs (M1-S10/S11): subscription deltas to owner
//! cells, the owner-side publish pump, local delivery, and the output cap.

use core::convert::Infallible;

use super::*;

// ---- pub/sub plane programs (M1-S10/S11) ----------------------------------------

/// Pump-side dispatch for the six public pub/sub commands. Subscribe-family
/// ops mutate the connection state, sync this cell's registries, and ship
/// the 0→1/1→0 transition deltas — **awaited before the confirmation frames
/// are emitted**, so once a client sees its confirmation, a PUBLISH from
/// anywhere reaches it. PUBLISH routes to the channel's owner; PUBSUB is an
/// introspection program over the owner views.
#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column pubsub")]
#[allow(clippy::disallowed_types, reason = "container: A")]
pub(super) async fn dispatch_pubsub<
    'u,
    O: PlaneObserver + 'static,
    F: SegmentFs + Clone + 'static,
>(
    shared: &'u Rc<Shared<O, F>>,
    units: &'u GateUnits,
    key: ConnKey,
    id: CommandId,
    argv: &[&[u8]],
    proto: Protocol,
    pending: &mut VecDeque<PendingReply<'u>>,
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
            let mut frames = OpenReply::new(shared.take_reply_buf());
            let Some(changes) = shared.with_conn(key, |conn| {
                if adding {
                    pubsub::apply_subscribe(&names, kind, &mut conn.cx, frames.reply())
                } else {
                    let names = (!names.is_empty()).then_some(names.as_slice());
                    pubsub::apply_unsubscribe(names, kind, &mut conn.cx, frames.reply())
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
            let mut legs = Legs::new();
            for (kind, name, delta) in &notes {
                send_sub_delta(shared, units, *kind, name, *delta, &mut legs).await;
            }
            while legs.oldest().await.is_some() {}
            pending.push_back(PendingReply::Done(frames.finish()));
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
                let mut legs = CountedLegs::new(acc, proto);
                for cell in targets {
                    let fan = PubFanArgv::new(channel, payload, None);
                    let (to, origin) = (CellId(cell), ApplyOrigin::Program);
                    let Ok(sent) = send_windowed(&mut legs, shared, async |_| {
                        send_apply(shared, units, to, origin, Protocol::Resp2, 0, fan.argv()).await
                    })
                    .await;
                    match sent {
                        Ok(waiter) => {
                            note_fan(&shared.node);
                            legs.push(waiter);
                        }
                        // Derived never: a leg's five slices are within the
                        // codec's cap, and a command with no leg out has a
                        // unit. A fan short of a cell is the refusal, never a
                        // count that leaves out that cell's subscribers.
                        Err(refused) => {
                            debug_assert!(false, "a publish fan's leg is sent");
                            let reply = refused.into_reply(shared, proto);
                            legs.refuse(shared, reply);
                            break;
                        }
                    }
                }
                pending.push_back(PendingReply::Counted(legs));
                pending.extend(trailing_self_frames(shared, self_frames));
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
                let origin = ApplyOrigin::Program;
                match send_apply(shared, units, owner, origin, Protocol::Resp2, 0, &publ[..argc])
                    .await
                {
                    Ok(waiter) => {
                        pending.push_back(match seq {
                            Some(seq) => PendingReply::Publish { waiter, proto, seq },
                            None => PendingReply::Remote { waiter, proto },
                        });
                    }
                    Err(refused) => {
                        pending.push_back(PendingReply::Done(refused.into_reply(shared, proto)));
                    }
                }
            }
        }
        CommandId::Pubsub => {
            let reply = program_pubsub(shared, units, proto, argv).await;
            pending.push_back(PendingReply::Done(reply));
        }
        _ => unreachable!("is_plane_pubsub covers exactly the arms above"),
    }
    true
}

/// Ships one subscription transition: channel deltas go to the owner cell,
/// pattern deltas replicate to every cell (plus this cell's slot directly).
async fn send_sub_delta<'u, O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &'u Rc<Shared<O, F>>,
    units: &'u GateUnits,
    kind: SubKind,
    name: &[u8],
    delta: i32,
    legs: &mut Legs<'u>,
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
            } else {
                send_delta_leg(shared, units, owner, subd, legs).await;
            }
        }
        SubKind::Pattern => {
            shared.pubsub.borrow_mut().apply_delta(kind, name, shared.cell.0, delta);
            if !shared.route_local_only {
                for cell in peer_cells(shared) {
                    send_delta_leg(shared, units, cell, subd, legs).await;
                }
            }
        }
    }
}

/// One delta leg to `to`, its oldest sibling settled first while the task
/// has every unit out: a subscription's legs run in windows. Nothing reads
/// a delta's ack, but every fabric op replies, so each is awaited.
async fn send_delta_leg<'u, O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &'u Rc<Shared<O, F>>,
    units: &'u GateUnits,
    to: CellId,
    subd: &[&[u8]],
    legs: &mut Legs<'u>,
) {
    let Ok(sent) = send_windowed(legs, shared, async |_| {
        send_apply(shared, units, to, ApplyOrigin::Program, Protocol::Resp2, 0, subd).await
    })
    .await;
    match sent {
        Ok(waiter) => legs.push(waiter),
        // A delta the codec refuses is skipped: the subscription's reply is
        // its own frames, and a delta's four slices are within the cap.
        Err(SendRefused::Reply(_)) => {}
        Err(SendRefused::NoUnit) => debug_assert!(false, "a task with no leg out has a unit"),
    }
}

/// A closed connection's cell-level 1→0 transitions: kind, name and delta.
pub(super) type SubNotes = Vec<(SubKind, Vec<u8>, i32)>;

/// Close-path subscription cleanup, on the closed connection's own pump:
/// ships the 1→0 deltas and consumes the acks (nothing awaits them, but
/// every fabric op replies — credit and orphan-tripwire hygiene).
pub(super) async fn flush_sub_deltas<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    units: &GateUnits,
    notes: SubNotes,
) {
    let mut legs = Legs::new();
    for (kind, name, delta) in &notes {
        send_sub_delta(shared, units, *kind, name, *delta, &mut legs).await;
    }
    while legs.oldest().await.is_some() {}
}

/// Removes a draining connection from the local registries once its queue
/// is empty, returning the cell-level transitions to notify. The names
/// move out of the context's two lists, the ones that went 1→0 into the
/// notes, so nothing is copied.
pub(super) fn unsubscribe_closed_conn<
    O: PlaneObserver + 'static,
    F: SegmentFs + Clone + 'static,
>(
    shared: &Shared<O, F>,
    key: ConnKey,
    cx: &mut ConnCx,
) -> SubNotes {
    let mut notes = Vec::new();
    let mut ps = shared.pubsub.borrow_mut();
    for channel in cx.sub_channels.drain(..) {
        if ps.local_remove(SubKind::Channel, &channel, key) {
            notes.push((SubKind::Channel, channel, -1));
        }
    }
    for pattern in cx.sub_patterns.drain(..) {
        if ps.local_remove(SubKind::Pattern, &pattern, key) {
            notes.push((SubKind::Pattern, pattern, -1));
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
///
/// The reply waits on the fan's legs, so the legs draw on the credit every
/// general send leaves free toward each peer, through the cell's one fan
/// reserve: one fan open at a time, one leg toward each peer, so a leg
/// never waits for a credit an unanswered `INF.PUB` holds and every reply
/// it waits on is sent at a peer's drain (ADR-0192 D3). An overlapping pump
/// must keep one leg in flight per peer.
pub(super) async fn owner_pub_pump<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: Rc<Shared<O, F>>,
    units: GateUnits,
) {
    // The cell's one owner pump, made at plane start and never ending,
    // takes the reserve at its first poll and holds it across every park
    // (ADR-0192 D3: the owner pump holds it while it runs).
    let mut reserve = shared.fan_reserve.take();
    debug_assert!(reserve.is_some(), "the cell's one owner pump holds its fan reserve");
    loop {
        // The origins in rotation, each one's publishes in arrival order.
        let next = shared.owner_pubs.borrow_mut().pop_rotating(&shared.node);
        let Some((origin, item)) = next else {
            // Parked in the step that read every origin empty:
            // FABRIC-IN wakes it when it queues a publish.
            shared.pub_pump_parked.set(true);
            park().await;
            continue;
        };
        let (mut total, _) = deliver_local(&shared, item.channel(), item.payload(), None);
        let mut fold = |outcome: OwnedOutcome| {
            if let OwnedOutcome::Int(n) = outcome {
                total += n;
            }
        };
        let fanned = match reserve.as_mut() {
            Some(reserve) => {
                let mut legs = FanLegs::open(reserve, &shared.node.fabric_reserve_refused_cell);
                let fanned = fan_legs(&shared, &units, origin, &item, &mut legs, &mut fold).await;
                legs.settle(&mut fold).await;
                fanned
            }
            None => fan_unreserved(&shared, &units, origin, &item, &mut fold).await,
        };
        // A fan short of a cell is the refusal, never a count that leaves
        // out that cell's subscribers.
        let outcome = match fanned {
            Ok(()) => Outcome::Int(total),
            Err(_) => staged_outcome(&[], &StagedReply::Refused),
        };
        // Publish the reply now — the origin's pump is suspended on it.
        let mut fabric = shared.fabric.borrow_mut();
        fabric.reply(origin, item.token, &outcome);
        fabric.flush();
        drop(fabric);
        shared.recycle_cmd_buf(item.frame.into_buf());
    }
}

/// An owner pump's fan as a send's window: each settled leg's outcome goes
/// to the fan's fold.
struct Folded<'a, L, G> {
    legs: &'a mut L,
    fold: &'a mut G,
}

impl<G: FnMut(OwnedOutcome)> LegWindow for Folded<'_, Legs<'_>, G> {
    type Stop = Infallible;

    async fn settle_oldest<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        _shared: &Shared<O, F>,
    ) -> Result<bool, Infallible> {
        let Some(outcome) = self.legs.oldest().await else { return Ok(false) };
        (self.fold)(outcome);
        Ok(true)
    }
}

impl<G: FnMut(OwnedOutcome)> LegWindow for Folded<'_, FanLegs<'_, '_>, G> {
    type Stop = Infallible;

    async fn settle_oldest<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        &mut self,
        _shared: &Shared<O, F>,
    ) -> Result<bool, Infallible> {
        let Some(outcome) = self.legs.oldest().await else { return Ok(false) };
        (self.fold)(outcome);
        Ok(true)
    }
}

/// One fan's legs, one per subscriber-bearing peer in ascending order
/// (`fan_targets`), each on the reserved credit. `Err` is a leg that could
/// not be sent, derived never: the pump has a unit per peer and one leg
/// toward each, and a leg's five slices are within the codec's cap. The fan
/// sends no more, and the publisher reads the refusal.
async fn fan_legs<'u, O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &'u Rc<Shared<O, F>>,
    units: &'u GateUnits,
    origin: CellId,
    item: &OwnerPub,
    legs: &mut FanLegs<'_, 'u>,
    fold: &mut impl FnMut(OwnedOutcome),
) -> Result<(), SendRefused> {
    let targets = shared.pubsub.borrow().fan_targets(item.channel(), shared.cell.0);
    let mut window = Folded { legs, fold };
    for cell in targets {
        let tag = leg_tag(item, origin, cell);
        let Ok(sent) = send_windowed(&mut window, shared, async |window| {
            #[cfg(not(inf_canary_fan_draws_general))]
            let sent = send_fan_leg(
                shared,
                units,
                window.legs,
                CellId(cell),
                item.channel(),
                item.payload(),
                tag,
            )
            .await;
            // Canary: every leg a general send that waits for a credit, as if
            // no credit were kept for the fan.
            #[cfg(inf_canary_fan_draws_general)]
            let sent = {
                let fan = PubFanArgv::new(item.channel(), item.payload(), tag);
                let (to, origin) = (CellId(cell), ApplyOrigin::Program);
                send_apply(shared, units, to, origin, Protocol::Resp2, 0, fan.argv())
                    .await
                    .map(|waiter| window.legs.push(waiter))
            };
            sent
        })
        .await;
        if let Err(refused) = sent {
            debug_assert!(false, "an owner pump's fan leg is sent");
            return Err(refused);
        }
        note_fan(&shared.node);
    }
    Ok(())
}

/// The fan without the cell's reserve, derived never (the running pump
/// holds it): each leg a general send and its wait, counted as a refused
/// leg, the replies folded in leg order. `Err` is a leg that could not be
/// sent, as [`fan_legs`] has it, once every leg out has settled.
async fn fan_unreserved<'u, O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &'u Rc<Shared<O, F>>,
    units: &'u GateUnits,
    origin: CellId,
    item: &OwnerPub,
    fold: &mut impl FnMut(OwnedOutcome),
) -> Result<(), SendRefused> {
    let targets = shared.pubsub.borrow().fan_targets(item.channel(), shared.cell.0);
    let mut legs = Legs::new();
    let mut window = Folded { legs: &mut legs, fold };
    let mut fanned = Ok(());
    for cell in targets {
        let fan = PubFanArgv::new(item.channel(), item.payload(), leg_tag(item, origin, cell));
        let (to, origin) = (CellId(cell), ApplyOrigin::Program);
        let refused = &shared.node.fabric_reserve_refused_cell;
        refused.set(refused.get() + 1);
        let Ok(sent) = send_windowed(&mut window, shared, async |_| {
            send_apply(shared, units, to, origin, Protocol::Resp2, 0, fan.argv()).await
        })
        .await;
        match sent {
            Ok(waiter) => {
                note_fan(&shared.node);
                window.legs.push(waiter);
            }
            Err(refused) => {
                debug_assert!(false, "an owner pump's fan leg is sent");
                fanned = Err(refused);
                break;
            }
        }
    }
    while let Ok(true) = window.settle_oldest(shared).await {}
    fanned
}

/// ADR-0101 D2: the origin cell's leg echoes the publisher tag; every other
/// leg keeps the three-arg form.
fn leg_tag(item: &OwnerPub, origin: CellId, cell: u16) -> Option<(&[u8], &[u8])> {
    item.tag().filter(|_| cell == origin.0)
}

/// A publisher subscribed to its own channel reads its frames after its
/// count reply (Redis order), as a trailing reply; an empty stash goes back
/// to the pool and adds none.
fn trailing_self_frames<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    self_frames: OwnedReply,
) -> Option<PendingReply<'static>> {
    if self_frames.bytes().is_empty() {
        shared.recycle_reply_buf(self_frames.into_bytes());
        return None;
    }
    Some(PendingReply::Done(self_frames))
}

/// PUBSUB introspection over the cell registries: CHANNELS merges the owner
/// views (KEYS-style header arithmetic), NUMSUB asks each channel's owner,
/// NUMPAT answers locally (the pattern index is replicated).
pub(super) async fn program_pubsub<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    units: &GateUnits,
    proto: Protocol,
    argv: &[&[u8]],
) -> OwnedReply {
    let sub = argv[1];
    if sub.eq_ignore_ascii_case(b"CHANNELS") && argv.len() <= 3 {
        let pattern = argv.get(2).copied();
        let local = shared.pubsub.borrow().live_owned_channels(pattern);
        let mut legs = OrderedLegs::new();
        if shared.cells > 1 && !shared.route_local_only {
            let mut request: Vec<&[u8]> = vec![b"INF.PUBSUB", b"CHANNELS"];
            if let Some(p) = pattern {
                request.push(p);
            }
            for cell in peer_cells(shared) {
                match send_ordered(shared, units, cell, &request, &mut legs).await {
                    Ok(()) => {}
                    Err(refused) => return refused.into_reply(shared, proto),
                }
            }
        }
        merge_channels(shared, proto, &local, legs).await
    } else if sub.eq_ignore_ascii_case(b"NUMSUB") {
        enum Count {
            Local(i64),
            Remote,
        }
        let mut legs = OrderedLegs::new();
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
                match send_ordered(shared, units, owner, numsub, &mut legs).await {
                    Ok(()) => parts.push((name, Count::Remote)),
                    Err(refused) => return refused.into_reply(shared, proto),
                }
            }
        }
        let mut reply = OpenReply::new(shared.take_reply_buf());
        RespWriter::new(reply.reply(), proto).array_header(parts.len() * 2);
        for (name, count) in parts {
            let count = match count {
                Count::Local(n) => n,
                Count::Remote => match legs.next_outcome().await {
                    Some(OwnedOutcome::Int(n)) => n,
                    None => 0,
                    Some(OwnedOutcome::Reply(unused)) => {
                        shared.recycle_reply_buf(unused.into_bytes());
                        0
                    }
                    Some(
                        OwnedOutcome::Ok
                        | OwnedOutcome::Nil
                        | OwnedOutcome::Bool(_)
                        | OwnedOutcome::Err(_),
                    ) => 0,
                },
            };
            let mut w = RespWriter::new(reply.reply(), proto);
            w.bulk(name);
            w.int(count);
        }
        reply.finish()
    } else if sub.eq_ignore_ascii_case(b"NUMPAT") && argv.len() == 2 {
        int_reply(shared, proto, shared.pubsub.borrow().live_pattern_count() as i64)
    } else {
        let mut reply = OpenReply::new(shared.take_reply_buf());
        pubsub::pubsub_subcommand_error(sub, &mut RespWriter::new(reply.reply(), proto));
        reply.finish()
    }
}

/// One program leg whose outcome is read in leg order, its oldest sibling
/// settled first while the task has every unit out.
async fn send_ordered<'u, O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &'u Rc<Shared<O, F>>,
    units: &'u GateUnits,
    to: CellId,
    argv: &[&[u8]],
    legs: &mut OrderedLegs<'u>,
) -> Result<(), SendRefused> {
    let Ok(sent) = send_windowed(legs, shared, async |_| {
        send_apply(shared, units, to, ApplyOrigin::Program, Protocol::Resp2, 0, argv).await
    })
    .await;
    legs.push(sent?);
    Ok(())
}

/// `PUBSUB CHANNELS`' merge: this cell's names, then each peer's array
/// body, under one header (KEYS-style header arithmetic). Each peer leg is
/// read by `read_array_leg`, as `KEYS`'s are: its error is the reply, and a
/// success without an array header is a malformed peer reply, never the
/// client's whole answer (ADR-0190 D2, A1.2).
async fn merge_channels<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    local: &[Vec<u8>],
    mut legs: OrderedLegs<'_>,
) -> OwnedReply {
    let mut total = local.len();
    let mut bodies = Vec::new();
    // A fan of reads that has its answer returns it and drops the legs still out: each wait
    // gives its unit back at the drop and leaves its holder, Orphaned, until that request's one
    // reply (ADR-0177 D4). A fan that writes settles every leg it sent before it answers, so a
    // stop's barrier, which waits for no connection to be left, finds no write in flight
    // (ADR-0124 D2).
    while let Some(outcome) = legs.next_outcome().await {
        match outcome {
            OwnedOutcome::Reply(reply) => match read_array_leg(shared, proto, reply) {
                Ok(leg) => {
                    total += leg.count();
                    bodies.push(leg);
                }
                Err(answer) => return answer,
            },
            OwnedOutcome::Ok
            | OwnedOutcome::Int(_)
            | OwnedOutcome::Nil
            | OwnedOutcome::Bool(_)
            | OwnedOutcome::Err(_) => {
                return error_reply(shared, proto, CROSS_CELL_EXECUTION_FAILED);
            }
        }
    }
    let mut reply = OpenReply::new(shared.take_reply_buf());
    {
        let mut w = RespWriter::new(reply.reply(), proto);
        w.array_header(total);
        for name in local {
            w.bulk(name);
        }
        for body in &bodies {
            w.encoded(body.body());
        }
    }
    for body in bodies {
        shared.recycle_reply_buf(body.into_reply().into_bytes());
    }
    reply.finish()
}

/// Owner-side handling of the internal pub/sub Apply vocabulary. Returns
/// false when `argv` is not pub/sub plumbing (the caller falls through to
/// normal Apply execution).
pub(super) fn handle_pubsub_apply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    from: CellId,
    token: FabricToken,
    argv: &[&[u8]],
    scratch: &mut ReplyStream,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
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
        let self_frames = self_frames.into_bytes();
        if let Some((conn, seq)) = tag
            && !self_frames.is_empty()
        {
            // The stash is a writer into the connection's output: a slot
            // that cannot send drops the frames.
            let dropped = shared.with_conn(conn, |c| match c.link_mut() {
                Some(link) => {
                    link.self_push.push((seq, self_frames));
                    None
                }
                None => Some(self_frames),
            });
            if let Some(Some(frames)) = dropped {
                shared.recycle_reply_buf(frames);
            }
        }
        staged.push((from, token, StagedReply::Int(delivered)));
        true
    } else if name.eq_ignore_ascii_case(b"INF.PUB") && matches!(argv.len(), 3 | 5) {
        // Owner leg of a remote PUBLISH: park for the owner pump (the
        // fabric is mutably borrowed by this drain; fan-out needs sends),
        // its origin's deque reserving before the frame's one flat copy
        // (ADR-0177 D6); a full deque, derived never, refuses before any
        // effect.
        let parked = shared.owner_pubs.borrow_mut().publish_with(from, &shared.node, || {
            let mut buf = shared.take_cmd_buf();
            buf.clear();
            stage_argv_block(&mut buf, &argv[1..]);
            OwnerPub { token, frame: OwnedCmd::from_staged(buf) }
        });
        if let Err(ParkedFull) = parked {
            staged.push((from, token, StagedReply::Refused));
        }
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
            let reply = scratch.open();
            let mut w = RespWriter::new(reply, Protocol::Resp2);
            w.array_header(names.len());
            for name in &names {
                w.bulk(name);
            }
            let outcome = reply.outcome();
            staged.push((from, token, StagedReply::Bytes(start, scratch.len(), outcome)));
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
) -> (i64, OwnedReply) {
    let mut delivered: i64 = 0;
    // The publisher's own frames, held back to follow the publish reply:
    // pushes, whose outcome nothing reads. Opened over an empty buffer, as
    // the common publisher is not itself subscribed.
    let mut deferred = OpenReply::new(Vec::new());
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
    (delivered, deferred.finish())
}

fn deliver_one<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    key: ConnKey,
    defer: Option<ConnKey>,
    deferred: &mut OpenReply,
    write: impl FnOnce(&mut ReplyBuf, Protocol),
) -> bool {
    let now_ms = shared.now.get().as_millis();
    let caps = shared.knobs.get().cob_pubsub;
    shared
        .with_conn(key, |conn| {
            let (proto, id) = (conn.cx.proto, conn.cx.id);
            match &mut conn.slot {
                // A closing or draining slot is not a receiver.
                ConnSlot::Closing { .. } | ConnSlot::Draining { .. } => false,
                // Canary: a flushing slot's answer before the defer branch,
                // so the publisher's own frames are dropped.
                #[cfg(inf_canary_flushing_drops_own_frame)]
                ConnSlot::Flushing { stream, .. } => matches!(stream, PeerStream::Open),
                // The publisher's own frames ride the reply path instead
                // (emitted right after the receiver count), in Active and
                // Flushing alike (ADR-0101).
                ConnSlot::Active { .. } | ConnSlot::Flushing { .. } if defer == Some(key) => {
                    write(deferred.reply(), proto);
                    true
                }
                ConnSlot::Active { link, .. } => {
                    write(link.out.open(), proto);
                    enforce_output_cap(&shared.node, link, id, now_ms, caps);
                    true
                }
                // Canary: the delivery is written into a flushing slot.
                #[cfg(inf_canary_flushing_takes_delivery)]
                ConnSlot::Flushing { link, .. } => {
                    write(link.out.open(), proto);
                    true
                }
                // A flushing slot is written nothing that arrives after its
                // mark or its end of stream, and counts as a receiver until
                // the server reads its end of stream (ADR-0196 D4, A1.2).
                #[cfg(not(inf_canary_flushing_takes_delivery))]
                ConnSlot::Flushing { stream, .. } => {
                    let node = &shared.node;
                    let dropped = node.flushing_deliveries_dropped.get() + 1;
                    node.flushing_deliveries_dropped.set(dropped);
                    // Canary: the count reads the parser, not the stream, so
                    // a flushing `Ended` slot whose parser ended is counted.
                    #[cfg(inf_canary_half_closed_counted)]
                    return !matches!(stream, PeerStream::Ended { by: ClosedBy::Peer { .. } });
                    #[cfg(not(inf_canary_half_closed_counted))]
                    match stream {
                        PeerStream::Open => true,
                        PeerStream::Ended { .. } => false,
                    }
                }
            }
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
    conn: &mut Link,
    id: u64,
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
    if (over_hard || soft_expired) && node.clients.borrow_mut().request_kill(id) {
        conn.cob_kill_sent = true;
        node.cob_disconnections.set(node.cob_disconnections.get() + 1);
    }
}

fn note_fan(node: &NodeInfo) {
    node.pubsub_fan_msgs.set(node.pubsub_fan_msgs.get() + 1);
}
