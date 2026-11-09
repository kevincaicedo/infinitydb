//! Namespace DDL and checkpoint programs: `INF.NS CREATE/DROP/SET`, the
//! catalog fan to every cell (`CREATE` and `DROP` persist before they
//! fan, ADR-0103 and ADR-0186 D1; `SET` fans first, ADR-0015 D3),
//! rollback, and the apply-send path with its credit wait. Each DDL reads
//! the catalog writer's answer through its own token and owes the writer
//! a notice once its fan is done (ADR-0189 D2, D5).

use inf_store::ddl::{
    Answer, CreatePersisted, DdlRefusal, DropPersisted, Receipt, Requested, TieredHeld,
    TombstoneHeld,
};
use inf_wire::{Domain, Refusal};

#[cfg(feature = "doc")]
use super::json_gather::{GatherTarget, json_gather};
use super::scatter::send_whole;
use super::*;
use crate::control::{CkptCredit, CkptTarget, DdlNotice, DdlTicket};

/// Why a checkpoint producer could not reserve its units.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum CkptRefusal {
    /// No control plane is wired: a node without a data dir.
    NoPlane,
    /// This cell's quota cannot cover the reservation (ADR-0159 D1): every
    /// checkpoint producer answers `IdentityExhausted(Checkpoint)`
    /// (ADR-0159 A1.2, ADR-0163 D5.3) before any effect.
    Exhausted,
}

impl CkptRefusal {
    fn reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        self,
        shared: &Rc<Shared<O, F>>,
        proto: Protocol,
    ) -> OwnedReply {
        match self {
            CkptRefusal::NoPlane => error_reply(shared, proto, CKPT_NO_PLANE),
            CkptRefusal::Exhausted => {
                error_reply(shared, proto, Refusal::IdentityExhausted(Domain::Checkpoint).render())
            }
        }
    }
}

const CKPT_NO_PLANE: &str = "ERR checkpointing requires a durable node (no data dir)";

/// Reserves `K` checkpoint units from this cell's quota — every unit the
/// program will issue, before its first effect (ADR-0159 D1, A1.2).
fn reserve_ckpt<const K: usize, O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
) -> Result<[CkptCredit; K], CkptRefusal> {
    let mut quota = shared.ckpt_quota.borrow_mut();
    let Some(quota) = quota.as_mut() else { return Err(CkptRefusal::NoPlane) };
    quota.reserve::<K>().map_err(|inf_foundation::issue::IssueExhausted| CkptRefusal::Exhausted)
}

/// A namespace DDL's post-fan duty (ADR-0100 D3, ADR-0159 D2). One value
/// decides both whether the drop carries a tombstone and the credit that
/// stamps it, so a durable drop without its stamp cannot be represented.
enum NsCleanup {
    /// Not durable: no tombstone, no checkpoint request.
    NoTombstone,
    /// Durable: the drop carries a tombstone, stamped after the fan with
    /// this credit, reserved before the DDL's first effect.
    Tombstone(CkptCredit),
}

impl NsCleanup {
    fn has_tombstone(&self) -> bool {
        match self {
            NsCleanup::NoTombstone => false,
            NsCleanup::Tombstone(_) => true,
        }
    }

    /// Every cell applied the drop: the checkpoint requested now
    /// publishes MANIFESTs without the namespace and retires its
    /// tombstone; its epoch rides the drop's notice. `None` with no
    /// tombstone.
    fn stamp_epoch(self) -> Option<u64> {
        match self {
            NsCleanup::NoTombstone => None,
            NsCleanup::Tombstone(credit) => Some(credit.issue(CkptTarget::All).get()),
        }
    }
}

/// Reserves a namespace DDL's checkpoint units before its first effect
/// (ADR-0159 D2): a durable namespace's two, the tombstone-pacing request
/// and the cleanup; none for any other mode. A refusal changes nothing.
/// A namespace's mode is fixed at its CREATE, and the DDL ticket holds its
/// name, so the mode read here is the one the program applies.
fn reserve_ns_units<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    mode: NsMode,
) -> Result<(Option<CkptCredit>, NsCleanup), CkptRefusal> {
    match mode {
        NsMode::Durable => {
            let [pacing, cleanup] = reserve_ckpt::<2, O, F>(shared)?;
            Ok((Some(pacing), NsCleanup::Tombstone(cleanup)))
        }
        NsMode::Memory | NsMode::Topic => Ok((None, NsCleanup::NoTombstone)),
    }
}

/// A CREATE's admission, before any effect: a durable namespace needs this
/// node's durable storage and reserves its two units, the pacing unit the
/// tombstone cap may spend (ADR-0189 D6) and the rollback's cleanup
/// (ADR-0159 D2).
fn admit_ns_create<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    mode: NsMode,
) -> Result<(Option<Admission>, NsCleanup), OwnedReply> {
    if mode == NsMode::Durable && shared.durable.borrow().is_none() {
        let refusal = "ERR this node has no durable storage (start infinityd with a data dir)";
        return Err(error_reply(shared, proto, refusal));
    }
    match reserve_ns_units(shared, mode) {
        Ok((pacing, cleanup)) => Ok((pacing.map(Admission::First), cleanup)),
        Err(refusal) => Err(refusal.reply(shared, proto)),
    }
}

