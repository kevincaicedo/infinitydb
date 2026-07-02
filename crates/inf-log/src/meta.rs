//! Node META-file atomic-swap protocol (M2-S08, ADR-0015 D3): a small
//! whole-file envelope replaced by write-new + fsync + rename + dir-fsync.
//! This is the M2-S11 MANIFEST protocol class, first used by the M2-S08
//! namespace catalog; the payload is opaque bytes (`inf-store` owns the
//! catalog encoding — `inf-log` never knows namespace semantics).
//!
//! ```text
//! envelope := magic: [u8;8] = "INFMETA1"   — version-tagged
//!             payload_len: u32 LE
//!             payload: payload_len bytes
//!             crc: CRC32C(magic · payload_len · payload): u32 LE
//! ```
//!
//! Crash consistency: [`write_meta`] stages the full envelope in
//! [`META_STAGING_FILE`], fdatasyncs it, renames it over [`META_FILE`],
//! and dir-fsyncs — a reader sees either the old envelope or the new one,
//! never a blend. Staging debris from a crash mid-protocol is removed by
//! the next write and never read. Every step's failure propagates: a
//! failed swap is fatal to the caller (§8.4 fail-stop), and a corrupt
//! `META` is a named `InvalidData` error — never silently treated as
//! absent.

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
/// Protocol: remove stale `META.new` (absent is fine), create `META.new`,
/// write the envelope, fdatasync, rename onto `META`, dir-fsync. On return
/// the new payload survives power loss; a crash at any earlier point leaves
/// the previous `META` (or its absence) intact.
///
/// # Errors
/// Any step's I/O failure, unchanged — callers treat a failed swap as
/// fatal (§8.4); there is no partial-success state to continue from.
/// `InvalidInput` when `payload` exceeds the `u32` length field.
pub fn write_meta<F: SegmentFs>(fs: &F, dir: &Path, payload: &[u8]) -> io::Result<()> {
    let staged_path = dir.join(META_STAGING_FILE);
    match fs.remove_file(&staged_path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let envelope = encode_envelope(payload)?;
    let mut staged = fs.create_segment(&staged_path, 0)?;
    staged.write_at(0, &envelope)?;
    staged.sync_data()?;
    drop(staged);
    fs.rename(&staged_path, &dir.join(META_FILE))?;
    fs.sync_dir(dir)
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
    let file = match fs.open_read(&dir.join(META_FILE)) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let len = usize::try_from(file.file_size()?).expect("META size fits usize");
    if len < MIN_ENVELOPE_LEN {
        return Err(invalid(format!(
            "META too short: {len} bytes, envelope minimum is {MIN_ENVELOPE_LEN}"
        )));
    }
    let mut buf = vec![0u8; len];
    let mut read = 0;
    while read < buf.len() {
        let n = file.read_at(read as u64, &mut buf[read..])?;
        if n == 0 {
            return Err(invalid(format!("META torn: EOF at {read} of {len} bytes")));
        }
        read += n;
    }
    if buf[..META_MAGIC.len()] != META_MAGIC {
        return Err(invalid(format!("META bad magic {:02x?}", &buf[..META_MAGIC.len()])));
    }
    let payload_len =
        u32::from_le_bytes(buf[META_MAGIC.len()..HEADER_LEN].try_into().expect("4-byte slice"))
            as usize;
    let expected = MIN_ENVELOPE_LEN + payload_len;
    if len != expected {
        return Err(invalid(format!(
            "META length mismatch: envelope declares a {payload_len}-byte payload \
             ({expected} bytes total), file holds {len}"
        )));
    }
    let (covered, trailer) = buf.split_at(len - TRAILER_LEN);
    let stored = u32::from_le_bytes(trailer.try_into().expect("4-byte trailer"));
    let computed = crc32c(covered);
    if stored != computed {
        return Err(invalid(format!(
            "META CRC mismatch: stored {stored:#010x}, computed {computed:#010x}"
        )));
    }
    buf.truncate(len - TRAILER_LEN);
    buf.drain(..HEADER_LEN);
    Ok(Some(buf))
}

fn encode_envelope(payload: &[u8]) -> io::Result<Vec<u8>> {
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "META payload exceeds the u32 length field")
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
