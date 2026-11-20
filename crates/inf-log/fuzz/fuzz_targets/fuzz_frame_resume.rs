//! Resumed record walk fuzz target (ADR-0174 A1: a recovery step that has
//! charged its budget stops at the next record, inside a frame, and the next
//! step resumes there through a cursor its own record iterator produced, over
//! the frame the segment reader still holds). Oracles on arbitrary bytes and
//! arbitrary park points (a bit per record: stop there and resume):
//!
//! 1. No panic anywhere in `RecordIter::cursor`, `FrameRef::records_from` or
//!    `SegmentReader::held_frame`.
//! 2. A walk parked at any set of records yields exactly what one walk
//!    yields: every record with its LSN, then the same end — `None` on an
//!    honest frame, the same typed error (a record decode error, a body
//!    short of its count, bytes after the counted records) on any other.
//! 3. The same over the reader: the frame `next_step` returned, re-issued by
//!    `held_frame` at each park, walks as the frame itself does; no other
//!    base is held, and no frame is held once the next step ran.
//!
//! Two legs per input: the bytes walked as frames as they stand (their CRCs
//! as found), and the bytes as the body of one frame whose header and
//! trailer are rebuilt around them, so record decoding sees arbitrary
//! bodies under any record count past the CRC check.
#![no_main]

use std::path::Path;

use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    DEFAULT_MAX_FRAME_LEN, FRAME_HEADER_LEN, FRAME_MAGIC, FRAME_MAGIC_V3, FrameIter, FrameRef,
    ReadStep, ReaderConfig, SegmentId, SegmentReader,
};
use libfuzzer_sys::fuzz_target;

/// One walk's items, each in a form that outlives the frame's borrow.
fn walk_whole(frame: &FrameRef<'_>) -> Vec<String> {
    frame.records().map(|item| format!("{item:?}")).collect()
}

/// The walk stopped after every record whose bit is set in `parks` and
/// resumed from its cursor over the frame `again` hands out each time.
fn walk_parked<'a>(again: impl Fn() -> FrameRef<'a>, parks: u64) -> Vec<String> {
    let mut items = Vec::new();
    let mut walk = again().records();
    // Bound: each turn yields one item or ends; the items are at most the
    // body's bytes plus the end.
    for turn in 0u32.. {
        if turn < 64 && parks & (1u64 << turn) != 0 {
            let cursor = walk.cursor();
            walk = again().records_from(&cursor).expect("a cursor of this frame's own walk");
        }
        let Some(item) = walk.next() else { break };
        let failed = item.is_err();
        items.push(format!("{item:?}"));
        if failed {
            break;
        }
    }
    items
}

/// `body` framed as one v2 (or v3) frame at segment offset 0, its record
/// count `declared`, its trailer computed: CRC-valid by construction.
fn frame_around(body: &[u8], declared: u32, aligned: bool) -> Vec<u8> {
    let frame_len = (FRAME_HEADER_LEN + body.len() + 4) as u32;
    let mut image = Vec::with_capacity(frame_len as usize);
    image.extend_from_slice(if aligned { &FRAME_MAGIC_V3 } else { &FRAME_MAGIC });
    image.extend_from_slice(&frame_len.to_le_bytes());
    image.extend_from_slice(&declared.to_le_bytes());
    image.extend_from_slice(&0u32.to_le_bytes());
    image.extend_from_slice(&(FRAME_HEADER_LEN as u32).to_le_bytes());
    image.extend_from_slice(&1u32.to_le_bytes());
    image.extend_from_slice(&1u64.to_le_bytes());
    image.extend_from_slice(&0u64.to_le_bytes());
    image.extend_from_slice(body);
    let crc = inf_simd::crc32c(&image);
    image.extend_from_slice(&crc.to_le_bytes());
    image
}

/// Oracle 3: the image as a segment file, read by the reader; its first
/// frame walked whole and parked over `held_frame`.
fn reader_leg(image: &[u8], chunk_seed: u16, parks: u64) {
    let fs = MemFs::new();
    let dir = Path::new("fuzz");
    fs.create_dir_all(dir).expect("dir");
    let mut file = fs.create_segment(&dir.join("seg-000000.ilog"), 0).expect("create");
    file.write_at(0, image).expect("write");
    let chunk = 8 + usize::from(chunk_seed) % (64 << 10);
    let cfg = ReaderConfig { chunk_bytes: chunk, max_frame_len: DEFAULT_MAX_FRAME_LEN };
    let mut reader = SegmentReader::open(&fs, dir, SegmentId(0), cfg).expect("open");
    let returned = match reader.next_step() {
        Ok(ReadStep::Frame(frame)) => {
            let base = frame.first_lsn().offset - frame.header_len() as u32;
            Some((base, frame.first_lsn(), walk_whole(&frame)))
        }
        Ok(ReadStep::End(_)) | Err(_) => None,
    };
    let Some((base, first_lsn, whole)) = returned else {
        assert!(reader.held_frame(0).is_none(), "no frame returned, none held");
        return;
    };
    let held = reader.held_frame(base).expect("the frame just returned is held");
    assert_eq!(held.first_lsn(), first_lsn, "the held frame is the one returned");
    assert_eq!(walk_whole(&held), whole, "the held frame walks as the returned one");
    let again = || reader.held_frame(base).expect("held until the next step");
    assert_eq!(walk_parked(again, parks), whole, "a parked walk over the held frame");
    assert!(reader.held_frame(base.wrapping_add(1)).is_none(), "another base is not held");
    let _ = reader.next_step();
    assert!(reader.held_frame(base).is_none(), "the next step releases the frame");
}

fuzz_target!(|input: (u64, u32, u16, bool, &[u8])| {
    let (parks, declared_seed, chunk_seed, aligned, data) = input;

    // Leg 1: the bytes as frames, as they stand.
    for item in FrameIter::new(data, DEFAULT_MAX_FRAME_LEN) {
        let Ok((_, frame)) = item else { break };
        assert_eq!(walk_parked(|| frame, parks), walk_whole(&frame), "a parked walk");
    }

    // Leg 2: the bytes as one CRC-valid frame's body, any record count.
    if data.len() > (DEFAULT_MAX_FRAME_LEN as usize) - FRAME_HEADER_LEN - 4 {
        return;
    }
    let declared = 1 + declared_seed % 64;
    let image = frame_around(data, declared, aligned);
    if let Ok((frame, _)) = inf_log::decode_frame(&image, DEFAULT_MAX_FRAME_LEN) {
        assert_eq!(walk_parked(|| frame, parks), walk_whole(&frame), "a parked walk");
    }
    reader_leg(&image, chunk_seed, parks);
});