/// A durable DDL's tombstone admission (ADR-0189 D6, N14): `First` holds
/// the pacing unit reserved before any effect, and the pacing transition
/// exists only from it, on the writer's `Paceable` verdict; once spent it
/// is `Paced`, and a second `Paceable` is answered as `Busy`.
enum Admission {
    First(CkptCredit),
    Paced,
}

impl Admission {
    /// The pace on a `Paceable` refusal: in `First`, issue the pacing unit
    /// as one all-cell checkpoint and wait until every cell published it,
    /// as `INF.CKPT WAIT` waits, so every paceable tombstone is ready when
    /// the program asks again; `false` once already paced (counted, never
    /// reached: one pace suffices, ADR-0189 D6).
    async fn pace<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        admission: &mut Option<Admission>,
        shared: &Rc<Shared<O, F>>,
        control: &ControlHandle,
    ) -> bool {
        let Some(Admission::First(unit)) = admission.take() else {
            control.note_ddl_repaced();
            *admission = Some(Admission::Paced);
            return false;
        };
        *admission = Some(Admission::Paced);
        control.note_ddl_paced();
        let epoch = unit.issue(CkptTarget::All);
        while shared.ckpt_sweep.borrow().observed().min_published < epoch.get() {
            shared.ckpt_waiters.wait(0).await;
        }
        true
    }

    /// The planted canary's pace before the first request, on the gauge
    /// word's counts (the shape the writer's verdict replaces): a pace the
    /// writer's own fold may not need.
    async fn pace_on_gauges<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        admission: &mut Option<Admission>,
        shared: &Rc<Shared<O, F>>,
        control: &ControlHandle,
    ) {
        let gauges = control.tombstone_gauges();
        let after_ready = usize::from(gauges.live - gauges.ready.min(gauges.live));
        if cfg!(inf_canary_ns_cap_from_gauges)
            && admission.is_some()
            && after_ready >= crate::control::DROPPED_NS_MAX
        {
            Admission::pace(admission, shared, control).await;
        }
    }
}

/// One command on this cell's namespace `ns`, executed into a pool reply
/// that carries its outcome (ADR-0190 D2); the flag is `QUIT`'s close.
fn execute_ns_local<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    origin: ExecOrigin,
    argv: &[&[u8]],
    proto: Protocol,
    id: u64,
    db: u16,
    ns: NsId,
) -> (OwnedReply, bool) {
    let mut reply = OpenReply::new(shared.take_reply_buf());
    let close =
        shared.execute_owned_into(origin, argv, proto, id, db, Some(ns), false, reply.reply());
    (reply.finish(), close)
}

/// One named-namespace command on the pump (M2-S08). Returns `false` when
/// the connection is gone.
#[allow(clippy::too_many_arguments)] // the pump dispatch context
#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column ns_program")]
#[allow(clippy::disallowed_types, reason = "container: A")]
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
            // A `JSON.MGET` over two owners is the gather, as in a numbered
            // database (ADR-0173 D4).
            #[cfg(feature = "doc")]
            if meta.id == CommandId::JsonMget {
                let target = GatherTarget { origin, proto, id, db, scope: ScatterScope::Ns(ns) };
                pending.push_back(json_gather(shared, target, meta, argv, inflight).await);
                return true;
            }
            // Every other multi-key command that spans cells is refused in
            // a named namespace (ADR-0015 deviations).
            pending.push_back(PendingReply::Done(error_reply(
                shared,
                proto,
                "ERR multi-key commands spanning cells are not supported in named namespaces",
            )));
            return true;
        }
        if owner.0 != shared.cell.0 {
            let scope = ScatterScope::Ns(ns);
            pending.push_back(send_whole(shared, owner, proto, db, scope, argv, inflight).await);
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
    let (reply, close) = execute_ns_local(shared, origin, argv, proto, id, db, ns);
    if close {
        close_after_reply(shared, key);
    }
    let gated = if is_write
        && stages(reply.outcome(), meta.id)
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
/// D6): a request issues one epoch from this cell's quota into the
/// board (cells run checkpoints in their own MAINTAIN slices — L1);
/// `WAIT` parks the pump until the target's **published** epoch covers
/// the request. Publication happens at the MANIFEST swap's dir-fsync
/// commit, so `WAIT` returns only after durability — a swap abort does
/// not publish; the retried swap does (fault-injection verified).
/// `LASTSAVE` = unix seconds of the newest publication this cell has
/// observed (ADR-0159 A1.4): the cell's `LastSave`, which INFO's
/// `rdb_last_save_time` reads too. It is 0 before the first (deviation
/// documented; Redis reports process-start time).
pub(super) async fn program_ckpt<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    id: CommandId,
    argv: &[&[u8]],
) -> OwnedReply {
    let control = shared.control.borrow().clone();
    let Some(control) = control else {
        return error_reply(shared, proto, CKPT_NO_PLANE);
    };
    if shared.durable.borrow().is_none() {
        return error_reply(shared, proto, CKPT_NO_PLANE);
    }
    if id == CommandId::Lastsave {
        return int_reply(shared, proto, shared.node.lastsave.unix_s() as i64);
    }
    let args = match parse_ckpt_args(shared.cells, id, argv) {
        Ok(args) => args,
        Err(message) => return error_reply(shared, proto, message),
    };
    // ADR-0159 D1: one unit, after parsing and the `CELL` check, before
    // any request — a refusal changes nothing.
    let credit = match reserve_ckpt::<1, O, F>(shared) {
        Ok([credit]) => credit,
        Err(refusal) => return refusal.reply(shared, proto),
    };
    let target = args.cell.map_or(CkptTarget::All, |k| CkptTarget::Cell(CellId(k)));
    let epoch = credit.issue(target);
    if args.wait {
        wait_for_ckpt(shared, &control, args.cell, epoch).await;
    }
    if id == CommandId::Bgsave {
        simple_reply(shared, proto, "Background saving started")
    } else {
        simple_reply(shared, proto, "OK")
    }
}

