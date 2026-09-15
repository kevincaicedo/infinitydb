//! Namespace DDL and checkpoint programs: `INF.NS CREATE/DROP/SET`, the
//! catalog fan to every cell with persist-then-fan ordering (ADR-0100/
//! ADR-0103), rollback, and the apply-send path with its credit wait.

use super::*;

/// One named-namespace command on the pump (M2-S08). Returns `false` when
/// the connection is gone.
#[allow(clippy::too_many_arguments)] // the pump dispatch context
pub(super) async fn dispatch_ns<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    key: ConnKey,
    origin: ExecOrigin,
    meta: &'static inf_wire::CommandMeta,
    argv: &[&[u8]],
    proto: Protocol,
    id: u64,
    db: u16,
    ns: NsId,
    pending: &mut VecDeque<PendingReply>,
    inflight: &mut usize,
) -> bool {
    // The registry is authoritative: a dropped namespace answers a typed
    // error before any routing.
    let class = {
        let ks = shared.store.borrow();
        if ks.ns_get_by_id(ns).is_none() {
            pending.push_back(PendingReply::Done(error_reply(
                shared,
                proto,
                "ERR the selected namespace was dropped (INF.NS USE again)",
            )));
            return true;
        }
        ks.ns_fsync_class(ns)
    };
    let keys = extract_keys_slices(meta, argv);
    let owner_of = |k: &[u8]| shared.router.cell_of(SlotRouter::slot_of(k));
    if !keys.is_empty() && !shared.route_local_only {
        let owner = owner_of(keys[0]);
        if keys[1..].iter().any(|k| owner_of(k) != owner) {
            // M3-S11: the JSON surface binds the named-ns multi-key
            // programs (exactly the ADR-0032 D5 plan) — the MGET gather
            // with ns-aware sub-ops. The generic refusal below stays for
            // every other command until M5.
            #[cfg(feature = "doc")]
            if meta.id == CommandId::JsonMget {
                let path = argv[argv.len() - 1];
                let mut parts = Vec::with_capacity(argv.len() - 2);
                for k in &argv[1..argv.len() - 1] {
                    let sub: [&[u8]; 3] = [b"JSON.MGET", k, path];
                    if shared.router.is_local(k, shared.cell) {
                        let mut buf = shared.take_reply_buf();
                        shared.execute_owned_into(
                            origin,
                            &sub,
                            proto,
                            id,
                            db,
                            Some(ns),
                            false,
                            &mut buf,
                        );
                        parts.push(GatherPart::Done(buf));
                    } else {
                        match send_apply_ns(
                            shared,
                            owner_of(k),
                            ApplyOrigin::Client,
                            proto,
                            ns,
                            &sub,
                        )
                        .await
                        {
                            Ok(waiter) => {
                                parts.push(GatherPart::Wait(waiter));
                                *inflight += 1;
                            }
                            Err(refusal) => parts.push(GatherPart::Done(refusal)),
                        }
                    }
                }
                pending.push_back(PendingReply::Gather { parts, proto, unwrap_single: true });
                return true;
            }
            // Recorded M2 limitation (ADR-0015 deviations): multi-key
            // commands spanning cells bind with the M3 named-ns programs.
            pending.push_back(PendingReply::Done(error_reply(
                shared,
                proto,
                "ERR multi-key commands spanning cells are not yet supported in named namespaces \
                     (M2)",
            )));
            return true;
        }
        if owner.0 != shared.cell.0 {
            match send_apply_ns(shared, owner, ApplyOrigin::Client, proto, ns, argv).await {
                Ok(waiter) => {
                    *inflight += 1;
                    pending.push_back(PendingReply::Remote { waiter, proto });
                }
                Err(refusal) => pending.push_back(PendingReply::Done(refusal)),
            }
            return true;
        }
    }
    // `DBSIZE` on a namespace-bound connection is the node-wide count
    // (review of 2026-08-28, M4.5-S37 finding 2 — it answered this cell's
    // table alone while the compat matrix declared it `full`): the local
    // leg — the tiered drain (ADR-0093 A3) or the memory table's count —
    // plus every peer's typed contribution through `ApplyNs`, the
    // `Counted` shape the default database's scatter uses. A leg that
    // errors or cannot be sent makes the reply that error, never a
    // partial sum.
    if meta.id == CommandId::Dbsize && shared.cells > 1 && !shared.route_local_only {
        let acc = match ns_dbsize_local(shared, origin, ns, proto).await {
            Ok(n) => n,
            Err(reply) => {
                pending.push_back(PendingReply::Done(reply));
                return true;
            }
        };
        let mut waiters = Vec::new();
        let mut refusal = None;
        for cell in peer_cells(shared) {
            match send_apply_ns(shared, cell, ApplyOrigin::Client, proto, ns, &[b"DBSIZE"]).await {
                Ok(waiter) => {
                    waiters.push(waiter);
                    *inflight += 1;
                }
                Err(reply) => {
                    refusal = Some(reply);
                    break;
                }
            }
        }
        pending.push_back(PendingReply::Counted { waiters, acc, proto, refusal });
        return true;
    }
    // Node-wide iteration + the random probe (review of 2026-08-30, C1 /
    // F-L13-07): a namespace-bound connection reaches the same scatter
    // programs the default database uses, with `ApplyNs` legs — before
    // this arm they fell through to local execution and served one cell
    // of `cells` while reporting a complete answer (`SCAN` cursor 0,
    // `KEYS` with no marker). Single-cell nodes keep the local path
    // below — there the cell is the node.
    if matches!(meta.id, CommandId::Keys | CommandId::Scan | CommandId::Randomkey)
        && shared.cells > 1
        && !shared.route_local_only
    {
        let scope = ScatterScope::Ns(ns);
        let reply = match meta.id {
            CommandId::Keys => program_keys(shared, origin, proto, id, db, scope, argv).await,
            CommandId::Scan => program_scan(shared, origin, proto, id, db, scope, argv).await,
            _ => program_randomkey(shared, origin, proto, id, db, scope, argv).await,
        };
        pending.push_back(PendingReply::Done(reply));
        return true;
    }
    // Tiered namespaces execute through the async tiered arm (M4-S26):
    // suspension-capable resolution with its own admission + staging.
    // Keyspace-level commands (INFO, CONFIG, INF.NS, SELECT, pub/sub)
    // stay on the ordinary path — `execute` owns them regardless of the
    // selected namespace — and so does every command that addresses
    // nothing in the keyspace (`KeyspaceScope::None`: `PING`, `ECHO`,
    // `HELLO`, `QUIT`, `CLIENT`, `COMMAND`, `LOLWUT`, `DEBUG SLEEP`, …).
    // ADR-0108, review of 2026-08-30 (the batch-8 residual): the arm
    // below used to see them and answer its string-family refusal — a
    // hand-kept list is exactly the shape Theme 3 forbids, so the class
    // is read from the registry (`inf_wire::keyspace_scope`) and its
    // totality is asserted by a table-iterating test.
    let keyspace_level = keyspace_level(meta, argv);
    if !keyspace_level && shared.store.borrow().is_tiered(ns) {
        match tiered::dispatch_tiered(shared, origin, ns, meta, argv, proto, class).await {
            tiered::TieredReply::Done(reply) => pending.push_back(PendingReply::Done(reply)),
            tiered::TieredReply::Gated { reply, seq } => {
                let waiter = {
                    let mut durable = shared.durable.borrow_mut();
                    let cell = durable.as_mut().expect("tiered implies the durable plane");
                    cell.note_gated_ack();
                    cell.ack_gate.waiter(seq)
                };
                pending.push_back(PendingReply::Durable { waiter, reply });
            }
        }
        return true;
    }
    // Local path: durable admission parks on the drain waitlist instead of
    // erroring — with ADR-0083 the fabric path paces the same way, so the
    // typed verdict is shared and the two paths cannot drift (M4.5-S27).
    let is_write = meta.flags.contains(CmdFlags::WRITE);
    if class.is_some() && is_write {
        loop {
            match shared.durable_admission(ns, meta, argv) {
                DurableAdmission::Admit => break,
                DurableAdmission::Refuse(refusal) => {
                    pending.push_back(PendingReply::Done(error_reply(shared, proto, refusal)));
                    return true;
                }
                DurableAdmission::Park => {
                    let wait = {
                        let mut durable = shared.durable.borrow_mut();
                        let cell = durable.as_mut().expect("durable admission ran");
                        cell.note_parked();
                        cell.drained.wait(())
                    };
                    wait.await;
                }
            }
        }
    }
    // Maintenance bracket, local pump named-ns row (ADR-0072 D3 /
    // ADR-0076 D3 row 2): pre-half after the admission loop, before
    // execute; commit-half after the staging window.
    #[cfg(feature = "doc")]
    let bracket: Option<Option<inf_doc::PathProgram>> = if is_write
        && meta.id != CommandId::Copy
        && !keys.is_empty()
        && shared.store.borrow().ns_indexed(ns)
    {
        Some(shared.json_mutation_path(meta.id, argv))
    } else {
        None
    };
    #[cfg(feature = "doc")]
    if let Some(path) = &bracket
        && let Err(refusal) = shared.store.borrow_mut().idx_bracket_begin(ns, &keys, path.as_ref())
    {
        pending.push_back(PendingReply::Done(error_reply(shared, proto, refusal.message())));
        return true;
    }
    let mut reply = shared.take_reply_buf();
    let close = shared.execute_owned_into(origin, argv, proto, id, db, Some(ns), false, &mut reply);
    if close {
        close_after_reply(shared, key);
    }
    let gated = if is_write
        && (reply.first() != Some(&b'-') || stages_despite_error(meta.id))
        && let Some(class) = class
        && let Some(seq) = shared.stage_durable_effects(ns, meta, argv, class)
        && class == FsyncClass::Always
    {
        Some(seq)
    } else {
        None
    };
    #[cfg(feature = "doc")]
    if bracket.is_some() {
        shared.store.borrow_mut().idx_bracket_commit(ns, &keys);
    }
    if let Some(seq) = gated {
        let waiter = {
            let mut durable = shared.durable.borrow_mut();
            let cell = durable.as_mut().expect("staged above");
            cell.note_gated_ack();
            cell.ack_gate.waiter(seq)
        };
        pending.push_back(PendingReply::Durable { waiter, reply });
        return shared.with_conn(key, |_| ()).is_some();
    }
    pending.push_back(PendingReply::Done(reply));
    true
}

