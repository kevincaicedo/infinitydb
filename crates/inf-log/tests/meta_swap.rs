//! M2-S08 ACs (ADR-0015 D3): the node META atomic-swap protocol round-trips
//! over both fs tiers, rewrites replace cleanly, crash debris (`META.new`)
//! never breaks a subsequent swap, corruption is a named `InvalidData`
//! error — never an absent-file fallback — and a failed staging fsync
//! aborts the swap with the previous `META` intact.

use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs, StdSegmentFs};
use inf_log::meta::{META_FILE, META_STAGING_FILE, read_meta, write_meta};
use std::io;
use std::path::{Path, PathBuf};

fn mem_fs_with_dir() -> (MemFs, PathBuf) {
    let fs = MemFs::new();
    let dir = PathBuf::from("data/node");
    fs.create_dir_all(&dir).expect("dir");
    (fs, dir)
}

/// XOR-flip one byte of a file in place (corruption injection).
fn flip_byte(fs: &MemFs, path: &Path, offset: u64) {
    let mut file = fs.open_write(path).expect("open");
    let mut byte = [0u8; 1];
    assert_eq!(file.read_at(offset, &mut byte).expect("read"), 1);
    file.write_at(offset, &[byte[0] ^ 0xFF]).expect("write");
}

/// Replace a file with the first `keep` bytes of its current contents
/// (a torn write, in MemFs terms).
fn truncate_file(fs: &MemFs, path: &Path, keep: usize) {
    let bytes = fs.contents(path).expect("file exists");
    fs.remove_file(path).expect("remove");
    let mut file = fs.create_segment(path, 0).expect("recreate");
    file.write_at(0, &bytes[..keep]).expect("write prefix");
}

fn read_err(fs: &MemFs, dir: &Path) -> io::Error {
    read_meta(fs, dir).expect_err("corrupt META must not read back")
}

#[test]
fn absent_meta_reads_none() {
    let (fs, dir) = mem_fs_with_dir();
    assert_eq!(read_meta(&fs, &dir).expect("absent is fine"), None);
}

#[test]
fn round_trip_over_mem_fs() {
    let (fs, dir) = mem_fs_with_dir();
    write_meta(&fs, &dir, b"catalog-v1").expect("write");
    assert_eq!(read_meta(&fs, &dir).expect("read").as_deref(), Some(&b"catalog-v1"[..]));
    // The rename consumed the staging file: exactly one entry remains.
    assert_eq!(fs.list_dir(&dir).expect("list"), vec![META_FILE.to_string()]);
}

#[test]
fn empty_payload_round_trips() {
    let (fs, dir) = mem_fs_with_dir();
    write_meta(&fs, &dir, b"").expect("write");
    assert_eq!(read_meta(&fs, &dir).expect("read").as_deref(), Some(&b""[..]));
}

#[test]
fn rewrite_replaces_previous_payload() {
    let (fs, dir) = mem_fs_with_dir();
    write_meta(&fs, &dir, b"catalog-v1").expect("first write");
    write_meta(&fs, &dir, b"catalog-v2, longer than the first").expect("rewrite");
    assert_eq!(
        read_meta(&fs, &dir).expect("read").as_deref(),
        Some(&b"catalog-v2, longer than the first"[..])
    );
}

#[test]
fn stale_staging_debris_does_not_break_the_next_swap() {
    let (fs, dir) = mem_fs_with_dir();
    write_meta(&fs, &dir, b"catalog-v1").expect("first write");
    // Simulated crash between staging create and rename: junk debris.
    let mut debris = fs.create_segment(&dir.join(META_STAGING_FILE), 0).expect("debris");
    debris.write_at(0, b"half-written garbage").expect("junk");
    drop(debris);
    // Debris is ignored by the reader and cleared by the next writer.
    assert_eq!(read_meta(&fs, &dir).expect("read").as_deref(), Some(&b"catalog-v1"[..]));
    write_meta(&fs, &dir, b"catalog-v2").expect("write over debris");
    assert_eq!(read_meta(&fs, &dir).expect("read").as_deref(), Some(&b"catalog-v2"[..]));
    assert_eq!(fs.list_dir(&dir).expect("list"), vec![META_FILE.to_string()]);
}

