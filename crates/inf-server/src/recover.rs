//! Boot-time per-cell log recovery (M2-S08, ADR-0015 D7): scan the cell's
//! log directory, replay every frame through `Keyspace::apply_record` (the
//! blind idempotent upsert), and reopen the tail segment for appending.
//! Boot-time blocking file I/O is the sanctioned exception to the cell
//! denylist (§3.3) and rides the injected `SegmentFs` seam for DST.
//!
//! Parallel-boot orchestration, `-LOADING`, and progress reporting are
//! M2-S15; checkpoint loading is M2-S13 (until then recovery replays the
//! whole retained log — correct, unbounded only by segment count).

use std::io;

use inf_foundation::time::Nanos;
use inf_log::fs::StdSegmentFs;
use inf_log::{ReadEnd, ReaderConfig, SegmentReader, SegmentRotor, create_cell_dirs, scan_log_dir};
use inf_store::{Keyspace, ReplayOutcome, WallAnchor};

use crate::durable::DurableConfig;

/// What one cell's recovery did (log lines + `INFO persistence` inputs).
#[derive(Copy, Clone, Debug, Default)]
pub struct RecoverStats {
    pub segments: u64,
    pub frames: u64,
    pub records_applied: u64,
    pub records_skipped: u64,
}

/// Opens (or creates) cell `cell`'s log under the node data dir, replaying
/// any existing segments into `ks` first. Returns the rotor positioned at
/// the tail, ready for `begin_frame_deferred`.
///
/// # Errors
/// Scan errors, read errors (torn-tail policy is M2-S14 — today any
/// mid-segment corruption is surfaced, never skipped), store apply
/// failures, and rotor open failures — all fail-stop at boot (§8.4).
pub fn open_cell_log(
    ks: &mut Keyspace,
    cell: u16,
    cfg: &DurableConfig,
    anchor: WallAnchor,
    now: Nanos,
) -> io::Result<(SegmentRotor<StdSegmentFs>, RecoverStats)> {
    let fs = StdSegmentFs;
    let shard_dir = cfg.data_dir.join(format!("shard-{cell}"));
    let dirs = create_cell_dirs(&fs, &shard_dir)?;
    let scan = scan_log_dir(&fs, &dirs.log).map_err(io_invalid)?;
    let mut stats = RecoverStats::default();

    if scan.is_empty() {
        let rotor = SegmentRotor::create_fresh(fs, dirs.log, cfg.segment).map_err(io_invalid)?;
        return Ok((rotor, stats));
    }

    let mut tail_offset = 0u32;
    for &segment in scan.segments() {
        let mut reader = SegmentReader::open(&fs, &dirs.log, segment, ReaderConfig::default())
            .map_err(io_invalid)?;
        let end = reader
            .apply_frames(|frame| {
                stats.frames += 1;
                for record in frame.records() {
                    let (_lsn, record) = record.map_err(io_invalid)?;
                    match ks.apply_record(&record, now, anchor).map_err(io_invalid)? {
                        ReplayOutcome::Applied => stats.records_applied += 1,
                        ReplayOutcome::SkippedUnknownNs | ReplayOutcome::SkippedReserved => {
                            stats.records_skipped += 1;
                        }
                    }
                }
                Ok::<(), io::Error>(())
            })
            .map_err(io_invalid)?;
        stats.segments += 1;
        if Some(segment) == scan.tail() {
            tail_offset = match end {
                ReadEnd::ZeroTail { at } | ReadEnd::FileEnd { at } => at,
            };
        }
    }

    let rotor = SegmentRotor::open_existing(fs, dirs.log, cfg.segment, &scan, tail_offset)
        .map_err(io_invalid)?;
    Ok((rotor, stats))
}

fn io_invalid(err: impl std::fmt::Debug) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{err:?}"))
}
