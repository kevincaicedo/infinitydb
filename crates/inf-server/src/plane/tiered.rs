//! M4-S26 — tiered command execution: the string family against
//! [`TieredTable`] with cold-read suspension (the S08 `cold_hardened`
//! shape at the plane), WAL staging with displacement origins staged
//! first (ADR-0057 D4 + ADR-0059 D9), the ADR-0063 D2 admission gate,
//! and the tail-stall park with its typed `STALLED` timeout.
//!
//! Custody rules this module lives by (§3.3 / L6): every borrow of
//! `Shared::store` / `Shared::tier` / `Shared::durable` is scoped to a
//! synchronous block — only plain data and waiters cross an `.await`;
//! after every resume the command re-resolves through the index and
//! retries with the fetched-but-mismatched address excluded (the ≈2⁻²²
//! fingerprint false positive).
//!
//! Recorded policy decisions (S26 owns them; see the M4 ledger):
//! - **Cold `DEL`/`GETDEL` verify**: a deletion whose candidate is cold
//!   fetches + verifies first — one Foreground cold read. A blind kill
//!   by `(hash, addr)` would return wrong counts and could kill an
//!   innocent colliding key, and its unknown length would silently
//!   degrade per-file dead-byte exactness (the compaction trigger's
//!   input). The §3.3 index-only rule stays binding for the TTL wheel
//!   and eviction — neither runs on tiered namespaces in M4.
//! - **No expiry on tiered namespaces in M4**: `SET` expiry options and
//!   the `EXPIRE` family refuse typed; `TTL` answers -1 for live keys.
//! - **Values at or above `BLOB-THRESHOLD`** refuse typed until the
//!   blob leg of this story lands (behind the same D8 refusal).

use super::*;
use crate::exec::Argv;
use inf_store::{LogicalAddr, TieredLookup, TieredTable};

/// Typed refusals (compat register entries ride the D8 lift).
const ERR_NO_EXPIRY: &str = "ERR expiry is not supported on tiered namespaces in M4";
const ERR_UNSUPPORTED: &str =
    "ERR this command is not supported on tiered namespaces in M4 (string family only)";
const ERR_COLD_IO: &str = "ERR cold read failed (tier I/O error)";
const ERR_COLD_BUSY: &str = "BUSY cold-read queue saturated, try again";
const ERR_STALLED: &str =
    "STALLED tiered write timed out waiting for flush progress (TAIL-STALL-TIMEOUT)";
const ERR_FAILED: &str = "ERR durable plane failed (fail-stop)";
const ERR_BLOB_READ: &str = "ERR blob extent read failed (tier I/O error)";
const ERR_TOO_LARGE: &str = "ERR value exceeds BLOB-MAX for this namespace";
const ERR_OOM: &str = "OOM command not allowed when used memory > 'maxmemory'.";

/// Outcome of one tiered command.
pub(super) enum TieredReply {
    Done(Vec<u8>),
    /// An `always`-class write: the reply may only ship once the fsync
    /// watermark covers `seq` (§8.2 — ack after fsync).
    Gated {
        reply: Vec<u8>,
        seq: u64,
    },
}

/// One resolved key.
enum Resolved {
    Miss,
    /// RAM-resident; the caller re-reads parts inside its own borrow
    /// (no await separates resolution from use on the Ram arm).
    Ram(LogicalAddr),
    /// Cold record fetched and key-verified (owned copy).
    Cold {
        addr: LogicalAddr,
        value: Vec<u8>,
        version: u32,
        encoded_len: usize,
    },
    /// Blob-resident record: the value fetched from its extent (M4-S17
    /// wired by M4-S26). `encoded_len` is the 24-byte-reference record's
    /// length — the displacement/accounting unit, never the value's.
    Extent {
        addr: LogicalAddr,
        value: Vec<u8>,
        version: u32,
        encoded_len: usize,
    },
    /// Intermediate: a cold-fetched, key-verified extent record whose
    /// value fetch still has to run (the decode happens inside a borrow;
    /// the fetch awaits after it).
    ColdExtent {
        addr: LogicalAddr,
        ext: inf_store::ExtentRef,
        version: u32,
        encoded_len: usize,
    },
    /// Terminal typed reply (I/O error, saturation, dropped namespace).
    Fail(&'static str),
}

/// Why a write could not complete this attempt.
enum WriteBlock {
    /// Staging ring full — park on `drained`, re-resolve, retry.
    StagingFull,
    /// Tail allocation stalled on flush/release progress — park on the
    /// stall gate (deadline-bounded, ADR-0053 D4).
    Stall,
    /// Terminal typed reply.
    Reply(Vec<u8>),
}

/// The displaced record a write kills: `(addr, encoded_len, version)`.
type Displaced = Option<(LogicalAddr, usize, u32)>;

/// One tiered command, executed to a complete reply. `class` is the
/// namespace's fsync class (tiered ⊆ durable — ADR-0062 D1).
pub(super) async fn dispatch_tiered<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    origin: ExecOrigin,
    ns: NsId,
    meta: &'static inf_wire::CommandMeta,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    let started = shared.now.get();
    let cold_before = cold_issued(shared);
    let outcome = run_command(shared, ns, meta, argv, proto, class).await;
    // Split service histograms (ADR-0064 D3): µs on the loop clock,
    // lane-tagged by whether this command issued any cold read.
    let elapsed = shared.now.get().saturating_sub(started).as_micros();
    let served_cold = cold_issued(shared) > cold_before;
    if let Some(tier) = shared.tier.borrow_mut().as_mut() {
        if served_cold {
            tier.cold_us.record(elapsed);
        } else {
            tier.ram_hit_us.record(elapsed);
        }
    }
    let reply_bytes = match &outcome {
        TieredReply::Done(r) | TieredReply::Gated { reply: r, .. } => r.as_slice(),
    };
    shared.observer.borrow_mut().on_execute(
        shared.cell,
        origin,
        argv,
        reply_bytes,
        shared.now.get(),
    );
    outcome
}

