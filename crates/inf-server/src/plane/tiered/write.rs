//! The tiered write side: the write funnel (admission gate, displacement
//! staging, shadow-slot writes, blob writes, the tail-stall park) and the
//! string-family write commands that enter through it.

use super::*;

// ---- the write funnel ----

/// One synchronous write attempt: admission → apply → take origins →
/// stage markers-then-record (ADR-0057 D4 order). Returns the staged
/// mutation seq. Apply precedes staging so a typed apply failure stages
/// nothing; staging is infallible after `would_fit` (no await between).
#[allow(clippy::too_many_arguments)] // one internal write funnel
fn try_write<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    class: Option<FsyncClass>,
    key: &[u8],
    hash: u64,
    value: &[u8],
    old: Displaced,
    guard: WriteGuard,
    proto: Protocol,
) -> Result<u64, WriteBlock> {
    let mut ks = shared.store.borrow_mut();
    let mut durable = shared.durable.borrow_mut();
    let Some(cell) = durable.as_mut() else {
        return Err(WriteBlock::Reply(error_bytes(shared, proto, ERR_FAILED)));
    };
    if cell.failed {
        return Err(WriteBlock::Reply(error_bytes(shared, proto, ERR_FAILED)));
    }
    let Some(table) = ks.tiered_store_mut(ns) else {
        return Err(WriteBlock::Reply(error_bytes(
            shared,
            proto,
            "ERR the selected namespace was dropped (INF.NS USE again)",
        )));
    };
    // F-L06-03 (review of 2026-08-30): the resolve that produced `old`
    // suspended after its verification — re-verify under this borrow
    // (no await separates this lookup from the apply below) and make
    // the caller re-resolve on a mismatch. The RAM/cold arms pay
    // nothing: they returned under the borrow that verified them.
    if guard == WriteGuard::Reverify {
        let current = table.lookup(key, hash, &[]);
        let still = match old {
            None => !matches!(current, TieredLookup::Ram(_)),
            Some((addr, _, _)) => matches!(
                current,
                TieredLookup::Ram(now) | TieredLookup::Cold(now) if now == addr
            ),
        };
        if !still {
            table.note_write_replan();
            return Err(WriteBlock::Replan);
        }
    }
    // Blob routing (ADR-0061 D1): the threshold is a plane decision; the
    // store refuses misrouted values typed.
    let blob = value.len() >= table.blob_config().threshold_bytes as usize;
    if blob && value.len() as u64 > table.blob_config().max_bytes {
        return Err(WriteBlock::Reply(error_bytes(shared, proto, ERR_TOO_LARGE)));
    }
    // Admission worst-fit: the staged record for a blob write is the
    // 24-byte reference, never the value bytes.
    let staged_len = if blob {
        MutationEffect::StringSetExtent { ns, key, extent_id: u64::MAX, offset: 0, len: u64::MAX }
            .encoded_len()
    } else {
        MutationEffect::StringSet { ns, key, value }.encoded_len()
    };
    let marker = MutationEffect::ColdDisplace { ns, old_addr: (1u64 << 48) - 1 }.encoded_len();
    if !cell.would_fit(4 * marker + staged_len) {
        return Err(WriteBlock::StagingFull);
    }
    // ADR-0063 D2: admission consults the cached verdict before any
    // new-byte placement reaches `stage_wal` — and before
    // `ExtentWriter::create`, so a full device is never probed with a
    // doomed file creation per blob attempt.
    if let Some(cause) = table.disk_full() {
        return Err(WriteBlock::Reply(diskfull_bytes(shared, proto, cause)));
    }
    let class = class.expect("tiered namespaces always carry a durability class");
    if blob {
        return write_blob(shared, ns, class, key, hash, value, old, proto, &mut ks, cell);
    }
    let table = ks.tiered_store_mut(ns).expect("resolved above");
    let applied = match old {
        None => table.insert(key, value, hash),
        Some((addr, len, version)) => table.update(key, value, hash, addr, len, version),
    };
    let new_addr = match applied {
        Ok(addr) => addr,
        Err(err) => {
            return Err(write_block_of(shared, table, key, StallProbe::Inline(value), err, proto));
        }
    };
    // M4.5-S31 rider (ADR-0084 D5): an in-place rewrite (same address)
    // displaces no slot — replay's key-verified upsert re-covers it, so
    // the current-address marker is dropped. Moved overwrites keep it.
    let moved = old.is_none_or(|(addr, _, _)| addr != new_addr);
    stage_displacements(cell, table, ns, hash, old, class, moved);
    Ok(cell.stage_tiered(table, &MutationEffect::StringSet { ns, key, value }, class))
}

/// Stages the ADR-0059 D9 origin markers + the ADR-0057 D4 current-
/// address marker, in that order, ahead of the mutation record.
///
/// `moved = false` (an in-place rewrite — M4.5-S31 rider, ADR-0084 D5)
/// drops the current-address marker: the record's address is unchanged
/// and replay's rule-2 upsert resolves it by key (imaged or WAL-born in
/// RAM; the one unlogged path — compaction relocation — is exactly what
/// the origin markers repair, so those stage unconditionally).
fn stage_displacements<F: SegmentFs>(
    cell: &mut DurableCell<F>,
    table: &mut TieredTable,
    ns: NsId,
    hash: u64,
    old: Displaced,
    class: FsyncClass,
    moved: bool,
) {
    if let Some((addr, _, _)) = old {
        for (origin_addr, _stamp) in table.take_displacement_origins(hash, addr) {
            let marker = MutationEffect::ColdDisplace { ns, old_addr: origin_addr };
            let _ = cell.stage_tiered(table, &marker, class);
        }
        if moved {
            let marker = MutationEffect::ColdDisplace { ns, old_addr: addr.to_raw() };
            let _ = cell.stage_tiered(table, &marker, class);
        }
    }
}

/// One shadow-write attempt's outcome (M4.5-S37).
enum ShadowAttempt {
    /// The record is appended, the ticket registered, the SET staged.
    Staged(u64),
    /// Admitted in principle, blocked at the plane's gates — park/reply
    /// exactly as the synchronous path would.
    Blocked(WriteBlock),
    /// Not the shadow shape (RAM hit, no exact candidate needing one,
    /// a store-side refusal, the knob off): the synchronous resolve.
    Ineligible,
}

