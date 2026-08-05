//! M4-S21 — disk-full admission behavior (ADR-0063): at disk budget or
//! device-full, new-tier-byte placements fail with the typed `DISKFULL`
//! refusal — fail writes, never corrupt, never silently drop (the
//! M1-S07 OOM-honesty pattern applied to disk); reads, deletes, expiry,
//! and in-place updates proceed, and compaction keeps working from the
//! 5% reserve (D3 — held open by asymmetry: `relocate` and flush are
//! never budget-refused).
//!
//! The plan's AC map onto this suite plus its companions:
//! - **taxonomy behavior per path** — budget leg here; the tier-flush
//!   and blob device legs here through the `*_nospace` fault points
//!   (the same points the crash-matrix rows kill under);
//! - **zero corruption** — the fill/churn tests verify every surviving
//!   key's bytes (RAM and cold alike) after refusal + reclaim +
//!   resume; the kill-and-recover legs live in `tests/crash-matrix`
//!   (`m4.toml` `tier_write_nospace` / `blob_write_nospace` rows) and
//!   the `m4-diskfull` DST scenario;
//! - **recovery after space frees is clean and automatic** — budget
//!   raise, compaction reclaim, and device-latch clearing are each
//!   proven to reopen admission with no operator step;
//! - **reads and compaction keep working at the cap** — asserted at
//!   the refusal point, not after it.

use std::collections::BTreeMap;
use std::path::Path;

use inf_foundation::fault::{self, FaultSpec};
use inf_log::blob::{ExtentId, ExtentWriter};
use inf_log::flush::{TierFileMeta, unlink_tier_file};
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    MutationEffect, NsId, StagingConfig, StagingRing, TIER_FRAME_BYTES, TierFlush, TierFlushConfig,
    TierIoMode, tier_extract, tier_frame_offset, tier_frame_span,
};
use inf_store::{
    AddressSpaceConfig, CompactionConfig, CompactionWork, DemotionConfig, DiskFullCause,
    LogicalAddr, OpError, TieredTable,
};

const NS: NsId = NsId(61);
const SHARD: &str = "shard-0";
const PAGE: u64 = 4 << 10;
/// 10% mutable window so inline churn seals at test scale (the
/// `tiered_blob_reclaim.rs` rationale; the knob is config, ADR-0053 D2).
const MUTABLE_PERMILLE: u32 = 100;
const MEM_BUDGET: u64 = 4 << 20;
const DISK_BUDGET: u64 = 16 << 20;

#[derive(Clone, Debug)]
struct Entry {
    addr: u64,
    len: usize,
    version: u32,
    generation: u64,
}

/// Inline-record churn rig on `MemFs` with the MAINTAIN round the plane
/// will run (ADR-0063 D2 cadence): demotion legs → one flush slice →
/// release → reclaim → **admission refresh** — the one point where both
/// halves of `disk_used` are simultaneously fresh.
struct Rig {
    fs: MemFs,
    table: TieredTable,
    flush: TierFlush<MemFs>,
    ring: StagingRing,
    model: BTreeMap<u64, Entry>,
    ckpt_id: u64,
}

fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

