//! Minimal tier file — the M4-S04 steel-thread cut of the cold tier.
//!
//! One file covers one contiguous logical range of one (cell, namespace)
//! address space: data byte `delta` of the range lives in frame
//! `delta / TIER_FRAME_DATA` at in-frame offset `delta % TIER_FRAME_DATA`
//! — pure arithmetic, no per-record directory (the §3.2 mapping rule).
//! On disk the file is a 4 KiB header block followed by 4 KiB frames
//! (`TIER_FRAME_DATA` payload bytes + a CRC32C trailer each), so every
//! read is naturally 4 KiB-aligned. The partial tail frame is written
//! zero-padded and rewritten in place as later flushes extend it; frames
//! below the last `sync` are covered by fdatasync.
//!
//! **Steel-thread scope, stated:** single file, no MANIFEST entry, no
//! footer, no early-seal/sealed-dead-gap handling (a caller must append
//! contiguously — asserted), no fault points, and reads trust the CRC
//! alone. M4-S11 owns tier-file format v1 (footer, seal-at-record-
//! boundary, ADR-0052 gap rule, fault points, `fuzz_tierfile_decode` in
//! the same PR) and the §8.4 fsync-fail-stop wiring; S12 owns the
//! MANIFEST. Nothing here is frozen until S11 lands v1.

use std::io;
use std::path::{Path, PathBuf};

use inf_foundation::LogicalAddr;
use inf_simd::crc32c;

use crate::fs::{SegmentFile, SegmentFs};
use crate::record::NsId;

/// On-disk frame size — one aligned read unit.
pub const TIER_FRAME_BYTES: usize = 4096;
/// Payload bytes per frame (the rest is the CRC32C trailer).
pub const TIER_FRAME_DATA: usize = TIER_FRAME_BYTES - 4;
/// Header block size (magic + identity, zero-padded to one frame).
pub const TIER_HEADER_BYTES: usize = 4096;
const TIER_MAGIC: &[u8; 4] = b"ITF0";

/// `tier-NNNNNN.itier` (§4 layout).
#[must_use]
pub fn tier_file_name(id: u32) -> String {
    format!("tier-{id:06}.itier")
}

/// The frame window covering `len` data bytes at range-relative offset
/// `delta`: (first frame index, frame count, skip inside the first
/// frame's payload).
#[must_use]
pub fn tier_frame_span(delta: u64, len: usize) -> (u64, u32, usize) {
    assert!(len > 0, "empty read");
    let data = TIER_FRAME_DATA as u64;
    let first = delta / data;
    let last = (delta + len as u64 - 1) / data;
    let count = u32::try_from(last - first + 1).expect("cold read spans a sane frame count");
    (first, count, (delta % data) as usize)
}

/// Disk byte offset of frame `frame` (the read offset for the window).
#[must_use]
pub fn tier_frame_offset(frame: u64) -> u64 {
    TIER_HEADER_BYTES as u64 + frame * TIER_FRAME_BYTES as u64
}

/// A CRC-failed frame in a cold read — served-data corruption, typed
/// (an operating condition of the storage, never a panic: the §3.1
/// durable copy is authoritative only when it verifies).
#[derive(Debug, PartialEq, Eq)]
pub struct TierCorruption {
    /// Frame index within the read window.
    pub window_frame: u32,
}

/// Verifies every frame CRC in a read window and copies `len` record
/// bytes (starting `skip` into the first frame's payload) into `out`.
/// `window` is raw disk frames as read (a multiple of
/// [`TIER_FRAME_BYTES`]).
///
/// # Errors
/// [`TierCorruption`] naming the first frame whose CRC fails.
///
/// # Panics
/// Panics when the window is too small for `skip + len` — the caller
/// computed the span, so a mismatch is a programmer error.
pub fn tier_extract(
    window: &[u8],
    skip: usize,
    len: usize,
    out: &mut Vec<u8>,
) -> Result<(), TierCorruption> {
    assert_eq!(window.len() % TIER_FRAME_BYTES, 0, "window is whole frames");
    let frames = window.len() / TIER_FRAME_BYTES;
    assert!(skip + len <= frames * TIER_FRAME_DATA, "window too small for the record");
    out.clear();
    out.reserve(len);
    let mut remaining = len;
    let mut skip = skip;
    for frame in 0..frames {
        let at = frame * TIER_FRAME_BYTES;
        let payload = &window[at..at + TIER_FRAME_DATA];
        let stored = u32::from_le_bytes(
            window[at + TIER_FRAME_DATA..at + TIER_FRAME_BYTES].try_into().expect("4 bytes"),
        );
        if crc32c(payload) != stored {
            return Err(TierCorruption { window_frame: frame as u32 });
        }
        let take = remaining.min(TIER_FRAME_DATA - skip);
        out.extend_from_slice(&payload[skip..skip + take]);
        remaining -= take;
        skip = 0;
        if remaining == 0 {
            break;
        }
    }
    assert_eq!(remaining, 0, "window covered the record");
    Ok(())
}