fn cold_issued<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
) -> u64 {
    shared
        .tier
        .borrow()
        .as_ref()
        .and_then(|t| t.cold.as_ref().map(|c| c.counters().enqueued))
        .unwrap_or(0)
}

async fn run_command<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    meta: &'static inf_wire::CommandMeta,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    match meta.id {
        CommandId::Get => read_value(shared, ns, argv.arg(1), proto).await,
        CommandId::Mget => mget(shared, ns, argv, proto).await,
        CommandId::Exists | CommandId::Touch => exists(shared, ns, argv, proto).await,
        CommandId::Strlen => strlen(shared, ns, argv.arg(1), proto).await,
        CommandId::Type => type_cmd(shared, ns, argv.arg(1), proto).await,
        CommandId::Ttl | CommandId::Pttl => ttl(shared, ns, argv.arg(1), proto).await,
        CommandId::Getrange | CommandId::Substr => getrange(shared, ns, argv, proto).await,
        CommandId::Dbsize => {
            let mut reply = shared.take_reply_buf();
            let len = shared.store.borrow().tiered_store(ns).map_or(0, TieredTable::len);
            RespWriter::new(&mut reply, proto).int(len as i64);
            TieredReply::Done(reply)
        }
        CommandId::Scan => scan(shared, ns, argv, proto).await,
        CommandId::Set => set_cmd(shared, ns, argv, proto, class).await,
        CommandId::Setnx => setnx(shared, ns, argv, proto, class).await,
        CommandId::Getset => getset(shared, ns, argv, proto, class).await,
        CommandId::Getdel => getdel(shared, ns, argv, proto, class).await,
        CommandId::Getex => {
            if argv.len() > 2 {
                return done_error(shared, proto, ERR_NO_EXPIRY);
            }
            read_value(shared, ns, argv.arg(1), proto).await
        }
        CommandId::Append => append(shared, ns, argv, proto, class).await,
        CommandId::Setrange => setrange(shared, ns, argv, proto, class).await,
        CommandId::Incr | CommandId::Decr | CommandId::IncrBy | CommandId::DecrBy => {
            incr(shared, ns, meta.id, argv, proto, class).await
        }
        CommandId::IncrByFloat => incrbyfloat(shared, ns, argv, proto, class).await,
        CommandId::Mset => mset(shared, ns, argv, proto, class).await,
        CommandId::Del | CommandId::Unlink => del(shared, ns, argv, proto, class).await,
        CommandId::Expire
        | CommandId::Pexpire
        | CommandId::Expireat
        | CommandId::Pexpireat
        | CommandId::Persist
        | CommandId::Setex
        | CommandId::Psetex => done_error(shared, proto, ERR_NO_EXPIRY),
        _ => done_error(shared, proto, ERR_UNSUPPORTED),
    }
}

fn done_error<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    message: &str,
) -> TieredReply {
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).error(message);
    TieredReply::Done(reply)
}

// ---- resolution (the hardened fetch-verify-retry shape) ----

/// Cold-read plan for one attempt, yielded out of the borrow.
struct ColdPlan {
    wait: inf_runtime::ColdWait,
    addr: LogicalAddr,
    frames: u64,
    skip: usize,
}

enum Probe {
    Ram(LogicalAddr),
    Miss,
    Cold(ColdPlan),
    Fail(&'static str),
}

/// Probes the index and, on a cold candidate, enqueues the first read
/// window — all inside one borrow scope.
fn probe<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    key: &[u8],
    hash: u64,
    exclude: &[LogicalAddr],
    at: u64,
    len: usize,
) -> Probe {
    let ks = shared.store.borrow();
    let Some(table) = ks.tiered_store(ns) else {
        return Probe::Fail("ERR the selected namespace was dropped (INF.NS USE again)");
    };
    let addr = if at == 0 {
        match table.lookup(key, hash, exclude) {
            TieredLookup::Ram(addr) => return Probe::Ram(addr),
            TieredLookup::Miss => return Probe::Miss,
            TieredLookup::Cold(addr) => addr,
        }
    } else {
        // Continuation window of a staged assembly: the caller already
        // verified the record is still cold at this address.
        LogicalAddr::from_raw(at).expect("continuation address is 48-bit")
    };
    let tier = shared.tier.borrow();
    let Some(t) = tier.as_ref().and_then(|t| t.ns(ns)) else {
        return Probe::Fail("ERR the selected namespace was dropped (INF.NS USE again)");
    };
    let Some(cold) = tier.as_ref().and_then(|t| t.cold.clone()) else {
        return Probe::Fail(ERR_COLD_IO);
    };
    let Some((fd, file, offset, frames, skip)) = t.plan_cold_read(addr, len) else {
        // The catalog raced a retirement; the slot must have been
        // repointed — re-resolve observes the new address.
        return Probe::Fail("__replan");
    };
    let bytes = frames as usize * inf_log::TIER_FRAME_BYTES;
    match cold.enqueue(fd, file, offset, bytes, inf_runtime::ReadClass::Foreground, 0) {
        Ok(wait) => Probe::Cold(ColdPlan { wait, addr, frames, skip }),
        Err(_) => Probe::Fail(ERR_COLD_BUSY),
    }
}

