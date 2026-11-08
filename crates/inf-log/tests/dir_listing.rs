#![allow(
    clippy::disallowed_methods,
    reason = "test target: real directory fixtures and whole listings, not cell code"
)]
//! The seam's directory listing (ADR-0202 D1–D5): a stream returns one
//! bounded batch at a time and resumes where it stopped. On every tier a
//! name present in the directory from the open to `End` is returned exactly
//! once, byte-equal to the name created, with names removed after their
//! batch returned them; every `More` batch holds a name; a batch holds at
//! most 256 names and 8 KiB of name bytes; a name over 1,024 bytes refuses
//! the listing. The control is the test's own set of created names, which
//! shares no code with any stream.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use inf_log::fs::mem::MemFs;
use inf_log::fs::sim::{SimDisk, SimDiskConfig};
use inf_log::fs::{BatchEnd, DirStream, NameBatch, OpenedDir, SegmentFs, StdSegmentFs};
use inf_log::limits::{LISTING_BATCH_BYTES, LISTING_BATCH_NAMES, LISTING_NAME_BYTES_MAX};
use proptest::prelude::*;

const DIR: &str = "/data/shard-0/ns-17/cold";

/// What one whole listing returned: every name in the order returned, a
/// repeat included, and the names the walk removed after their batch.
struct Walked {
    returned: Vec<Vec<u8>>,
    removed: BTreeSet<Vec<u8>>,
}

/// Lists `dir` to its `End`, checking each batch's two bounds and that a
/// `More` batch holds a name, and removes the `k`-th name returned when
/// `remove[k % len]` is set, after its batch returned it.
fn walk<F: SegmentFs>(fs: &F, dir: &Path, remove: &[bool]) -> Result<Walked, String> {
    let mut stream = match fs.open_listing(dir).map_err(|e| format!("open: {e}"))? {
        OpenedDir::Absent => return Err("the directory is absent".into()),
        OpenedDir::Open(stream) => stream,
    };
    let mut batch = NameBatch::new();
    let mut walked = Walked { returned: Vec::new(), removed: BTreeSet::new() };
    // Bound: each `More` batch returns a name of a finite directory.
    for calls in 0u32.. {
        if calls > 1 << 20 {
            return Err("the stream never ended".into());
        }
        let end = stream.next_batch(&mut batch).map_err(|e| format!("next_batch: {e}"))?;
        let bytes: usize = batch.iter().map(|name| name.len()).sum();
        if batch.len() > LISTING_BATCH_NAMES || bytes > LISTING_BATCH_BYTES {
            return Err(format!("a batch of {} names and {bytes} bytes", batch.len()));
        }
        if end == BatchEnd::More && batch.is_empty() {
            return Err("MORE EMPTY: a More batch holds no name".into());
        }
        let names: Vec<Vec<u8>> = batch.iter().map(|name| name.as_bytes().to_vec()).collect();
        for name in names {
            let index = walked.returned.len();
            if !remove.is_empty() && remove[index % remove.len()] && !walked.removed.contains(&name)
            {
                fs.remove_file(&dir.join(OsStr::from_bytes(&name)))
                    .map_err(|e| format!("remove: {e}"))?;
                walked.removed.insert(name.clone());
            }
            walked.returned.push(name);
        }
        if end == BatchEnd::End {
            // Every later call answers `End` with an empty batch.
            let again = stream.next_batch(&mut batch).map_err(|e| format!("after End: {e}"))?;
            if again != BatchEnd::End || !batch.is_empty() {
                return Err("a call after End returned names".into());
            }
            return Ok(walked);
        }
    }
    unreachable!("the loop returns")
}