impl Rig {
    fn new(disk_budget: u64) -> Rig {
        // A 256 KiB MAINTAIN quantum: sealing keeps pace with the fill,
        // so `tail − flushed` stays near the mutable target instead of
        // ballooning to the whole window — the admission projection then
        // closes *after* the 7/8 pressure arm, the flagship ordering
        // (a 4 KiB quantum starves sealing and inverts it — that is a
        // driver-pacing artifact, not an admission property).
        let demote = DemotionConfig {
            mem_budget_bytes: MEM_BUDGET,
            mutable_permille: MUTABLE_PERMILLE,
            slice_bytes: 256 << 10,
        };
        let mut table = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            2048,
        )
        .expect("ring");
        table.set_compaction_config(CompactionConfig { dead_ratio_pct: 50, slice_bytes: 1 << 20 });
        table.set_disk_budget(disk_budget);
        let fs = MemFs::new();
        let flush = TierFlush::new(
            fs.clone(),
            TierFlushConfig {
                shard_dir: Path::new(SHARD).join(format!("ns-{}", NS.0)),
                cell: 0,
                ns: NS,
                mode: TierIoMode::Buffered,
                file_capacity: 256 << 10,
                slice_bytes: 1 << 20,
            },
            0,
        );
        Rig {
            fs,
            table,
            flush,
            ring: StagingRing::new(StagingConfig::default()),
            model: BTreeMap::new(),
            ckpt_id: 0,
        }
    }

    fn key(id: u64) -> Vec<u8> {
        format!("k:{id:06}").into_bytes()
    }

    /// Deterministic value for (key id, generation) — regenerable for
    /// the content oracle, never held per key.
    fn value_for(id: u64, generation: u64) -> Vec<u8> {
        let len = 3072 + ((id.wrapping_mul(31) ^ generation) % 64) as usize;
        (0..len).map(|i| (i as u64 ^ id.wrapping_mul(7) ^ generation) as u8).collect()
    }

    fn stage(&mut self, effect: &MutationEffect<'_>) {
        if self.table.stage_wal(&mut self.ring, effect).is_err() {
            self.ring = StagingRing::new(StagingConfig::default());
            self.table.stage_wal(&mut self.ring, effect).expect("a fresh ring has room");
        }
    }

    /// SET through the routed entry (insert or overwrite), staging the
    /// WAL record first (the M2 order). `Err` propagates the refusal —
    /// the caller decides whether it expected one.
    fn set(&mut self, id: u64, generation: u64) -> Result<(), OpError> {
        let key = Self::key(id);
        let hash = TieredTable::hash_key(&key);
        let value = Self::value_for(id, generation);
        let old = self.model.get(&id).cloned();
        if let Some(o) = &old {
            let addr = LogicalAddr::from_raw(o.addr).expect("48-bit");
            let _ = self.table.take_displacement_origins(hash, addr);
        }
        self.stage(&MutationEffect::StringSet { ns: NS, key: &key, value: &value });
        let placed = match &old {
            Some(o) => self.table.update(
                &key,
                &value,
                hash,
                LogicalAddr::from_raw(o.addr).expect("48-bit"),
                o.len,
                o.version,
            )?,
            None => self.table.insert(&key, &value, hash)?,
        };
        let parts = self.table.record(placed);
        self.model.insert(
            id,
            Entry {
                addr: placed.to_raw(),
                len: parts.encoded_len,
                version: parts.version,
                generation,
            },
        );
        Ok(())
    }

    fn del(&mut self, id: u64) {
        let Some(entry) = self.model.remove(&id) else { return };
        let key = Self::key(id);
        let hash = TieredTable::hash_key(&key);
        let addr = LogicalAddr::from_raw(entry.addr).expect("48-bit");
        let _ = self.table.take_displacement_origins(hash, addr);
        self.stage(&MutationEffect::Delete { ns: NS, key: &key });
        self.table.delete(hash, addr, entry.len);
    }

    /// One MAINTAIN round in the ADR-0063 D2 shape: the demote legs run
    /// to their due-target (drivers loop on slice returns, never on
    /// `demote_due` — the S07 contract), then the admission refresh at
    /// the point where both usage halves are fresh. A single-slice
    /// round would freeze the initial mutable-window transient forever
    /// (each leg's quantum equals the insert refill rate) — a pacing
    /// artifact, not an admission property.
    fn maintain_round(&mut self) -> u64 {
        let mut total = 0u64;
        for _ in 0..64 {
            let sealed = self.table.seal_slice();
            let f = self.table.flush_slice(&mut self.flush).expect("flush slice");
            let released = self.table.release_slice();
            let step = sealed + released + f.appended_bytes + u64::from(f.gaps_crossed);
            total += step;
            if step == 0 || !self.table.demote_due() {
                break;
            }
        }
        self.table.refresh_disk_admission(self.flush.disk_bytes());
        total
    }

    /// Seal → flush → release → refresh to quiescence.
    fn drain(&mut self) {
        while self.maintain_round() > 0 {}
    }

    /// One bounded compaction leg (pressure-armed — the disk-budget
    /// consumer under test). Returns relocated bytes.
    fn compact_round(&mut self, budget: u64, pressure: bool) -> u64 {
        let mut spent = 0u64;
        let mut relocated = 0u64;
        while spent < budget {
            let work = self.table.compaction_work(&self.flush, pressure, budget - spent);
            let CompactionWork::Read { file_id, addr, len } = work else { break };
            let Some(bytes) = self.read_file_chunk(file_id, addr, len) else { break };
            let applied = self.table.compaction_apply(file_id, addr, &bytes);
            spent += applied.consumed.max(applied.need).max(1);
            relocated += applied.relocated_bytes;
            if applied.stalled {
                break;
            }
        }
        if relocated > 0 {
            self.refresh_model_addresses();
        }
        relocated
    }

    /// The S15 retirement pipeline (walk stamp → retire scan → manifest
    /// exclusion → commit + detach + unlink). Returns files retired.
    fn publish_cycle(&mut self) -> usize {
        self.ckpt_id += 1;
        self.table.begin_ckpt_walk(self.ckpt_id);
        self.table.end_ckpt_walk();
        self.table.retire_scan(self.ckpt_id, &self.flush);
        let _section = self.table.tier_manifest(NS.0, &self.flush);
        let ids = self.table.commit_retirement();
        for &id in &ids {
            let meta = self.flush.detach_sealed(id).expect("retired files are sealed");
            unlink_tier_file(&self.fs, &meta).expect("unlink");
        }
        ids.len()
    }

    /// Relocations moved records: re-resolve every model address through
    /// the index (the exact-pair discipline — a live key always resolves).
    fn refresh_model_addresses(&mut self) {
        for (id, entry) in &mut self.model {
            let key = Self::key(*id);
            let hash = TieredTable::hash_key(&key);
            match self.table.lookup(&key, hash, &[]) {
                inf_store::TieredLookup::Ram(addr) | inf_store::TieredLookup::Cold(addr) => {
                    entry.addr = addr.to_raw();
                }
                inf_store::TieredLookup::Miss => panic!("live key {id} lost across relocation"),
            }
        }
    }

    /// Reads `len` record bytes at `addr` out of one sealed tier file
    /// (the `tiered_blob_reclaim.rs` synchronous cold-read shape).
    fn read_span(&self, meta: &TierFileMeta, addr: u64, len: usize) -> Option<Vec<u8>> {
        let file = self.fs.open_tier(&meta.path, TierIoMode::Buffered).ok()?;
        let (first, count, skip) = tier_frame_span(addr - meta.base.to_raw(), len);
        let from = tier_frame_offset(first);
        let mut frames = vec![0u8; count as usize * TIER_FRAME_BYTES];
        let mut done = 0usize;
        while done < frames.len() {
            let n = file.read_at(from + done as u64, &mut frames[done..]).ok()?;
            if n == 0 {
                return None;
            }
            done += n;
        }
        let mut out = Vec::new();
        tier_extract(&frames, skip, len, &mut out).ok()?;
        Some(out)
    }

    /// A compaction scan chunk, addressed by file id.
    fn read_file_chunk(&self, file_id: u32, addr: LogicalAddr, len: u64) -> Option<Vec<u8>> {
        let meta = self.flush.sealed().iter().find(|m| m.id == file_id)?.clone();
        self.read_span(&meta, addr.to_raw(), usize::try_from(len).expect("fits"))
    }

    /// A verification read, addressed by containment.
    fn read_cold(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
        let meta = self
            .flush
            .sealed()
            .iter()
            .find(|m| addr >= m.base.to_raw() && addr + len as u64 <= m.base.to_raw() + m.data_len)?
            .clone();
        self.read_span(&meta, addr, len)
    }

    /// The zero-corruption content oracle: every model key resolves and
    /// its bytes — RAM or cold — decode to exactly the modeled value.
    /// Call after a final drain so every cold address sits in a sealed
    /// file (`flush_drain` seals the active file).
    fn assert_content(&mut self) {
        self.table.flush_drain(&mut self.flush).expect("drain");
        self.table.refresh_disk_admission(self.flush.disk_bytes());
        let model: Vec<(u64, Entry)> = self.model.iter().map(|(k, v)| (*k, v.clone())).collect();
        for (id, entry) in model {
            let key = Self::key(id);
            let hash = TieredTable::hash_key(&key);
            let expect = Self::value_for(id, entry.generation);
            match self.table.lookup(&key, hash, &[]) {
                inf_store::TieredLookup::Ram(addr) => {
                    let parts = self.table.record(addr);
                    assert_eq!(parts.key, &key[..], "key {id}");
                    assert_eq!(parts.value, &expect[..], "RAM value bytes for key {id}");
                }
                inf_store::TieredLookup::Cold(addr) => {
                    let bytes = self
                        .read_cold(addr.to_raw(), entry.len)
                        .unwrap_or_else(|| panic!("cold read for key {id}"));
                    let parts = TieredTable::decode_record(&bytes);
                    assert_eq!(parts.key, &key[..], "key {id}");
                    assert_eq!(parts.value, &expect[..], "cold value bytes for key {id}");
                }
                inf_store::TieredLookup::Miss => panic!("live key {id} lost"),
            }
        }
    }

    /// Fills fresh keys until the admission refuses, running the
    /// MAINTAIN round between memory-refused attempts. Returns
    /// `(next unused id, the refusal)`.
    fn fill_to_refusal(&mut self, from: u64) -> (u64, OpError) {
        let mut id = from;
        loop {
            match self.set(id, 1) {
                Ok(()) => id += 1,
                Err(OpError::OutOfMemory) => {
                    assert!(self.maintain_round() > 0, "the fill must drain");
                }
                Err(e @ OpError::DiskFull(_)) => return (id, e),
                Err(e) => panic!("unexpected refusal {e:?}"),
            }
            assert!(id < 100_000, "the disk budget never bound");
        }
    }
}