/// Resolves one key: RAM hit, verified cold fetch, or miss — the S08
/// hardened loop (re-resolve after every resume; exclude on mismatch).
async fn resolve<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    key: &[u8],
    hash: u64,
) -> Resolved {
    let mut exclude: Vec<LogicalAddr> = Vec::new();
    let mut replans: u8 = 0;
    'attempt: loop {
        let plan = match probe(shared, ns, key, hash, &exclude, 0, TieredTable::RECORD_HEADER_LEN) {
            Probe::Ram(addr) => {
                // Blob-resident RAM records carry a 24-byte reference —
                // fetch the value from the extent (chunked async reads).
                let ext = {
                    let ks = shared.store.borrow();
                    let table = ks.tiered_store(ns).expect("resolved on this table");
                    let parts = table.record(addr);
                    (parts.type_tag == inf_store::TypeTag::StringExtent).then(|| {
                        (
                            inf_store::ExtentRef::decode(parts.value),
                            parts.version,
                            parts.encoded_len,
                        )
                    })
                };
                let Some((ext, version, encoded_len)) = ext else {
                    return Resolved::Ram(addr);
                };
                return match fetch_extent(shared, ns, ext).await {
                    Some(value) => Resolved::Extent { addr, value, version, encoded_len },
                    None => Resolved::Fail(ERR_BLOB_READ),
                };
            }
            Probe::Miss => return Resolved::Miss,
            Probe::Fail("__replan") => {
                replans += 1;
                if replans > 8 {
                    debug_assert!(false, "cold slot outside every catalogued file");
                    return Resolved::Fail(ERR_COLD_IO);
                }
                continue;
            }
            Probe::Fail(message) => return Resolved::Fail(message),
            Probe::Cold(plan) => plan,
        };
        let ColdPlan { wait, addr, frames, skip } = plan;
        let done = wait.await;
        if done.outcome().is_err() {
            return Resolved::Fail(ERR_COLD_IO);
        }
        // Re-resolve after the resume; decode inside the borrow.
        enum After {
            Serve(Resolved),
            Retry,
            Stage { total: usize, assembled: Vec<u8> },
        }
        let after = {
            let ks = shared.store.borrow();
            let Some(table) = ks.tiered_store(ns) else {
                return Resolved::Fail("ERR the selected namespace was dropped (INF.NS USE again)");
            };
            match table.lookup(key, hash, &exclude) {
                TieredLookup::Ram(promoted) => After::Serve(Resolved::Ram(promoted)),
                TieredLookup::Miss => After::Serve(Resolved::Miss),
                TieredLookup::Cold(now) if now != addr => After::Retry,
                TieredLookup::Cold(_) => done.bytes(|window| {
                    let mut head = Vec::new();
                    if inf_log::tier_extract(
                        window,
                        skip,
                        TieredTable::RECORD_HEADER_LEN,
                        &mut head,
                    )
                    .is_err()
                    {
                        return After::Serve(Resolved::Fail(ERR_COLD_IO));
                    }
                    let total = TieredTable::record_len_from_header(&head);
                    let window_data = frames as usize * inf_log::TIER_FRAME_DATA;
                    if skip + total <= window_data {
                        let mut record = Vec::new();
                        if inf_log::tier_extract(window, skip, total, &mut record).is_err() {
                            return After::Serve(Resolved::Fail(ERR_COLD_IO));
                        }
                        let parts = TieredTable::decode_record(&record);
                        if parts.key == key {
                            if parts.type_tag == inf_store::TypeTag::StringExtent {
                                After::Serve(Resolved::ColdExtent {
                                    addr,
                                    ext: inf_store::ExtentRef::decode(parts.value),
                                    version: parts.version,
                                    encoded_len: parts.encoded_len,
                                })
                            } else {
                                After::Serve(Resolved::Cold {
                                    addr,
                                    value: parts.value.to_vec(),
                                    version: parts.version,
                                    encoded_len: parts.encoded_len,
                                })
                            }
                        } else {
                            After::Retry
                        }
                    } else {
                        let take = window_data - skip;
                        let mut assembled = Vec::with_capacity(total);
                        if inf_log::tier_extract(window, skip, take, &mut assembled).is_err() {
                            return After::Serve(Resolved::Fail(ERR_COLD_IO));
                        }
                        After::Stage { total, assembled }
                    }
                }),
            }
        };
        drop(done); // custody home before any further await
        match after {
            After::Serve(Resolved::ColdExtent { addr, ext, version, encoded_len }) => {
                return match fetch_extent(shared, ns, ext).await {
                    Some(value) => Resolved::Extent { addr, value, version, encoded_len },
                    None => Resolved::Fail(ERR_BLOB_READ),
                };
            }
            After::Serve(resolved) => return resolved,
            After::Retry => {
                if !exclude.contains(&addr) {
                    exclude.push(addr);
                }
                continue;
            }
            After::Stage { total, mut assembled } => {
                while assembled.len() < total {
                    let at = addr.to_raw() + assembled.len() as u64;
                    let remaining = total - assembled.len();
                    let plan = match probe(shared, ns, key, hash, &exclude, at, remaining) {
                        Probe::Cold(plan) => plan,
                        Probe::Fail(message) if message != "__replan" => {
                            return Resolved::Fail(message);
                        }
                        _ => continue 'attempt, // state moved: re-resolve whole
                    };
                    let done = plan.wait.await;
                    if done.outcome().is_err() {
                        return Resolved::Fail(ERR_COLD_IO);
                    }
                    let ok = {
                        let ks = shared.store.borrow();
                        let still = ks.tiered_store(ns).map(|t| {
                            matches!(t.lookup(key, hash, &exclude),
                                TieredLookup::Cold(now) if now == addr)
                        });
                        if still != Some(true) {
                            false
                        } else {
                            done.bytes(|window| {
                                let take = remaining.min(
                                    plan.frames as usize * inf_log::TIER_FRAME_DATA - plan.skip,
                                );
                                let mut piece = Vec::new();
                                inf_log::tier_extract(window, plan.skip, take, &mut piece)
                                    .is_ok()
                                    .then(|| assembled.extend_from_slice(&piece))
                                    .is_some()
                            })
                        }
                    };
                    drop(done);
                    if !ok {
                        continue 'attempt;
                    }
                }
                let parts = TieredTable::decode_record(&assembled);
                if parts.key == key {
                    if parts.type_tag == inf_store::TypeTag::StringExtent {
                        let ext = inf_store::ExtentRef::decode(parts.value);
                        let (version, encoded_len) = (parts.version, parts.encoded_len);
                        return match fetch_extent(shared, ns, ext).await {
                            Some(value) => {
                                Resolved::Extent { addr, value, version, encoded_len }
                            }
                            None => Resolved::Fail(ERR_BLOB_READ),
                        };
                    }
                    return Resolved::Cold {
                        addr,
                        value: parts.value.to_vec(),
                        version: parts.version,
                        encoded_len: parts.encoded_len,
                    };
                }
                if !exclude.contains(&addr) {
                    exclude.push(addr);
                }
                continue 'attempt;
            }
        }
    }
}

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
    if let Err(err) = applied {
        return Err(write_block_of(shared, table, key, value, err, proto));
    }
    stage_displacements(cell, table, ns, hash, old, class);
    Ok(cell.stage_tiered(table, &MutationEffect::StringSet { ns, key, value }, class))
}

