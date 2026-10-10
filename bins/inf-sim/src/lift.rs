//! The lift-regime residue plant (review 2026-08-30, F-L14-01; batch 21).
//!
//! `m2-mode-transition`'s lift regime (batch 20) put tiered + indexed
//! traffic in the main life and measured that the discarded-life residue
//! shape (`recover_stamp.rs`, `recover_lift.rs`) is lifted on 3 of 400
//! seeds — never on the arm's own class. This module writes that shape
//! into the sim image deterministically, between the prelude cut and the
//! transition boot, on every cell of every arm seed:
//!
//! - the resume segment `N` (the prelude's last data-bearing segment) is
//!   zeroed beyond its data end and a validating frame of the *same* life
//!   is placed beyond the gap (`ghost`, epoch `e`, seq `last + 2`) — the
//!   hole the audit cannot classify locally;
//! - segment `N + 1` is written by a later life `e + 1`: a tiered
//!   `SET` and an indexed `DocFull` — exactly the records the lift replays
//!   and the end-of-replay checks must not run before.
//!
//! The transition boot then lifts on every planted cell
//! (`stale_residue_slacks ≥ 1`), and the index-equality oracle runs against
//! a sidecar loaded from the prelude's forced checkpoint: pre-batch-19
//! ordering commits that sidecar at the first probe step and the planted
//! document never reaches the tree.

use std::io;
use std::path::{Path, PathBuf};

use inf_doc::JsonParser;
use inf_foundation::CellId;
use inf_log::fs::sim::DEFAULT_SECTOR_BYTES;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    DocLineage, FRAME_HEADER_LEN, FrameBuilder, FrameLayout, FrameStamp, Lsn, NsId, ReaderConfig,
    RecordView, SegmentId, SegmentReader, parse_segment_file_name, segment_file_name,
};
use inf_server::SimDisk;
use inf_store::SlotRouter;

/// The lift regime's namespaces, captured at DDL time (the node is gone
/// when the plant runs).
#[derive(Copy, Clone, Debug)]
pub(crate) struct LiftNs {
    pub(crate) tier: NsId,
    pub(crate) idx: NsId,
}

/// What was planted on one cell — the oracle's expectations.
#[derive(Clone, Debug)]
pub(crate) struct PlantedCell {
    pub(crate) cell: usize,
    /// Life-2 tiered record behind the lift.
    pub(crate) tier_key: Vec<u8>,
    /// The discarded life's residue record — must never replay.
    pub(crate) ghost_key: Vec<u8>,
    /// Life-2 indexed document behind the lift (`$.meta.tag` = `tag`).
    pub(crate) doc_key: Vec<u8>,
    pub(crate) tag: i64,
    pub(crate) residue_segment: SegmentId,
    pub(crate) lifted_segment: SegmentId,
}

pub(crate) const LIFTED_VALUE: &[u8] = b"lifted";
pub(crate) const GHOST_VALUE: &[u8] = b"stale";

/// The planted document's canonical text — what `JSON.GET` answers.
pub(crate) fn planted_doc_text(tag: i64) -> Vec<u8> {
    format!("{{\"values\":[0,0],\"meta\":{{\"tag\":{tag}}}}}").into_bytes()
}

/// A key of the form `<prefix>:<cell>:<n>` that the contiguous slot
/// router owns on `cell` — a planted record must replay into the cell
/// whose log carries it.
pub(crate) fn local_key(prefix: &str, cell: usize, cells: u16) -> Vec<u8> {
    let router = SlotRouter::new_contiguous(cells);
    let owner = CellId(u16::try_from(cell).expect("cell index fits u16"));
    (0u32..)
        .map(|n| format!("{prefix}:{cell}:{n}").into_bytes())
        .find(|key| router.is_local(key, owner))
        .expect("some key routes to every cell")
}

