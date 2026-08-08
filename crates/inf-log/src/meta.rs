//! Node META-file atomic-swap protocol (M2-S08, ADR-0015 D3): a small
//! whole-file envelope replaced by write-new + fsync + rename + dir-fsync.
//! This is the **MANIFEST protocol class** (M2-S11): [`crate::manifest`]
//! rides the same envelope and the same swap steps; the payload is opaque
//! bytes at this layer (`inf-store` owns the catalog encoding,
//! [`crate::manifest`] owns the manifest schema — `inf-log::meta` never
//! knows either).
//!
//! ```text
//! envelope := magic: [u8;8] = "INFMETA1"   — version-tagged
//!             payload_len: u32 LE
//!             payload: payload_len bytes
//!             crc: CRC32C(magic · payload_len · payload): u32 LE
//! ```
//!
//! Crash consistency: [`write_envelope`] stages the full envelope in the
//! staging file, fdatasyncs it, renames it over the committed file, and
//! dir-fsyncs — a reader sees either the old envelope or the new one,
//! never a blend. Staging debris from a crash mid-protocol is removed by
//! the next write and never read. Every step's failure propagates; the
//! caller owns the failure policy (the catalog treats it as fail-stop —
//! ADR-0015 D3; the manifest aborts the publication and keeps the old
//! recovery unit — ADR-0017). A corrupt committed file is a named
//! `InvalidData` error — never silently treated as absent.
//!
//! Swap steps, in order (each becomes a named fault point at M2-S16):
//! remove stale staging → create staging → write envelope → fdatasync →
//! rename → dir-fsync.

use std::io;
use std::path::Path;

use inf_simd::crc32c;

use crate::fs::{SegmentFile, SegmentFs};

/// The committed catalog file name.
pub const META_FILE: &str = "META";
/// The staging name for a not-yet-committed envelope. A leftover file with
/// this name (crash between create and rename) is ignored by [`read_meta`]
/// and cleared by the next [`write_meta`].
pub const META_STAGING_FILE: &str = "META.new";
/// Envelope magic; the trailing `1` is the format version.
pub const META_MAGIC: [u8; 8] = *b"INFMETA1";

const HEADER_LEN: usize = META_MAGIC.len() + 4;
const TRAILER_LEN: usize = 4;
/// Smallest well-formed envelope: magic + length + empty payload + CRC.
const MIN_ENVELOPE_LEN: usize = HEADER_LEN + TRAILER_LEN;

/// Durably replace `dir/META` with an envelope holding `payload`.
///
/// # Errors
/// Any swap step's I/O failure, unchanged — the catalog caller treats a
/// failed swap as fatal (§8.4); there is no partial-success state to
/// continue from. `InvalidInput` when `payload` exceeds the `u32` length
/// field.
pub fn write_meta<F: SegmentFs>(fs: &F, dir: &Path, payload: &[u8]) -> io::Result<()> {
    write_envelope(fs, dir, META_STAGING_FILE, META_FILE, payload)
}

/// Read and validate `dir/META`.
///
/// Returns `Ok(None)` when `META` is absent (first boot — empty catalog)
/// and `Ok(Some(payload))` on a valid envelope.
///
/// # Errors
/// `InvalidData`, naming what failed, on bad magic, a short or torn file,
/// an envelope/file length mismatch, or a CRC mismatch — corruption is
/// fail-stop for the caller, never treated as an absent file. Other I/O
/// errors propagate unchanged.
pub fn read_meta<F: SegmentFs>(fs: &F, dir: &Path) -> io::Result<Option<Vec<u8>>> {
    read_envelope(fs, &dir.join(META_FILE))
}