/// Stages the ADR-0059 D9 origin markers + the ADR-0057 D4 current-
/// address marker, in that order, ahead of the mutation record.
fn stage_displacements<F: SegmentFs>(
    cell: &mut DurableCell<F>,
    table: &mut TieredTable,
    ns: NsId,
    hash: u64,
    old: Displaced,
    class: FsyncClass,
) {
    if let Some((addr, _, _)) = old {
        for (origin_addr, _stamp) in table.take_displacement_origins(hash, addr) {
            let marker = MutationEffect::ColdDisplace { ns, old_addr: origin_addr };
            let _ = cell.stage_tiered(table, &marker, class);
        }
        let marker = MutationEffect::ColdDisplace { ns, old_addr: addr.to_raw() };
        let _ = cell.stage_tiered(table, &marker, class);
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
    .and_then(|mut writer| {
        writer.append_chunk(value).map_err(std::io::Error::other)?;
        writer.finish_deferred().map_err(std::io::Error::other)
    });
    let (sealed, handle) = match sealed {
        Ok(pair) => pair,
        // The failed extent is abandoned (never referenced, id never
        // reused); the orphan sweep reclaims the file (ADR-0061 D3).
        Err(err) => {
            let reply = if err.to_string().contains("StorageFull")
                || err.raw_os_error() == Some(28)
            {
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
        // sweep reclaims it. The handle just closes.
        drop(handle);
        return Err(write_block_of(shared, table, key, value, err, proto));
    }
    // D3 ordering: the barrier's ledger position precedes this
    // iteration's linked frame fsync (seal_log registers later in the
    // same iteration) — the ack is fenced mechanically.
    let fd = inf_log::fs::SegmentFile::raw_fd(&handle);
    let ticket = cell.commit.register_extent_barrier(handle, shared.now.get());
    if let Some(fd) = fd {
        tier.queue_extent_sync(fd, ticket);
    }
    stage_displacements(cell, table, ns, hash, old, class);
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
    value: &[u8],
    err: inf_store::OpError,
    proto: Protocol,
) -> WriteBlock {
    match err {
        inf_store::OpError::OutOfMemory => {
            // Ring window exhausted: distinguish "flush will free this"
            // (park on the stall gate) from genuine exhaustion.
            if table.write_stall_target(key, value).is_some() {
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

fn error_bytes<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
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
    let hash = TieredTable::hash_key(key);
    let deadline = stall_deadline(shared, ns);
    loop {
        let (old, old_value): (Displaced, Option<Vec<u8>>) =
            match resolve(shared, ns, key, hash).await {
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
                Resolved::ColdExtent { .. } => unreachable!("resolve post-processes extents"),
                Resolved::Fail(message) => return Err(error_bytes(shared, proto, message)),
            };
        let value = compute(old_value.as_deref(), old.map(|(_, _, v)| v))?;
        match try_write(shared, ns, class, key, hash, &value, old, proto) {
            Ok(seq) => return Ok((seq, old_value)),
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

// ---- read commands ----

async fn read_value<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    key: &[u8],
    proto: Protocol,
) -> TieredReply {
    let hash = TieredTable::hash_key(key);
    let mut reply = shared.take_reply_buf();
    let mut w = RespWriter::new(&mut reply, proto);
    match resolve(shared, ns, key, hash).await {
        Resolved::Miss => w.null(),
        Resolved::Ram(addr) => {
            let ks = shared.store.borrow();
            let table = ks.tiered_store(ns).expect("resolved on this table");
            w.bulk(table.record(addr).value);
        }
        Resolved::Cold { value, .. } | Resolved::Extent { value, .. } => w.bulk(&value),
        Resolved::ColdExtent { .. } => unreachable!("resolve post-processes extents"),
        Resolved::Fail(message) => w.error(message),
    }
    TieredReply::Done(reply)
}

async fn mget<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
) -> TieredReply {
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).array_header(argv.len() - 1);
    for key in &argv[1..] {
        let hash = TieredTable::hash_key(key);
        let mut w = RespWriter::new(&mut reply, proto);
        match resolve(shared, ns, key, hash).await {
            Resolved::Ram(addr) => {
                let ks = shared.store.borrow();
                let table = ks.tiered_store(ns).expect("resolved on this table");
                w.bulk(table.record(addr).value);
            }
            Resolved::Cold { value, .. } | Resolved::Extent { value, .. } => w.bulk(&value),
            _ => w.null(),
        }
    }
    TieredReply::Done(reply)
}

async fn exists<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
) -> TieredReply {
    let mut count = 0i64;
    for key in &argv[1..] {
        let hash = TieredTable::hash_key(key);
        match resolve(shared, ns, key, hash).await {
            Resolved::Ram(_) | Resolved::Cold { .. } | Resolved::Extent { .. } => count += 1,
            _ => {}
        }
    }
    let mut reply = shared.take_reply_buf();
    RespWriter::new(&mut reply, proto).int(count);
    TieredReply::Done(reply)
}

async fn strlen<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    key: &[u8],
    proto: Protocol,
) -> TieredReply {
    let hash = TieredTable::hash_key(key);
    let mut reply = shared.take_reply_buf();
    let mut w = RespWriter::new(&mut reply, proto);
    match resolve(shared, ns, key, hash).await {
        Resolved::Miss => w.int(0),
        Resolved::Ram(addr) => {
            let ks = shared.store.borrow();
            let table = ks.tiered_store(ns).expect("resolved on this table");
            w.int(table.record(addr).value.len() as i64);
        }
        Resolved::Cold { value, .. } | Resolved::Extent { value, .. } => {
            w.int(value.len() as i64);
        }
        Resolved::ColdExtent { .. } => unreachable!("resolve post-processes extents"),
        Resolved::Fail(message) => w.error(message),
    }
    TieredReply::Done(reply)
}

async fn type_cmd<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    key: &[u8],
    proto: Protocol,
) -> TieredReply {
    let hash = TieredTable::hash_key(key);
    let mut reply = shared.take_reply_buf();
    let mut w = RespWriter::new(&mut reply, proto);
    match resolve(shared, ns, key, hash).await {
        Resolved::Ram(_) | Resolved::Cold { .. } | Resolved::Extent { .. } => w.simple("string"),
        Resolved::ColdExtent { .. } => unreachable!("resolve post-processes extents"),
        Resolved::Miss => w.simple("none"),
        Resolved::Fail(message) => w.error(message),
    }
    TieredReply::Done(reply)
}

async fn ttl<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    key: &[u8],
    proto: Protocol,
) -> TieredReply {
    let hash = TieredTable::hash_key(key);
    let mut reply = shared.take_reply_buf();
    let mut w = RespWriter::new(&mut reply, proto);
    match resolve(shared, ns, key, hash).await {
        // No expiry on tiered namespaces in M4: live keys never expire.
        Resolved::Ram(_) | Resolved::Cold { .. } | Resolved::Extent { .. } => w.int(-1),
        Resolved::ColdExtent { .. } => unreachable!("resolve post-processes extents"),
        Resolved::Miss => w.int(-2),
        Resolved::Fail(message) => w.error(message),
    }
    TieredReply::Done(reply)
}

async fn getrange<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
) -> TieredReply {
    let (Some(start), Some(end)) = (parse_i64(argv.arg(2)), parse_i64(argv.arg(3))) else {
        return done_error(shared, proto, "ERR value is not an integer or out of range");
    };
    let key = argv.arg(1);
    let hash = TieredTable::hash_key(key);
    let mut reply = shared.take_reply_buf();
    let mut w = RespWriter::new(&mut reply, proto);
    let slice_of = |value: &[u8], w: &mut RespWriter<'_>| {
        let len = value.len() as i64;
        let from = if start < 0 { (len + start).max(0) } else { start.min(len) };
        let to = if end < 0 { len + end } else { end.min(len - 1) };
        if from > to || len == 0 {
            w.bulk(b"");
        } else {
            w.bulk(&value[from as usize..=(to as usize)]);
        }
    };
    match resolve(shared, ns, key, hash).await {
        Resolved::Miss => w.bulk(b""),
        Resolved::Ram(addr) => {
            let ks = shared.store.borrow();
            let table = ks.tiered_store(ns).expect("resolved on this table");
            slice_of(table.record(addr).value, &mut w);
        }
        Resolved::Cold { value, .. } | Resolved::Extent { value, .. } => slice_of(&value, &mut w),
        Resolved::ColdExtent { .. } => unreachable!("resolve post-processes extents"),
        Resolved::Fail(message) => w.error(message),
    }
    TieredReply::Done(reply)
}