/// `INF.CKPT [CELL k] [WAIT]` + `BGSAVE`/`LASTSAVE` (M2-S20, ADR-0021
/// D6): requests bump the control board's epochs (cells run checkpoints
/// in their own MAINTAIN slices — L1); `WAIT` parks the pump until every
/// targeted slot's **published** epoch covers the request. Publication
/// happens at the MANIFEST swap's dir-fsync commit, so `WAIT` returns
/// only after durability — a swap abort does not publish; the retried
/// swap does (fault-injection verified). `LASTSAVE` = unix seconds of
/// the newest publication across cells (0 before the first — deviation
/// documented; Redis reports process-start time).
pub(super) async fn program_ckpt<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    id: CommandId,
    argv: &[&[u8]],
) -> Vec<u8> {
    let no_plane = "ERR checkpointing requires a durable node (no data dir)";
    let control = shared.control.borrow().clone();
    let Some(control) = control else {
        return error_reply(shared, proto, no_plane);
    };
    if shared.durable.borrow().is_none() {
        return error_reply(shared, proto, no_plane);
    }
    if id == CommandId::Lastsave {
        let unix_s = control.ckpt_board().max_unix_ms() / 1000;
        return int_reply(shared, proto, unix_s as i64);
    }
    let mut cell: Option<u16> = None;
    let mut wait = false;
    if id == CommandId::InfCkpt {
        let mut i = 1;
        while i < argv.len() {
            if argv[i].eq_ignore_ascii_case(b"WAIT") {
                wait = true;
                i += 1;
            } else if argv[i].eq_ignore_ascii_case(b"CELL") && i + 1 < argv.len() {
                let parsed = core::str::from_utf8(argv[i + 1]).ok().and_then(|s| s.parse().ok());
                let Some(k) = parsed.filter(|&k: &u16| k < shared.cells) else {
                    return error_reply(
                        shared,
                        proto,
                        "ERR CELL wants an index below the cell count",
                    );
                };
                cell = Some(k);
                i += 2;
            } else {
                return error_reply(shared, proto, "ERR syntax: INF.CKPT [CELL k] [WAIT]");
            }
        }
    } else if argv.len() > 2 || (argv.len() == 2 && !argv[1].eq_ignore_ascii_case(b"SCHEDULE")) {
        // BGSAVE [SCHEDULE]: SCHEDULE is accepted and moot — checkpoints
        // never fork, so there is nothing to defer (deviation documented).
        return error_reply(shared, proto, "ERR syntax: BGSAVE [SCHEDULE]");
    }
    let epoch = match cell {
        Some(k) => control.request_ckpt_cell(k),
        None => control.request_ckpt_all(),
    };
    if wait {
        loop {
            let board = control.ckpt_board();
            let satisfied = match cell {
                Some(k) => board.slot(k).published() >= epoch,
                None => board.min_published() >= epoch,
            };
            if satisfied {
                break;
            }
            shared.ckpt_waiters.wait(0).await;
        }
    }
    if id == CommandId::Bgsave {
        simple_reply(shared, proto, "Background saving started")
    } else {
        simple_reply(shared, proto, "OK")
    }
}