/// The oracle: every created name the walk did not remove returned exactly
/// once, every removed name returned at least once (before its removal),
/// and nothing returned that was not created.
fn judge(created: &BTreeSet<Vec<u8>>, walked: &Walked) -> Result<(), String> {
    let mut counts: BTreeMap<&[u8], usize> = BTreeMap::new();
    for name in &walked.returned {
        *counts.entry(name.as_slice()).or_default() += 1;
    }
    for name in counts.keys() {
        if !created.contains(*name) {
            return Err(format!("FOREIGN: {name:?} was never created"));
        }
    }
    for name in created {
        let seen = counts.get(name.as_slice()).copied().unwrap_or(0);
        if walked.removed.contains(name) {
            if seen == 0 {
                return Err(format!("MISSED: removed {name:?} was never returned"));
            }
        } else if seen != 1 {
            return Err(format!("ONCE: untouched {name:?} returned {seen} times"));
        }
    }
    Ok(())
}

fn create_all<F: SegmentFs>(fs: &F, dir: &Path, names: &BTreeSet<Vec<u8>>) {
    fs.create_dir_all(dir).expect("dir");
    for name in names {
        fs.create_meta(&dir.join(OsStr::from_bytes(name))).expect("create");
    }
}

/// A fresh real directory for one case of the std tier's leg.
fn std_dir(case: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "inf-log-listing-{}-{case}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    root.join("cold")
}

fn run_std(names: &BTreeSet<Vec<u8>>, remove: &[bool], case: &str) -> Result<(), String> {
    let dir = std_dir(case);
    create_all(&StdSegmentFs, &dir, names);
    let result = walk(&StdSegmentFs, &dir, remove).and_then(|walked| judge(names, &walked));
    let _ = std::fs::remove_dir_all(dir.parent().expect("root"));
    result
}

fn run_tiers(names: &BTreeSet<Vec<u8>>, remove: &[bool]) -> Result<(), String> {
    let dir = Path::new(DIR);
    let mem = MemFs::new();
    create_all(&mem, dir, names);
    walk(&mem, dir, remove)
        .and_then(|walked| judge(names, &walked))
        .map_err(|e| format!("mem: {e}"))?;
    let sim = SimDisk::new();
    create_all(&sim, dir, names);
    walk(&sim, dir, remove)
        .and_then(|walked| judge(names, &walked))
        .map_err(|e| format!("sim: {e}"))
}

/// A name of 1..=`max` bytes: no `/`, no NUL, never `.` or `..`.
fn any_name(max: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(1u8..=255, 1..=max)
        .prop_map(|bytes| bytes.into_iter().map(|b| if b == b'/' { b'_' } else { b }).collect())
        .prop_filter("not a dot entry", |name: &Vec<u8>| name != b"." && name != b"..")
}

/// A name a real directory takes on every CI host: lowercase ASCII, so a
/// case-insensitive filesystem holds each as its own entry.
fn portable_name(max: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop::sample::select(b"abcdefghijklmnopqrstuvwxyz0123456789-_".to_vec()),
        1..=max,
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// The memory and simulated tiers over arbitrary name bytes (non-UTF-8
    /// among them) up to the 1,024-byte bound, with returned names removed
    /// between batches: every untouched name exactly once, byte-equal.
    #[test]
    fn a_listing_returns_each_untouched_name_once(
        names in prop::collection::btree_set(any_name(LISTING_NAME_BYTES_MAX), 0..700),
        remove in prop::collection::vec(any::<bool>(), 0..16),
    ) {
        prop_assert_eq!(run_tiers(&names, &remove), Ok(()));
    }

    /// The std tier on a real temporary directory, with returned names
    /// removed between batches: POSIX `readdir`'s rule within one open
    /// stream (ADR-0202 D3 (a)), on this host's filesystem.
    #[test]
    fn a_std_listing_returns_each_untouched_name_once(
        names in prop::collection::btree_set(portable_name(255), 0..700),
        remove in prop::collection::vec(any::<bool>(), 0..16),
    ) {
        prop_assert_eq!(run_std(&names, &remove, "prop"), Ok(()));
    }
}