async fn scan<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
) -> TieredReply {
    let Some(cursor) = std::str::from_utf8(argv.arg(1)).ok().and_then(|s| s.parse::<u64>().ok())
    else {
        return done_error(shared, proto, "ERR invalid cursor");
    };
    let mut count = 10usize;
    let mut i = 2;
    while i + 1 < argv.len() {
        if argv.arg(i).eq_ignore_ascii_case(b"COUNT") {
            match std::str::from_utf8(argv.arg(i + 1)).ok().and_then(|s| s.parse::<usize>().ok()) {
                Some(n) if n > 0 => count = n.min(10_000),
                _ => {
                    return done_error(
                        shared,
                        proto,
                        "ERR value is not an integer or out of range",
                    );
                }
            }
            i += 2;
        } else {
            // MATCH/TYPE filters are not wired on tiered namespaces yet.
            return done_error(shared, proto, ERR_UNSUPPORTED);
        }
    }
    // Slice the index inside one borrow; resolve cold keys after.
    let (next, slots): (u64, Vec<(u64, LogicalAddr)>) = {
        let ks = shared.store.borrow();
        let Some(table) = ks.tiered_store(ns) else {
            return done_error(shared, proto, "ERR the selected namespace was dropped");
        };
        let mut slots = Vec::with_capacity(count);
        let next = table.scan_slots(cursor, count, |hash, addr| slots.push((hash, addr)));
        (next, slots)
    };
    let mut keys: Vec<Vec<u8>> = Vec::with_capacity(slots.len());
    for (hash, addr) in slots {
        let ram_key = {
            let ks = shared.store.borrow();
            let Some(table) = ks.tiered_store(ns) else { break };
            match table.space().resolve(addr) {
                inf_store::AddrClass::Cold => None,
                _ => Some(table.record(addr).key.to_vec()),
            }
        };
        match ram_key {
            Some(key) => keys.push(key),
            None => {
                // Cold slot: fetch the record to name the key — what a
                // beyond-RAM enumeration inherently costs (SCAN allows
                // duplicates/races; the decoded key is authoritative).
                if let Some(key) = fetch_key(shared, ns, hash, addr).await {
                    keys.push(key);
                }
            }
        }
    }
    let mut reply = shared.take_reply_buf();
    let mut w = RespWriter::new(&mut reply, proto);
    w.array_header(2);
    w.bulk(next.to_string().as_bytes());
    w.array_header(keys.len());
    for key in &keys {
        w.bulk(key);
    }
    TieredReply::Done(reply)
}