/// The namespace-DDL program (M2-S08, ADR-0015 D2/D3): parse → allocate id
/// (CREATE) → apply locally → fan `INF.NSFAN` to every peer (AllOk) →
/// persist the catalog through the control thread → `+OK` only after the
/// swap is durable. `DROP` reorders to *apply → request persist → fan
/// (carrying the persist epoch) → wait → request checkpoint + stamp →
/// `+OK`* (ADR-0100 D3/D4), so every cell can hold its tier-file teardown
/// on the swap that makes the drop durable.
pub(super) async fn program_ns_ddl<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> Vec<u8> {
    let _ = (origin, id, db);
    let Some(control) = shared.control.borrow().clone() else {
        return error_reply(
            shared,
            proto,
            "ERR namespace DDL requires the node control plane (planeless tier is read-only here)",
        );
    };
    // ADR-0108 D1: one namespace DDL program per node at a time. Two
    // programs whose fans crossed left a namespace on some cells only
    // (a `DROP` landing between two `CREATE` legs — the `m2-ns-ddl-race`
    // DST's phantom); the ticket serializes them from before the first
    // effect to after the reply, on every exit path (it is a guard).
    // Parked programs wake on the release edge MAINTAIN detects.
    let _ticket = loop {
        if let Some(ticket) = control.ddl_try_acquire(shared.cell.0) {
            break ticket;
        }
        shared.ddl_waiters.wait(0).await;
    };
    let create = argv[1].eq_ignore_ascii_case(b"CREATE");
    // ADR-0103: a CREATE persisted before the fan; its pending entry
    // retires once the fan is done (either way), and it needs no
    // trailing persist. `created` carries what a rollback needs
    // (ADR-0108 D3): the id, the name, and whether the namespace is
    // durable (tombstone) / tiered (teardown hold).
    let mut created: Option<(u32, Vec<u8>, bool, bool)> = None;
    let fan: Vec<Vec<u8>> = if create {
        let draft = match crate::admin::parse_ns_create(argv) {
            Ok(draft) => draft,
            Err(msg) => return error_reply(shared, proto, &msg),
        };
        if draft.mode == NsMode::Durable && shared.durable.borrow().is_none() {
            return error_reply(
                shared,
                proto,
                "ERR this node has no durable storage (start infinityd with a data dir)",
            );
        }
        let ns_id = control.alloc_ns_id();
        let spec = draft.with_id(ns_id);
        // ADR-0103 D1 (review of 2026-08-30, C14): persist before
        // serving. Validate without applying (D3), swap `META` with the
        // spec in it, read the writer's verdict, and only then make the
        // namespace exist anywhere — a namespace any cell can name is
        // one the catalog already names. The pre-ADR order (apply → fan
        // → persist) served the namespace on every cell for the whole
        // swap window and lost `always`-acked writes to a cut inside it.
        if let Err(e) = shared.store.borrow().ns_create_check(&spec) {
            let mut reply = shared.take_reply_buf();
            crate::admin::ns_error(e, &mut RespWriter::new(&mut reply, proto));
            return reply;
        }
        if control.pending_creates() >= crate::control::PENDING_CREATE_MAX {
            return error_reply(
                shared,
                proto,
                "BUSY too many namespace creations in flight — retry (ADR-0103 D2)",
            );
        }
        let (epoch, verdict) = control.request_persist_create(
            shared.store.borrow().export_catalog(
                control.next_ns_id(),
                control.next_index_id(),
                control.next_index_generation(),
            ),
            spec.clone(),
        );
        while !control.persisted(epoch) {
            shared.ddl_waiters.wait(0).await;
        }
        match verdict.get() {
            Some(crate::control::CreateOutcome::Accepted) => {}
            Some(crate::control::CreateOutcome::NameExists) => {
                let mut reply = shared.take_reply_buf();
                crate::admin::ns_error(
                    inf_store::NsError::Exists,
                    &mut RespWriter::new(&mut reply, proto),
                );
                return reply;
            }
            Some(crate::control::CreateOutcome::AtCapacity) => {
                return error_reply(
                    shared,
                    proto,
                    "BUSY too many namespace creations in flight — retry (ADR-0103 D2)",
                );
            }
            None => {
                debug_assert!(false, "the writer sets the verdict before the epoch");
                control.create_applied(ns_id);
                return error_reply(shared, proto, "ERR internal: catalog verdict missing");
            }
        }
        // Crash-matrix point (ADR-0103 D5): the on-disk state of a cut
        // after the swap — META names a namespace no cell serves.
        if inf_foundation::fault::fire(crate::fault::NS_CREATE_AFTER_META) {
            control.create_applied(ns_id);
            return error_reply(shared, proto, "ERR fault: ns_create_after_meta");
        }
        if let Err(e) = shared.store.borrow_mut().ns_create(spec.clone()) {
            // The OS refused the ring reservation (the one check D3
            // cannot run ahead): the pending entry retires and a later
            // persist drops the definition (recorded residual).
            control.create_applied(ns_id);
            let mut reply = shared.take_reply_buf();
            crate::admin::ns_error(e, &mut RespWriter::new(&mut reply, proto));
            return reply;
        }
        created =
            Some((ns_id, spec.name.clone(), spec.mode == NsMode::Durable, spec.tier.is_some()));
        let fsync = spec.fsync.map_or("-", |f| match f {
            FsyncClass::Everysec => "everysec",
            FsyncClass::Always => "always",
        });
        let policy = spec.policy.map_or("-", inf_store::EvictionPolicy::name);
        let maxmemory = spec.maxmemory.map_or_else(|| "-".to_string(), |b| b.to_string());
        vec![
            b"INF.NSFAN".to_vec(),
            b"CREATE".to_vec(),
            spec.name.clone(),
            spec.mode.name().as_bytes().to_vec(),
            fsync.as_bytes().to_vec(),
            policy.as_bytes().to_vec(),
            maxmemory.into_bytes(),
            ns_id.to_string().into_bytes(),
            tier_to_fan(spec.tier.as_ref()),
        ]
    } else if argv[1].eq_ignore_ascii_case(b"SET") {
        // M4-S19 (ADR-0062 D3) / M4-S27 (ADR-0068 D3): hot-reload is DDL
        // — registry + store update locally, fan to peers, catalog
        // persist-then-ack.
        let (name, update) = {
            let store = shared.store.borrow();
            match crate::admin::parse_ns_set(argv, &store) {
                Ok(parsed) => parsed,
                Err(msg) => return error_reply(shared, proto, &msg),
            }
        };
        let fan_tail = match &update {
            crate::admin::NsSetUpdate::Tier(tier) => vec![tier_to_fan(Some(tier))],
            crate::admin::NsSetUpdate::MemoryPressure { policy, maxmemory } => vec![
                b"MEMCFG".to_vec(),
                policy.map_or_else(|| b"-".to_vec(), |p| p.name().as_bytes().to_vec()),
                maxmemory.map_or_else(|| b"-".to_vec(), |b| b.to_string().into_bytes()),
            ],
        };
        if let Err(e) = crate::admin::apply_ns_set(&mut shared.store.borrow_mut(), &name, update) {
            let mut reply = shared.take_reply_buf();
            crate::admin::ns_error(e, &mut RespWriter::new(&mut reply, proto));
            return reply;
        }
        let mut fan = vec![b"INF.NSFAN".to_vec(), b"SET".to_vec(), name];
        fan.extend(fan_tail);
        fan
    } else {
        if argv.len() != 3 {
            return error_reply(shared, proto, "ERR wrong number of arguments for 'INF.NS|DROP'");
        }
        return program_ns_drop(shared, &control, proto, argv[2]).await;
    };
    // Fan to peers (AllOk — partial failure surfaces as the first error
    // leg, the recorded M1 scatter semantics). Every leg answers: a leg
    // that cannot be sent is a failed leg, never a skipped one (the H1
    // shape — `if let Ok` silently dropped the cell).
    let fan_argv: Vec<&[u8]> = fan.iter().map(Vec::as_slice).collect();
    let failure = fan_all_or_first_error(shared, proto, &fan_argv).await;
    if let Some((id, name, durable, tiered)) = created {
        if let Some(error) = failure {
            // ADR-0108 D3: a `CREATE` whose fan failed on any peer rolls
            // back — the origin drops its copy, fans `DROP`, persists the
            // drop — so the namespace's existence is exactly what the
            // reply says. Before this the peers that accepted their leg
            // (and the origin) served it while the client held an error,
            // and `META` kept naming it until the next persist.
            rollback_create(shared, &control, proto, id, &name, durable, tiered).await;
            return error;
        }
        // Every leg answered `+OK`: each cell's export carries the
        // namespace from here on (ADR-0103 D2).
        control.create_applied(id);
        return simple_reply(shared, proto, "OK");
    }
    if let Some(error) = failure {
        return error;
    }
    // Persist the catalog; ack only once the swap is durable (a DDL whose
    // definition can vanish after +OK would be a §8.2 violation).
    let epoch = control.request_persist(shared.store.borrow().export_catalog(
        control.next_ns_id(),
        control.next_index_id(),
        control.next_index_generation(),
    ));
    while !control.persisted(epoch) {
        shared.ddl_waiters.wait(0).await;
    }
    simple_reply(shared, proto, "OK")
}