/// The generic swap: stage `staging_name`, fdatasync, rename onto
/// `committed_name`, dir-fsync. On return the new payload survives power
/// loss; a crash at any earlier point leaves the previous committed file
/// (or its absence) intact. Shared by the catalog (`META`) and the
/// per-cell recovery-unit manifest (`MANIFEST` — M2-S11).
///
/// # Errors
/// Any step's I/O failure, unchanged. `InvalidInput` when `payload`
/// exceeds the `u32` length field.
pub fn write_envelope<F: SegmentFs>(
    fs: &F,
    dir: &Path,
    staging_name: &str,
    committed_name: &str,
    payload: &[u8],
) -> io::Result<()> {
    let staged_path = dir.join(staging_name);
    // Step 1 (fault point: stale-staging remove): absent is fine.
    match fs.remove_file(&staged_path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let envelope = encode_envelope(payload)?;
    // Step 2 + 3 (fault points: staging create, staging write). The
    // create carries no durability of its own — the fdatasync below owns
    // the data, the final dir-fsync owns the name (M2-S12: a create-time
    // sync is a wasted device barrier).
    let mut staged = fs.create_meta(&staged_path)?;
    staged.write_at(0, &envelope)?;
    // Step 4 (fault point: staging fdatasync).
    staged.sync_data()?;
    drop(staged);
    // Step 5 (M2-S16 `manifest_rename_fail`: rename — the commit point
    // once durable; a failed swap leaves the old envelope authoritative).
    if inf_foundation::fault::fire(crate::fault::MANIFEST_RENAME_FAIL) {
        return Err(crate::fault::injected(crate::fault::MANIFEST_RENAME_FAIL));
    }
    fs.rename(&staged_path, &dir.join(committed_name))?;
    // Step 6 (M2-S16 `dir_fsync_fail`: the barrier making the name durable).
    if inf_foundation::fault::fire(crate::fault::DIR_FSYNC_FAIL) {
        return Err(crate::fault::injected(crate::fault::DIR_FSYNC_FAIL));
    }
    fs.sync_dir(dir)
}

/// Read and validate one committed envelope file. `Ok(None)` when absent.
///
/// # Errors
/// As [`read_meta`]: corruption is a named `InvalidData`, never `None`.
pub fn read_envelope<F: SegmentFs>(fs: &F, path: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = match fs.open_read(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let len = usize::try_from(file.file_size()?).expect("envelope size fits usize");
    let mut buf = vec![0u8; len];
    let mut read = 0;
    while read < buf.len() {
        let n = file.read_at(read as u64, &mut buf[read..])?;
        if n == 0 {
            return Err(invalid(format!("envelope torn: EOF at {read} of {len} bytes")));
        }
        read += n;
    }
    decode_envelope(&buf)?;
    buf.truncate(len - TRAILER_LEN);
    buf.drain(..HEADER_LEN);
    Ok(Some(buf))
}

/// Validate one envelope image and return its payload slice. Pure — the
/// fuzz seam for every envelope-carried format (frame decoders get their
/// own targets; this covers the META/MANIFEST transport).
///
/// # Errors
/// `InvalidData` naming what failed: short file, bad magic, length
/// mismatch, or CRC mismatch.
pub fn decode_envelope(buf: &[u8]) -> io::Result<&[u8]> {
    let len = buf.len();
    if len < MIN_ENVELOPE_LEN {
        return Err(invalid(format!(
            "envelope too short: {len} bytes, envelope minimum is {MIN_ENVELOPE_LEN}"
        )));
    }
    if buf[..META_MAGIC.len()] != META_MAGIC {
        return Err(invalid(format!("envelope bad magic {:02x?}", &buf[..META_MAGIC.len()])));
    }
    let payload_len =
        u32::from_le_bytes(buf[META_MAGIC.len()..HEADER_LEN].try_into().expect("4-byte slice"))
            as usize;
    let expected = MIN_ENVELOPE_LEN + payload_len;
    if len != expected {
        return Err(invalid(format!(
            "envelope length mismatch: envelope declares a {payload_len}-byte payload \
             ({expected} bytes total), file holds {len}"
        )));
    }
    let (covered, trailer) = buf.split_at(len - TRAILER_LEN);
    let stored = u32::from_le_bytes(trailer.try_into().expect("4-byte trailer"));
    let computed = crc32c(covered);
    if stored != computed {
        return Err(invalid(format!(
            "envelope CRC mismatch: stored {stored:#010x}, computed {computed:#010x}"
        )));
    }
    Ok(&covered[HEADER_LEN..])
}

pub(crate) fn encode_envelope(payload: &[u8]) -> io::Result<Vec<u8>> {
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "envelope payload exceeds the u32 length field")
    })?;
    let mut envelope = Vec::with_capacity(MIN_ENVELOPE_LEN + payload.len());
    envelope.extend_from_slice(&META_MAGIC);
    envelope.extend_from_slice(&payload_len.to_le_bytes());
    envelope.extend_from_slice(payload);
    let crc = crc32c(&envelope);
    envelope.extend_from_slice(&crc.to_le_bytes());
    Ok(envelope)
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