/// The parsed `INF.CKPT [CELL k] [WAIT]` / `BGSAVE [SCHEDULE]` arguments.
struct CkptArgs {
    /// `CELL k`, validated below the cell count.
    cell: Option<u16>,
    wait: bool,
}

fn parse_ckpt_args(cells: u16, id: CommandId, argv: &[&[u8]]) -> Result<CkptArgs, &'static str> {
    let mut args = CkptArgs { cell: None, wait: false };
    if id == CommandId::InfCkpt {
        let mut i = 1;
        while i < argv.len() {
            if argv[i].eq_ignore_ascii_case(b"WAIT") {
                args.wait = true;
                i += 1;
            } else if argv[i].eq_ignore_ascii_case(b"CELL") && i + 1 < argv.len() {
                let parsed = core::str::from_utf8(argv[i + 1]).ok().and_then(|s| s.parse().ok());
                let Some(k) = parsed.filter(|&k: &u16| k < cells) else {
                    return Err("ERR CELL wants an index below the cell count");
                };
                args.cell = Some(k);
                i += 2;
            } else {
                return Err("ERR syntax: INF.CKPT [CELL k] [WAIT]");
            }
        }
    } else if argv.len() > 2 || (argv.len() == 2 && !argv[1].eq_ignore_ascii_case(b"SCHEDULE")) {
        // BGSAVE [SCHEDULE]: SCHEDULE is accepted and moot — checkpoints
        // never fork, so there is nothing to defer (deviation documented).
        return Err("ERR syntax: BGSAVE [SCHEDULE]");
    }
    Ok(args)
}

/// Parks until the target published `epoch`. `CELL k` reads slot `k`
/// through the sweep's confirmation, which raises this cell's `LastSave`
/// in the same step (ADR-0159 A1.4); an all-cell wait reads this
/// cell's completed sweep, a lower bound that is never early (D4). The
/// sweep's completion wakes the waitlist.
async fn wait_for_ckpt<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    control: &Arc<ControlHandle>,
    cell: Option<u16>,
    epoch: crate::control::CheckpointEpoch,
) {
    loop {
        let satisfied = match cell {
            Some(k) => {
                let slot = control.ckpt_board().slot(k);
                let lastsave = &shared.node.lastsave;
                shared.ckpt_sweep.borrow_mut().confirm_cell_wait(slot, epoch, lastsave)
            }
            None => shared.ckpt_sweep.borrow().observed().min_published >= epoch.get(),
        };
        if satisfied {
            return;
        }
        shared.ckpt_waiters.wait(0).await;
    }
}

/// The namespace-DDL program, one per node at a time (ADR-0108
/// D1): `CREATE` in `program_ns_create`, `SET` in `program_ns_set`, `DROP`
/// in `program_ns_drop`. Each asks the catalog writer for its `META` and
/// reads the writer's answer through its own token (ADR-0189 D2).
pub(super) async fn program_ns_ddl<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    proto: Protocol,
    id: u64,
    db: u16,
    argv: &[&[u8]],
) -> OwnedReply {
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
    // Parked programs wake on the release edge MAINTAIN detects. A
    // program's notice is bound after it, so it is posted first.
    let ticket = loop {
        if let Some(ticket) = control.ddl_try_acquire(shared.cell.0) {
            break ticket;
        }
        shared.ddl_waiters.wait(0).await;
    };
    if argv[1].eq_ignore_ascii_case(b"CREATE") {
        return program_ns_create(shared, &control, &ticket, proto, argv).await;
    }
    if argv[1].eq_ignore_ascii_case(b"SET") {
        return program_ns_set(shared, &control, proto, argv).await;
    }
    if argv.len() != 3 {
        return error_reply(shared, proto, "ERR wrong number of arguments for 'INF.NS|DROP'");
    }
    program_ns_drop(shared, &control, proto, argv[2]).await
}

/// The reply of a namespace error.
fn ns_error_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    proto: Protocol,
    e: inf_store::NsError,
) -> OwnedReply {
    let mut reply = OpenReply::new(shared.take_reply_buf());
    crate::admin::ns_error(e, &mut RespWriter::new(reply.reply(), proto));
    reply.finish()
}

/// The `INF.NSFAN CREATE` leg of `spec`: its fields as the peer parses
/// them (`-` = none).
fn create_fan(spec: &inf_store::NsSpec) -> Vec<Vec<u8>> {
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
        spec.id.0.to_string().into_bytes(),
        tier_to_fan(spec.tier.as_ref()),
    ]
}