/// `INF.NS DROP` (ADR-0100 D3/D4/D5): apply locally → request the catalog
/// persist that carries the drop (a durable namespace's tombstone joins
/// the payload) → fan `INF.NSFAN DROP name epoch` so every peer holds its
/// tier-file teardown on that epoch → wait for the swap → request the
/// node-wide checkpoint that retires the tombstone and stamp it → `+OK`.
/// At the tombstone cap the drop first waits for a node-wide checkpoint
/// (the `INF.CKPT WAIT` machinery) so the persist retires everything
/// stamped — bounded backpressure, never an unbounded set.
async fn program_ns_drop<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    control: &Arc<ControlHandle>,
    proto: Protocol,
    name: &[u8],
) -> Vec<u8> {
    let durable = shared.store.borrow().ns_get(name).is_some_and(|s| s.mode == NsMode::Durable);
    if durable && control.drop_tombstones() >= crate::control::DROPPED_NS_MAX {
        let epoch = control.request_ckpt_all();
        while control.ckpt_board().min_published() < epoch {
            shared.ckpt_waiters.wait(0).await;
        }
    }
    let spec = match shared.store.borrow_mut().ns_drop(name) {
        Ok(spec) => spec,
        Err(e) => {
            let mut reply = shared.take_reply_buf();
            crate::admin::ns_error(e, &mut RespWriter::new(&mut reply, proto));
            return reply;
        }
    };
    // Crash-matrix point: the on-disk state of a cut before the swap
    // (nothing durable changed; the origin alone lacks the namespace).
    // No persist will ever carry this drop, so the tier files hold
    // until the restart restores the namespace from the intact META.
    if inf_foundation::fault::fire(crate::fault::NS_DROP_BEFORE_META) {
        if spec.tier.is_some() {
            shared.ns_drop_releases.borrow_mut().push((spec.id, u64::MAX));
        }
        return error_reply(shared, proto, "ERR fault: ns_drop_before_meta");
    }
    let drop = (spec.mode == NsMode::Durable).then_some(spec.id.0);
    let epoch = control.request_persist_drop(
        shared.store.borrow().export_catalog(
            control.next_ns_id(),
            control.next_index_id(),
            control.next_index_generation(),
        ),
        spec.id.0,
        drop.is_some(),
    );
    if spec.tier.is_some() {
        shared.ns_drop_releases.borrow_mut().push((spec.id, epoch));
    }
    // The fan carries the epoch (ADR-0100 D4): peers park their teardown
    // on it — no second fan, no derived epoch.
    let epoch_text = epoch.to_string();
    let fan: [&[u8]; 4] = [b"INF.NSFAN", b"DROP", name, epoch_text.as_bytes()];
    while !control.persisted(epoch) {
        shared.ddl_waiters.wait(0).await;
    }
    // Crash-matrix point: the on-disk state of a cut after the swap
    // (META lacks the namespace and carries its tombstone; every
    // MANIFEST still names it).
    if inf_foundation::fault::fire(crate::fault::NS_DROP_AFTER_META) {
        return error_reply(shared, proto, "ERR fault: ns_drop_after_meta");
    }
    if let Some(error) = fan_all_or_first_error(shared, proto, &fan).await {
        return error;
    }
    if let Some(id) = drop {
        // Every cell applied the drop: a checkpoint requested now
        // publishes MANIFESTs without it (ADR-0100 D3).
        let ckpt_epoch = control.request_ckpt_all();
        control.stamp_drop(id, ckpt_epoch);
    }
    simple_reply(shared, proto, "OK")
}