#[test]
fn flipped_payload_byte_is_a_crc_mismatch() {
    let (fs, dir) = mem_fs_with_dir();
    write_meta(&fs, &dir, b"catalog-v1").expect("write");
    // Payload starts after the 8-byte magic + 4-byte length.
    flip_byte(&fs, &dir.join(META_FILE), 12);
    let err = read_err(&fs, &dir);
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("CRC mismatch"), "got {err}");
}

#[test]
fn bad_magic_is_named() {
    let (fs, dir) = mem_fs_with_dir();
    write_meta(&fs, &dir, b"catalog-v1").expect("write");
    flip_byte(&fs, &dir.join(META_FILE), 0);
    let err = read_err(&fs, &dir);
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("bad magic"), "got {err}");
}

#[test]
fn truncated_file_is_named() {
    let (fs, dir) = mem_fs_with_dir();
    let meta = dir.join(META_FILE);

    // Torn mid-payload: header intact, payload/CRC bytes missing.
    write_meta(&fs, &dir, b"catalog-v1").expect("write");
    truncate_file(&fs, &meta, 18);
    let err = read_err(&fs, &dir);
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("length mismatch"), "got {err}");

    // Torn below the envelope minimum.
    write_meta(&fs, &dir, b"catalog-v1").expect("rewrite");
    truncate_file(&fs, &meta, 7);
    let err = read_err(&fs, &dir);
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("too short"), "got {err}");
}

#[test]
fn trailing_bytes_are_a_length_mismatch() {
    let (fs, dir) = mem_fs_with_dir();
    write_meta(&fs, &dir, b"catalog-v1").expect("write");
    let meta = dir.join(META_FILE);
    let len = fs.contents(&meta).expect("file").len() as u64;
    fs.open_write(&meta).expect("open").write_at(len, b"junk").expect("extend");
    let err = read_err(&fs, &dir);
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("length mismatch"), "got {err}");
}

#[test]
fn fsync_failure_aborts_the_swap_and_preserves_old_meta() {
    let (fs, dir) = mem_fs_with_dir();
    write_meta(&fs, &dir, b"catalog-v1").expect("first write");
    // The staging fdatasync fails: the swap must error out (fail-stop —
    // §8.4) without touching the committed META.
    fs.fail_next_sync_data();
    write_meta(&fs, &dir, b"catalog-v2").expect_err("failed fsync aborts the swap");
    assert_eq!(read_meta(&fs, &dir).expect("read").as_deref(), Some(&b"catalog-v1"[..]));
    // A later swap (post-restart in real life) still goes through.
    write_meta(&fs, &dir, b"catalog-v3").expect("recovery write");
    assert_eq!(read_meta(&fs, &dir).expect("read").as_deref(), Some(&b"catalog-v3"[..]));
}

/// StdSegmentFs smoke: the same protocol against the real filesystem —
/// absent, write, read, rewrite, no staging debris left behind.
#[test]
fn std_fs_round_trip_smoke() {
    let root = std::env::temp_dir().join(format!("inf-log-s08-meta-{}", std::process::id()));
    if root.exists() {
        std::fs::remove_dir_all(&root).expect("clear stale test dir");
    }
    let fs = StdSegmentFs;
    fs.create_dir_all(&root).expect("dir");

    assert_eq!(read_meta(&fs, &root).expect("absent is fine"), None);
    write_meta(&fs, &root, b"catalog-v1").expect("write");
    assert_eq!(read_meta(&fs, &root).expect("read").as_deref(), Some(&b"catalog-v1"[..]));
    write_meta(&fs, &root, b"catalog-v2").expect("rewrite");
    assert_eq!(read_meta(&fs, &root).expect("read").as_deref(), Some(&b"catalog-v2"[..]));
    assert!(!root.join(META_STAGING_FILE).exists(), "rename consumed the staging file");

    std::fs::remove_dir_all(&root).expect("cleanup");
}