/// The shadow write (ADR-0093 D2): inside one borrow, the exact-hash
/// probe, every admission check in order, then insert + register +
/// stage — or a counted refusal and the synchronous path. Never a
/// cold read, never a `ColdDisplace`.
fn try_shadow_write<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    class: Option<FsyncClass>,
    key: &[u8],
    hash: u64,
    value: &[u8],
    proto: Protocol,
) -> ShadowAttempt {
    let mut ks = shared.store.borrow_mut();
    let Some(table) = ks.tiered_store_mut(ns) else { return ShadowAttempt::Ineligible };
    if !table.shadow_enabled() {
        return ShadowAttempt::Ineligible;
    }
    // Inline values only (D1): the extent path keeps its markers.
    if value.len() >= table.blob_config().threshold_bytes as usize {
        return ShadowAttempt::Ineligible;
    }
    let cold = match table.shadow_probe(key, hash) {
        inf_store::ShadowProbe::One(cold) => Some(cold),
        // No exact-hash cold slot: `lookup`'s candidate was another key
        // (64-bit evidence — the sidecar, not the fingerprint); the
        // insert is correct without a read and without a ticket.
        inf_store::ShadowProbe::NoCandidate => None,
        inf_store::ShadowProbe::Many => {
            table.note_shadow_multi();
            return ShadowAttempt::Ineligible;
        }
        // The one exact candidate already carries a ticket (ADR-0093 A2:
        // a second key colliding with a ticketed slot): the synchronous
        // path's read tells the keys apart; one cold address, one ticket.
        inf_store::ShadowProbe::Ticketed(_) => {
            table.note_shadow_ticketed();
            return ShadowAttempt::Ineligible;
        }
        // An absent key or a RAM hit: the ordinary paths, byte-for-byte.
        inf_store::ShadowProbe::Miss | inf_store::ShadowProbe::RamHit(_) => {
            return ShadowAttempt::Ineligible;
        }
    };
    // The pinned-suffix arithmetic wants the RAM record's size (header
    // + key + value — the TTL-less string layout); `would_fit` below
    // wants the WAL record's.
    let record_len = TieredTable::RECORD_HEADER_LEN + key.len() + value.len();
    if let Some(cold) = cold
        && table.shadow_admit(hash, cold, record_len).is_err()
    {
        return ShadowAttempt::Ineligible;
    }
    let encoded_len = MutationEffect::StringSet { ns, key, value }.encoded_len();
    let mut durable = shared.durable.borrow_mut();
    let Some(cell) = durable.as_mut() else {
        return ShadowAttempt::Blocked(WriteBlock::Reply(error_bytes(shared, proto, ERR_FAILED)));
    };
    if cell.failed {
        return ShadowAttempt::Blocked(WriteBlock::Reply(error_bytes(shared, proto, ERR_FAILED)));
    }
    // Staging (D2 step 7): the record alone — no markers are staged.
    if !cell.would_fit(encoded_len) {
        shared.node.shadow_fallback_staging.set(shared.node.shadow_fallback_staging.get() + 1);
        return ShadowAttempt::Blocked(WriteBlock::StagingFull);
    }
    if let Some(cause) = table.disk_full() {
        return ShadowAttempt::Blocked(WriteBlock::Reply(diskfull_bytes(shared, proto, cause)));
    }
    let class = class.expect("tiered namespaces always carry a durability class");
    let new_addr = match table.insert(key, value, hash) {
        Ok(addr) => addr,
        Err(err) => {
            return ShadowAttempt::Blocked(write_block_of(
                shared,
                table,
                key,
                StallProbe::Inline(value),
                err,
                proto,
            ));
        }
    };
    match cold {
        Some(cold) => table.register_shadow(hash, cold, new_addr),
        None => table.note_shadow_exact_miss_insert(),
    }
    let seq = cell.stage_tiered(table, &MutationEffect::StringSet { ns, key, value }, class);
    ShadowAttempt::Staged(seq)
}

/// Reads one whole cold record at `addr` — header window first, the
/// exact remainder after (the S08 two-round contract) — through
/// `ColdReads` in `class`, with no index involvement: the caller
/// re-validates whatever the bytes are for (ADR-0093 D4). `Err` names
/// the typed failure (I/O, CRC, the address outside every catalogued
/// file, the queue saturated).
pub(in crate::plane) async fn read_cold_record<
    O: PlaneObserver + 'static,
    F: SegmentFs + Clone + 'static,
>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    addr: LogicalAddr,
    class: inf_runtime::ReadClass,
) -> Result<Vec<u8>, &'static str> {
    // The deterministic stand-in for a device error on a twin read
    // (M2-S16 fault point; the DST arms it around a `DBSIZE` drain).
    if inf_foundation::fault::fire(crate::fault::SHADOW_TWIN_READ_FAIL) {
        return Err("injected twin read failure (fault point shadow_twin_read_fail)");
    }
    let mut image: Vec<u8> = Vec::new();
    // 0 = unknown until the header decodes.
    let mut total: usize = 0;
    loop {
        let want = if total == 0 { TieredTable::RECORD_HEADER_LEN } else { total - image.len() };
        let at = addr.to_raw() + image.len() as u64;
        let (wait, frames, skip) = {
            let tier = shared.tier.borrow();
            let Some(t) = tier.as_ref().and_then(|t| t.ns(ns)) else {
                return Err("ERR the selected namespace was dropped (INF.NS USE again)");
            };
            let Some(cold) = tier.as_ref().and_then(|t| t.cold.clone()) else {
                return Err(ERR_COLD_IO);
            };
            let Some(at) = LogicalAddr::from_raw(at) else { return Err(ERR_COLD_IO) };
            let Some((fd, file, offset, frames, skip)) = t.plan_cold_read(at, want) else {
                return Err(ERR_COLD_IO); // outside every catalogued file
            };
            let bytes = frames as usize * inf_log::TIER_FRAME_BYTES;
            let now_us = shared.now.get().as_micros();
            match cold.enqueue(fd, file, offset, bytes, class, now_us) {
                Ok(wait) => (wait, frames, skip),
                Err(_) => return Err(ERR_COLD_BUSY),
            }
        };
        let done = wait.await;
        if done.outcome().is_err() {
            return Err(ERR_COLD_IO);
        }
        let extracted = done.bytes(|window| {
            let window_data = frames as usize * inf_log::TIER_FRAME_DATA - skip;
            if total == 0 {
                let mut head = Vec::new();
                inf_log::tier_extract(window, skip, TieredTable::RECORD_HEADER_LEN, &mut head)
                    .ok()?;
                let len = TieredTable::record_len_from_header(&head);
                let take = len.min(window_data);
                let mut piece = Vec::with_capacity(len);
                inf_log::tier_extract(window, skip, take, &mut piece).ok()?;
                Some((len, piece))
            } else {
                let take = want.min(window_data);
                let mut piece = Vec::new();
                inf_log::tier_extract(window, skip, take, &mut piece).ok()?;
                Some((total, piece))
            }
        });
        drop(done); // custody home before any further await
        let Some((len, piece)) = extracted else { return Err(ERR_COLD_IO) };
        total = len;
        image.extend_from_slice(&piece);
        if image.len() >= total {
            image.truncate(total);
            return Ok(image);
        }
    }
}