/// Fans one DDL vector to every peer and returns the first error reply,
/// or `None` when every leg answered a non-error. Every leg is awaited —
/// a leg that could not be sent or answered no bytes is an error leg.
async fn fan_all_or_first_error<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    fan: &[&[u8]],
) -> Option<Vec<u8>> {
    let mut failure: Option<Vec<u8>> = None;
    for cell in peer_cells(shared) {
        let leg = match send_apply(shared, cell, ApplyOrigin::Program, proto, 0, fan).await {
            Ok(waiter) => match waiter.await {
                OwnedOutcome::Bytes(bytes) => bytes,
                _ => error_reply(shared, proto, "ERR cross-cell DDL leg answered no reply bytes"),
            },
            Err(refusal) => refusal,
        };
        if leg.first() == Some(&b'-') && failure.is_none() {
            failure = Some(leg);
        }
    }
    failure
}

/// ADR-0108 D3: undoes a `CREATE` whose fan failed — the mirror of
/// `program_ns_drop` for a namespace nothing has been promised about.
/// The origin drops its copy, the catalog persists the drop (the
/// pending entry retires with it; a durable namespace gets its
/// tombstone), every peer gets `INF.NSFAN DROP name epoch` (a peer that
/// never applied its leg answers "not found" — accepted), and a durable
/// drop is stamped for retirement. Runs under the DDL ticket.
async fn rollback_create<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    control: &Arc<ControlHandle>,
    proto: Protocol,
    id: u32,
    name: &[u8],
    durable: bool,
    tiered: bool,
) {
    // The origin's own copy (`Unknown` here would mean the fan's local
    // apply never happened — nothing to undo).
    let _ = shared.store.borrow_mut().ns_drop(name);
    let epoch = control.request_persist_drop(
        shared.store.borrow().export_catalog(
            control.next_ns_id(),
            control.next_index_id(),
            control.next_index_generation(),
        ),
        id,
        durable,
    );
    if tiered {
        shared.ns_drop_releases.borrow_mut().push((NsId(id), epoch));
    }
    while !control.persisted(epoch) {
        shared.ddl_waiters.wait(0).await;
    }
    let epoch_text = epoch.to_string();
    let fan: [&[u8]; 4] = [b"INF.NSFAN", b"DROP", name, epoch_text.as_bytes()];
    // Peers that refused or never received their CREATE leg answer
    // "namespace not found" — the rollback is idempotent per cell.
    let _ = fan_all_or_first_error(shared, proto, &fan).await;
    if durable {
        let ckpt_epoch = control.request_ckpt_all();
        control.stamp_drop(id, ckpt_epoch);
    }
}