/// The `INF.NSFAN DROP name epoch id mode` leg of an accepted drop
/// (ADR-0189 D5): the peer builds its own receipt from the epoch, checks
/// the id and the mode against the namespace the name holds there, and
/// applies.
fn drop_fan(name: &[u8], receipt: &DropPersisted, mode: NsMode) -> [Vec<u8>; 6] {
    [
        b"INF.NSFAN".to_vec(),
        b"DROP".to_vec(),
        name.to_vec(),
        receipt.fan_epoch().to_string().into_bytes(),
        receipt.id().0.to_string().into_bytes(),
        mode.name().as_bytes().to_vec(),
    ]
}

/// Fans an accepted drop's leg to every peer, one at a time, every leg
/// awaited (ADR-0189 D5, ADR-0108 D2), and returns once every leg applied
/// (`Removed` and `Absent` alike). A leg read as refused is D4's fail-stop:
/// `META` already drops the namespace, and a peer left serving it would
/// export it, so the node stops rather than answer.
async fn fan_drop<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    name: &[u8],
    receipt: &DropPersisted,
    mode: NsMode,
) {
    let fan = drop_fan(name, receipt, mode);
    let fan_argv: Vec<&[u8]> = fan.iter().map(Vec::as_slice).collect();
    for cell in peer_cells(shared) {
        let mut outcome = fan_leg(shared, proto, cell, &fan_argv).await;
        // Crash-matrix point: the origin reads an applied leg as refused
        // (test builds only).
        if inf_foundation::fault::fire(crate::fault::NS_LEG_READ_REFUSED) {
            outcome = LegOutcome::Refused(error_reply(shared, proto, "ERR fault: leg read"));
        }
        if let LegOutcome::Refused(reply) = outcome {
            let detail = format!(
                "cell {}'s leg of namespace {} at epoch {} answered {}",
                cell.0,
                receipt.id().0,
                receipt.fan_epoch(),
                String::from_utf8_lossy(reply.bytes()).trim_end()
            );
            super::ddl_fail_stop("DROP leg read", &detail);
        }
    }
}

/// The notice a DDL owes the catalog writer once its fan is done
/// (ADR-0189 D5): the writer keeps the DDL's effect in every payload until
/// it arrives. Built only from an accepted answer's receipt or by
/// [`NoticeDue::roll_back`]; [`NoticeDue::post`] sends it, and on every
/// other exit dropping the value sends it with no stamp, a blocking send
/// like every catalog request. Bound after the DDL ticket, it drops first,
/// so the notice reaches the writer before the next DDL's request.
#[must_use = "a dropped notice posts at once, with no stamp"]
struct NoticeDue {
    control: Arc<ControlHandle>,
    id: NsId,
    /// `None` once posted or rolled back.
    kind: Option<NoticeKind>,
}

/// Which notice a [`NoticeDue`] posts.
#[derive(Copy, Clone, Debug)]
enum NoticeKind {
    CreateApplied,
    DropFanned,
}

impl NoticeDue {
    /// The notice an accepted `CREATE` owes.
    fn created(control: &Arc<ControlHandle>, receipt: &CreatePersisted) -> NoticeDue {
        let (control, id) = (Arc::clone(control), receipt.id());
        NoticeDue { control, id, kind: Some(NoticeKind::CreateApplied) }
    }

    /// The notice an accepted `DROP` owes.
    fn dropped(control: &Arc<ControlHandle>, receipt: &DropPersisted) -> NoticeDue {
        let (control, id) = (Arc::clone(control), receipt.id());
        NoticeDue { control, id, kind: Some(NoticeKind::DropFanned) }
    }

    /// The rollback's send consumes the `CREATE`'s notice, posting nothing
    /// (the rollback's `DROP` replaces its pending `CREATE`), and returns
    /// the notice the rollback owes.
    fn roll_back(mut self) -> NoticeDue {
        // Canaries `inf_canary_ns_notice_at_replacement` and
        // `inf_canary_ns_notice_posted_at_replacement`: the `CREATE`'s
        // notice posts as the rollback replaces it, instead of being
        // consumed by the rollback's send.
        if cfg!(inf_canary_ns_notice_at_replacement)
            || cfg!(inf_canary_ns_notice_posted_at_replacement)
        {
            self.send(None);
        }
        self.kind = None;
        let (control, id) = (Arc::clone(&self.control), self.id);
        NoticeDue { control, id, kind: Some(NoticeKind::DropFanned) }
    }

    /// Posts the notice; `stamp` is the epoch of the checkpoint that
    /// retires a durable drop's tombstone, `None` for any other.
    fn post(mut self, stamp: Option<u64>) {
        self.send(stamp);
    }

    fn send(&mut self, stamp: Option<u64>) {
        let id = self.id;
        let notice = match self.kind.take() {
            None => return,
            Some(NoticeKind::CreateApplied) => DdlNotice::CreateApplied { id },
            Some(NoticeKind::DropFanned) => DdlNotice::DropFanned { id, stamp },
        };
        self.control.post_notice(notice);
    }
}

impl Drop for NoticeDue {
    fn drop(&mut self) {
        self.send(None);
    }
}

