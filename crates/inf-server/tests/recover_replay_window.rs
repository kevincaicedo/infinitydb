//! ADR-0174 D1 at the server tier, on the injected `MemFs` seam: a cell
//! log whose tiered tail re-appends more than the namespace's RAM window
//! (`MEM-BUDGET + MAINTAIN-SLICE`) boots through `open_cell_log` — replay
//! demotes through the recovered pipeline instead of failing the boot —
//! and every acknowledged key reads back its bytes, from RAM or through
//! the tier file's CRC-verified frames. Red before the replay seam: the
//! boot refuses with `replay apply failed … OutOfMemory`.

use std::path::{Path, PathBuf};

use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::tier::{parse_tier_file_name, probe_tier_file};
use inf_log::{
    MutationEffect, NsId, TIER_FRAME_BYTES, tier_extract, tier_frame_offset, tier_frame_span,
};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, TierSpec, TieredLookup, TieredTable};

mod support;
use support::*;

const TIER_NS: NsId = NsId(17);
/// The smallest legal window: `MEM-BUDGET 3mb` + `MAINTAIN-SLICE 1mb`.
const MEM_BUDGET: u64 = 3 << 20;
const WINDOW: u64 = MEM_BUDGET + (1 << 20);
const VALUE_LEN: usize = 4 << 10;
const RECORDS_PER_FRAME: usize = 12;

/// `fresh_keyspace` plus a materialized tiered namespace at the smallest
/// window (no MANIFEST — the first-crash shape: the catalog names it, no
/// checkpoint does).
fn tiered_keyspace() -> Keyspace {
    let mut ks = fresh_keyspace();
    ks.ns_create(NsSpec {
        id: TIER_NS,
        name: b"hot".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
        tier: Some(TierSpec::for_budget(MEM_BUDGET)),
    })
    .expect("tiered ns");
    ks
}

#[test]
fn a_tiered_tail_above_the_window_boots_through_open_cell_log() {
    let fs = MemFs::new();
    let mut log = LogBuilder::new(&fs, &cfg());
    // Distinct keys until the tail alone exceeds the window: every one
    // re-appends at replay, so the recovered table must demote.
    let value = vec![0xA7u8; VALUE_LEN];
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut tail_bytes = 0u64;
    while tail_bytes <= WINDOW + (1 << 20) {
        let batch: Vec<Vec<u8>> = (0..RECORDS_PER_FRAME)
            .map(|i| format!("tail:{:06}", keys.len() + i).into_bytes())
            .collect();
        let effects: Vec<MutationEffect<'_>> = batch
            .iter()
            .map(|key| MutationEffect::StringSet { ns: TIER_NS, key, value: &value })
            .collect();
        log.frame(&effects);
        for key in &batch {
            tail_bytes += (key.len() + value.len() + 8) as u64;
        }
        keys.extend(batch);
    }
    assert!(tail_bytes > WINDOW, "the regime: the tail alone exceeds the window");
    drop(log);

    let mut ks = tiered_keyspace();
    let (_rotor, stats) = match recover(&fs, &mut ks) {
        Ok(recovered) => recovered,
        Err(err) => panic!("the boot refused a tail of {tail_bytes} bytes: {err}"),
    };
    // Engagement: the boot demoted, and its files are this boot's own.
    let replay = stats.tier_replay.counters;
    assert!(replay.demote_steps > 0, "VACUOUS: the boot did not demote ({replay:?})");
    assert!(replay.tier_bytes > 0 && replay.files_sealed > 0, "{replay:?}");
    let files = tier_files(&fs);
    assert!(!files.is_empty(), "the demoted bytes are on disk");
    let (mut ram, mut cold) = (0u64, 0u64);
    for key in &keys {
        let hash = ks.hasher().hash(key);
        let table = ks.tiered_store_mut(TIER_NS).expect("tiered");
        let got = match table.lookup(key, hash, &[]) {
            TieredLookup::Ram(addr) => {
                ram += 1;
                table.record(addr).value.to_vec()
            }
            TieredLookup::Cold(addr) => {
                cold += 1;
                read_cold_value(&fs, &files, addr.to_raw(), key)
            }
            TieredLookup::Miss => {
                panic!("acknowledged key {:?} absent after the boot", String::from_utf8_lossy(key))
            }
        };
        assert!(got == value, "key {:?} reads other bytes", String::from_utf8_lossy(key));
    }
    assert!(ram > 0 && cold > 0, "VACUOUS: {ram} RAM and {cold} cold keys");
}

/// The namespace's tier files: (base, readable bytes, path), each probed
/// by its header identity and footer — every boot file is sealed by the
/// hand-over.
fn tier_files(fs: &MemFs) -> Vec<(u64, u64, PathBuf)> {
    let cold = Path::new("data").join(format!("shard-{CELL}")).join("ns-17").join("cold");
    let mut files = Vec::new();
    for name in fs.list_dir(&cold).expect("cold dir") {
        if parse_tier_file_name(&name).is_none() {
            continue;
        }
        let path = cold.join(&name);
        let (header, footer) = probe_tier_file(fs, &path).expect("probe");
        let footer = footer.expect("the hand-over sealed every boot file");
        files.push((header.identity.base.to_raw(), footer.data_len, path));
    }
    files
}

/// The cold-read path's decode of one record: the frames covering the
/// address, CRC-verified by `tier_extract`, the header sizing the record,
/// then the record itself; the key must be the key asked for.
fn read_cold_value(fs: &MemFs, files: &[(u64, u64, PathBuf)], addr: u64, key: &[u8]) -> Vec<u8> {
    let (base, len, path) = files
        .iter()
        .find(|(base, len, _)| addr >= *base && addr < base + len)
        .expect("a catalogued file holds the cold address");
    let read = |at: u64, want: usize| -> Vec<u8> {
        assert!(at + want as u64 <= base + len, "the record lies whole in its file");
        let (first, count, skip) = tier_frame_span(at - base, want);
        let mut window = vec![0u8; count as usize * TIER_FRAME_BYTES];
        let file = fs.open_read(path).expect("open");
        let mut done = 0usize;
        while done < window.len() {
            let n = file.read_at(tier_frame_offset(first) + done as u64, &mut window[done..]);
            let n = n.expect("read");
            assert!(n > 0, "short tier file");
            done += n;
        }
        let mut out = Vec::new();
        tier_extract(&window, skip, want, &mut out).expect("frame CRCs");
        out
    };
    let head = read(addr, TieredTable::RECORD_HEADER_LEN);
    let record = read(addr, TieredTable::record_len_from_header(&head));
    let parts = TieredTable::decode_record(&record);
    assert_eq!(parts.key, key, "the cold record carries the key asked for");
    parts.value.to_vec()
}