/// Ship `argv` to `to` as an `ApplyNs` (named-namespace op — ADR-0015 D1)
/// and return the reply waiter. Mirrors [`send_apply`]; never RTT-recorded
/// (`always` replies are owner-deferred and would mispair the queue).
pub(super) async fn send_apply_ns<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    to: CellId,
    origin: ApplyOrigin,
    proto: Protocol,
    ns: NsId,
    argv: &[&[u8]],
) -> Result<GateWait<u64, OwnedOutcome>, Vec<u8>> {
    let program = origin == ApplyOrigin::Program;
    let Some(args) = ApplyArgs::new(argv) else {
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, proto).error("ERR too many arguments for cross-cell execution");
        return Err(reply);
    };
    let slot = SlotRouter::slot_of(argv.get(1).copied().unwrap_or(b""));
    let (token, waiter) = {
        let mut fabric = shared.fabric.borrow_mut();
        let token = fabric.next_token();
        (token, shared.gate.waiter(token.0))
    };
    let proto_byte: u8 = match proto {
        Protocol::Resp3 => 3,
        Protocol::Resp2 => 2,
    };
    let op = Op::ApplyNs { token, slot, cmd: proto_byte, ns: ns.0, args, program };
    loop {
        let sent = shared.fabric.borrow_mut().send(to, &op);
        match sent {
            Ok(()) => break,
            Err(SendError::NoCredit { .. }) => shared.credit_waiters.wait(to).await,
        }
    }
    Ok(waiter)
}