/// The recovery-safety contract (ADR-0063 D2): admission is open until
/// the plane's first refresh — recovery re-appends replay bytes the
/// prior life already admitted, and refusing them would turn a full
/// disk into a boot failure. The first refresh closes the verdict.
#[test]
fn admission_is_open_until_the_first_refresh() {
    let mut rig = Rig::new(1 << 20); // 1 MiB — absurdly small on purpose
    // Far past the admit limit, with no refresh: every placement admits
    // (the replay-shaped phase). 6 MiB through a 4 MiB window forces
    // real flushes, so tier-file bytes alone exceed the whole budget.
    let mut id = 0u64;
    let mut placed = 0u64;
    while placed < (6 << 20) {
        match rig.set(id, 1) {
            Ok(()) => {
                placed += Rig::value_for(id, 1).len() as u64;
                id += 1;
            }
            Err(OpError::OutOfMemory) => {
                // Pre-refresh MAINTAIN legs, deliberately without the
                // admission refresh (the replay driver does not run it).
                rig.table.seal_slice();
                rig.table.flush_slice(&mut rig.flush).expect("flush");
                rig.table.release_slice();
            }
            Err(e) => panic!("pre-refresh placement refused: {e:?}"),
        }
    }
    assert!(rig.table.disk_full().is_none(), "no verdict before the first refresh");
    // The first refresh sees usage far past the budget and closes.
    rig.table.refresh_disk_admission(rig.flush.disk_bytes());
    let err = rig.set(id, 1).expect_err("closed after the first refresh");
    let OpError::DiskFull(DiskFullCause::Budget { used, budget }) = err else {
        panic!("expected the budget cause, got {err:?}");
    };
    assert_eq!(budget, 1 << 20);
    assert!(used > budget, "the snapshot names the real usage ({used} > {budget})");
}