/// `n` distinct names of exactly `len` bytes.
fn names_of(len: usize, n: usize) -> BTreeSet<Vec<u8>> {
    (0..n)
        .map(|i| {
            let mut name = format!("{i:08}").into_bytes();
            name.resize(len, b'x');
            name.truncate(len);
            name
        })
        .collect()
}

/// The batch's two bounds at their edges: names of 8, 32, 255 and 1,024
/// bytes; a directory of exactly 256 names (one batch at the names bound)
/// and of 257 (a full batch, then one carried name); names summing to
/// exactly 8 KiB (the bytes bound) and one past it (a carried name); names
/// of every length from 33 to 64 bytes, so a batch ends at the bytes bound
/// on every remainder `8192 % len`; one-byte names; an empty directory.
#[test]
fn a_listing_ends_its_batches_at_both_bounds() {
    for (len, n) in [(8, 256), (8, 257), (8, 513), (32, 256), (255, 64), (1024, 8), (1024, 17)] {
        let names = names_of(len, n);
        assert_eq!(run_tiers(&names, &[]), Ok(()), "{n} names of {len} bytes");
        if len <= 255 {
            assert_eq!(run_std(&names, &[], "bounds"), Ok(()), "std: {n} names of {len} bytes");
        }
    }
    for len in 33..=64 {
        let names = names_of(len, 300);
        assert_eq!(run_tiers(&names, &[true, false]), Ok(()), "{len}-byte names");
    }
    let one: BTreeSet<Vec<u8>> = (b'a'..=b'z').map(|b| vec![b]).collect();
    assert_eq!(run_tiers(&one, &[]), Ok(()), "one-byte names");
    assert_eq!(run_std(&one, &[], "one"), Ok(()), "std: one-byte names");
    assert_eq!(run_tiers(&BTreeSet::new(), &[]), Ok(()), "an empty directory");
    assert_eq!(run_std(&BTreeSet::new(), &[], "empty"), Ok(()), "std: an empty directory");
}

/// A name over the bound refuses the listing with `InvalidData` naming the
/// directory, and a name at it is listed (ADR-0202 D2). No supported
/// filesystem holds one, so the memory and simulated tiers carry the row.
#[test]
fn a_listing_refuses_an_overlong_name() {
    fn refuse<F: SegmentFs>(fs: &F, tier: &str) {
        let dir = Path::new(DIR);
        let at = vec![b'a'; LISTING_NAME_BYTES_MAX];
        let over = vec![b'b'; LISTING_NAME_BYTES_MAX + 1];
        create_all(fs, dir, &BTreeSet::from([at.clone()]));
        let walked = walk(fs, dir, &[]).unwrap_or_else(|e| panic!("{tier}: {e}"));
        assert_eq!(walked.returned, vec![at], "{tier}: a name at the bound is listed");
        fs.create_meta(&dir.join(OsStr::from_bytes(&over))).expect("create");
        let OpenedDir::Open(mut stream) = fs.open_listing(dir).expect("open") else {
            panic!("{tier}: the directory is present")
        };
        let mut batch = NameBatch::new();
        let err = stream.next_batch(&mut batch).expect_err("an overlong name refuses");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{tier}");
        assert!(err.to_string().contains(DIR), "{tier}: the refusal names the directory: {err}");
        assert!(batch.is_empty(), "{tier}: after an Err the batch holds no name");
    }
    refuse(&MemFs::new(), "mem");
    refuse(&SimDisk::new(), "sim");
}

/// A name that is not UTF-8 is returned as its own bytes on all three tiers
/// (on the std tier where the host's filesystem takes such a name: Linux).
#[test]
fn a_non_utf8_name_round_trips() {
    let names = BTreeSet::from([b"blob-00000\xff.iblob".to_vec(), b"tier-\xc3\x28.itier".to_vec()]);
    assert_eq!(run_tiers(&names, &[]), Ok(()));
    if cfg!(target_os = "linux") {
        assert_eq!(run_std(&names, &[], "utf8"), Ok(()));
    }
}

