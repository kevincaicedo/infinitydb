//! Shared fixture for the `tiered_*` integration tests (review
//! 2026-08-30 L18 R13, batch 65): the page/budget shape, the xorshift
//! key generator, the flush and address-space configs, the MAINTAIN
//! drain loop and the tier-file cold reader each binary used to carry
//! as its own copy. Every test keeps its own `Rig` — the scenarios
//! differ — and delegates these to here.
#![allow(dead_code, reason = "four test binaries each use a subset of this fixture")]

use std::path::Path;

use inf_log::fs::mem::MemFs;
use inf_log::{
    NsId, TIER_FRAME_BYTES, TierFlush, TierFlushConfig, TierIoMode, tier_extract,
    tier_frame_offset, tier_frame_span,
};
use inf_store::{AddressSpaceConfig, DemotionConfig, LogicalAddr, TieredTable};

pub const PAGE: u64 = 4 << 10;
pub const BUDGET: u64 = 1 << 20;
pub const SHARD: &str = "shard-0";

/// xorshift64 — the tests' deterministic key stream.
pub fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

pub fn flush_config(ns: NsId, file_capacity: u64) -> TierFlushConfig {
    TierFlushConfig {
        shard_dir: Path::new(SHARD).to_path_buf(),
        cell: 0,
        ns,
        mode: TierIoMode::Buffered,
        file_capacity,
        slice_bytes: PAGE,
    }
}

pub fn space_config(demote: DemotionConfig, origin: u64) -> AddressSpaceConfig {
    AddressSpaceConfig {
        reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
        page_bytes: PAGE as usize,
        life_origin: LogicalAddr::from_raw(origin).expect("48-bit"),
    }
}

/// One MAINTAIN drain: seal / flush / release until a round makes no
/// progress (the reactor spreads these over iterations).
pub fn maintain(table: &mut TieredTable, flush: &mut TierFlush<MemFs>) {
    loop {
        let sealed = table.seal_slice();
        let f = table.flush_slice(flush).expect("flush slice");
        let released = table.release_slice();
        if sealed + released + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
            break;
        }
    }
}

/// Reads one cold record straight from the tier-file bytes the pipeline
/// wrote (the audit's cold path — no table access): sealed files first,
/// then the active file up to its durable length.
pub fn read_cold(flush: &TierFlush<MemFs>, fs: &MemFs, addr: u64, len: usize) -> Option<Vec<u8>> {
    let contains = |base: u64, flen: u64| addr >= base && addr + len as u64 <= base + flen;
    let (base, path) = flush
        .sealed()
        .iter()
        .find(|m| contains(m.base.to_raw(), m.data_len))
        .map(|m| (m.base.to_raw(), m.path.clone()))
        .or_else(|| {
            let (_, base, _, durable_len, path) = flush.active()?;
            contains(base.to_raw(), durable_len).then(|| (base.to_raw(), path.to_path_buf()))
        })?;
    let image = fs.contents(&path)?;
    let (first, count, skip) = tier_frame_span(addr - base, len);
    let from = tier_frame_offset(first) as usize;
    let to = from + count as usize * TIER_FRAME_BYTES;
    let mut out = Vec::new();
    tier_extract(image.get(from..to)?, skip, len, &mut out).ok()?;
    Some(out)
}