/// The core cap behavior (ADR-0063 D1/D2): the refusal is typed with
/// the snapshot numbers and mutates nothing; reads, deletes, and
/// in-place updates proceed at the cap; `disk_used` never exceeds the
/// budget at any observation point; the observables count.
#[test]
fn budget_cap_refuses_typed_and_nonconsuming_ops_proceed() {
    let mut rig = Rig::new(DISK_BUDGET);
    rig.table.refresh_disk_admission(rig.flush.disk_bytes());
    let (next_id, refusal) = rig.fill_to_refusal(0);
    let OpError::DiskFull(DiskFullCause::Budget { used, budget }) = refusal else {
        panic!("expected the budget cause, got {refusal:?}");
    };
    assert_eq!(budget, DISK_BUDGET);
    assert!(used <= DISK_BUDGET, "the enforced snapshot respects the cap ({used})");
    // The budget held at every observation point of the fill: the final
    // usage is the proof aggregate (per-round asserts ride the loop's
    // refresh — disk_used is exactly what refresh snapshotted).
    let disk_used = rig.table.disk_used(rig.flush.disk_bytes());
    assert!(disk_used <= DISK_BUDGET, "disk_used {disk_used} exceeded the budget {DISK_BUDGET}");
    assert!(rig.table.diskfull_refusals() >= 1, "the refusal counted");
    assert!(
        matches!(rig.table.disk_full(), Some(DiskFullCause::Budget { .. })),
        "the verdict reads budget-closed"
    );

    // Refusal mutates nothing: the tail, live bytes, and user-byte
    // accounting are exactly what they were before another attempt.
    let tail = rig.table.space().tail().to_raw();
    let live = rig.table.live_bytes();
    let user = rig.table.write_accounting().user_bytes;
    let key = Rig::key(next_id + 1);
    let hash = TieredTable::hash_key(&key);
    let value = Rig::value_for(next_id + 1, 1);
    assert!(matches!(
        rig.table.insert(&key, &value, hash),
        Err(OpError::DiskFull(DiskFullCause::Budget { .. }))
    ));
    assert_eq!(rig.table.space().tail().to_raw(), tail, "refusal moved the tail");
    assert_eq!(rig.table.live_bytes(), live, "refusal changed live bytes");
    assert_eq!(rig.table.write_accounting().user_bytes, user, "refusal charged user bytes");

    // Reads serve at the cap — the oldest key is cold by now, the
    // newest is RAM; both resolve.
    let first = Rig::key(0);
    let h_first = TieredTable::hash_key(&first);
    assert!(
        matches!(rig.table.lookup(&first, h_first, &[]), inf_store::TieredLookup::Cold(_)),
        "the oldest key reads cold — and still resolves at the cap"
    );
    let last = Rig::key(next_id - 1);
    let h_last = TieredTable::hash_key(&last);
    assert!(
        !matches!(rig.table.lookup(&last, h_last, &[]), inf_store::TieredLookup::Miss),
        "the newest key resolves at the cap"
    );

    // Freeing ops proceed: DEL is index + accounting only.
    rig.del(0);
    assert!(
        matches!(rig.table.lookup(&first, h_first, &[]), inf_store::TieredLookup::Miss),
        "the delete applied at the cap"
    );

    // The in-place arm proceeds: a same-length rewrite of a mutable
    // record consumes no new tier byte (D1's refusal scope, verbatim —
    // same (id, generation) ⇒ same encoded length ⇒ the routed `update`
    // takes its in-place branch above the ro-boundary).
    let hot = next_id - 1;
    let addr_before = rig.model.get(&hot).expect("model").addr;
    rig.set(hot, 1).expect("the in-place arm is not gated");
    assert_eq!(rig.model.get(&hot).expect("model").addr, addr_before, "rewritten in place");

    // The content oracle: nothing the refusals touched corrupted a byte.
    rig.assert_content();
}