/// Outcome of a synchronous [`try_send_apply`] first attempt (de-async
/// fast path, ADR-0030 D4).
pub(super) enum SendNow {
    /// Staged on the first attempt; the reply waiter is registered.
    Sent(GateWait<u64, OwnedOutcome>),
    /// The argv exceeds the codec's argument cap; carries the refusal.
    Refused(Vec<u8>),
    /// No fabric credit on the first attempt — the caller falls back to
    /// the async path, which waits for credits. The drawn token is
    /// abandoned: a skipped monotonic value, never registered and never
    /// sent, so no reply or RTT pairing can ever reference it.
    NoCredit,
}

/// Synchronous first-attempt [`send_apply`]: token draw + stage in one
/// fabric borrow, waiter registered only after a successful stage (safe
/// for the same reason as `send_apply`'s post-staging registration — the
/// peer cannot observe the op until FABRIC-OUT publishes it, after this
/// synchronous stretch).
pub(super) fn try_send_apply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    to: CellId,
    origin: ApplyOrigin,
    proto: Protocol,
    db: u16,
    argv: &[&[u8]],
) -> SendNow {
    let program = origin == ApplyOrigin::Program;
    let Some(args) = ApplyArgs::new(argv) else {
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, proto).error("ERR too many arguments for cross-cell execution");
        return SendNow::Refused(reply);
    };
    let slot = SlotRouter::slot_of(argv.get(1).copied().unwrap_or(b""));
    let proto_byte: u8 = match proto {
        Protocol::Resp3 => 3,
        Protocol::Resp2 => 2,
    };
    debug_assert!(db < 16, "db rides 4 bits of the Apply cmd byte");
    let cmd_byte = proto_byte | ((db as u8) << 4);
    let (token, sent) = {
        let mut fabric = shared.fabric.borrow_mut();
        let token = fabric.next_token();
        let sent = fabric.send(to, &Op::Apply { token, slot, cmd: cmd_byte, args, program });
        (token, sent)
    };
    if sent.is_err() {
        return SendNow::NoCredit;
    }
    let waiter = shared.gate.waiter(token.0);
    if argv.first().copied() != Some(&b"INF.PUB"[..]) {
        shared.rtt_sent.borrow_mut()[usize::from(to.0)].push_back((token.0, shared.now.get()));
    }
    SendNow::Sent(waiter)
}