/// Fetches the record at a cold slot solely to learn its key (SCAN).
async fn fetch_key<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    _hash: u64,
    addr: LogicalAddr,
) -> Option<Vec<u8>> {
    let plan = {
        let tier = shared.tier.borrow();
        let t = tier.as_ref().and_then(|t| t.ns(ns))?;
        let cold = tier.as_ref().and_then(|t| t.cold.clone())?;
        let (fd, file, offset, frames, skip) =
            t.plan_cold_read(addr, TieredTable::RECORD_HEADER_LEN)?;
        let bytes = frames as usize * inf_log::TIER_FRAME_BYTES;
        let wait =
            cold.enqueue(fd, file, offset, bytes, inf_runtime::ReadClass::Foreground, 0).ok()?;
        ColdPlan { wait, addr, frames, skip }
    };
    let done = plan.wait.await;
    done.outcome().ok()?;
    done.bytes(|window| {
        let mut head = Vec::new();
        inf_log::tier_extract(window, plan.skip, TieredTable::RECORD_HEADER_LEN, &mut head).ok()?;
        let total = TieredTable::record_len_from_header(&head);
        let window_data = plan.frames as usize * inf_log::TIER_FRAME_DATA;
        // SCAN key resolution reads one window; records spilling past it
        // are skipped this slice (rare: keys live in the record head).
        let take = total.min(window_data - plan.skip);
        let mut record = Vec::new();
        inf_log::tier_extract(window, plan.skip, take, &mut record).ok()?;
        (take == total).then(|| TieredTable::decode_record(&record).key.to_vec())
    })
}