/// Steel-thread tier-file writer over the injected fs seam (the
/// `SyncIckWriter` pattern — blocking writes, driven from flush slices
/// and tests, never from command futures).
pub struct TierWriter<F: SegmentFs> {
    file: F::File,
    path: PathBuf,
    base: LogicalAddr,
    /// Data bytes appended (range-relative length).
    data_len: u64,
    /// Partial tail-frame payload (zero-padded on write).
    tail: Box<[u8]>,
    tail_fill: usize,
    /// Data bytes covered by the last `sync` (fdatasync barrier).
    durable_len: u64,
}

impl<F: SegmentFs> TierWriter<F> {
    /// Creates `shard_dir/cold/tier-NNNNNN.itier` with its header block
    /// and dir-fsync barriers (a file must exist durably before anything
    /// refers to it — the segment-create rule).
    ///
    /// # Errors
    /// I/O failures from the fs seam.
    pub fn create(
        fs: &F,
        shard_dir: &Path,
        id: u32,
        cell: u32,
        ns: NsId,
        base: LogicalAddr,
    ) -> io::Result<TierWriter<F>> {
        let cold_dir = shard_dir.join("cold");
        fs.create_dir_all(&cold_dir)?;
        fs.sync_dir(shard_dir)?;
        let path = cold_dir.join(tier_file_name(id));
        let mut file = fs.create_segment(&path, 0)?;
        let mut header = vec![0u8; TIER_HEADER_BYTES];
        header[0..4].copy_from_slice(TIER_MAGIC);
        header[4..8].copy_from_slice(&1u32.to_le_bytes()); // format version
        header[8..12].copy_from_slice(&cell.to_le_bytes());
        header[12..16].copy_from_slice(&ns.0.to_le_bytes());
        header[16..24].copy_from_slice(&base.to_raw().to_le_bytes());
        file.write_at(0, &header)?;
        fs.sync_dir(&cold_dir)?;
        Ok(TierWriter {
            file,
            path,
            base,
            data_len: 0,
            tail: vec![0u8; TIER_FRAME_DATA].into_boxed_slice(),
            tail_fill: 0,
            durable_len: 0,
        })
    }

    /// First logical address of this file's range.
    #[must_use]
    pub fn base(&self) -> LogicalAddr {
        self.base
    }

    /// Data bytes appended so far (`base + data_len` = next address).
    #[must_use]
    pub fn data_len(&self) -> u64 {
        self.data_len
    }

    /// Data bytes covered by fdatasync — the `flushed`-watermark input:
    /// the caller advances `flushed` to at most `base + durable_len`.
    #[must_use]
    pub fn durable_len(&self) -> u64 {
        self.durable_len
    }

    /// Backend fd for cold-read ops (`None` on fd-less test tiers).
    #[must_use]
    pub fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        self.file.raw_fd()
    }

    /// The file's path (test observability).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends record bytes at `addr`. The steel thread flushes one
    /// contiguous range into one file, so appends must be contiguous —
    /// S11's early-seal/gap machinery replaces this assert.
    ///
    /// # Errors
    /// I/O failures from the fs seam.
    ///
    /// # Panics
    /// Panics when `addr` is not the write cursor (`base + data_len`).
    pub fn append(&mut self, addr: LogicalAddr, bytes: &[u8]) -> io::Result<()> {
        assert_eq!(
            addr.to_raw(),
            self.base.to_raw() + self.data_len,
            "steel-thread tier appends are contiguous (S11 owns gaps)"
        );
        let mut bytes = bytes;
        while !bytes.is_empty() {
            let take = bytes.len().min(TIER_FRAME_DATA - self.tail_fill);
            self.tail[self.tail_fill..self.tail_fill + take].copy_from_slice(&bytes[..take]);
            self.tail_fill += take;
            self.data_len += take as u64;
            bytes = &bytes[take..];
            if self.tail_fill == TIER_FRAME_DATA {
                self.write_tail_frame()?;
                self.tail.fill(0);
                self.tail_fill = 0;
            }
        }
        Ok(())
    }

    /// Writes the partial tail frame (if any) and fdatasyncs; afterwards
    /// every appended byte is durable and [`durable_len`](Self::durable_len)
    /// says so. Failure surfaces as an error the caller must treat as
    /// fatal (§8.4 — S11 wires the typed fail-stop).
    ///
    /// # Errors
    /// I/O failures from the fs seam.
    pub fn sync(&mut self) -> io::Result<()> {
        if self.tail_fill > 0 {
            self.write_tail_frame()?;
        }
        self.file.sync_data()?;
        self.durable_len = self.data_len;
        Ok(())
    }

    /// Writes the current tail frame at its disk slot (full frames land
    /// here once; the partial tail is rewritten in place as it fills).
    fn write_tail_frame(&mut self) -> io::Result<()> {
        let frame_index = (self.data_len - 1) / TIER_FRAME_DATA as u64;
        let mut frame = vec![0u8; TIER_FRAME_BYTES];
        frame[..TIER_FRAME_DATA].copy_from_slice(&self.tail);
        frame[TIER_FRAME_DATA..].copy_from_slice(&crc32c(&self.tail).to_le_bytes());
        self.file.write_at(tier_frame_offset(frame_index), &frame)
    }
}