fn frame(
    segment: SegmentId,
    offset: u32,
    records: &[RecordView<'_>],
    stamp: FrameStamp,
) -> Vec<u8> {
    let mut b = FrameBuilder::new();
    for record in records {
        b.append(record);
    }
    let first = Lsn::new(segment, offset + u32::try_from(FRAME_HEADER_LEN).expect("40"));
    b.finalize(first, stamp, FrameLayout::Packed).to_vec()
}

fn poke(disk: &SimDisk, path: &Path, offset: u64, bytes: &[u8]) -> io::Result<()> {
    let mut file = disk.open_write(path)?;
    file.write_at(offset, bytes)?;
    file.sync_data()
}

/// The prelude's resume point on one cell: the highest-numbered segment
/// holding at least one whole frame, its data end (the reader's offset
/// after the last whole frame — a torn or foreign frame ends the walk,
/// as recovery's does), the last whole frame's stamp, and the file size.
fn resume_point(
    disk: &SimDisk,
    log_dir: &Path,
) -> Result<(SegmentId, u32, FrameStamp, u64), String> {
    let mut ids: Vec<SegmentId> = disk
        .list_dir(log_dir)
        .map_err(|e| format!("list {}: {e}", log_dir.display()))?
        .iter()
        .filter_map(|name| parse_segment_file_name(name))
        .collect();
    ids.sort_unstable_by_key(|id| std::cmp::Reverse(id.0));
    for id in ids {
        let mut reader = SegmentReader::open(disk, log_dir, id, ReaderConfig::default())
            .map_err(|e| format!("open segment {}: {e}", id.0))?;
        let mut last = None;
        while let Ok(Some(frame)) = reader.next_frame() {
            last = frame.stamp();
        }
        let end = reader.offset();
        if let Some(stamp) = last {
            let size = disk
                .open_read(&log_dir.join(segment_file_name(id)))
                .and_then(|f| f.file_size())
                .map_err(|e| format!("size of segment {}: {e}", id.0))?;
            return Ok((id, end, stamp, size));
        }
    }
    Err("no data-bearing segment".to_owned())
}

/// The torn-tail plant (batch 35, N17): the FLUSH → FUA prelude's cut
/// lands after its traffic drained, its frames are sub-sector, and the
/// rotor's immediate prealloc left an empty next segment — so recovery
/// legally resumes *there* at offset 0 and the transition never touches
/// packed data. This writes what the sim's own sector-granular cut writes
/// for a frame that spans sectors: the first sector of the next frame,
/// zeros behind it. Recovery then truncates at the last whole frame,
/// removes the empty next segment and reopens the packed tail under the
/// `Direct` rotor (ADR-0086 D4 as amended). Returns the planted cells.
pub(crate) fn plant_torn_tail(
    disk: &SimDisk,
    data_dir: &Path,
    cells: u16,
    ns: NsId,
) -> Result<Vec<usize>, String> {
    let sector = usize::try_from(DEFAULT_SECTOR_BYTES).expect("sector fits usize");
    let mut planted = Vec::new();
    for cell in 0..usize::from(cells) {
        let log_dir: PathBuf = data_dir.join(format!("shard-{cell}")).join("log");
        let (seg, end, last, size) =
            resume_point(disk, &log_dir).map_err(|e| format!("cell {cell}: {e}"))?;
        let key = local_key("torn", cell, cells);
        let value = [b't'; 1024];
        let torn = frame(
            seg,
            end,
            &[RecordView::StringPostImage { ns, key: &key, value: &value }],
            FrameStamp {
                epoch: last.epoch,
                seq: last.seq + 1,
                covered_lsn: Lsn::new(seg, end).to_u64(),
            },
        );
        assert!(torn.len() > sector, "the planted frame spans sectors");
        if u64::from(end) + sector as u64 > size {
            return Err(format!(
                "cell {cell}: segment {} has no sector past {end} of {size}",
                seg.0
            ));
        }
        let seg_path = log_dir.join(segment_file_name(seg));
        let slack = usize::try_from(size - u64::from(end)).expect("segment fits usize");
        poke(disk, &seg_path, u64::from(end), &vec![0u8; slack])
            .map_err(|e| format!("cell {cell}: zero slack: {e}"))?;
        poke(disk, &seg_path, u64::from(end), &torn[..sector])
            .map_err(|e| format!("cell {cell}: torn sector: {e}"))?;
        planted.push(cell);
    }
    Ok(planted)
}

/// Writes the lift shape into every cell's log (see the module doc).
/// Returns one record per planted cell; a cell whose resume segment has
/// no room for the residue frame is skipped and named (the oracle counts
/// planted cells, never assumes them).
pub(crate) fn plant_lifted_tail(
    disk: &SimDisk,
    data_dir: &Path,
    cells: u16,
    segment_bytes: u64,
    ns: LiftNs,
) -> Result<(Vec<PlantedCell>, Vec<String>), String> {
    let mut planted = Vec::new();
    let mut skipped = Vec::new();
    let idoc_of = |tag: i64| {
        JsonParser::new().parse(&planted_doc_text(tag)).expect("the planted text is valid JSON")
    };
    for cell in 0..usize::from(cells) {
        let log_dir: PathBuf = data_dir.join(format!("shard-{cell}")).join("log");
        let (seg, end, last, size) =
            resume_point(disk, &log_dir).map_err(|e| format!("cell {cell}: {e}"))?;
        let tier_key = local_key("lift:t", cell, cells);
        let ghost_key = local_key("lift:ghost", cell, cells);
        let doc_key = local_key("lift:d", cell, cells);
        let tag = 1_000 + i64::try_from(cell).expect("cell fits i64");

        // The residue: a validating frame of the prelude's life beyond a
        // zero gap. Its attestation covers only the prefix (`end`), so a
        // boot that sees no later life reads a plain torn tail.
        let residue_at = end + 63;
        let covered = Lsn::new(seg, end).to_u64();
        let residue = frame(
            seg,
            residue_at,
            &[RecordView::StringPostImage { ns: ns.tier, key: &ghost_key, value: GHOST_VALUE }],
            FrameStamp { epoch: last.epoch, seq: last.seq + 2, covered_lsn: covered },
        );
        let residue_end = u64::from(residue_at) + residue.len() as u64;
        if residue_end > size {
            skipped.push(format!(
                "cell {cell}: segment {} data end {end} leaves {} B, residue needs {}",
                seg.0,
                size - u64::from(end),
                residue_end - u64::from(end)
            ));
            continue;
        }
        let seg_path = log_dir.join(segment_file_name(seg));
        let slack = usize::try_from(size - u64::from(end)).expect("segment fits usize");
        poke(disk, &seg_path, u64::from(end), &vec![0u8; slack])
            .map_err(|e| format!("cell {cell}: zero slack: {e}"))?;
        poke(disk, &seg_path, u64::from(residue_at), &residue)
            .map_err(|e| format!("cell {cell}: residue: {e}"))?;

        // Life 2 in the next segment: a tiered control record, then the
        // indexed document — the records the lift replays.
        let next = SegmentId(seg.0 + 1);
        let next_path = log_dir.join(segment_file_name(next));
        match disk.open_write(&next_path) {
            Ok(mut file) => {
                let len = file.file_size().map_err(|e| format!("cell {cell}: {e}"))?;
                let zeros = vec![0u8; usize::try_from(len).expect("segment fits usize")];
                file.write_at(0, &zeros).map_err(|e| format!("cell {cell}: zero next: {e}"))?;
                file.sync_data().map_err(|e| format!("cell {cell}: sync next: {e}"))?;
            }
            Err(_) => {
                let mut file = disk
                    .create_segment(&next_path, segment_bytes)
                    .map_err(|e| format!("cell {cell}: create next: {e}"))?;
                file.sync_data().map_err(|e| format!("cell {cell}: sync next: {e}"))?;
                disk.sync_dir(&log_dir).map_err(|e| format!("cell {cell}: sync dir: {e}"))?;
            }
        }
        let life2 = last.epoch + 1;
        let first = frame(
            next,
            0,
            &[RecordView::StringPostImage { ns: ns.tier, key: &tier_key, value: LIFTED_VALUE }],
            FrameStamp { epoch: life2, seq: 1, covered_lsn: covered },
        );
        let first_len = u32::try_from(first.len()).expect("fits u32");
        poke(disk, &next_path, 0, &first).map_err(|e| format!("cell {cell}: life 2: {e}"))?;
        let idoc = idoc_of(tag);
        let second = frame(
            next,
            first_len,
            &[RecordView::DocFull {
                ns: ns.idx,
                key: &doc_key,
                lineage: DocLineage::FIRST,
                version: 1,
                idoc: &idoc,
            }],
            FrameStamp {
                epoch: life2,
                seq: 2,
                covered_lsn: Lsn::new(next, 0).advance(first_len).to_u64(),
            },
        );
        poke(disk, &next_path, u64::from(first_len), &second)
            .map_err(|e| format!("cell {cell}: life 2 doc: {e}"))?;
        planted.push(PlantedCell {
            cell,
            tier_key,
            ghost_key,
            doc_key,
            tag,
            residue_segment: seg,
            lifted_segment: next,
        });
    }
    Ok((planted, skipped))
}
