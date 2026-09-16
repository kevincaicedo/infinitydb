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
//! - **Shadow-slot reconciliation (M4.5-S37, ADR-0093)**: a plain `SET`
//!   whose only exact-hash candidate is cold appends its record and
//!   registers the candidate as a shadow instead of reading it on the
//!   command's critical path (`try_shadow_write`); the MAINTAIN
//!   reconciler (`shadow_pump`) reads and verifies it later. `DEL`/
//!   `GETDEL` resolve an open ticket synchronously first (`delete_one`)
//!   — a deleted key must never resurface with its unverified twin.

use super::*;
use crate::exec::Argv;
use inf_store::{LogicalAddr, TieredLookup, TieredTable};
mod write;

use write::{
    TWIN_SCRATCH_CAP, append, del, error_bytes, getdel, getset, incr, incrbyfloat, mset, set_cmd,
    setnx, setrange,
};
// `pumps` reconciles shadow twins through the tiered read path.
pub(super) use write::read_cold_record;

// ---- shared data definitions (behaviour lives in the child modules) ----------

/// Fixed-capacity `(twin, encoded_len)` scratch — the walk's only state
/// besides the cursor. Never heap-allocated.
#[derive(Default)]
pub(super) struct TwinScratch {
    slots: [(Option<LogicalAddr>, usize); TWIN_SCRATCH_CAP],
    len: usize,
}

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
    /// Terminal typed reply (I/O error, saturation, dropped namespace).
    Fail(&'static str),
}

/// Whether a verified cold fetch feeds the ADR-0085 promotion hook.
/// Reads do; the write funnels' resolves never do — a write's
/// `overwrite` already copies to the tail, so promoting first would
/// double-place the record — and a deletion's fetch is a kill, not an
/// access. `SCAN`'s enumeration (`fetch_key`) never reaches `resolve`.
#[derive(Copy, Clone, PartialEq, Eq)]
enum PromoteOnCold {
    Read,
    Never,
}

/// Why a write could not complete this attempt.
enum WriteBlock {
    /// Staging ring full — park on `drained`, re-resolve, retry.
    StagingFull,
    /// Tail allocation stalled on flush/release progress — park on the
    /// stall gate (deadline-bounded, ADR-0053 D4).
    Stall,
    /// The resolved slot moved while the write was suspended on its
    /// extent read (review of 2026-08-30, F-L06-03) — re-resolve; the
    /// caller bounds the retries.
    Replan,
    /// Terminal typed reply.
    Reply(Vec<u8>),
}

/// The displaced record a write kills: `(addr, encoded_len, version)`.
type Displaced = Option<(LogicalAddr, usize, u32)>;

/// Whether a resolved `Displaced` is still current when the write runs
/// (F-L06-03). Every `resolve` arm but one returns under the borrow
/// that verified the slot; the blob arm suspends on `fetch_extent`
/// after its verification (one await per cold window, up to 65 k of
/// them), so a concurrent write, delete, compaction relocation or
/// promotion may have moved the slot underneath it — and the store
/// answers a stale address with a panic, correctly (`Index::replace`).
#[derive(Copy, Clone, PartialEq, Eq)]
enum WriteGuard {
    /// No suspension separates the resolve from the write.
    Current,
    /// The resolve suspended after its verification: re-verify the
    /// slot under the write's own borrow — the guard `delete_one`
    /// already had.
    Reverify,
}

/// Bound on consecutive replans of one write (a legal interleaving
/// that keeps recurring is a livelock in the making — L6/§17 "put a
/// limit on everything"); past it the client gets a typed retry.
const WRITE_REPLAN_MAX: u8 = 32;
const ERR_REPLAN_EXHAUSTED: &str =
    "BUSY the key kept changing under this write's extent read — retry";

/// What a refused write's stall probe is sized from (ADR-0102 D3,
/// F-L06-05): the inline record's value, or the 24-byte extent
/// reference — never a blob's bytes.
enum StallProbe<'a> {
    Inline(&'a [u8]),
    Extent,
}