/// The reply a DDL renders for the writer's refusal: `NameExists` as the
/// namespace error every `CREATE` path answers; `PendingConflict` and
/// `IdNotNext`, which the ticket makes unreachable, as internal errors; the
/// tombstone cap's (ADR-0189 D6; ADR-0172 D1) as `CapacityBusy(Catalog)`
/// while a held slot returns without an operator, a `Paceable` the program
/// could not pace included, and `LimitReached(Catalog)` once every held
/// slot waits on one; the tiered lifetime bound's (ADR-0148 D1; ADR-0172 D3)
/// as `CapacityBusy(TieredNamespace)` while a grant is Retiring and
/// `LimitReached(TieredNamespace)` once every grant names a live namespace;
/// each capacity refusal counted.
fn ddl_refusal_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    control: &ControlHandle,
    proto: Protocol,
    refusal: DdlRefusal,
) -> OwnedReply {
    match refusal {
        DdlRefusal::NameExists => ns_error_reply(shared, proto, inf_store::NsError::Exists),
        DdlRefusal::PendingConflict => error_reply(shared, proto, DDL_PENDING_CONFLICT),
        DdlRefusal::IdNotNext => error_reply(shared, proto, DDL_ID_NOT_NEXT),
        DdlRefusal::TombstoneCapacity(TombstoneHeld::Paceable | TombstoneHeld::Busy) => {
            control.note_capacity_refused(true);
            error_reply(shared, proto, Refusal::CapacityBusy(Domain::Catalog).render())
        }
        DdlRefusal::TombstoneCapacity(TombstoneHeld::Limit) => {
            control.note_capacity_refused(false);
            error_reply(shared, proto, Refusal::LimitReached(Domain::Catalog).render())
        }
        DdlRefusal::TieredCapacity(TieredHeld::Busy) => {
            control.note_tiered_refused(true);
            error_reply(shared, proto, Refusal::CapacityBusy(Domain::TieredNamespace).render())
        }
        DdlRefusal::TieredCapacity(TieredHeld::Limit) => {
            control.note_tiered_refused(false);
            error_reply(shared, proto, Refusal::LimitReached(Domain::TieredNamespace).render())
        }
    }
}

/// The internal error of a DDL the writer found another pending for.
const DDL_PENDING_CONFLICT: &str = "ERR internal: another namespace DDL is pending";

/// The internal error of a `CREATE` whose reserved id the writer found was
/// not the counter's next: its id is never committed.
const DDL_ID_NOT_NEXT: &str = "ERR internal: the namespace id is not the catalog's next";

/// Parks until the writer answers `requested` through its own slot
/// (ADR-0189 D2): the word's edge wakes the DDL waitlist.
async fn await_answer<R: Receipt, O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    control: &ControlHandle,
    mut requested: Requested<R>,
) -> Result<R, DdlRefusal> {
    loop {
        match requested.answer(control.catalog_word()) {
            Answer::Pending(again) => requested = again,
            Answer::Refused(refusal) => return Err(refusal),
            Answer::Accepted(receipt) => return Ok(receipt),
        }
        shared.ddl_waiters.wait(0).await;
    }
}

/// `INF.NS CREATE` (ADR-0103, ADR-0189 D5): validate without applying,
/// ask the writer for the `META` that names the namespace and wait for
/// its answer, then apply locally and fan, so no cell serves a namespace
/// the catalog does not hold. A failed leg rolls it back. Every exit
/// after the answer posts the `CREATE`'s notice. The id is reserved under
/// the ticket and spent only by the writer's acceptance (ADR-0148 D4).
async fn program_ns_create<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    control: &Arc<ControlHandle>,
    ticket: &DdlTicket,
    proto: Protocol,
    argv: &[&[u8]],
) -> OwnedReply {
    let draft = match crate::admin::parse_ns_create(argv) {
        Ok(draft) => draft,
        Err(msg) => return error_reply(shared, proto, &msg),
    };
    let (mut admission, cleanup) = match admit_ns_create(shared, proto, draft.mode) {
        Ok(admitted) => admitted,
        Err(refusal) => return refusal,
    };
    let reservation = match ticket.reserve_ns_id() {
        Ok(reservation) => reservation,
        Err(refusal) => return error_reply(shared, proto, refusal.render()),
    };
    let spec = draft.with_id(reservation.id().0);
    if let Err(e) = shared.store.borrow().ns_create_check(&spec) {
        return ns_error_reply(shared, proto, e);
    }
    Admission::pace_on_gauges(&mut admission, shared, control).await;
    let mut reservation = Some(reservation);
    let receipt = loop {
        // A refusal spent no id: the ticket reserves the same one again.
        let reserved = match reservation.take().map_or_else(|| ticket.reserve_ns_id(), Ok) {
            Ok(reserved) => reserved,
            Err(refusal) => return error_reply(shared, proto, refusal.render()),
        };
        let export = shared.store.borrow().export_catalog(
            control.next_ns_id(),
            control.next_index_id(),
            control.next_index_generation(),
        );
        let requested = control.request_create(export, spec.clone(), reserved);
        match await_answer(shared, control, requested).await {
            Ok(receipt) => break receipt,
            Err(DdlRefusal::TombstoneCapacity(TombstoneHeld::Paceable))
                if Admission::pace(&mut admission, shared, control).await => {}
            Err(refusal) => return ddl_refusal_reply(shared, control, proto, refusal),
        }
    };
    let notice = NoticeDue::created(control, &receipt);
    // Crash-matrix point (ADR-0103 D5): the on-disk state of a cut
    // after the swap — META names a namespace no cell serves.
    if inf_foundation::fault::fire(crate::fault::NS_CREATE_AFTER_META) {
        return error_reply(shared, proto, "ERR fault: ns_create_after_meta");
    }
    // Crash-matrix point (ADR-0148 D5): the origin's own store refuses
    // the apply after the swap, in place of `ns_create` (test builds only).
    let refused = if inf_foundation::fault::fire(crate::fault::NS_CREATE_ORIGIN_REFUSED) {
        Some(error_reply(shared, proto, "ERR fault: ns_create_origin_refused"))
    } else {
        let created = super::shared::apply_create(shared, spec.clone());
        created.err().map(|refused| {
            let mut reply = OpenReply::new(shared.take_reply_buf());
            refused.write(&mut RespWriter::new(reply.reply(), proto));
            reply.finish()
        })
    };
    let rollback = Rollback { id: spec.id, name: &spec.name, mode: spec.mode, cleanup };
    if let Some(refusal) = refused {
        // The origin's own apply was refused after the answer (the OS
        // refusing the ring reservation, or an admission bound lowered on
        // this cell during the wait): `META` names the namespace, so the
        // `CREATE` rolls back before it answers (ADR-0148 D5).
        rollback_create(shared, control, proto, rollback, notice).await;
        return refusal;
    }
    let fan = create_fan(&spec);
    let fan_argv: Vec<&[u8]> = fan.iter().map(Vec::as_slice).collect();
    if let Some(error) = fan_all_or_first_error(shared, proto, &fan_argv).await {
        rollback_create(shared, control, proto, rollback, notice).await;
        return error;
    }
    notice.post(None);
    simple_reply(shared, proto, "OK")
}