/// The blob write path (M4-S17 wired by M4-S26): extent create + chunk
/// writes (blocking, bounded by the value — the ADR-0061 cost the plan
/// accepts), `finish_deferred`, the coverage-neutral ledger barrier
/// registered **before** the referencing record stages (D3 — the
/// done-prefix rule fences the ack behind extent durability; the
/// fdatasync op itself rides the next MAINTAIN), then markers + the
/// `StringSetExtent` record. A typed apply failure abandons the extent
/// file to the orphan sweep — the D3 quarantine rule.
/// Why an out-of-line write failed, typed end to end (F-L04-09): the
/// plane used to erase `ExtentWriteFailure` into `io::Error::other` and
/// then string-match for a word the message never carried.
enum BlobWriteError {
    Open(std::io::Error),
    Chunk(std::io::Error),
    Seal(inf_log::blob::ExtentWriteFailure),
}

impl BlobWriteError {
    fn is_storage_full(&self) -> bool {
        match self {
            BlobWriteError::Open(e) | BlobWriteError::Chunk(e) => inf_log::is_storage_exhausted(e),
            BlobWriteError::Seal(f) => f.is_storage_full(),
        }
    }
}

#[allow(clippy::too_many_arguments)] // the write funnel's blob half
fn write_blob<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    class: FsyncClass,
    key: &[u8],
    hash: u64,
    value: &[u8],
    old: Displaced,
    proto: Protocol,
    ks: &mut Keyspace,
    cell: &mut DurableCell<F>,
) -> Result<u64, WriteBlock> {
    let mut tier_slot = shared.tier.borrow_mut();
    let Some(tier) = tier_slot.as_mut() else {
        return Err(WriteBlock::Reply(error_bytes(shared, proto, ERR_FAILED)));
    };
    let table = ks.tiered_store_mut(ns).expect("caller resolved the table");
    let extent_id = table.allocate_extent_id();
    let (cell_index, dir, mode) = {
        let t = tier.ns(ns).expect("tiered namespace has plane state");
        (tier.cell_index(), t.dir.clone(), t.io_mode)
    };
    let sealed = inf_log::blob::ExtentWriter::create(
        tier.fs(),
        &dir,
        inf_log::blob::ExtentId(extent_id),
        cell_index,
        ns,
        value.len() as u64,
        mode,
    )
    .map_err(BlobWriteError::Open)
    .and_then(|mut writer| {
        writer.append_chunk(value).map_err(BlobWriteError::Chunk)?;
        writer.finish_deferred().map_err(BlobWriteError::Seal)
    });
    // ADR-0088 D1/D5 (recorded limitation 2): the extent write is
    // synchronous foreground device I/O outside the driver — metered as
    // `BlobWrite` so the budget's foreground term is complete, never
    // deferred (the M5 blob story owns the `IoOp` path).
    cell.charge_foreground(inf_runtime::IoClass::BlobWrite, value.len() as u64, 1);
    let (sealed, handle) = match sealed {
        Ok(pair) => pair,
        // The failed extent is abandoned (never referenced, id never
        // reused); the orphan sweep reclaims the file (ADR-0061 D3).
        // The type survives to the reply (F-L04-09): a space refusal is
        // `DISKFULL` on the blob path exactly as on the inline path.
        Err(err) => {
            let reply = if err.is_storage_full() {
                diskfull_bytes(shared, proto, inf_store::DiskFullCause::Device)
            } else {
                error_bytes(shared, proto, ERR_BLOB_READ)
            };
            return Err(WriteBlock::Reply(reply));
        }
    };
    let applied = match old {
        None => table.insert_extent(key, hash, &sealed),
        Some((addr, len, version)) => table.update_extent(key, hash, &sealed, addr, len, version),
    };
    if let Err(err) = applied {
        // Abandon: the extent is durable-referenced by nothing; the
        // sweep reclaims it. The handle just closes. The stall probe is
        // sized from the 24-byte reference the store refused, never
        // from the value (F-L06-05, ADR-0102 D3).
        drop(handle);
        return Err(write_block_of(shared, table, key, StallProbe::Extent, err, proto));
    }
    // D3 ordering: the barrier's ledger position precedes this
    // iteration's linked frame fsync (seal_log registers later in the
    // same iteration) — the ack is fenced mechanically.
    let fd = inf_log::fs::SegmentFile::raw_fd(&handle);
    let ticket = cell.commit.register_extent_barrier(handle, shared.now.get());
    if let Some(fd) = fd {
        tier.queue_extent_sync(fd, ticket);
    }
    // Extent updates are never in-place (ADR-0061 D4 — the branch is
    // structurally excluded), so the displacement marker always stages.
    stage_displacements(cell, table, ns, hash, old, class, true);
    let effect = MutationEffect::StringSetExtent {
        ns,
        key,
        extent_id: sealed.extent_id().0,
        offset: 0,
        len: sealed.data_len(),
    };
    Ok(cell.stage_tiered(table, &effect, class))
}

/// Maps a typed apply failure onto park-or-reply.
fn write_block_of<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    table: &mut TieredTable,
    key: &[u8],
    probe: StallProbe<'_>,
    err: inf_store::OpError,
    proto: Protocol,
) -> WriteBlock {
    match err {
        inf_store::OpError::OutOfMemory => {
            // Ring window exhausted: distinguish "flush will free this"
            // (park on the stall gate) from genuine exhaustion.
            let target = match probe {
                StallProbe::Inline(value) => table.write_stall_target(key, value),
                StallProbe::Extent => table.extent_stall_target(key),
            };
            if target.is_some() {
                WriteBlock::Stall
            } else {
                WriteBlock::Reply(error_bytes(shared, proto, ERR_OOM))
            }
        }
        inf_store::OpError::DiskFull(cause) => {
            WriteBlock::Reply(diskfull_bytes(shared, proto, cause))
        }
        inf_store::OpError::TooLarge => WriteBlock::Reply(error_bytes(
            shared,
            proto,
            "ERR value exceeds the tiered record bound",
        )),
        other => {
            debug_assert!(false, "unexpected tiered apply failure: {other:?}");
            WriteBlock::Reply(error_bytes(shared, proto, "ERR internal tiered apply failure"))
        }
    }
}

