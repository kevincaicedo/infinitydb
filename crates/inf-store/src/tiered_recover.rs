//! Tiered-namespace boot recovery (M4-S12, ADR-0057 D6): consume one
//! MANIFEST v2 tier section and hand back a serving-ready pair — the
//! table (new life at the manifested flushed watermark) and the flush
//! pipeline (catalog seeded, next file id reserved). The composition:
//!
//! 1. **Map tier files.** Sealed files verify header identity + footer
//!    only (two block reads — frame CRCs verify lazily on read; an eager
//!    pass would read the whole cold tier at boot). The at-most-one
//!    unsealed file recovers through `recover_seal_existing` at its
//!    manifested length (truncate → CRC every retained frame → seal
//!    `Recovered` — ADR-0056 D5, idempotent across recovery crashes).
//!    Files on disk the manifest does not name are dead-life garbage,
//!    deleted before any new flush.
//! 2. **Seed the new life** at `life_origin = flushed` — gapless and
//!    collision-free because no durable artifact names an address in
//!    `[flushed, old tail)`: only sub-`flushed` bytes flush (§3.1), refs
//!    sit below their walk watermark ≤ `flushed`, and image records
//!    carry no addresses at all.
//! 3. The caller then loads the checkpoint (images re-append at the new
//!    tail; ref sections apply through [`apply_ref_section`] with the
//!    manifested-watermark cross-check) and replays the WAL tail through
//!    the `TieredTable::apply_*` rules (ADR-0057 D4) — zero disk reads
//!    in either step.
//!
//! Fail-stop philosophy: the manifest is the only authority — a named
//! file that is missing, mis-identified, or shorter than its manifested
//! range is `InvalidData`, never a silent skip (§8.4, ADR-0017 D5).

use std::io;

use inf_foundation::LogicalAddr;
use inf_log::blob::parse_extent_file_name;
use inf_log::ckpt::{IckBlobRefSection, IckLiveSetSection, IckRefSection};
use inf_log::flush::{TierFileMeta, TierFlush, TierFlushConfig};
use inf_log::fs::SegmentFs;
use inf_log::manifest::TierNsManifest;
use inf_log::tier::{
    SealReason, TierIdentity, TierWriter, parse_tier_file_name, probe_tier_file, tier_file_name,
};

use crate::address_space::AddressSpaceConfig;
use crate::demote::DemotionConfig;
use crate::tiered::TieredTable;

/// Boot facts for the log line and the ledger (counts, not policy).
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct TierRecoverStats {
    /// Manifested files accepted on the sealed fast path.
    pub files_sealed: u32,
    /// Unsealed (or torn-seal) files re-sealed `Recovered` at their
    /// manifested length.
    pub files_resealed: u32,
    /// Un-manifested dead-life files deleted from the cold dir.
    pub files_removed: u32,
}

/// One recovered tiered namespace: the new-life table, the seeded flush
/// pipeline, and the boot facts.
pub struct RecoveredTier<F: SegmentFs> {
    /// New life at `life_origin = manifested flushed`; watermarks all at
    /// the origin (RAM starts cold — disclosed, L10). Live/dead counters
    /// boot *unreconciled* for the cold set (S14's lazy rebuild owns
    /// them).
    pub table: TieredTable,
    /// Catalog seeded with the manifested files; `next_id` above every
    /// named id.
    pub flush: TierFlush<F>,
    /// Blob-extent ids present on disk (names only — no content reads;
    /// M4-S17, ADR-0061 D6). The caller hands this to
    /// [`TieredTable::extent_sweep_seed`] **after** checkpoint + tail
    /// replay complete: liveness is decided by the post-replay
    /// refcounts, never by the manifest — the tier-file "unmanifested ⇒
    /// garbage" rule deliberately does not extend to extents.
    pub extents_listed: Vec<u64>,
    /// Boot facts.
    pub stats: TierRecoverStats,
}