/// A `DROP`'s admission, before any effect (ADR-0159 D2): a durable
/// namespace reserves its two units under the ticket, its pacing unit for
/// the tombstone cap, which the writer alone decides (ADR-0189 D6), and its
/// cleanup.
fn admit_ns_drop<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    mode: NsMode,
) -> Result<(Option<Admission>, NsCleanup), OwnedReply> {
    let (pacing, cleanup) =
        reserve_ns_units(shared, mode).map_err(|refusal| refusal.reply(shared, proto))?;
    Ok((pacing.map(Admission::First), cleanup))
}

/// `INF.NS DROP` (ADR-0189 D5): resolve the name to its spec and reserve
/// the units without applying anything → ask the writer for the `META`
/// that drops the namespace (a durable one's tombstone in its place) and
/// wait for its answer, every cell, the origin included, serving the
/// namespace meanwhile → apply locally with the receipt → fan `INF.NSFAN
/// DROP name epoch id mode`, each peer applying with its own receipt →
/// post the `DropFanned` notice, stamped with the cleanup checkpoint's
/// epoch when the drop is durable and every leg applied → `+OK`. So no
/// cell drops a namespace a durable `META` still lists, and no checkpoint
/// published before the swap omits it.
async fn program_ns_drop<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    control: &Arc<ControlHandle>,
    proto: Protocol,
    name: &[u8],
) -> OwnedReply {
    // The ticket holds the name: the spec read here is the one dropped.
    let spec = match shared.store.borrow().ns_drop_check(name) {
        Ok(spec) => spec.clone(),
        Err(e) => return ns_error_reply(shared, proto, e),
    };
    let (mut admission, cleanup) = match admit_ns_drop(shared, proto, spec.mode) {
        Ok(admitted) => admitted,
        Err(refusal) => return refusal,
    };
    // Canary `inf_canary_ns_apply_before_swap`: the origin drops the
    // namespace before its request (the order the receipt replaces), so a
    // checkpoint it publishes in the wait omits it.
    #[cfg(inf_canary_ns_apply_before_swap)]
    let _ = shared.store.borrow_mut().ns_drop_planted(name);
    // Crash-matrix point: the on-disk state of a cut before the answer
    // (nothing durable changed and every cell serves the namespace, so a
    // checkpoint any cell publishes before the node stops still names it).
    if inf_foundation::fault::fire(crate::fault::NS_DROP_BEFORE_META) {
        return error_reply(shared, proto, "ERR fault: ns_drop_before_meta");
    }
    Admission::pace_on_gauges(&mut admission, shared, control).await;
    let receipt = loop {
        let export = shared.store.borrow().export_catalog(
            control.next_ns_id(),
            control.next_index_id(),
            control.next_index_generation(),
        );
        let requested = control.request_drop(export, spec.id, cleanup.has_tombstone());
        match await_answer(shared, control, requested).await {
            Ok(receipt) => break receipt,
            Err(DdlRefusal::TombstoneCapacity(TombstoneHeld::Paceable))
                if Admission::pace(&mut admission, shared, control).await => {}
            // Nothing was applied anywhere: every cell still serves it.
            Err(refusal) => return ddl_refusal_reply(shared, control, proto, refusal),
        }
    };
    let notice = NoticeDue::dropped(control, &receipt);
    // Crash-matrix point: the on-disk state of a cut after the answer and
    // before the origin's apply (`META` lacks the namespace and carries a
    // durable one's tombstone; every cell still serves it).
    if inf_foundation::fault::fire(crate::fault::NS_DROP_AFTER_META) {
        if cfg!(inf_canary_ns_notice_forgotten) {
            std::mem::forget(notice);
        }
        return error_reply(shared, proto, "ERR fault: ns_drop_after_meta");
    }
    if let Err(e) = apply_drop(shared, &receipt) {
        drop_apply_fail_stop(shared, &receipt, &e);
    }
    // Crash-matrix point: the on-disk state of a cut after the origin's
    // apply and before its first leg (`META` lacks the namespace, the
    // origin applied, every peer still serves it); the node runs on and
    // the tombstone stays unstamped until the next boot re-stamps it.
    if inf_foundation::fault::fire(crate::fault::NS_DROP_AFTER_APPLY) {
        return error_reply(shared, proto, "ERR fault: ns_drop_after_apply");
    }
    fan_drop(shared, proto, name, &receipt, spec.mode).await;
    notice.post(cleanup.stamp_epoch());
    simple_reply(shared, proto, "OK")
}