/// Ship `argv` to `to` as an `Apply` and return the reply waiter, waiting
/// for fabric credits when exhausted (backpressure, never unbounded
/// queueing). The send time is queued for delivery-side RTT recording.
/// `Err` carries the refusal reply when the argv exceeds the codec's
/// argument cap.
pub(super) async fn send_apply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    to: CellId,
    origin: ApplyOrigin,
    proto: Protocol,
    db: u16,
    argv: &[&[u8]],
) -> Result<GateWait<u64, OwnedOutcome>, Vec<u8>> {
    let program = origin == ApplyOrigin::Program;
    let Some(args) = ApplyArgs::new(argv) else {
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, proto).error("ERR too many arguments for cross-cell execution");
        return Err(reply);
    };
    // Routing is the `to` cell; the slot field is advisory (keyless scatter
    // applies carry the empty-key slot).
    let slot = SlotRouter::slot_of(argv.get(1).copied().unwrap_or(b""));
    // `cmd` packs `{db:4 | proto:4}` (ADR-0009) — SELECT travels with the
    // op on the byte the codec already had; db is < 16 by SELECT bounds.
    let proto_byte: u8 = match proto {
        Protocol::Resp3 => 3,
        Protocol::Resp2 => 2,
    };
    debug_assert!(db < 16, "db rides 4 bits of the Apply cmd byte");
    let cmd_byte = proto_byte | ((db as u8) << 4);
    // Token draw + first send attempt share one fabric borrow (M2.5
    // Phase H). Registering the waiter *after* staging stays safe: `send`
    // only stages into the outbound pack — the peer cannot observe the op
    // until FABRIC-OUT publishes it, after this synchronous stretch — so
    // no reply can precede the registration (and the gate parks any value
    // arriving before the waiter's first poll regardless).
    let (token, op, mut sent) = {
        let mut fabric = shared.fabric.borrow_mut();
        let token = fabric.next_token();
        let op = Op::Apply { token, slot, cmd: cmd_byte, args, program };
        let sent = fabric.send(to, &op);
        (token, op, sent)
    };
    let waiter = shared.gate.waiter(token.0);
    while let Err(SendError::NoCredit { .. }) = sent {
        shared.credit_waiters.wait(to).await;
        sent = shared.fabric.borrow_mut().send(to, &op);
    }
    // RTT pairing relies on in-order replies; `INF.PUB` replies are deferred
    // by the owner pump (fan acks first), so its hops are not RTT samples —
    // the fan legs (`INF.PUBFAN`) cover pub/sub in the histogram instead.
    if argv.first().copied() != Some(&b"INF.PUB"[..]) {
        shared.rtt_sent.borrow_mut()[usize::from(to.0)].push_back((token.0, shared.now.get()));
    }
    Ok(waiter)
}