impl WriteGuard {
    fn of(resolved: &Resolved) -> WriteGuard {
        match resolved {
            Resolved::Extent { .. } => WriteGuard::Reverify,
            Resolved::Miss | Resolved::Ram(_) | Resolved::Cold { .. } | Resolved::Fail(_) => {
                WriteGuard::Current
            }
        }
    }
}

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
    // lane-tagged by whether this command issued any cold read. The
    // loop clock is frozen per reactor iteration, so a command that
    // never suspends records exactly 0 whatever its true service time:
    // the ram-hit lane resolves *iteration crossings* (parks, stalls),
    // not microseconds — `INFO` therefore refuses to render its
    // percentiles as numbers (`admin::tiering_section`). The cold lane
    // always crosses an iteration and stays honest.
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
        super::ExecScope::Ns(ns),
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
        CommandId::Dbsize => dbsize(shared, ns, proto).await,
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
    // The enqueue stamp and `on_completion`'s stamp must come from the
    // same injected clock (L7): the plane completes with `cx.now`, so a
    // zero here turns `cold_read_p99_us` into absolute uptime — the
    // v0.4.0-alpha soak's 85899345919 µs fingerprint (instrument fix).
    let now_us = shared.now.get().as_micros();
    // Deterministic stand-in for a saturated cold queue (the BUSY leg's
    // fault point — review of 2026-08-30, C2′).
    if inf_foundation::fault::fire(crate::fault::COLD_ENQUEUE_FULL) {
        return Probe::Fail(ERR_COLD_BUSY);
    }
    match cold.enqueue(fd, file, offset, bytes, inf_runtime::ReadClass::Foreground, now_us) {
        Ok(wait) => Probe::Cold(ColdPlan { wait, addr, frames, skip }),
        Err(_) => Probe::Fail(ERR_COLD_BUSY),
    }
}

/// Serves a verified cold record image: the ADR-0085 promotion offer
/// first (reads only — `promote`), then the inline value or the extent
/// fetch. `image` is the verbatim record the resolve loop fetched and
/// key-verified; promotion relocates exactly these bytes, so a
/// re-encode can never re-type the record (the ADR-0059 D2 rule).
async fn serve_cold_image<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    hash: u64,
    addr: LogicalAddr,
    image: Vec<u8>,
    promote: PromoteOnCold,
) -> Resolved {
    let (version, encoded_len, ext) = {
        let parts = TieredTable::decode_record(&image);
        (parts.version, parts.encoded_len, parts.extent_ref())
    };
    if promote == PromoteOnCold::Read {
        // No await separates the loop's verify from this borrow, so the
        // pair is still current on this single-threaded cell — and
        // `try_promote` re-verifies it anyway (best-effort: a skip of
        // any kind is exactly the pre-S30 behavior).
        let mut ks = shared.store.borrow_mut();
        if let Some(table) = ks.tiered_store_mut(ns) {
            table.try_promote(hash, addr, &image);
        }
    }
    match ext {
        Some(ext) => match fetch_extent(shared, ns, ext).await {
            Some(value) => Resolved::Extent { addr, value, version, encoded_len },
            None => Resolved::Fail(ERR_BLOB_READ),
        },
        None => {
            let value = TieredTable::decode_record(&image).value.to_vec();
            Resolved::Cold { addr, value, version, encoded_len }
        }
    }
}

/// Resolves one key: RAM hit, verified cold fetch, or miss — the S08
/// hardened loop (re-resolve after every resume; exclude on mismatch).
/// Counting wrapper over [`resolve_inner`]: every terminal `Fail` —
/// which every consumer now surfaces typed (review of 2026-08-30, C2′)
/// — increments the always-on `cold_read_errors` counter, so the
/// failure rate is scrapeable (`INFO tiering`), not just visible
/// per-reply (L10).
async fn resolve<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    key: &[u8],
    hash: u64,
    promote: PromoteOnCold,
) -> Resolved {
    let resolved = resolve_inner(shared, ns, key, hash, promote).await;
    if matches!(resolved, Resolved::Fail(_))
        && let Some(table) = shared.store.borrow_mut().tiered_store_mut(ns)
    {
        table.note_cold_read_error();
    }
    resolved
}

async fn resolve_inner<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    key: &[u8],
    hash: u64,
    promote: PromoteOnCold,
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
            /// Key-verified single-window fetch: the verbatim image
            /// serves (and may promote) outside the borrow.
            ServeCold {
                image: Vec<u8>,
            },
            Retry,
            Stage {
                total: usize,
                assembled: Vec<u8>,
            },
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
                        if TieredTable::decode_record(&record).key == key {
                            After::ServeCold { image: record }
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
            After::ServeCold { image } => {
                return serve_cold_image(shared, ns, hash, addr, image, promote).await;
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
                if TieredTable::decode_record(&assembled).key == key {
                    return serve_cold_image(shared, ns, hash, addr, assembled, promote).await;
                }
                if !exclude.contains(&addr) {
                    exclude.push(addr);
                }
                continue 'attempt;
            }
        }
    }
}