pub(super) fn error_bytes<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    message: &str,
) -> Vec<u8> {
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).error(message);
    reply
}

fn diskfull_bytes<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    cause: inf_store::DiskFullCause,
) -> Vec<u8> {
    let message = match cause {
        inf_store::DiskFullCause::Budget { used, budget } => {
            format!("DISKFULL tiered namespace disk budget exhausted (used={used} budget={budget})")
        }
        inf_store::DiskFullCause::Device => {
            "DISKFULL tier device out of space (ENOSPC)".to_string()
        }
    };
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).error(&message);
    reply
}

/// Drives one value write to completion: resolve → attempt → park loops
/// (staging drain / tail stall with the ADR-0053 D4 typed timeout).
/// `compute` turns the resolved old value into the new value bytes (or
/// a terminal reply — the INCR family's parse errors).
async fn write_value<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    class: Option<FsyncClass>,
    key: &[u8],
    proto: Protocol,
    compute: impl Fn(Option<&[u8]>, Option<u32>) -> Result<Vec<u8>, Vec<u8>>,
) -> Result<(u64, Option<Vec<u8>>), Vec<u8>> {
    let hash = shared.hasher.hash(key);
    let deadline = stall_deadline(shared, ns);
    let mut replans: u8 = 0;
    loop {
        let resolved = resolve(shared, ns, key, hash, PromoteOnCold::Never).await;
        let guard = WriteGuard::of(&resolved);
        let (old, old_value): (Displaced, Option<Vec<u8>>) = match resolved {
            Resolved::Miss => (None, None),
            Resolved::Ram(addr) => {
                let ks = shared.store.borrow();
                let table = ks.tiered_store(ns).expect("resolved on this table");
                let parts = table.record(addr);
                (Some((addr, parts.encoded_len, parts.version)), Some(parts.value.to_vec()))
            }
            Resolved::Cold { addr, value, version, encoded_len }
            | Resolved::Extent { addr, value, version, encoded_len } => {
                (Some((addr, encoded_len, version)), Some(value))
            }
            Resolved::Fail(message) => return Err(error_bytes(shared, proto, message)),
        };
        let value = compute(old_value.as_deref(), old.map(|(_, _, v)| v))?;
        match try_write(shared, ns, class, key, hash, &value, old, guard, proto) {
            Ok(seq) => return Ok((seq, old_value)),
            Err(WriteBlock::Replan) => {
                replans += 1;
                if replans > WRITE_REPLAN_MAX {
                    return Err(error_bytes(shared, proto, ERR_REPLAN_EXHAUSTED));
                }
            }
            Err(WriteBlock::StagingFull) => {
                let wait = {
                    let durable = shared.durable.borrow();
                    let Some(cell) = durable.as_ref() else {
                        return Err(error_bytes(shared, proto, ERR_FAILED));
                    };
                    cell.drained.wait(())
                };
                wait.await;
            }
            Err(WriteBlock::Stall) => {
                if shared.now.get() >= deadline {
                    return Err(error_bytes(shared, proto, ERR_STALLED));
                }
                let wait = {
                    let tier = shared.tier.borrow();
                    let Some(t) = tier.as_ref().and_then(|t| t.ns(ns)) else {
                        return Err(error_bytes(shared, proto, ERR_STALLED));
                    };
                    t.stall_waiters.wait(())
                };
                wait.await;
            }
            Err(WriteBlock::Reply(reply)) => return Err(reply),
        }
    }
}

fn stall_deadline<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
) -> Nanos {
    let ms = shared
        .tier
        .borrow()
        .as_ref()
        .and_then(|t| t.ns(ns))
        .map_or(1_000, |t| u64::from(t.tail_stall_timeout_ms));
    shared.now.get().saturating_add(Nanos::from_millis(ms))
}

// ---- write commands ----

pub(super) async fn set_cmd<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    let key = argv.arg(1);
    let value = argv.arg(2);
    let (mut nx, mut xx, mut get_old) = (false, false, false);
    // The numbered-db grammar (`exec::set`, Redis's family rule): `NX XX`
    // is a syntax error, a repeat is not; an unknown token is a syntax
    // error; only the expiry family (`EX`/`PX`/`EXAT`/`PXAT`/`KEEPTTL`)
    // answers the declared M4 refusal (L13 style row, review 2026-08-30).
    for i in 3..argv.len() {
        let opt = argv.arg(i);
        if opt.eq_ignore_ascii_case(b"NX") || opt.eq_ignore_ascii_case(b"XX") {
            let want_nx = opt.eq_ignore_ascii_case(b"NX");
            if (want_nx && xx) || (!want_nx && nx) {
                return done_error(shared, proto, "ERR syntax error");
            }
            nx |= want_nx;
            xx |= !want_nx;
        } else if opt.eq_ignore_ascii_case(b"GET") {
            get_old = true;
        } else if opt.eq_ignore_ascii_case(b"KEEPTTL") || crate::exec::expire_option(opt).is_some()
        {
            return done_error(shared, proto, ERR_NO_EXPIRY);
        } else {
            return done_error(shared, proto, "ERR syntax error");
        }
    }
    // Conditional SETs resolve first; the write helper re-resolves per
    // attempt, so the condition is re-evaluated with it. A SET with no
    // option is the shadow-eligible shape (ADR-0093 D1).
    let plain = !nx && !xx && !get_old;
    let outcome = write_conditional(shared, ns, class, key, proto, nx, xx, value, plain).await;
    let mut reply = shared.take_reply_buf();
    let mut w = RespWriter::new(&mut reply, proto);
    match outcome {
        Ok((seq, old_value, applied)) => {
            if get_old {
                match &old_value {
                    Some(v) => w.bulk(v),
                    None => w.null(),
                }
            } else if applied {
                w.simple("OK");
            } else {
                w.null();
            }
            if applied && class == Some(FsyncClass::Always) {
                return TieredReply::Gated { reply, seq };
            }
            TieredReply::Done(reply)
        }
        Err(err) => {
            reply.clear();
            reply.extend_from_slice(&err);
            shared.recycle_reply_buf(err);
            TieredReply::Done(reply)
        }
    }
}