/// D4's fail-stop for the origin's own apply of an accepted drop: `META`
/// already drops the namespace, and this cell would keep serving it. The
/// node's keyspace is cataloged at both boot sites, so no error reaches
/// here but a defect.
fn drop_apply_fail_stop<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    receipt: &DropPersisted,
    error: &inf_store::NsError,
) -> ! {
    let detail = format!(
        "cell {} cannot apply the drop of namespace {} at epoch {}: {error:?}",
        shared.cell.0,
        receipt.id().0,
        receipt.fan_epoch()
    );
    super::ddl_fail_stop("DROP apply", &detail)
}

/// What a `CREATE`'s rollback undoes.
struct Rollback<'a> {
    id: NsId,
    name: &'a [u8],
    mode: NsMode,
    /// A durable namespace's tombstone stamp, reserved by its `CREATE`
    /// (ADR-0159 D2).
    cleanup: NsCleanup,
}

/// ADR-0108 D3 in ADR-0189 D5's order: undoes a `CREATE` whose origin
/// apply or fan leg failed after its answer, for a namespace nothing has
/// been promised about. The origin asks the writer for the `META` that
/// drops it (the writer replaces the pending `CREATE` with the drop; a
/// durable namespace gets its tombstone), its send consuming the
/// `CREATE`'s notice, and waits for the answer, every cell that applied
/// the `CREATE` serving it meanwhile; then the origin applies the drop
/// with the receipt and every peer gets `INF.NSFAN DROP name epoch id
/// mode` (a cell that never applied its `CREATE` finds the namespace
/// absent, an applied drop), and the notice posts with the stamp of the
/// cleanup credit the `CREATE` reserved when every leg applied (ADR-0159
/// D2). Runs under the DDL ticket.
async fn rollback_create<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    control: &Arc<ControlHandle>,
    proto: Protocol,
    rollback: Rollback<'_>,
    notice: NoticeDue,
) {
    let Rollback { id, name, mode, cleanup } = rollback;
    let export = shared.store.borrow().export_catalog(
        control.next_ns_id(),
        control.next_index_id(),
        control.next_index_generation(),
    );
    let requested = control.request_drop(export, id, cleanup.has_tombstone());
    let notice = notice.roll_back();
    let Ok(receipt) = await_answer(shared, control, requested).await else {
        unreachable!("the writer accepts a rollback's drop of its own pending CREATE");
    };
    // `Absent` here is an origin whose own apply was refused.
    if let Err(e) = apply_drop(shared, &receipt) {
        drop_apply_fail_stop(shared, &receipt, &e);
    }
    fan_drop(shared, proto, name, &receipt, mode).await;
    notice.post(cleanup.stamp_epoch());
}

/// `INF.NS SET` (ADR-0062 D3, ADR-0068 D3): hot-reload is
/// DDL — apply locally → fan `INF.NSFAN SET` to every peer → ask the
/// writer for the `META` that carries the new configuration and wait for
/// its answer → `+OK` (ADR-0015 D3's order). The request names no change:
/// every cell's export already carries the new configuration.
async fn program_ns_set<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    control: &Arc<ControlHandle>,
    proto: Protocol,
    argv: &[&[u8]],
) -> OwnedReply {
    let (name, update, id) = {
        let store = shared.store.borrow();
        let (name, update) = match crate::admin::parse_ns_set(argv, &store) {
            Ok(parsed) => parsed,
            Err(msg) => return error_reply(shared, proto, &msg),
        };
        // The parse resolved the name; the ticket keeps it until the reply.
        let Some(id) = store.ns_get(&name).map(|spec| spec.id) else {
            return ns_error_reply(shared, proto, inf_store::NsError::Unknown);
        };
        (name, update, id)
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
        return ns_error_reply(shared, proto, e);
    }
    let mut fan = vec![b"INF.NSFAN".to_vec(), b"SET".to_vec(), name];
    fan.extend(fan_tail);
    let fan_argv: Vec<&[u8]> = fan.iter().map(Vec::as_slice).collect();
    if let Some(error) = fan_all_or_first_error(shared, proto, &fan_argv).await {
        return error;
    }
    let export = shared.store.borrow().export_catalog(
        control.next_ns_id(),
        control.next_index_id(),
        control.next_index_generation(),
    );
    let requested = control.request_set(export, id);
    match await_answer(shared, control, requested).await {
        Ok(_receipt) => simple_reply(shared, proto, "OK"),
        Err(refusal) => ddl_refusal_reply(shared, control, proto, refusal),
    }
}