/// Fetches a blob-resident value from its extent (M4-S26 wiring the
/// M4-S17 read path): chunked `ColdReads` windows against the extent's
/// creation-mode fd — the reader owns the handle across the awaits, so
/// a concurrent reclaim's unlink cannot invalidate the reads (POSIX
/// keeps the inode; the S08 cancellation test's contract). Pins ride a
/// synthetic high-bit `TierFileId` so extent reads never alias a tier
/// file's retirement gate.
async fn fetch_extent<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    ext: inf_store::ExtentRef,
) -> Option<Vec<u8>> {
    debug_assert_eq!(ext.offset, 0, "v1 extent references start at 0");
    let reader = {
        let tier = shared.tier.borrow();
        tier.as_ref()?.open_extent_reader(ns, ext.extent_id).ok()?
    };
    let fd = reader.raw_fd()?;
    let file = inf_runtime::TierFileId::new(0x8000_0000 | (ext.extent_id as u32 & 0x7FFF_FFFF));
    let total_frames = ext.len.div_ceil(inf_log::TIER_FRAME_DATA as u64);
    let window_frames_cap = (crate::tier_cell::COLD_POOL_BUF / inf_log::TIER_FRAME_BYTES) as u64;
    let mut out: Vec<u8> = Vec::with_capacity(ext.len as usize);
    while (out.len() as u64) < ext.len {
        let offset = out.len() as u64;
        let remaining = (ext.len - offset) as usize;
        let (first, _, skip) = inf_log::tier_frame_span(offset, remaining);
        let frames = window_frames_cap.min(total_frames - first);
        let wait = {
            let tier = shared.tier.borrow();
            let cold = tier.as_ref().and_then(|t| t.cold.clone())?;
            cold.enqueue(
                fd,
                file,
                inf_log::blob::extent_frame_offset(first),
                frames as usize * inf_log::TIER_FRAME_BYTES,
                inf_runtime::ReadClass::Foreground,
                0,
            )
            .ok()?
        };
        let done = wait.await;
        done.outcome().ok()?;
        let ok = done.bytes(|window| {
            let take = remaining.min(frames as usize * inf_log::TIER_FRAME_DATA - skip);
            inf_log::tier_extract(window, skip, take, &mut out).is_ok()
        });
        drop(done);
        if !ok {
            return None;
        }
    }
    drop(reader); // the fd stayed open across every read
    Some(out)
}

// ---- write commands ----