/// SET with NX/XX semantics: `(seq, old value, applied?)`.
#[allow(clippy::too_many_arguments)] // one conditional-write funnel
async fn write_conditional<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    class: Option<FsyncClass>,
    key: &[u8],
    proto: Protocol,
    nx: bool,
    xx: bool,
    value: &[u8],
    plain: bool,
) -> Result<(u64, Option<Vec<u8>>, bool), Vec<u8>> {
    let hash = shared.hasher.hash(key);
    let deadline = stall_deadline(shared, ns);
    let mut replans: u8 = 0;
    loop {
        // M4.5-S37 step 1 (`bench-diagnostics` only): the ceiling arm —
        // a plain SET (no NX/XX: the reply does not depend on the old
        // state) whose only candidate is cold is written as an insert,
        // the verifying cold read skipped. UNSOUND: the cold record is
        // orphaned (two candidates for one key until the orphan's file
        // retires) and a fingerprint collision would leave two live
        // keys — ADR-0085 D5 is exactly why the product never does
        // this. The instrument measures the read's cost; counted.
        #[cfg(feature = "bench-diagnostics")]
        let blind = !nx && !xx && shared.blind_overwrite_ceiling.get() && {
            let ks = shared.store.borrow();
            ks.tiered_store(ns).is_some_and(|table| {
                matches!(table.lookup(key, hash, &[]), inf_store::TieredLookup::Cold(_))
            })
        };
        #[cfg(not(feature = "bench-diagnostics"))]
        let blind = false;
        // M4.5-S37 (ADR-0093 D1/D2): the plain, unconditional, inline
        // SET may take the shadow path — every refusal falls through to
        // the synchronous resolve below, exactly as before.
        if plain && !blind {
            match try_shadow_write(shared, ns, class, key, hash, value, proto) {
                ShadowAttempt::Staged(seq) => return Ok((seq, None, true)),
                ShadowAttempt::Blocked(WriteBlock::StagingFull) => {
                    let wait = {
                        let durable = shared.durable.borrow();
                        let Some(cell) = durable.as_ref() else {
                            return Err(error_bytes(shared, proto, ERR_FAILED));
                        };
                        cell.drained.wait(())
                    };
                    wait.await;
                    continue;
                }
                ShadowAttempt::Blocked(WriteBlock::Stall) => {
                    if shared.now.get() >= deadline {
                        return Err(error_bytes(shared, proto, ERR_STALLED));
                    }
                    let wait = {
                        let tier = shared.tier.borrow();
                        let Some(t) = tier.as_ref().and_then(|t| t.ns(ns)) else {
                            return Err(error_bytes(shared, proto, ERR_STALLED));
                        };
                        t.stall_waiters.wait(())
                    };
                    wait.await;
                    continue;
                }
                ShadowAttempt::Blocked(WriteBlock::Reply(reply)) => return Err(reply),
                ShadowAttempt::Blocked(WriteBlock::Replan) | ShadowAttempt::Ineligible => {}
            }
        }
        let (old, old_value, guard): (Displaced, Option<Vec<u8>>, WriteGuard) = if blind {
            #[cfg(feature = "bench-diagnostics")]
            shared
                .node
                .blind_overwrites_ceiling
                .set(shared.node.blind_overwrites_ceiling.get() + 1);
            (None, None, WriteGuard::Current)
        } else {
            let resolved = resolve(shared, ns, key, hash, PromoteOnCold::Never).await;
            let guard = WriteGuard::of(&resolved);
            match resolved {
                Resolved::Miss => (None, None, guard),
                Resolved::Ram(addr) => {
                    let ks = shared.store.borrow();
                    let table = ks.tiered_store(ns).expect("resolved on this table");
                    let parts = table.record(addr);
                    (
                        Some((addr, parts.encoded_len, parts.version)),
                        Some(parts.value.to_vec()),
                        guard,
                    )
                }
                Resolved::Cold { addr, value, version, encoded_len }
                | Resolved::Extent { addr, value, version, encoded_len } => {
                    (Some((addr, encoded_len, version)), Some(value), guard)
                }
                Resolved::Fail(message) => return Err(error_bytes(shared, proto, message)),
            }
        };
        if (nx && old.is_some()) || (xx && old.is_none()) {
            return Ok((0, old_value, false));
        }
        match try_write(shared, ns, class, key, hash, value, old, guard, proto) {
            Ok(seq) => return Ok((seq, old_value, true)),
            Err(WriteBlock::Replan) => {
                replans += 1;
                if replans > WRITE_REPLAN_MAX {
                    return Err(error_bytes(shared, proto, ERR_REPLAN_EXHAUSTED));
                }
            }
            Err(WriteBlock::StagingFull) => {
                let wait = {
                    let durable = shared.durable.borrow();
                    let Some(cell) = durable.as_ref() else {
                        return Err(error_bytes(shared, proto, ERR_FAILED));
                    };
                    cell.drained.wait(())
                };
                wait.await;
            }
            Err(WriteBlock::Stall) => {
                if shared.now.get() >= deadline {
                    return Err(error_bytes(shared, proto, ERR_STALLED));
                }
                let wait = {
                    let tier = shared.tier.borrow();
                    let Some(t) = tier.as_ref().and_then(|t| t.ns(ns)) else {
                        return Err(error_bytes(shared, proto, ERR_STALLED));
                    };
                    t.stall_waiters.wait(())
                };
                wait.await;
            }
            Err(WriteBlock::Reply(reply)) => return Err(reply),
        }
    }
}

pub(super) async fn setnx<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    match write_conditional(shared, ns, class, argv.arg(1), proto, true, false, argv.arg(2), false)
        .await
    {
        Ok((seq, _, applied)) => {
            let mut reply = shared.take_reply_buf();
            RespWriter::new(&mut reply, proto).int(i64::from(applied));
            if applied && class == Some(FsyncClass::Always) {
                TieredReply::Gated { reply, seq }
            } else {
                TieredReply::Done(reply)
            }
        }
        Err(err) => TieredReply::Done(err),
    }
}

pub(super) async fn getset<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    match write_conditional(shared, ns, class, argv.arg(1), proto, false, false, argv.arg(2), false)
        .await
    {
        Ok((seq, old_value, _)) => {
            let mut reply = shared.take_reply_buf();
            let mut w = RespWriter::new(&mut reply, proto);
            match &old_value {
                Some(v) => w.bulk(v),
                None => w.null(),
            }
            if class == Some(FsyncClass::Always) {
                TieredReply::Gated { reply, seq }
            } else {
                TieredReply::Done(reply)
            }
        }
        Err(err) => TieredReply::Done(err),
    }
}