/// Ordering (ADR-0063 D3): compaction's pressure input is active no
/// later than the first refusal — the composed signal (the ADR-0062
/// 7/8 materialized arm **or** admission closed) is what the plane
/// feeds `compaction_work`, so the engine is reclaiming by the time it
/// refuses. The materialized arm alone lags: the admission projection
/// sees unflushed RAM bytes the 7/8 arm cannot, and closes first —
/// flush catch-up then drives the 7/8 arm over its threshold too.
#[test]
fn compaction_pressure_is_active_no_later_than_the_first_refusal() {
    let mut rig = Rig::new(DISK_BUDGET);
    rig.table.refresh_disk_admission(rig.flush.disk_bytes());
    let (_, refusal) = rig.fill_to_refusal(0);
    assert!(matches!(refusal, OpError::DiskFull(DiskFullCause::Budget { .. })), "{refusal:?}");
    assert!(
        rig.table.compaction_pressure(rig.flush.disk_bytes()),
        "the composed pressure input is active at the first refusal"
    );
    // With inserts stopped, MAINTAIN drains the window: the material
    // usage crosses 7/8 of budget and the ADR-0062 arm fires on its
    // own — the projection closed admission *early*, never instead.
    rig.drain();
    assert!(
        rig.table.disk_pressure(rig.flush.disk_bytes()),
        "the materialized 7/8 arm engages as the window flushes (used {} of {})",
        rig.table.disk_used(rig.flush.disk_bytes()),
        DISK_BUDGET,
    );
    let disk_used = rig.table.disk_used(rig.flush.disk_bytes());
    assert!(disk_used <= DISK_BUDGET, "the cap held through the drain ({disk_used})");
}