async fn set_cmd<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    let key = argv.arg(1);
    let value = argv.arg(2);
    let (mut nx, mut xx, mut get_old) = (false, false, false);
    for i in 3..argv.len() {
        let opt = argv.arg(i);
        if opt.eq_ignore_ascii_case(b"NX") {
            nx = true;
        } else if opt.eq_ignore_ascii_case(b"XX") {
            xx = true;
        } else if opt.eq_ignore_ascii_case(b"GET") {
            get_old = true;
        } else {
            return done_error(shared, proto, ERR_NO_EXPIRY);
        }
    }
    // Conditional SETs resolve first; the write helper re-resolves per
    // attempt, so the condition is re-evaluated with it.
    let outcome = write_conditional(shared, ns, class, key, proto, nx, xx, value).await;
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
) -> Result<(u64, Option<Vec<u8>>, bool), Vec<u8>> {
    let hash = TieredTable::hash_key(key);
    let deadline = stall_deadline(shared, ns);
    loop {
        let (old, old_value): (Displaced, Option<Vec<u8>>) =
            match resolve(shared, ns, key, hash).await {
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
                Resolved::ColdExtent { .. } => unreachable!("resolve post-processes extents"),
                Resolved::Fail(message) => return Err(error_bytes(shared, proto, message)),
            };
        if (nx && old.is_some()) || (xx && old.is_none()) {
            return Ok((0, old_value, false));
        }
        match try_write(shared, ns, class, key, hash, value, old, proto) {
            Ok(seq) => return Ok((seq, old_value, true)),
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

async fn setnx<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    match write_conditional(shared, ns, class, argv.arg(1), proto, true, false, argv.arg(2)).await {
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

async fn getset<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    match write_conditional(shared, ns, class, argv.arg(1), proto, false, false, argv.arg(2)).await
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

async fn append<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    let suffix = argv.arg(2).to_vec();
    let outcome = write_value(shared, ns, class, argv.arg(1), proto, |old, _| {
        let mut value = old.map(<[u8]>::to_vec).unwrap_or_default();
        value.extend_from_slice(&suffix);
        Ok(value)
    })
    .await;
    int_write_reply(shared, proto, class, outcome, |old| {
        (old.map_or(0, |v| v.len()) + suffix.len()) as i64
    })
}

async fn setrange<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    argv: &[&[u8]],
    proto: Protocol,
    class: Option<FsyncClass>,
) -> TieredReply {
    let Some(offset) = parse_i64(argv.arg(2)).filter(|o| *o >= 0) else {
        return done_error(shared, proto, "ERR offset is out of range");
    };
    let offset = offset as usize;
    let patch = argv.arg(3).to_vec();
    let outcome = write_value(shared, ns, class, argv.arg(1), proto, |old, _| {
        let mut value = old.map(<[u8]>::to_vec).unwrap_or_default();
        if value.len() < offset + patch.len() {
            value.resize(offset + patch.len(), 0);
        }
        value[offset..offset + patch.len()].copy_from_slice(&patch);
        Ok(value)
    })
    .await;
    int_write_reply(shared, proto, class, outcome, |old| {
        old.map_or(0, |v| v.len()).max(offset + patch.len()) as i64
    })
}

async fn incr<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
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

async fn incrbyfloat<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
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

async fn getdel<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
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

async fn del<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
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
/// policy) → stage markers + `Delete` → apply. `Ok(None)` = the key was
/// absent; `Ok(Some((seq, value)))` = deleted (`want_value` carries the
/// old value out for GETDEL).
async fn delete_one<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    class: Option<FsyncClass>,
    key: &[u8],
    proto: Protocol,
    want_value: bool,
) -> Result<Option<(u64, Option<Vec<u8>>)>, Vec<u8>> {
    let hash = TieredTable::hash_key(key);
    loop {
        let (old, old_value): (Displaced, Option<Vec<u8>>) =
            match resolve(shared, ns, key, hash).await {
                Resolved::Miss => return Ok(None),
                Resolved::Ram(addr) => {
                    let ks = shared.store.borrow();
                    let table = ks.tiered_store(ns).expect("resolved on this table");
                    let parts = table.record(addr);
                    let value = want_value.then(|| parts.value.to_vec());
                    (Some((addr, parts.encoded_len, parts.version)), value)
                }
                Resolved::Cold { addr, value, version, encoded_len }
                | Resolved::Extent { addr, value, version, encoded_len } => {
                    (Some((addr, encoded_len, version)), Some(value))
                }
                Resolved::ColdExtent { .. } => unreachable!("resolve post-processes extents"),
                Resolved::Fail(message) => return Err(error_bytes(shared, proto, message)),
            };
        let Some((addr, len, _)) = old else { return Ok(None) };
        // Atomic block: fit check → apply → markers + Delete record.
        let staged = {
            let mut ks = shared.store.borrow_mut();
            let mut durable = shared.durable.borrow_mut();
            let Some(cell) = durable.as_mut() else {
                return Err(error_bytes(shared, proto, ERR_FAILED));
            };
            if cell.failed {
                return Err(error_bytes(shared, proto, ERR_FAILED));
            }
            let marker =
                MutationEffect::ColdDisplace { ns, old_addr: (1u64 << 48) - 1 }.encoded_len();
            let worst = 4 * marker + MutationEffect::Delete { ns, key }.encoded_len();
            if !cell.would_fit(worst) {
                None
            } else {
                let Some(table) = ks.tiered_store_mut(ns) else {
                    return Err(error_bytes(
                        shared,
                        proto,
                        "ERR the selected namespace was dropped (INF.NS USE again)",
                    ));
                };
                // The resolve above verified identity; a raced mutation
                // re-resolves (delete is index + accounting only).
                match table.lookup(key, hash, &[]) {
                    TieredLookup::Ram(now) | TieredLookup::Cold(now) if now == addr => {
                        table.delete(hash, addr, len);
                        let class =
                            class.expect("tiered namespaces always carry a durability class");
                        for (origin_addr, _) in table.take_displacement_origins(hash, addr) {
                            let m = MutationEffect::ColdDisplace { ns, old_addr: origin_addr };
                            cell.stage_tiered(table, &m, class);
                        }
                        let m = MutationEffect::ColdDisplace { ns, old_addr: addr.to_raw() };
                        cell.stage_tiered(table, &m, class);
                        let seq =
                            cell.stage_tiered(table, &MutationEffect::Delete { ns, key }, class);
                        Some(Some(seq))
                    }
                    _ => Some(None), // moved underneath us: re-resolve
                }
            }
        };
        match staged {
            Some(Some(seq)) => return Ok(Some((seq, old_value))),
            Some(None) => continue,
            None => {
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

async fn mset<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
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

// ---- small helpers ----

fn int_write_reply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    proto: Protocol,
    class: Option<FsyncClass>,
    outcome: Result<(u64, Option<Vec<u8>>), Vec<u8>>,
    int_of: impl FnOnce(Option<&[u8]>) -> i64,
) -> TieredReply {
    match outcome {
        Ok((seq, old_value)) => {
            let mut reply = shared.take_reply_buf();
            RespWriter::new(&mut reply, proto).int(int_of(old_value.as_deref()));
            if class == Some(FsyncClass::Always) {
                TieredReply::Gated { reply, seq }
            } else {
                TieredReply::Done(reply)
            }
        }
        Err(err) => TieredReply::Done(err),
    }
}

fn parse_i64(bytes: &[u8]) -> Option<i64> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// Redis's INCRBYFLOAT rendering: up to 17 significant digits, no
/// trailing zeros, never scientific notation.
fn format_float(f: f64) -> Vec<u8> {
    let mut text = format!("{f:.17}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    text.into_bytes()
}