pub(super) async fn append<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    let suffix = argv.arg(2).to_vec();
    let Some(max) = blob_max(shared, ns) else {
        return done_error(shared, proto, "ERR the selected namespace was dropped");
    };
    let outcome = write_value(shared, ns, class, argv.arg(1), proto, |old, _| {
        let old_len = old.map_or(0, <[u8]>::len);
        // Bound before building: a post-image past BLOB-MAX is refused
        // typed, never materialised (F-L13-04).
        if old_len.checked_add(suffix.len()).is_none_or(|end| end as u64 > max) {
            return Err(error_bytes(shared, proto, ERR_TOO_LARGE));
        }
        let mut value = Vec::with_capacity(old_len + suffix.len());
        value.extend_from_slice(old.unwrap_or_default());
        value.extend_from_slice(&suffix);
        Ok(value)
    })
    .await;
    int_write_reply(shared, proto, class, outcome, |old| {
        (old.map_or(0, |v| v.len()) + suffix.len()) as i64
    })
}

pub(super) async fn setrange<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    let Some(offset) = parse_i64(argv.arg(2)) else {
        return done_error(shared, proto, "ERR value is not an integer or out of range");
    };
    let Ok(offset) = usize::try_from(offset) else {
        return done_error(shared, proto, "ERR offset is out of range");
    };
    let patch = argv.arg(3).to_vec();
    // Redis `setrangeCommand`: an empty patch is a length read — no
    // bound check, no write, `0` for a missing key.
    if patch.is_empty() {
        return strlen(shared, ns, argv.arg(1), proto).await;
    }
    // F-L13-04 (review of 2026-08-30): the post-image is bounded before
    // a byte of it is built — `offset` is client-chosen, and a resize to
    // it was a `capacity overflow` panic (`i64::MAX`) or an allocator
    // abort of the whole node (`2^62`). BLOB-MAX is the namespace's
    // declared value cap, the same answer a `SET` past it gets.
    let Some(max) = blob_max(shared, ns) else {
        return done_error(shared, proto, "ERR the selected namespace was dropped");
    };
    let Some(end) = offset.checked_add(patch.len()).filter(|end| *end as u64 <= max) else {
        return done_error(shared, proto, ERR_TOO_LARGE);
    };
    let outcome = write_value(shared, ns, class, argv.arg(1), proto, |old, _| {
        let old = old.unwrap_or_default();
        let mut value = Vec::with_capacity(old.len().max(end));
        value.extend_from_slice(old);
        if value.len() < end {
            value.resize(end, 0);
        }
        value[offset..end].copy_from_slice(&patch);
        Ok(value)
    })
    .await;
    int_write_reply(shared, proto, class, outcome, |old| old.map_or(0, |v| v.len()).max(end) as i64)
}

pub(super) async fn incr<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    id: CommandId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    let delta = match id {
        CommandId::Incr => 1,
        CommandId::Decr => -1,
        _ => match parse_i64(argv.arg(2)) {
            Some(n) if id == CommandId::DecrBy => match n.checked_neg() {
                Some(n) => n,
                None => {
                    return done_error(shared, proto, "ERR decrement would overflow");
                }
            },
            Some(n) => n,
            None => {
                return done_error(shared, proto, "ERR value is not an integer or out of range");
            }
        },
    };
    let computed = std::cell::Cell::new(0i64);
    let outcome = {
        let computed = &computed;
        write_value(shared, ns, class, argv.arg(1), proto, move |old, _| {
            let current = match old {
                None => 0i64,
                Some(bytes) => match parse_i64(bytes) {
                    Some(n) => n,
                    None => {
                        return Err(error_bytes(
                            shared,
                            proto,
                            "ERR value is not an integer or out of range",
                        ));
                    }
                },
            };
            let Some(next) = current.checked_add(delta) else {
                return Err(error_bytes(
                    shared,
                    proto,
                    "ERR increment or decrement would overflow",
                ));
            };
            computed.set(next);
            Ok(next.to_string().into_bytes())
        })
        .await
    };
    match outcome {
        Ok((seq, _)) => {
            let mut reply = shared.take_reply_buf();
            RespWriter::new(&mut reply, proto).int(computed.get());
            if class == Some(FsyncClass::Always) {
                TieredReply::Gated { reply, seq }
            } else {
                TieredReply::Done(reply)
            }
        }
        Err(err) => TieredReply::Done(err),
    }
}

pub(super) async fn incrbyfloat<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    let Some(delta) = std::str::from_utf8(argv.arg(2)).ok().and_then(|s| s.parse::<f64>().ok())
    else {
        return done_error(shared, proto, "ERR value is not a valid float");
    };
    let rendered = std::cell::RefCell::new(Vec::new());
    let outcome = {
        let rendered = &rendered;
        write_value(shared, ns, class, argv.arg(1), proto, move |old, _| {
            let current = match old {
                None => 0f64,
                Some(bytes) => match std::str::from_utf8(bytes).ok().and_then(|s| s.parse().ok()) {
                    Some(f) => f,
                    None => {
                        return Err(error_bytes(shared, proto, "ERR value is not a valid float"));
                    }
                },
            };
            let next = current + delta;
            if !next.is_finite() {
                return Err(error_bytes(
                    shared,
                    proto,
                    "ERR increment would produce NaN or Infinity",
                ));
            }
            let text = format_float(next);
            *rendered.borrow_mut() = text.clone();
            Ok(text)
        })
        .await
    };
    match outcome {
        Ok((seq, _)) => {
            let mut reply = shared.take_reply_buf();
            RespWriter::new(&mut reply, proto).bulk(&rendered.borrow());
            if class == Some(FsyncClass::Always) {
                TieredReply::Gated { reply, seq }
            } else {
                TieredReply::Done(reply)
            }
        }
        Err(err) => TieredReply::Done(err),
    }
}

pub(super) async fn getdel<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    let key = argv.arg(1);
    match delete_one(shared, ns, class, key, proto, true).await {
        Ok(deleted) => {
            let mut reply = shared.take_reply_buf();
            let mut w = RespWriter::new(&mut reply, proto);
            match &deleted {
                Some((_, Some(v))) => w.bulk(v),
                _ => w.null(),
            }
            if let Some((seq, _)) = deleted
                && class == Some(FsyncClass::Always)
            {
                TieredReply::Gated { reply, seq }
            } else {
                TieredReply::Done(reply)
            }
        }
        Err(err) => TieredReply::Done(err),
    }
}

pub(super) async fn del<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    let mut removed = 0i64;
    let mut last_seq = 0;
    for key in &argv[1..] {
        match delete_one(shared, ns, class, key, proto, false).await {
            Ok(Some((seq, _))) => {
                removed += 1;
                last_seq = seq;
            }
            Ok(None) => {}
            Err(err) => return TieredReply::Done(err),
        }
    }
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).int(removed);
    if removed > 0 && class == Some(FsyncClass::Always) {
        TieredReply::Gated { reply, seq: last_seq }
    } else {
        TieredReply::Done(reply)
    }
}

