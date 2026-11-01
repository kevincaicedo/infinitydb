#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "test-only: filesystem fixtures outside cell code (ADR-0144 D5)"
)]
//! The reclaim oracle's own plants: each half of `reclaim::holdings` goes
//! red on the holding it exists to see, and green once that holding ends —
//! a file left under the root (the path half), a descriptor on an unlinked
//! file (the descriptor half) and a mapping of one (the mapping half). The
//! last two are Linux-only, as the halves are.
#![cfg(unix)]

mod reclaim;

use std::path::{Path, PathBuf};

/// A fresh root under the target's scratch directory, removed on drop.
struct Root(PathBuf);

impl Root {
    fn new(name: &str) -> Root {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("inf-reclaim-oracle-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("cold")).expect("root");
        Root(path)
    }

    /// A file of `len` bytes under the root, written through.
    fn file(&self, name: &str, len: usize) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, vec![0x5au8; len]).expect("write");
        path
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_file_left_under_the_root_is_red_unless_it_is_the_residual() {
    let root = Root::new("stray");
    assert!(reclaim::holdings(&root.0, &[]).is_clean(), "an empty root holds nothing");
    let stray = root.file("stray.bin", 64 << 10);
    let red = reclaim::holdings(&root.0, &[]);
    assert!(!red.is_clean(), "a file outside cold/ the leg did not name: {red:?}");
    assert_eq!(red.strays.len(), 1, "{red:?}");
    assert!(red.blocks_found > 0, "the stray holds blocks: {red:?}");
    let named = reclaim::holdings(&root.0, std::slice::from_ref(&stray));
    assert!(named.is_clean(), "the same file named as the residual: {named:?}");
    reclaim::assert_reclaimed(&root.0, std::slice::from_ref(&stray));
    let absent = reclaim::holdings(&root.0, &[root.0.join("cold/never-written")]);
    assert!(!absent.is_clean(), "a residual the walk does not find: {absent:?}");
    assert_eq!(absent.residual_absent.len(), 1, "{absent:?}");
}

/// The legs' assertion fails, naming the stray, where the report is red.
#[test]
#[should_panic(expected = "stray.bin")]
fn the_legs_assertion_names_what_is_held() {
    let root = Root::new("assertion");
    root.file("stray.bin", 4 << 10);
    reclaim::assert_reclaimed(&root.0, &[]);
}

#[cfg(target_os = "linux")]
#[test]
fn a_descriptor_on_an_unlinked_file_is_red_until_it_closes() {
    let root = Root::new("descriptor");
    let path = root.file("cold/0.iblob", 64 << 10);
    let held = std::fs::File::open(&path).expect("open");
    std::fs::remove_file(&path).expect("unlink");
    let red = reclaim::holdings(&root.0, &[]);
    assert!(!red.is_clean(), "an unlinked file held open: {red:?}");
    assert_eq!(red.descriptors.len(), 1, "{red:?}");
    assert!(red.mappings.is_empty(), "no mapping was made: {red:?}");
    assert_eq!(red.blocks_found, 0, "the walk no longer sees the file: {red:?}");
    drop(held);
    let green = reclaim::holdings(&root.0, &[]);
    assert!(green.is_clean(), "the descriptor closed: {green:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_mapping_of_an_unlinked_file_is_red_until_it_is_unmapped() {
    use std::os::fd::AsRawFd;

    /// The plant's one mapping, unmapped on drop.
    struct Mapping {
        address: *mut libc::c_void,
        length_bytes: usize,
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: `address` and `length_bytes` are exactly the mapping
            // `mmap` returned below, mapped once and unmapped only here; no
            // reference into it exists, since the plant never reads it.
            let rc = unsafe { libc::munmap(self.address, self.length_bytes) };
            assert_eq!(rc, 0, "munmap: {}", std::io::Error::last_os_error());
        }
    }

    let root = Root::new("mapping");
    let length_bytes = 64 << 10;
    let path = root.file("cold/0.iblob", length_bytes);
    let file = std::fs::File::open(&path).expect("open");
    // SAFETY: a null hint lets the kernel choose the address; the length is
    // the file's, non-zero, at offset 0, so the mapping covers only written
    // bytes; the descriptor is open for reading for the call, and the
    // mapping outlives it by design (a held mapping is the plant). The
    // result is checked against MAP_FAILED before use, and is never read.
    let address = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length_bytes,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    assert_ne!(address, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
    let mapping = Mapping { address, length_bytes };
    drop(file);
    std::fs::remove_file(&path).expect("unlink");
    let red = reclaim::holdings(&root.0, &[]);
    assert!(!red.is_clean(), "an unlinked file still mapped: {red:?}");
    assert_eq!(red.mappings.len(), 1, "{red:?}");
    assert!(red.descriptors.is_empty(), "its descriptor closed: {red:?}");
    drop(mapping);
    let green = reclaim::holdings(&root.0, &[]);
    assert!(green.is_clean(), "the mapping ended: {green:?}");
}