/// `Absent` is the directory's own `ENOENT`, and `list_dir` reads it as
/// `NotFound`; on the std tier a path that is a file is an `Err`.
#[test]
fn an_absent_directory_opens_absent() {
    fn absent<F: SegmentFs>(fs: &F, dir: &Path) {
        assert!(matches!(fs.open_listing(dir).expect("open"), OpenedDir::Absent));
        assert_eq!(fs.list_dir(dir).expect_err("absent").kind(), std::io::ErrorKind::NotFound);
    }
    absent(&MemFs::new(), Path::new(DIR));
    absent(&SimDisk::new(), Path::new(DIR));
    let dir = std_dir("absent");
    absent(&StdSegmentFs, &dir);
    StdSegmentFs.create_dir_all(dir.parent().expect("root")).expect("root");
    std::fs::write(&dir, b"a file").expect("file");
    assert!(StdSegmentFs.open_listing(&dir).is_err(), "a file is no directory");
    let _ = std::fs::remove_dir_all(dir.parent().expect("root"));
}

/// The simulated disk resumes after the greatest name it returned, a key by
/// identity: a name created behind it is not returned and one created ahead
/// of it is (ADR-0202 D5).
#[test]
fn the_simulated_stream_resumes_after_its_greatest_name() {
    let sim = SimDisk::new();
    let dir = Path::new(DIR);
    create_all(&sim, dir, &names_of(8, 300));
    let OpenedDir::Open(mut stream) = sim.open_listing(dir).expect("open") else {
        panic!("present")
    };
    let mut batch = NameBatch::new();
    assert_eq!(stream.next_batch(&mut batch).expect("first"), BatchEnd::More);
    assert_eq!(batch.len(), LISTING_BATCH_NAMES);
    let greatest = batch.name(batch.len() - 1).expect("a name").as_bytes().to_vec();
    sim.create_meta(&dir.join("00000000a")).expect("behind the key");
    sim.create_meta(&dir.join("zzz")).expect("ahead of the key");
    let mut rest = Vec::new();
    loop {
        let end = stream.next_batch(&mut batch).expect("next");
        rest.extend(batch.iter().map(|name| name.as_bytes().to_vec()));
        if end == BatchEnd::End {
            break;
        }
    }
    assert!(rest.iter().all(|name| name.as_slice() > greatest.as_slice()), "after the key");
    assert!(!rest.contains(&b"00000000a".to_vec()), "a name behind the key is not returned");
    assert!(rest.contains(&b"zzz".to_vec()), "a name ahead of the key is returned");
    assert_eq!(rest.len(), 300 - LISTING_BATCH_NAMES + 1);
}

/// The simulated disk's repeat mode (ADR-0202 D5): at its next batch a
/// stream returns once more, before any new name, each name of its last
/// batch removed since. Every untouched name is still returned exactly
/// once — a repeat moves no key — every removed name of a `More` batch
/// comes back once, and the disk counts the repeats, its opens and its
/// batches.
#[test]
fn the_repeat_mode_returns_removed_names_again_and_moves_no_key() {
    let sim = SimDisk::with_config(SimDiskConfig {
        listing_repeats_removed: true,
        ..SimDiskConfig::default()
    });
    let dir = Path::new(DIR);
    let names = names_of(8, 600);
    create_all(&sim, dir, &names);
    let walked = walk(&sim, dir, &[true, false, false]).expect("the walk");
    assert_eq!(judge(&names, &walked), Ok(()));
    let repeated = walked.returned.len() - names.len();
    assert!(repeated > 0, "VACUOUS: the mode repeated no removed name");
    assert_eq!(sim.listing_repeats_served(), repeated as u64, "the engagement counter");
    let twice = walked.returned.iter().filter(|name| walked.removed.contains(*name)).count();
    assert_eq!(twice - walked.removed.len(), repeated, "each repeat is a removed name, once");
    assert_eq!(sim.listings_opened(), 1);
    assert_eq!(sim.listing_batches(), 4, "600 names and the repeats in four batches");
}