// ---- read commands ----

async fn read_value<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    key: &[u8],
    proto: Protocol,
) -> TieredReply {
    let hash = shared.hasher.hash(key);
    let mut reply = shared.take_reply_buf();
    let mut w = RespWriter::new(&mut reply, proto);
    match resolve(shared, ns, key, hash, PromoteOnCold::Read).await {
        Resolved::Miss => w.null(),
        Resolved::Ram(addr) => {
            let ks = shared.store.borrow();
            let table = ks.tiered_store(ns).expect("resolved on this table");
            // Planted-bug canary (ADR-0129 A1, `scripts/sim-canaries.sh`):
            // the tiered read path lies like `exec.rs`'s GET, so the
            // tiered writers' own expectation is proven to have teeth.
            #[cfg(inf_canary_reply_lie)]
            w.bulk(&[table.record(addr).value, b"!"].concat());
            #[cfg(not(inf_canary_reply_lie))]
            w.bulk(table.record(addr).value);
        }
        #[cfg(inf_canary_reply_lie)]
        Resolved::Cold { value, .. } | Resolved::Extent { value, .. } => {
            w.bulk(&[value.as_slice(), b"!"].concat());
        }
        #[cfg(not(inf_canary_reply_lie))]
        Resolved::Cold { value, .. } | Resolved::Extent { value, .. } => w.bulk(&value),
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
        let hash = shared.hasher.hash(key);
        let mut w = RespWriter::new(&mut reply, proto);
        match resolve(shared, ns, key, hash, PromoteOnCold::Read).await {
            Resolved::Ram(addr) => {
                let ks = shared.store.borrow();
                let table = ks.tiered_store(ns).expect("resolved on this table");
                w.bulk(table.record(addr).value);
            }
            Resolved::Cold { value, .. } | Resolved::Extent { value, .. } => w.bulk(&value),
            Resolved::Miss => w.null(),
            // A failed read is never "not there" (review of 2026-08-30,
            // C2′/F-L06-04): RESP2 has no per-element error, so the
            // whole command answers typed — the partial array is
            // abandoned, exactly what GET answers for the same key.
            Resolved::Fail(message) => {
                reply.clear();
                RespWriter::new(&mut reply, proto).error(message);
                return TieredReply::Done(reply);
            }
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
        let hash = shared.hasher.hash(key);
        match resolve(shared, ns, key, hash, PromoteOnCold::Read).await {
            Resolved::Ram(_) | Resolved::Cold { .. } | Resolved::Extent { .. } => count += 1,
            Resolved::Miss => {}
            // Unreadable ≠ absent (C2′/F-L06-04): EXISTS is exactly what
            // a cache-fill path uses to decide whether to overwrite, so
            // a partial count under a failed read would license
            // overwriting live data. Typed, whole-command, like GET.
            Resolved::Fail(message) => return done_error(shared, proto, message),
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
    let hash = shared.hasher.hash(key);
    let mut reply = shared.take_reply_buf();
    let mut w = RespWriter::new(&mut reply, proto);
    match resolve(shared, ns, key, hash, PromoteOnCold::Read).await {
        Resolved::Miss => w.int(0),
        Resolved::Ram(addr) => {
            let ks = shared.store.borrow();
            let table = ks.tiered_store(ns).expect("resolved on this table");
            w.int(table.record(addr).value.len() as i64);
        }
        Resolved::Cold { value, .. } | Resolved::Extent { value, .. } => {
            w.int(value.len() as i64);
        }
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
    let hash = shared.hasher.hash(key);
    let mut reply = shared.take_reply_buf();
    let mut w = RespWriter::new(&mut reply, proto);
    match resolve(shared, ns, key, hash, PromoteOnCold::Read).await {
        Resolved::Ram(_) | Resolved::Cold { .. } | Resolved::Extent { .. } => w.simple("string"),
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
    let hash = shared.hasher.hash(key);
    let mut reply = shared.take_reply_buf();
    let mut w = RespWriter::new(&mut reply, proto);
    match resolve(shared, ns, key, hash, PromoteOnCold::Read).await {
        // No expiry on tiered namespaces in M4: live keys never expire.
        Resolved::Ram(_) | Resolved::Cold { .. } | Resolved::Extent { .. } => w.int(-1),
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
    let hash = shared.hasher.hash(key);
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
    match resolve(shared, ns, key, hash, PromoteOnCold::Read).await {
        Resolved::Miss => w.bulk(b""),
        Resolved::Ram(addr) => {
            let ks = shared.store.borrow();
            let table = ks.tiered_store(ns).expect("resolved on this table");
            slice_of(table.record(addr).value, &mut w);
        }
        Resolved::Cold { value, .. } | Resolved::Extent { value, .. } => slice_of(&value, &mut w),
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
    let Some(cursor) = crate::exec::parse_cursor(argv.arg(1)) else {
        return done_error(shared, proto, "ERR invalid cursor");
    };
    let mut count = 10usize;
    let mut i = 2;
    // The numbered-db grammar (`exec::scan`): an option without its value
    // is a syntax error (a lone trailing `COUNT` used to pass); the count
    // is bounded at 10 000 slots per call (the slice's allocation).
    while i < argv.len() {
        let opt = argv.arg(i);
        if opt.eq_ignore_ascii_case(b"COUNT") && i + 1 < argv.len() {
            match parse_i64(argv.arg(i + 1)) {
                Some(n) if n >= 1 => count = usize::try_from(n).unwrap_or(usize::MAX).min(10_000),
                Some(_) => return done_error(shared, proto, "ERR syntax error"),
                None => {
                    return done_error(
                        shared,
                        proto,
                        "ERR value is not an integer or out of range",
                    );
                }
            }
            i += 2;
        } else if (opt.eq_ignore_ascii_case(b"MATCH") || opt.eq_ignore_ascii_case(b"TYPE"))
            && i + 1 < argv.len()
        {
            // MATCH/TYPE filters are not wired on tiered namespaces yet.
            return done_error(shared, proto, ERR_UNSUPPORTED);
        } else {
            return done_error(shared, proto, "ERR syntax error");
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
    // Cold slots are named a **chunk** at a time (review of 2026-08-30,
    // F-L17-13 — L3): every intent of a chunk is in the cold FIFO before
    // the page suspends, which is ADR-0055 D1's premise — the drain
    // merges neighbouring windows (D4) and the device runs at queue depth
    // (D2) instead of one round trip and one reactor iteration per key.
    // The chunk is the engine's QD cap: the most the device takes at
    // once, and the most one page holds before it yields to other
    // connections. Naming a cold key is what a beyond-RAM enumeration
    // inherently costs (SCAN allows duplicates/races; the decoded key is
    // authoritative). A typed read failure fails the whole page — the
    // client retries its cursor — never a silently shorter page with an
    // advanced cursor (C2; the DBSIZE drain's rule).
    let chunk = {
        let tier = shared.tier.borrow();
        tier.as_ref().and_then(|t| t.cold.as_ref()).map_or(1, inf_runtime::ColdReads::qd_cap).max(1)
    };
    let fail = |shared: &Rc<Shared<O, F>>, message: &'static str| {
        if let Some(table) = shared.store.borrow_mut().tiered_store_mut(ns) {
            table.note_cold_read_error();
        }
        done_error(shared, proto, message)
    };
    let mut keys: Vec<Vec<u8>> = Vec::with_capacity(slots.len());
    let mut plans: Vec<ColdPlan> = Vec::with_capacity(chunk);
    let mut at = 0usize;
    while at < slots.len() {
        // Pass 1: RAM keys inline; cold intents enqueued, one chunk.
        while at < slots.len() && plans.len() < chunk {
            let (hash, addr) = slots[at];
            let ram_key = {
                let ks = shared.store.borrow();
                let Some(table) = ks.tiered_store(ns) else {
                    return done_error(shared, proto, "ERR the selected namespace was dropped");
                };
                match table.space().resolve(addr) {
                    inf_store::AddrClass::Cold => None,
                    _ => Some(table.record(addr).key.to_vec()),
                }
            };
            match ram_key {
                Some(key) => keys.push(key),
                None => match plan_key_fetch(shared, ns, hash, addr) {
                    Ok(KeyFetch::Planned(plan)) => plans.push(plan),
                    Ok(KeyFetch::Skip) => {}
                    // A full FIFO with intents of ours queued: drain them
                    // (their completions free the queue), then retry this
                    // slot. With nothing queued the page is refused typed,
                    // as before.
                    Ok(KeyFetch::Busy) if !plans.is_empty() => break,
                    Ok(KeyFetch::Busy) => return fail(shared, ERR_COLD_BUSY),
                    Err(message) => return fail(shared, message),
                },
            }
            at += 1;
        }
        // Pass 2: await in enqueue order — the awaited intent is never
        // behind another of ours in the FIFO, so a dry pool cannot wait
        // on a completion this page holds unpolled.
        for plan in plans.drain(..) {
            match decode_key(plan).await {
                Ok(Some(key)) => keys.push(key),
                Ok(None) => {}
                Err(message) => return fail(shared, message),
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

/// `DBSIZE` on a tiered namespace (ADR-0093 A3): exact under open
/// shadow tickets. `len()` is `index − open tickets`, which is a fact
/// only once every open ticket is verified same-key — so the command
/// first **drains** the unverified tickets: it raises the admission
/// fence (no new ticket while a drain runs, so the set only shrinks),
/// reads each unverified twin Foreground and verifies it (same key ⇒
/// verified, the `− 1` is right; collision ⇒ the ticket ends and the
/// slot counts as the other key it is), then answers. A twin that
/// cannot be read is a typed error — never an inexact integer.
/// Bounded by the ticket cap; no borrow is held across an await.
async fn dbsize<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    proto: Protocol,
) -> TieredReply {
    match dbsize_count(shared, ns, proto).await {
        Ok(len) => {
            let mut reply = shared.take_reply_buf();
            RespWriter::new(&mut reply, proto).int(len as i64);
            TieredReply::Done(reply)
        }
        Err(reply) => TieredReply::Done(reply),
    }
}

/// This cell's exact count for the tiered namespace — the [`dbsize`]
/// drain without the rendering, so a scattered `DBSIZE` (a
/// namespace-bound connection on a multi-cell node — the plane's
/// `Counted` shape) can sum typed contributions. `Err` is the typed
/// error reply, never a partial integer.
pub(super) async fn dbsize_count<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    proto: Protocol,
) -> Result<u64, Vec<u8>> {
    let snapshot = |shared: &Rc<Shared<O, F>>| -> Option<Vec<inf_store::ShadowTicket>> {
        let ks = shared.store.borrow();
        let table = ks.tiered_store(ns)?;
        Some(if table.shadow_unverified() == 0 {
            Vec::new()
        } else {
            table.shadow_unverified_tickets()
        })
    };
    let Some(pending) = snapshot(shared) else {
        return Err(error_bytes(shared, proto, "ERR the selected namespace was dropped"));
    };
    if !pending.is_empty() {
        // The fence brackets the drain — one raise, one lower, whatever
        // the drain answers (F-L13-09, review of 2026-08-30: three return
        // paths each lowered it). The await inside cannot strand a raised
        // fence: `CellExecutor` runs every task to completion (no
        // cancellation API), so a suspended drain always resumes here.
        let fence = |raise: bool| {
            if let Some(table) = shared.store.borrow_mut().tiered_store_mut(ns) {
                table.shadow_fence(raise);
            }
        };
        fence(true);
        let drained = drain_shadow_tickets(shared, ns, proto, pending, snapshot).await;
        fence(false);
        drained?;
    }
    let mut ks = shared.store.borrow_mut();
    match ks.tiered_store_mut(ns) {
        Some(table) => {
            if table.shadow_unverified() > 0 {
                // Unreachable under the fence unless a read raced a
                // retarget twice; say so rather than guess.
                return Err(error_bytes(
                    shared,
                    proto,
                    "ERR DBSIZE: shadow tickets still unverified after the drain",
                ));
            }
            Ok(table.len() as u64)
        }
        None => Ok(0),
    }
}

/// The fenced part of [`dbsize_count`]: verifies every open ticket by
/// its cold twin. Two passes at most: the fence stops new tickets, and a
/// ticket that moved under a concurrent overwrite is still verified by
/// its cold address — a second snapshot only catches a read the first
/// pass could not complete. `Err` is the typed reply; the caller lowers
/// the fence on every outcome.
async fn drain_shadow_tickets<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    proto: Protocol,
    mut pending: Vec<inf_store::ShadowTicket>,
    snapshot: impl Fn(&Rc<Shared<O, F>>) -> Option<Vec<inf_store::ShadowTicket>>,
) -> Result<(), Vec<u8>> {
    for _pass in 0..2 {
        if pending.is_empty() {
            break;
        }
        for ticket in pending.drain(..) {
            let image =
                read_cold_record(shared, ns, ticket.cold, inf_runtime::ReadClass::Foreground).await;
            let mut ks = shared.store.borrow_mut();
            let Some(table) = ks.tiered_store_mut(ns) else {
                return Err(error_bytes(shared, proto, "ERR the selected namespace was dropped"));
            };
            match image {
                Ok(image) => {
                    table.note_shadow_dbsize_read();
                    let _ = table.verify_shadow(ticket.hash, ticket.cold, &image);
                }
                Err(cause) => {
                    table.shadow_read_failed(ticket.cold);
                    let addr = ticket.cold.to_raw();
                    let mut reply = shared.take_reply_buf();
                    RespWriter::new(&mut reply, proto).error(&format!(
                        "ERR DBSIZE: shadow twin at {addr} unreadable ({cause}) — ADR-0093 A3"
                    ));
                    return Err(reply);
                }
            }
        }
        pending = snapshot(shared).unwrap_or_default();
    }
    Ok(())
}

/// A cold slot's key fetch, planned (SCAN key resolution).
enum KeyFetch {
    /// Enqueued; [`decode_key`] awaits and names the key.
    Planned(ColdPlan),
    /// The slot was displaced *and* re-indexed mid-scan (the SCAN
    /// contract's mutation case) — nothing to name.
    Skip,
    /// The cold FIFO refused the intent (`overflow_cap`, or the C2′ fault
    /// point) — the caller decides between draining and refusing.
    Busy,
}

/// Plans and enqueues the read that names the key at a cold slot. The
/// window is the frames covering the record's first
/// [`TieredTable::KEY_PREFIX_LEN`] bytes (header + TTL + the longest key
/// — `TieredTable::key_from_prefix`'s bound): one or two frames, so a
/// page's neighbouring windows merge at the drain instead of each
/// costing the pool buffer. `Err` is a typed cold-read failure the caller
/// must surface. The review of 2026-08-30 (C2, F-L07-05) found the
/// previous whole-record demand here silently omitted every cold value
/// past one window — and every read failure — while the cursor advanced.
fn plan_key_fetch<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    hash: u64,
    addr: LogicalAddr,
) -> Result<KeyFetch, &'static str> {
    let tier = shared.tier.borrow();
    let Some(t) = tier.as_ref().and_then(|t| t.ns(ns)) else {
        return Err("ERR the selected namespace was dropped (INF.NS USE again)");
    };
    let Some(cold) = tier.as_ref().and_then(|t| t.cold.clone()) else {
        return Err(ERR_COLD_IO);
    };
    match t.plan_cold_prefix(addr, TieredTable::KEY_PREFIX_LEN) {
        Some((fd, file, offset, frames, skip)) => {
            let bytes = frames as usize * inf_log::TIER_FRAME_BYTES;
            // Same-clock stamp as `on_completion` (the `cold_read_p99_us` pair).
            let now_us = shared.now.get().as_micros();
            // The BUSY leg's fault point (review of 2026-08-30, C2′):
            // a saturated queue fails the SCAN page typed.
            if inf_foundation::fault::fire(crate::fault::COLD_ENQUEUE_FULL) {
                return Ok(KeyFetch::Busy);
            }
            match cold.enqueue(fd, file, offset, bytes, inf_runtime::ReadClass::Foreground, now_us)
            {
                Ok(wait) => Ok(KeyFetch::Planned(ColdPlan { wait, addr, frames, skip })),
                Err(_) => Ok(KeyFetch::Busy),
            }
        }
        None => {
            // Outside every catalogued file: either the slot was displaced
            // and its file retired mid-scan (the index has moved on — a
            // legal mutation skip) or the index still names the pair (an
            // index/catalog inconsistency — say so, never drop the key).
            drop(tier);
            let ks = shared.store.borrow();
            let still = ks.tiered_store(ns).is_some_and(|t| t.contains_pair(hash, addr));
            if still { Err(ERR_COLD_IO) } else { Ok(KeyFetch::Skip) }
        }
    }
}

/// Awaits a planned key fetch and names the key. `Ok(None)` never
/// happens for a window planned by [`plan_key_fetch`] (it always covers
/// the key) and is refused typed if it does.
async fn decode_key(plan: ColdPlan) -> Result<Option<Vec<u8>>, &'static str> {
    let done = plan.wait.await;
    if done.outcome().is_err() {
        return Err(ERR_COLD_IO);
    }
    let key = done.bytes(|window| {
        let mut head = Vec::new();
        inf_log::tier_extract(window, plan.skip, TieredTable::RECORD_HEADER_LEN, &mut head).ok()?;
        let total = TieredTable::record_len_from_header(&head);
        let window_data = plan.frames as usize * inf_log::TIER_FRAME_DATA - plan.skip;
        let take = total.min(window_data);
        let mut prefix = Vec::new();
        inf_log::tier_extract(window, plan.skip, take, &mut prefix).ok()?;
        TieredTable::key_from_prefix(&prefix).map(<[u8]>::to_vec)
    });
    match key {
        Some(key) => Ok(Some(key)),
        None => {
            debug_assert!(false, "a key-prefix window always covers the record key");
            Err(ERR_COLD_IO)
        }
    }
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
            // Same-clock stamp as `on_completion` (the `cold_read_p99_us`
            // pair).
            cold.enqueue(
                fd,
                file,
                inf_log::blob::extent_frame_offset(first),
                frames as usize * inf_log::TIER_FRAME_BYTES,
                inf_runtime::ReadClass::Foreground,
                shared.now.get().as_micros(),
            )
            .ok()?
        };
        let done = wait.await;
        done.outcome().ok()?;
        // Extract into a scratch and extend — `tier_extract` REPLACES its
        // output's contents, so handing it the accumulator erased every
        // previously assembled window and this loop never terminated for
        // any extent needing more than one window (> 16,368 data bytes):
        // an unbounded foreground read spin with the connection's pump
        // held (review of 2026-09-01, N1 — found by the Group 0 widened
        // DST value generator; the sim livelocked on every seed).
        let ok = done.bytes(|window| {
            let take = remaining.min(frames as usize * inf_log::TIER_FRAME_DATA - skip);
            let mut piece = Vec::new();
            inf_log::tier_extract(window, skip, take, &mut piece)
                .is_ok()
                .then(|| out.extend_from_slice(&piece))
                .is_some()
        });
        drop(done);
        if !ok {
            return None;
        }
    }
    drop(reader); // the fd stayed open across every read
    Some(out)
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

/// Redis `string2ll` (`exec::parse_i64`): the numbered-db rule, so a
/// tiered namespace refuses `+5`, `007` and `-0` exactly as Redis does.
fn parse_i64(bytes: &[u8]) -> Option<i64> {
    crate::exec::parse_i64(bytes).ok()
}

/// The namespace's declared value cap (`BLOB-MAX`) — the bound every
/// grown post-image (`APPEND`, `SETRANGE`) is checked against before a
/// byte of it is built (F-L13-04).
fn blob_max(
    shared: &Rc<Shared<impl PlaneObserver + 'static, impl SegmentFs + Clone + 'static>>,
    ns: NsId,
) -> Option<u64> {
    let ks = shared.store.borrow();
    Some(ks.tiered_store(ns)?.blob_config().max_bytes)
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

#[cfg(test)]
mod twin_scratch_tests {
    use super::*;

    fn addr(i: u64) -> LogicalAddr {
        LogicalAddr::from_raw(i * 4096).expect("fits")
    }

    /// The scratch holds `DISPLACE_REGISTER_CAP - 1` twins and refuses
    /// the next (A13); `retain` compacts in place, in order.
    #[test]
    fn the_twin_scratch_is_bounded_by_the_register_and_retains_in_order() {
        let mut twins = TwinScratch::default();
        for i in 1..=TWIN_SCRATCH_CAP as u64 {
            assert!(twins.push(addr(i), i as usize), "slot {i} fits");
        }
        assert!(!twins.push(addr(99), 1), "one past the register refuses");
        assert_eq!(twins.len, TWIN_SCRATCH_CAP);
        assert!(twins.contains(addr(1)) && !twins.contains(addr(99)));
        twins.retain(|twin| twin != addr(2));
        assert_eq!(
            twins.iter().collect::<Vec<_>>(),
            (1..=TWIN_SCRATCH_CAP as u64)
                .filter(|i| *i != 2)
                .map(|i| (addr(i), i as usize))
                .collect::<Vec<_>>()
        );
        assert!(twins.push(addr(5), 5), "the freed slot is reusable");
        assert_eq!(std::mem::size_of::<TwinScratch>(), 24 * TWIN_SCRATCH_CAP + 8, "stack-only");
    }
}