impl<F: SegmentFs> core::fmt::Debug for TierWriter<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TierWriter")
            .field("path", &self.path)
            .field("base", &self.base)
            .field("data_len", &self.data_len)
            .field("durable_len", &self.durable_len)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::mem::MemFs;

    fn read_window(fs: &MemFs, path: &Path, first: u64, count: u32) -> Vec<u8> {
        let file = fs.open_read(path).expect("file exists");
        let mut window = vec![0u8; count as usize * TIER_FRAME_BYTES];
        let n = file.read_at(tier_frame_offset(first), &mut window).expect("read");
        assert_eq!(n, window.len(), "window inside the synced file");
        window
    }

    /// Round trip: appended records read back byte-exact through the
    /// frame-span arithmetic, including a record spanning frames.
    #[test]
    fn append_sync_extract_round_trip() {
        let fs = MemFs::new();
        let base = LogicalAddr::from_raw(0x5000).expect("fits");
        let mut writer =
            TierWriter::create(&fs, Path::new("shard-0"), 0, 0, NsId(17), base).expect("create");
        // Three records; the middle one spans the first frame boundary.
        let records: [(u64, Vec<u8>); 3] = [
            (0, vec![0xAA; 100]),
            (100, vec![0xBB; TIER_FRAME_DATA]),
            (100 + TIER_FRAME_DATA as u64, vec![0xCC; 300]),
        ];
        for (delta, bytes) in &records {
            writer.append(base.advanced(*delta).expect("fits"), bytes).expect("append");
        }
        assert_eq!(writer.durable_len(), 0, "nothing durable before sync");
        writer.sync().expect("sync");
        assert_eq!(writer.durable_len(), writer.data_len());
        let mut out = Vec::new();
        for (delta, bytes) in &records {
            let (first, count, skip) = tier_frame_span(*delta, bytes.len());
            let window = read_window(&fs, writer.path(), first, count);
            tier_extract(&window, skip, bytes.len(), &mut out).expect("clean frames");
            assert_eq!(&out, bytes, "byte-exact round trip");
        }
    }

    /// The partial tail frame extends across syncs — rewritten in place,
    /// CRC always covering the current payload.
    #[test]
    fn tail_frame_extends_across_syncs() {
        let fs = MemFs::new();
        let base = LogicalAddr::ZERO;
        let mut writer =
            TierWriter::create(&fs, Path::new("shard-0"), 1, 0, NsId(17), base).expect("create");
        writer.append(base, &[0x11; 64]).expect("append");
        writer.sync().expect("sync");
        writer.append(base.advanced(64).expect("fits"), &[0x22; 64]).expect("append");
        writer.sync().expect("sync");
        let (first, count, skip) = tier_frame_span(0, 128);
        let window = read_window(&fs, writer.path(), first, count);
        let mut out = Vec::new();
        tier_extract(&window, skip, 128, &mut out).expect("clean frame");
        assert_eq!(&out[..64], &[0x11; 64]);
        assert_eq!(&out[64..], &[0x22; 64]);
    }

    /// A flipped bit fails the frame CRC — typed, not panicking.
    #[test]
    fn corruption_is_detected_per_frame() {
        let fs = MemFs::new();
        let mut writer =
            TierWriter::create(&fs, Path::new("shard-0"), 2, 0, NsId(17), LogicalAddr::ZERO)
                .expect("create");
        writer.append(LogicalAddr::ZERO, &[0x33; 500]).expect("append");
        writer.sync().expect("sync");
        let (first, count, skip) = tier_frame_span(0, 500);
        let mut window = read_window(&fs, writer.path(), first, count);
        window[skip + 7] ^= 0x01;
        let mut out = Vec::new();
        assert_eq!(
            tier_extract(&window, skip, 500, &mut out),
            Err(TierCorruption { window_frame: 0 })
        );
    }

    /// Non-contiguous appends are the S11 gap machinery's job — refused
    /// loudly here.
    #[test]
    #[should_panic(expected = "contiguous")]
    fn non_contiguous_append_panics() {
        let fs = MemFs::new();
        let mut writer =
            TierWriter::create(&fs, Path::new("shard-0"), 3, 0, NsId(17), LogicalAddr::ZERO)
                .expect("create");
        writer.append(LogicalAddr::from_raw(64).expect("fits"), &[0u8; 8]).expect("append");
    }
}