/// Recovery leg 1 (ADR-0063 D5): a budget raise reopens admission on
/// the spot (`set_disk_budget` recomputes against the standing
/// snapshot); a budget cut closes it the same way.
#[test]
fn budget_reload_recomputes_admission_immediately() {
    let mut rig = Rig::new(DISK_BUDGET);
    rig.table.refresh_disk_admission(rig.flush.disk_bytes());
    let (next_id, _) = rig.fill_to_refusal(0);
    rig.table.set_disk_budget(4 * DISK_BUDGET);
    rig.set(next_id, 1).expect("the raise reopened admission — no operator step, no restart");
    rig.table.set_disk_budget(DISK_BUDGET / 4);
    assert!(
        matches!(rig.set(next_id + 1, 1), Err(OpError::DiskFull(DiskFullCause::Budget { .. }))),
        "the cut closed admission against the standing snapshot"
    );
}

/// Recovery leg 2 + the reserve asymmetry (ADR-0063 D3/D5): at the cap
/// with dead cold bytes, compaction relocates into the reserve while
/// foreground still refuses, retirement returns the space, and
/// admission resumes automatically — no operator step. Content-verified.
#[test]
fn compaction_reclaims_into_the_reserve_and_admission_resumes() {
    let mut rig = Rig::new(DISK_BUDGET);
    rig.table.refresh_disk_admission(rig.flush.disk_bytes());
    // Churn fill: overwrite earlier keys as later ones land, so sealed
    // cold files carry ≥ 50% dead bytes (ADR-0062 D9: survivors stay
    // interleaved or the compaction leg is vacuous).
    let mut id = 0u64;
    let mut rng = 0x5EED_D15Cu64;
    loop {
        let write = if id >= 8 && !seeded(&mut rng).is_multiple_of(4) {
            // 3-of-4 rewrites of an existing key (its old copy dies)…
            rig.set(seeded(&mut rng) % id, 2 + (seeded(&mut rng) % 8))
        } else {
            // …1-of-4 fresh keys (survivors interleave).
            let r = rig.set(id, 1);
            if r.is_ok() {
                id += 1;
            }
            r
        };
        match write {
            Ok(()) => {}
            Err(OpError::OutOfMemory) => {
                assert!(rig.maintain_round() > 0, "the fill must drain");
            }
            Err(OpError::DiskFull(_)) => break,
            Err(e) => panic!("unexpected refusal {e:?}"),
        }
        assert!(id < 100_000, "the disk budget never bound");
    }

    // At the cap: foreground refuses, compaction relocates — the
    // asymmetry is the reserve (D3). Then retirement + unlink return
    // the space and the refresh reopens admission, automatically.
    let mut relocated_while_refusing = 0u64;
    let mut reopened = false;
    for _ in 0..256 {
        let refusing = rig.table.disk_full().is_some();
        let relocated = rig.compact_round(1 << 20, true);
        if refusing {
            relocated_while_refusing += relocated;
        }
        rig.publish_cycle();
        rig.drain();
        if rig.table.disk_full().is_none() {
            reopened = true;
            break;
        }
    }
    assert!(
        relocated_while_refusing > 0,
        "compaction must keep working while foreground refuses — the reserve exists \
         for exactly this"
    );
    assert!(reopened, "reclaim must reopen admission with no operator step");
    let disk_used = rig.table.disk_used(rig.flush.disk_bytes());
    assert!(disk_used <= DISK_BUDGET, "usage {disk_used} exceeded the budget after reclaim");
    let next = 200_000;
    rig.set(next, 1).expect("admission resumed");
    // Zero corruption across refusal + compaction + retirement + resume.
    rig.assert_content();
}