/// Recovers one tiered namespace from its manifest section. The flush
/// config's `{shard_dir, cell, ns, mode}` name the namespace; `space`'s
/// `life_origin` is overridden with the manifested watermark;
/// `boot_ckpt_id` is the manifested checkpoint's id (the recovered
/// files' initial unref stamp — M4-S15, ADR-0059 D3).
///
/// # Errors
/// I/O failures; `InvalidData` when a named file is missing, its header
/// identity mismatches, or a sealed footer covers less than the
/// manifested range; `OutOfMemory` when the ring reservation fails.
pub fn recover_tiered_ns<F: SegmentFs>(
    fs: F,
    tier: &TierNsManifest,
    boot_ckpt_id: u64,
    flush_config: TierFlushConfig,
    space: AddressSpaceConfig,
    demote: DemotionConfig,
    initial_keys: usize,
) -> io::Result<RecoveredTier<F>> {
    assert_eq!(flush_config.ns.0, tier.ns, "manifest section vs pipeline namespace");
    let cold_dir = flush_config.shard_dir.join("cold");
    fs.create_dir_all(&cold_dir)?;
    let mut stats = TierRecoverStats::default();
    let mut catalog: Vec<TierFileMeta> = Vec::with_capacity(tier.files.len());
    for range in &tier.files {
        let path = cold_dir.join(tier_file_name(range.id));
        let expect = TierIdentity {
            cell: flush_config.cell,
            ns: flush_config.ns,
            base: LogicalAddr::from_raw(range.base).expect("manifest decode checked 48 bits"),
        };
        let (header, footer) = probe_tier_file(&fs, &path)?;
        if header.identity != expect {
            return Err(invalid(format!(
                "tier file {}: identity {:?} on disk, {expect:?} manifested",
                path.display(),
                header.identity
            )));
        }
        let reason = match footer {
            Some(footer) => {
                if footer.data_len < range.durable_len {
                    return Err(invalid(format!(
                        "tier file {}: sealed at {} but the manifest claims {} durable bytes",
                        path.display(),
                        footer.data_len,
                        range.durable_len
                    )));
                }
                stats.files_sealed += 1;
                footer.reason
            }
            None => {
                TierWriter::<F>::recover_seal_existing(
                    &fs,
                    &flush_config.shard_dir,
                    range.id,
                    expect,
                    range.durable_len,
                    flush_config.mode,
                )?;
                stats.files_resealed += 1;
                SealReason::Recovered
            }
        };
        catalog.push(TierFileMeta {
            id: range.id,
            base: expect.base,
            // The manifested prefix is the readable address range; a
            // sealed file's physical excess is inert (ADR-0057 D5).
            data_len: range.durable_len,
            reason,
            path,
        });
    }
    // Dead-life garbage: tier files the manifest does not name are
    // deleted before any new flush references their address space (the
    // S11 recovery pre-flush rule with the manifest as the authority).
    // Blob extents in the same directory are deliberately NOT this GC's
    // to touch (M4-S17, ADR-0061 D6): they are content-referenced and
    // refcount-governed — their ids are collected here (names only) and
    // the post-replay sweep decides.
    let mut extents_listed: Vec<u64> = Vec::new();
    for name in fs.list_dir(&cold_dir)? {
        if let Some(extent_id) = parse_extent_file_name(&name) {
            extents_listed.push(extent_id.0);
            continue;
        }
        let Some(id) = parse_tier_file_name(&name) else { continue };
        if tier.files.iter().all(|f| f.id != id) {
            fs.remove_file(&cold_dir.join(name))?;
            stats.files_removed += 1;
        }
    }
    extents_listed.sort_unstable();
    let next_id = tier.files.iter().map(|f| f.id + 1).max().unwrap_or(0);
    let mut table = TieredTable::new(
        AddressSpaceConfig {
            life_origin: LogicalAddr::from_raw(tier.flushed)
                .expect("manifest decode checked 48 bits"),
            ..space
        },
        demote,
        initial_keys,
    )
    .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "tier ring reservation failed"))?;
    // Live-set seeding (M4-S14, ADR-0058 D4): counts start at zero and
    // reconstruct through `apply_ref`/`apply_displace` as the checkpoint
    // and tail replay run; byte counters restore when the `.ick` 0x04
    // section arrives ([`apply_live_set_section`]).
    table.seed_recovered_files(&catalog, boot_ckpt_id);
    let flush = TierFlush::with_catalog(fs, flush_config, next_id, catalog);
    Ok(RecoveredTier { table, flush, extents_listed, stats })
}

/// Applies one validated `.ick` address-reference section (ADR-0057 D6
/// step 3): cross-checks the section's walk watermark against the
/// manifested flushed watermark — the §3.1 corollary's recovery half — a
/// section claiming refs above it means the checkpoint and manifest are
/// not one recovery unit (fail-stop), then applies every entry
/// idempotently.
///
/// # Errors
/// `InvalidData` on the watermark cross-check.
pub fn apply_ref_section(
    table: &mut TieredTable,
    section: &IckRefSection<'_>,
    manifested_flushed: u64,
) -> io::Result<()> {
    if section.walk_watermark > manifested_flushed {
        return Err(invalid(format!(
            "ick ref section (ns {}) walk watermark {} outruns the manifested flushed {}",
            section.ns, section.walk_watermark, manifested_flushed
        )));
    }
    for (hash, addr) in section.iter() {
        table.apply_ref(hash, LogicalAddr::from_raw(addr).expect("reader checked 48 bits"));
    }
    Ok(())
}

/// Applies one validated `.ick` live-set section (M4-S14, ADR-0058 D4/
/// D5): every entry restores under the D5 clamp rules — a length
/// mismatch or an unmanifested file id restores nothing, so the byte
/// counters can only ever under-count dead. Infallible by construction;
/// the decoder already audited shape, flags, and `dead ≤ len`.
pub fn apply_live_set_section(table: &mut TieredTable, section: &IckLiveSetSection<'_>) {
    for entry in section.iter() {
        table.restore_live_entry(&entry);
    }
}

/// Applies one validated `.ick` blob-reference section (M4-S17,
/// ADR-0061 D6): restores the reference map's cold entries so the
/// replayed tail's displacements decrement the right extent, and
/// advances the allocate-once id cursor past every restored id. Pairs
/// with [`apply_ref_section`] (the slots) — this section is
/// bookkeeping, not index content. Infallible by construction; the
/// decoder already audited shape, order, and lengths.
pub fn apply_blob_ref_section(table: &mut TieredTable, section: &IckBlobRefSection<'_>) {
    for entry in section.iter() {
        table.restore_extent_entry(entry.addr, entry.extent_id, entry.len);
    }
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