/// How one DDL leg ended (ADR-0189 D5), read from its reply's variant and
/// never from its bytes: an applied leg answers `Ok`; any other variant,
/// an error reply or a success where none belongs, is `Refused`.
enum LegOutcome {
    Applied,
    Refused(OwnedReply),
}

/// Fans one DDL vector to every peer and returns the first refused leg's
/// reply, or `None` when every leg applied. Every leg is awaited, one at a
/// time (ADR-0108 D2); a leg that could not be sent is a refused leg.
async fn fan_all_or_first_error<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    fan: &[&[u8]],
) -> Option<OwnedReply> {
    let mut failure: Option<OwnedReply> = None;
    for cell in peer_cells(shared) {
        match fan_leg(shared, proto, cell, fan).await {
            LegOutcome::Applied => {}
            LegOutcome::Refused(reply) if failure.is_none() => failure = Some(reply),
            LegOutcome::Refused(reply) => shared.recycle_reply_buf(reply.into_bytes()),
        }
    }
    failure
}

/// One DDL leg to `cell` and how it ended, read from its reply's variant
/// (ADR-0189 D5): `Ok` is `Applied`; an error reply is `Refused` with its
/// bytes; any other variant, a success reply where an applied leg answers
/// `Ok` included, is `Refused` and internal, rendered with the one text
/// every arm renders for a shape its fold cannot take (ADR-0190 D2).
async fn fan_leg<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    cell: CellId,
    fan: &[&[u8]],
) -> LegOutcome {
    let waiter = match send_apply(shared, cell, ApplyOrigin::Program, proto, 0, fan).await {
        Ok(waiter) => waiter,
        Err(refusal) => return LegOutcome::Refused(refusal),
    };
    match waiter.await {
        OwnedOutcome::Ok => LegOutcome::Applied,
        OwnedOutcome::Reply(reply) if reply.is_error() => LegOutcome::Refused(reply),
        OwnedOutcome::Reply(reply) => {
            shared.recycle_reply_buf(reply.into_bytes());
            LegOutcome::Refused(error_reply(shared, proto, CROSS_CELL_EXECUTION_FAILED))
        }
        OwnedOutcome::Int(_) | OwnedOutcome::Nil | OwnedOutcome::Bool(_) | OwnedOutcome::Err(_) => {
            LegOutcome::Refused(error_reply(shared, proto, CROSS_CELL_EXECUTION_FAILED))
        }
    }
}

/// The refusal of an argv wider than the codec's `MAX_APPLY_ARGS`.
fn too_many_args_reply(proto: Protocol) -> OwnedReply {
    let mut reply = OpenReply::new(Vec::new());
    RespWriter::new(reply.reply(), proto).error("ERR too many arguments for cross-cell execution");
    reply.finish()
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
) -> Result<GateWait<u64, OwnedOutcome>, OwnedReply> {
    let program = origin == ApplyOrigin::Program;
    let Some(args) = ApplyArgs::new(argv) else {
        return Err(too_many_args_reply(proto));
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
    Refused(OwnedReply),
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
        return SendNow::Refused(too_many_args_reply(proto));
    };
    let slot = SlotRouter::slot_of(argv.get(1).copied().unwrap_or(b""));
    let proto_byte: u8 = match proto {
        Protocol::Resp3 => 3,
        Protocol::Resp2 => 2,
    };
    let cmd_byte = proto_byte | (db_nibble(db) << 4);
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
) -> Result<GateWait<u64, OwnedOutcome>, OwnedReply> {
    let program = origin == ApplyOrigin::Program;
    let Some(args) = ApplyArgs::new(argv) else {
        return Err(too_many_args_reply(proto));
    };
    // Routing is the `to` cell; the slot field is advisory (keyless scatter
    // applies carry the empty-key slot).
    let slot = SlotRouter::slot_of(argv.get(1).copied().unwrap_or(b""));
    // `cmd` packs `{db:4 | proto:4}` (ADR-0009) — SELECT travels with the
    // op on the byte the codec already had; db fits the nibble by `DATABASES`.
    let proto_byte: u8 = match proto {
        Protocol::Resp3 => 3,
        Protocol::Resp2 => 2,
    };
    let cmd_byte = proto_byte | (db_nibble(db) << 4);
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

// `db` rides 4 bits of the fabric `Apply` cmd byte; the one owner of the
// database count proves it fits at compile time (review 2026-08-30 L17
// E6b) — a wider `DATABASES` fails the build, never mis-routes.
const _: () = assert!(crate::config::DATABASES <= 16, "db rides 4 bits of the Apply cmd byte");

/// The db index as the cmd byte's high nibble (`db < DATABASES ≤ 16` by
/// `SELECT`/`COPY` bounds and the assert above).
fn db_nibble(db: u16) -> u8 {
    debug_assert!(db < crate::config::DATABASES);
    (db & 0xF) as u8
}