/// The alarm (ADR-0063 D5): pressure with a tier full of *live* data is
/// not compactable — `nothing_compactable` counts instead of pretending
/// (rewriting live bytes reclaims nothing).
#[test]
fn pressure_with_nothing_compactable_counts_the_alarm() {
    let mut rig = Rig::new(DISK_BUDGET);
    rig.table.refresh_disk_admission(rig.flush.disk_bytes());
    // All-live fill (no overwrites, no deletes) to the refusal point:
    // the composed pressure input is active (admission arm), and there
    // is not one dead byte to reclaim.
    let (_, _refusal) = rig.fill_to_refusal(0);
    assert!(rig.table.compaction_pressure(rig.flush.disk_bytes()), "the fill sits under pressure");
    assert_eq!(rig.table.compact_idle_pressure(), 0);
    assert!(matches!(rig.table.compaction_work(&rig.flush, true, 1 << 20), CompactionWork::Idle));
    assert_eq!(rig.table.compact_idle_pressure(), 1, "the blind spot is counted, not papered over");
    assert!(matches!(rig.table.compaction_work(&rig.flush, true, 1 << 20), CompactionWork::Idle));
    assert_eq!(rig.table.compact_idle_pressure(), 2);
}

/// The device leg (ADR-0063 D4): a tier-flush write refused with ENOSPC
/// latches foreground admission (`DISKFULL` instead of an opaque
/// stall), the watermark freezes where the last barrier left it, and
/// the next successful flush — MAINTAIN retrying its backlog after
/// space frees — clears the latch. Budget 0: the device leg is
/// independent of any configured budget.
#[test]
fn device_enospc_latches_foreground_and_clears_on_successful_flush() {
    fault::disarm_all();
    let mut rig = Rig::new(0);
    rig.table.refresh_disk_admission(rig.flush.disk_bytes());
    // Fill the mutable window so a flush has work.
    let mut id = 0u64;
    while rig.set(id, 1).is_ok() {
        id += 1;
        if id > 4096 {
            break;
        }
    }
    rig.table.seal_slice();

    // The disk fills: every tier write refuses ENOSPC from here on.
    fault::arm(inf_log::fault::TIER_WRITE_NOSPACE, FaultSpec::Always);
    let flushed_before = rig.table.space().flushed().to_raw();
    let err = rig.table.flush_slice(&mut rig.flush).expect_err("the slice fails typed");
    assert!(err.is_storage_full(), "classified as space exhaustion: {err}");
    assert!(!err.is_fatal(), "write-time ENOSPC is the graceful leg (M2 precedent)");
    assert_eq!(
        rig.table.space().flushed().to_raw(),
        flushed_before,
        "the watermark froze where the last good barrier left it"
    );
    assert_eq!(rig.table.disk_full(), Some(DiskFullCause::Device), "the latch is set");
    let refusal = rig.set(id + 1, 1).expect_err("foreground refuses while latched");
    assert!(matches!(refusal, OpError::DiskFull(DiskFullCause::Device)), "{refusal:?}");
    // Reads and deletes proceed under the latch.
    let first = Rig::key(0);
    let h = TieredTable::hash_key(&first);
    assert!(!matches!(rig.table.lookup(&first, h, &[]), inf_store::TieredLookup::Miss));
    rig.del(0);

    // Space frees; MAINTAIN retries the same backlog and the successful
    // barrier clears the latch — recovery with no operator step.
    fault::disarm(inf_log::fault::TIER_WRITE_NOSPACE);
    // The failed barrier left every appended byte *staged* (retained
    // batch, rewound cursor — ADR-0063 D4): the retry's latch-probe
    // barrier rewrites the retained frames at their own offsets and
    // clears the latch — recovery in one MAINTAIN round, no operator
    // step. (`confirmed_bytes` may still read 0 here: a chunk end
    // inside a partial frame confirms at the next seal — the standing
    // ADR-0056 D5 holdback, not a recovery property. The content
    // oracle below is the durability proof: it drains, seals, and
    // re-reads every byte from the sealed files.)
    let _ = rig.table.flush_slice(&mut rig.flush).expect("the retry succeeds");
    assert!(rig.table.disk_full().is_none(), "the latch cleared");
    // The window drains over ordinary rounds and admission has resumed.
    let mut resumed = false;
    for _ in 0..64 {
        match rig.set(id + 2, 1) {
            Ok(()) => {
                resumed = true;
                break;
            }
            Err(OpError::OutOfMemory) => {
                rig.maintain_round();
            }
            Err(e) => panic!("resume refused: {e:?}"),
        }
    }
    assert!(resumed, "admission resumed after the latch cleared");
    rig.assert_content();
}