/// One key's deletion: resolve (cold verifies — the recorded S26
/// policy) → walk the winner's tickets (ADR-0093 A10/A13) → stage
/// markers + `Delete` → apply. `Ok(None)` = the key was absent;
/// `Ok(Some((seq, value)))` = deleted (`want_value` carries the old
/// value out for GETDEL). The two suspensions are here: each unverified
/// twin's `Foreground` read, and the drained wait when the staging
/// window is full. No store borrow crosses either.
async fn delete_one<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    class: Option<FsyncClass>,
    key: &[u8],
    proto: Protocol,
    want_value: bool,
) -> Result<Option<(u64, Option<Vec<u8>>)>, Vec<u8>> {
    let hash = shared.hasher.hash(key);
    loop {
        let Some(target) = resolve_delete_target(shared, ns, key, hash, want_value, proto).await?
        else {
            return Ok(None);
        };
        // The ticket walk: a cold-address cursor over the registry and
        // fixed scratch for the same-key twins (B23-R10 — no snapshot,
        // no heap). Stale (`verify` never defers) re-resolves.
        let mut twins = TwinScratch::default();
        let mut cursor: Option<LogicalAddr> = None;
        let mut stale = false;
        while let Some(ticket) = next_winner_ticket(shared, ns, target.addr, cursor) {
            cursor = Some(ticket.cold);
            let image = if ticket.verified_len.is_some() {
                None
            } else {
                let read =
                    read_cold_record(shared, ns, ticket.cold, inf_runtime::ReadClass::Foreground)
                        .await;
                Some(read.map_err(|message| error_bytes(shared, proto, message))?)
            };
            match note_winner_ticket(shared, ns, &ticket, image.as_deref(), &mut twins) {
                TicketStep::Next => {}
                TicketStep::Stale => {
                    stale = true;
                    break;
                }
                TicketStep::OverRegister => {
                    return Err(error_bytes(shared, proto, ERR_DEL_REGISTER));
                }
            }
        }
        if stale {
            continue;
        }
        match stage_delete(shared, ns, class, key, hash, &target, &mut twins, proto)? {
            Staged::Applied(seq) => return Ok(Some((seq, target.value))),
            Staged::Moved => continue,
            Staged::NoRoom => {
                let wait = {
                    let durable = shared.durable.borrow();
                    let Some(cell) = durable.as_ref() else {
                        return Err(error_bytes(shared, proto, ERR_FAILED));
                    };
                    cell.drained.wait(())
                };
                wait.await;
            }
        }
    }
}

const ERR_DEL_REGISTER: &str =
    "ERR DEL: displacement markers exceed the replay register (ADR-0093 A11)";

/// The record a `DEL` resolved: its address, encoded length and — for
/// GETDEL — its value.
struct DeleteTarget {
    addr: LogicalAddr,
    len: usize,
    value: Option<Vec<u8>>,
}

/// `DEL`'s command scratch (ADR-0093 A13): the same-key twins one
/// winner can carry and still fit the replay register — each twin costs
/// at least one marker, the winner one more, so the register minus one.
/// A twin past the cap refuses typed before its read; the count is
/// `tiering_shadow_delete_run_refused`. 24 B per slot on the stack.
pub(super) const TWIN_SCRATCH_CAP: usize = inf_store::DISPLACE_REGISTER_CAP - 1;

impl TwinScratch {
    /// `false` when the scratch is full (the caller refuses).
    pub(super) fn push(&mut self, twin: LogicalAddr, len: usize) -> bool {
        if self.len == TWIN_SCRATCH_CAP {
            return false;
        }
        self.slots[self.len] = (Some(twin), len);
        self.len += 1;
        true
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (LogicalAddr, usize)> + '_ {
        self.slots[..self.len].iter().filter_map(|(twin, len)| Some((*twin.as_ref()?, *len)))
    }

    pub(super) fn contains(&self, twin: LogicalAddr) -> bool {
        self.iter().any(|(t, _)| t == twin)
    }

    /// Keeps the twins `keep` accepts, in order, in place.
    pub(super) fn retain(&mut self, mut keep: impl FnMut(LogicalAddr) -> bool) {
        let mut kept = 0;
        for i in 0..self.len {
            let slot = self.slots[i];
            let Some(twin) = slot.0 else { continue };
            if keep(twin) {
                self.slots[kept] = slot;
                kept += 1;
            }
        }
        self.len = kept;
    }
}

enum TicketStep {
    Next,
    /// The table is gone or the ticket's state moved: re-resolve.
    Stale,
    /// A same-key twin past [`TWIN_SCRATCH_CAP`] (A11's refusal).
    OverRegister,
}

enum Staged {
    Applied(u64),
    /// The key moved underneath the walk: re-resolve.
    Moved,
    /// The staging window cannot fit the worst-case run: wait drained.
    NoRoom,
}

/// The resolve arm of `delete_one`: `Ok(None)` = absent.
async fn resolve_delete_target<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    key: &[u8],
    hash: u64,
    want_value: bool,
    proto: Protocol,
) -> Result<Option<DeleteTarget>, Vec<u8>> {
    let target = match resolve(shared, ns, key, hash, PromoteOnCold::Never).await {
        Resolved::Miss => None,
        Resolved::Ram(addr) => {
            let ks = shared.store.borrow();
            let table = ks.tiered_store(ns).expect("resolved on this table");
            let parts = table.record(addr);
            let value = want_value.then(|| parts.value.to_vec());
            Some(DeleteTarget { addr, len: parts.encoded_len, value })
        }
        Resolved::Cold { addr, value, encoded_len, .. }
        | Resolved::Extent { addr, value, encoded_len, .. } => {
            Some(DeleteTarget { addr, len: encoded_len, value: Some(value) })
        }
        Resolved::Fail(message) => return Err(error_bytes(shared, proto, message)),
    };
    Ok(target)
}

/// One cursor step over the winner's tickets (A13), inside its own
/// borrow. A dropped table reads as no ticket; the atomic block refuses
/// it typed.
fn next_winner_ticket<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    winner: LogicalAddr,
    after: Option<LogicalAddr>,
) -> Option<inf_store::ShadowTicket> {
    let ks = shared.store.borrow();
    ks.tiered_store(ns)?.shadow_ticket_of_winner_after(winner, after)
}

