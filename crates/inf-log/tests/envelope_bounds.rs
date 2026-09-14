//! Review 2026-08-30 F-L02-05: the META/MANIFEST envelope read is bounded
//! **before** it allocates. `read_envelope` sized its buffer from the
//! inode length and looked at magic, length field and CRC only after the
//! whole file was in memory — a multi-GiB `MANIFEST` (a wrong restore, a
//! corrupt inode length) aborted the boot on the allocation instead of
//! the named `InvalidData` fail-stop. Now `MAX_ENVELOPE_LEN` (64 MiB, the
//! log's one-object bound) is checked against `file_size()` first: an
//! oversized file is refused by size, unread.

use std::path::{Path, PathBuf};

use inf_log::fs::StdSegmentFs;
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::meta::{MAX_ENVELOPE_LEN, MIN_ENVELOPE_LEN, read_envelope, write_envelope};
use inf_log::read_manifest;

/// Goal: an envelope file one byte over the bound is refused by its size,
/// and not one byte of it is read. Method: `MemFs`, a `MANIFEST` of
/// `MAX_ENVELOPE_LEN + 1` bytes, the fs read oracle before and after.
#[test]
fn an_oversized_envelope_is_refused_by_size_before_any_read() {
    let fs = MemFs::new();
    let dir = PathBuf::from("data/shard-0");
    fs.create_dir_all(&dir).expect("dir");
    let path = dir.join("MANIFEST");
    drop(fs.create_segment(&path, MAX_ENVELOPE_LEN + 1).expect("oversized file"));
    let reads_before = fs.reads();
    let err = read_envelope(&fs, &path).expect_err("an oversized envelope must not read back");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains(&MAX_ENVELOPE_LEN.to_string()),
        "the refusal names the bound: {err}"
    );
    assert_eq!(fs.reads(), reads_before, "refused by size: nothing was read ({err})");
}

/// Goal: the bound admits every honest envelope — exactly `MAX_ENVELOPE_LEN`
/// bytes round-trips. Method: write the largest payload the bound allows
/// and read it back through the same path.
#[test]
fn an_envelope_at_the_bound_round_trips() {
    let fs = MemFs::new();
    let dir = PathBuf::from("data/shard-0");
    fs.create_dir_all(&dir).expect("dir");
    let payload_len = usize::try_from(MAX_ENVELOPE_LEN).expect("fits") - MIN_ENVELOPE_LEN;
    let payload = vec![0xA5u8; payload_len];
    write_envelope(&fs, &dir, "MANIFEST.new", "MANIFEST", &payload).expect("write");
    let back = read_envelope(&fs, &dir.join("MANIFEST")).expect("read").expect("present");
    assert_eq!(back.len(), payload_len);
    assert!(back.iter().all(|&b| b == 0xA5));
}

struct SparseShard(PathBuf);

impl SparseShard {
    fn new(len: u64) -> Self {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("envelope-bounds-{}", std::process::id()));
        std::fs::create_dir(&path).expect("unique scratch directory");
        let file = std::fs::File::create(path.join("MANIFEST")).expect("sparse MANIFEST");
        file.set_len(len).expect("sparse length");
        Self(path)
    }
}

impl Drop for SparseShard {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove scratch directory");
    }
}

/// Goal: the finding's own regime — a sparse 8 GiB `MANIFEST` on a real
/// filesystem boots into the named refusal, never an allocation of the
/// inode length (the pre-fix tree dies here under a 4 GiB address-space
/// cap: `memory allocation of 8589934592 bytes failed`). Method: the
/// sparse-file tier of `tail_bounds.rs`, through `read_manifest`.
#[test]
fn a_sparse_multi_gib_manifest_is_a_named_refusal_not_an_allocation() {
    let shard = SparseShard::new(8 << 30);
    let err = read_manifest(&StdSegmentFs, &shard.0).expect_err("refused");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("8589934592"), "the refusal names the size: {err}");
    let mut file = StdSegmentFs.open_read(&shard.0.join("MANIFEST")).expect("still there");
    assert_eq!(file.file_size().expect("size"), 8 << 30, "the file is preserved");
    let _ = &mut file;
}