/// The blob leg (ADR-0063 D4): extent-write ENOSPC is a per-op typed
/// abort — never a latch (a latch would refuse the very attempts that
/// are its only recovery probe). The store's admission state is
/// untouched; the next attempt succeeds once space frees.
#[test]
fn blob_write_enospc_is_per_op_typed_and_self_heals() {
    fault::disarm_all();
    let mut rig = Rig::new(0);
    rig.table.refresh_disk_admission(rig.flush.disk_bytes());
    let shard = Path::new(SHARD).join(format!("ns-{}", NS.0));
    // Under one CRC frame of data: `append_chunk` only stages, so the
    // refused device write is `finish`'s tail frame — the path that
    // surfaces the *typed* [`ExtentWriteFailure::Write`] the classifier
    // rides (a full-frame refusal surfaces the same `StorageFull` kind
    // from `append_chunk` as a raw I/O error).
    let value = vec![0x42u8; 1 << 10];

    fault::arm(inf_log::fault::BLOB_WRITE_NOSPACE, FaultSpec::Always);
    let extent_id = ExtentId(rig.table.allocate_extent_id());
    let mut w = ExtentWriter::create(
        &rig.fs,
        &shard,
        extent_id,
        0,
        NS,
        value.len() as u64,
        TierIoMode::Buffered,
    )
    .expect("create precedes the data write");
    w.append_chunk(&value).expect("a sub-frame chunk only stages");
    let err = w.finish().expect_err("the extent write refuses at the tail frame");
    assert!(err.is_storage_full(), "classified as space exhaustion: {err}");
    // Per-op refusal, no latch: inline placements are unaffected.
    assert!(rig.table.disk_full().is_none(), "no latch on the blob leg");
    rig.set(0, 1).expect("inline placement unaffected");

    // Space frees: the next attempt is its own recovery probe. The
    // failed id stays quarantined (allocate-once); a fresh one lands.
    fault::disarm(inf_log::fault::BLOB_WRITE_NOSPACE);
    let extent_id = ExtentId(rig.table.allocate_extent_id());
    let mut w = ExtentWriter::create(
        &rig.fs,
        &shard,
        extent_id,
        0,
        NS,
        value.len() as u64,
        TierIoMode::Buffered,
    )
    .expect("create");
    w.append_chunk(&value).expect("the retry writes");
    let sealed = w.finish().expect("and seals");
    rig.table.note_blob_bytes(sealed.device_bytes());
    rig.stage(&MutationEffect::StringSetExtent {
        ns: NS,
        key: b"blob",
        extent_id: sealed.extent_id().0,
        offset: 0,
        len: sealed.data_len(),
    });
    rig.table
        .insert_extent(b"blob", TieredTable::hash_key(b"blob"), &sealed)
        .expect("the reference lands");
}

/// The budget leg bounds blobs too (ADR-0063 D2): `append_extent`
/// debits the extent's device bytes, so a blob landing past the admit
/// limit refuses typed — after the extent was written (the wiring-time
/// gate consults `disk_full()` *before* `ExtentWriter::create`; here
/// the orphan sweep owns the already-written file, ADR-0061 D6).
#[test]
fn blob_placement_respects_the_disk_budget() {
    fault::disarm_all();
    let mut rig = Rig::new(1 << 20);
    let shard = Path::new(SHARD).join(format!("ns-{}", NS.0));
    // Close the verdict with a refresh against real usage first.
    rig.table.refresh_disk_admission(rig.flush.disk_bytes());
    let value = vec![0x51u8; 2 << 20]; // one extent larger than the whole budget
    let extent_id = ExtentId(rig.table.allocate_extent_id());
    let mut w = ExtentWriter::create(
        &rig.fs,
        &shard,
        extent_id,
        0,
        NS,
        value.len() as u64,
        TierIoMode::Buffered,
    )
    .expect("create");
    w.append_chunk(&value).expect("chunk");
    let sealed = w.finish().expect("finish");
    let err = rig
        .table
        .insert_extent(b"big", TieredTable::hash_key(b"big"), &sealed)
        .expect_err("the reference refuses past the admit limit");
    assert!(matches!(err, OpError::DiskFull(DiskFullCause::Budget { .. })), "{err:?}");
    assert_eq!(rig.table.extent_stats().live, 0, "no reference registered on refusal");
}