/// One ticket's verdict (A10): a ticket already verified same-key (A1)
/// carries the twin's exact length and needs no read; an unverified one
/// is verified by its full-key `image`. Same-key twins land in the
/// scratch; a collision verdict ends its ticket and leaves the other
/// key alone.
fn note_winner_ticket<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    ticket: &inf_store::ShadowTicket,
    image: Option<&[u8]>,
    twins: &mut TwinScratch,
) -> TicketStep {
    let mut ks = shared.store.borrow_mut();
    let Some(table) = ks.tiered_store_mut(ns) else {
        return TicketStep::Stale;
    };
    table.note_shadow_forced_delete();
    let twin_len = match (ticket.verified_len, image) {
        (Some(len), _) => len as usize,
        (None, Some(image)) => match table.verify_shadow(ticket.hash, ticket.cold, image) {
            inf_store::ShadowVerdict::SameKey => image.len(),
            inf_store::ShadowVerdict::Collision => return TicketStep::Next,
            _ => return TicketStep::Stale,
        },
        (None, None) => return TicketStep::Stale,
    };
    if twins.push(ticket.cold, twin_len) {
        TicketStep::Next
    } else {
        table.note_shadow_delete_run_refused();
        TicketStep::OverRegister
    }
}

/// The atomic block: fit check → identity recheck → markers + `Delete`.
/// One store borrow, no suspension.
#[allow(clippy::too_many_arguments)]
fn stage_delete<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    class: Option<FsyncClass>,
    key: &[u8],
    hash: u64,
    target: &DeleteTarget,
    twins: &mut TwinScratch,
    proto: Protocol,
) -> Result<Staged, Vec<u8>> {
    let mut ks = shared.store.borrow_mut();
    let mut durable = shared.durable.borrow_mut();
    let Some(cell) = durable.as_mut() else {
        return Err(error_bytes(shared, proto, ERR_FAILED));
    };
    if cell.failed {
        return Err(error_bytes(shared, proto, ERR_FAILED));
    }
    // The marker run (ADR-0059 D9, ADR-0093 A11): each same-key twin's
    // origins and its address, then the winner's origins and its
    // address. The store's caps bound every record's origins, so the
    // budget is `RELOC_ORIGIN_CAP + 1` markers per record; the exact run
    // is counted against the replay register in `stage_delete_run`.
    let marker = MutationEffect::ColdDisplace { ns, old_addr: (1u64 << 48) - 1 }.encoded_len();
    let records = twins.len + 1;
    let worst = records * (inf_store::RELOC_ORIGIN_CAP + 1) * marker
        + MutationEffect::Delete { ns, key }.encoded_len();
    if !cell.would_fit(worst) {
        return Ok(Staged::NoRoom);
    }
    let Some(table) = ks.tiered_store_mut(ns) else {
        return Err(error_bytes(
            shared,
            proto,
            "ERR the selected namespace was dropped (INF.NS USE again)",
        ));
    };
    // The resolve verified identity; a raced mutation re-resolves
    // (delete is index + accounting only).
    match table.lookup(key, hash, &[]) {
        TieredLookup::Ram(now) | TieredLookup::Cold(now) if now == target.addr => {}
        _ => return Ok(Staged::Moved),
    }
    // Twins still slotted: one a MAINTAIN settle chained into the
    // winner's origins meanwhile is covered by the winner's list.
    twins.retain(|twin| table.contains_pair(hash, twin));
    // A ticket this pass did not verify (none can appear on an existing
    // address — retargets land on new ones — but the store's assert is
    // the proof, not this comment): re-resolve.
    if table.shadow_winner_tickets(target.addr).any(|t| !twins.contains(t.cold)) {
        return Ok(Staged::Moved);
    }
    let class = class.expect("tiered namespaces always carry a durability class");
    match stage_delete_run(cell, table, ns, class, key, hash, target, twins) {
        Some(seq) => Ok(Staged::Applied(seq)),
        None => {
            table.note_shadow_delete_run_refused();
            Err(error_bytes(shared, proto, ERR_DEL_REGISTER))
        }
    }
}

/// The marker run, counted against the replay register first (`None` =
/// over it, nothing staged, tickets intact — ADR-0059 D9's bound; on
/// engine-written input the run never exceeds it). Every verified
/// same-key twin first (ADR-0093 D3/A11): its origins' markers, its own
/// marker and its exact death, the ticket ending with the slot — then
/// the winner as any deleted record.
#[allow(clippy::too_many_arguments)]
fn stage_delete_run<F: SegmentFs + Clone + 'static>(
    cell: &mut DurableCell<F>,
    table: &mut TieredTable,
    ns: NsId,
    class: FsyncClass,
    key: &[u8],
    hash: u64,
    target: &DeleteTarget,
    twins: &TwinScratch,
) -> Option<u64> {
    let run: usize =
        twins.iter().map(|(twin, _)| table.displacement_origins_len(hash, twin) + 1).sum::<usize>()
            + table.displacement_origins_len(hash, target.addr)
            + 1;
    if run > inf_store::DISPLACE_REGISTER_CAP {
        return None;
    }
    for (twin, twin_len) in twins.iter() {
        for (origin_addr, _) in table.take_displacement_origins(hash, twin) {
            let m = MutationEffect::ColdDisplace { ns, old_addr: origin_addr };
            cell.stage_tiered(table, &m, class);
        }
        let m = MutationEffect::ColdDisplace { ns, old_addr: twin.to_raw() };
        cell.stage_tiered(table, &m, class);
        table.delete(hash, twin, twin_len);
    }
    table.delete(hash, target.addr, target.len);
    for (origin_addr, _) in table.take_displacement_origins(hash, target.addr) {
        let m = MutationEffect::ColdDisplace { ns, old_addr: origin_addr };
        cell.stage_tiered(table, &m, class);
    }
    let m = MutationEffect::ColdDisplace { ns, old_addr: target.addr.to_raw() };
    cell.stage_tiered(table, &m, class);
    Some(cell.stage_tiered(table, &MutationEffect::Delete { ns, key }, class))
}

pub(super) async fn mset<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    if argv.len() < 3 || !(argv.len() - 1).is_multiple_of(2) {
        return done_error(shared, proto, "ERR wrong number of arguments for 'mset' command");
    }
    let mut last_seq = 0;
    let mut i = 1;
    while i + 1 < argv.len() {
        match write_conditional(
            shared,
            ns,
            class,
            argv.arg(i),
            proto,
            false,
            false,
            argv.arg(i + 1),
            true,
        )
        .await
        {
            Ok((seq, _, _)) => last_seq = seq,
            Err(err) => return TieredReply::Done(err),
        }
        i += 2;
    }
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).simple("OK");
    if class == Some(FsyncClass::Always) {
        TieredReply::Gated { reply, seq: last_seq }
    } else {
        TieredReply::Done(reply)
    }
}
