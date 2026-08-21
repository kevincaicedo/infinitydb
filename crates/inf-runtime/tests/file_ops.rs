//! File-op conformance (M2-S05, ADR-0013 D1): `LogWrite` + linked/standalone
//! fdatasync against real files, on every real backend — io_uring exercises
//! `IOSQE_IO_LINK` + cancellation, kqueue exercises the synchronous fallback
//! ordering. Same observable contract, asserted by the same suite (the
//! scripted-driver flavor lives in `inf-log/tests/group_commit_loop.rs`).

#![cfg(any(target_os = "macos", all(target_os = "linux", feature = "uring")))]

use std::fs::OpenOptions;
use std::os::fd::IntoRawFd;
use std::path::PathBuf;

use inf_alloc::BufferPool;
use inf_runtime::{
    BackendDriver, Completion, CompletionResult, CompletionToken, IoOp, StableBytes, TokenClass,
    Wait, WriteBarrier,
};

#[cfg(target_os = "macos")]
fn make_driver() -> impl BackendDriver {
    inf_runtime::KqueueDriver::new().expect("kqueue")
}

#[cfg(all(target_os = "linux", feature = "uring"))]
fn make_driver() -> impl BackendDriver {
    inf_runtime::UringDriver::new(64).expect("io_uring")
}

fn temp_file(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("inf-runtime-fileops-{tag}-{}", std::process::id()))
}

fn wtoken(seq: u32) -> CompletionToken {
    CompletionToken::new(TokenClass::LogWrite, seq, 0)
}

fn ftoken(seq: u32) -> CompletionToken {
    CompletionToken::new(TokenClass::Fsync, seq, 0)
}

/// Reap until `want` completions arrived (bounded).
fn reap(driver: &mut impl BackendDriver, pool: &mut BufferPool, want: usize) -> Vec<Completion> {
    let mut out = Vec::new();
    for _ in 0..1000 {
        driver
            .submit_and_reap(
                pool,
                Wait::Park { timeout: Some(std::time::Duration::from_millis(5)) },
                &mut out,
            )
            .expect("submit");
        if out.len() >= want {
            return out;
        }
    }
    panic!("expected {want} completions, got {}: {out:?}", out.len());
}

#[test]
fn log_write_with_linked_fsync_orders_write_before_sync() {
    let path = temp_file("linked");
    let file =
        OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    file.set_len(64 << 10).unwrap();
    let fd = file.into_raw_fd();

    let mut driver = make_driver();
    let mut pool = BufferPool::new(4, 1024);
    let frame = b"IFR1-test-frame-bytes-0123456789".to_vec();
    // SAFETY: `frame` outlives the reap loop below; the op is terminal
    // before this function returns.
    let data = unsafe { StableBytes::new(&frame) };
    driver.push(IoOp::LogWrite {
        fd,
        offset: 4096,
        data,
        token: wtoken(1),
        barrier: WriteBarrier::LinkedFsync { fsync_token: ftoken(1) },
    });

    let out = reap(&mut driver, &mut pool, 2);
    assert!(matches!(
        (&out[0].result, out[0].token.class()),
        (CompletionResult::LogWritten, TokenClass::LogWrite)
    ));
    assert!(
        matches!(
            (&out[1].result, out[1].token.class()),
            (CompletionResult::Synced, TokenClass::Fsync)
        ),
        "the linked fdatasync completes after — never before — its write: {out:?}"
    );

    let written = std::fs::read(&path).unwrap();
    assert_eq!(&written[4096..4096 + frame.len()], &frame[..]);
    drop(frame);
    // SAFETY: fd came from into_raw_fd above; closing it exactly once.
    unsafe { libc::close(fd) };
    std::fs::remove_file(&path).ok();
}

#[test]
fn failed_write_cancels_the_linked_fsync() {
    let path = temp_file("failw");
    std::fs::write(&path, b"ro").unwrap();
    // Read-only fd: the write must fail, and the linked fsync with it.
    let fd = OpenOptions::new().read(true).open(&path).unwrap().into_raw_fd();

    let mut driver = make_driver();
    let mut pool = BufferPool::new(4, 1024);
    let frame = b"never lands".to_vec();
    // SAFETY: `frame` outlives the reap loop; the op is terminal before return.
    let data = unsafe { StableBytes::new(&frame) };
    driver.push(IoOp::LogWrite {
        fd,
        offset: 0,
        data,
        token: wtoken(2),
        barrier: WriteBarrier::LinkedFsync { fsync_token: ftoken(2) },
    });

    let out = reap(&mut driver, &mut pool, 2);
    match (&out[0].result, out[0].token.class()) {
        (CompletionResult::Error { errno, .. }, TokenClass::LogWrite) => {
            assert_eq!(*errno, libc::EBADF, "write on a read-only fd");
        }
        other => panic!("expected the write error first, got {other:?}"),
    }
    match (&out[1].result, out[1].token.class()) {
        (CompletionResult::Error { errno, .. }, TokenClass::Fsync) => {
            assert_eq!(*errno, libc::ECANCELED, "no sync-past-failed-write (ADR-0013 D1)");
        }
        other => panic!("expected the cancelled fsync second, got {other:?}"),
    }

    // SAFETY: fd came from into_raw_fd above; closing it exactly once.
    unsafe { libc::close(fd) };
    std::fs::remove_file(&path).ok();
}

#[test]
fn standalone_fdatasync_and_sequential_offsets() {
    let path = temp_file("seq");
    let file =
        OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    file.set_len(64 << 10).unwrap();
    let fd = file.into_raw_fd();

    let mut driver = make_driver();
    let mut pool = BufferPool::new(4, 1024);
    // One frame in flight at a time (the staging-lease discipline), offsets
    // append-ordered — the LOG step's exact shape.
    let frames: Vec<Vec<u8>> = (0..4u8).map(|i| vec![b'a' + i; 100]).collect();
    let mut offset = 0u64;
    for (i, frame) in frames.iter().enumerate() {
        // SAFETY: `frame` outlives its op's terminal completion (reaped
        // before the next push).
        let data = unsafe { StableBytes::new(frame) };
        driver.push(IoOp::LogWrite {
            fd,
            offset,
            data,
            token: wtoken(10 + i as u32),
            barrier: WriteBarrier::None,
        });
        let out = reap(&mut driver, &mut pool, 1);
        assert!(matches!(out[0].result, CompletionResult::LogWritten));
        offset += frame.len() as u64;
    }
    driver.push(IoOp::Fdatasync { fd, token: ftoken(99) });
    let out = reap(&mut driver, &mut pool, 1);
    assert!(matches!(out[0].result, CompletionResult::Synced));

    let written = std::fs::read(&path).unwrap();
    for (i, frame) in frames.iter().enumerate() {
        let at = i * 100;
        assert_eq!(&written[at..at + 100], &frame[..], "frame {i} at its reserved offset");
    }
    // SAFETY: fd came from into_raw_fd above; closing it exactly once.
    unsafe { libc::close(fd) };
    std::fs::remove_file(&path).ok();
}

/// A write-through `LogWrite` (M4.5-S34, ADR-0086 D1): `RWF_DSYNC` on an
/// `O_DIRECT` fd from a 4 KiB-aligned source completes `LogWritten` alone
/// — no `Synced` follows — and the bytes are on the file. kqueue executes
/// the same contract as write + fsync. Skips where the filesystem refuses
/// `O_DIRECT` (typed, never a silent fallback).
#[test]
fn write_through_completes_alone_and_lands() {
    let path = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("inf-runtime-fileops-through-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECT);
    }
    let file = match options.open(&path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("skipping: O_DIRECT open refused: {err}");
            return;
        }
    };
    // Pre-written extents (ADR-0086 D4): zero two blocks first so the
    // write-through write is an overwrite of allocated, written storage.
    let mut window = inf_alloc::AlignedBox::new(8192);
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(window.bytes(), 0).expect("pre-zero");
        file.sync_data().expect("commit the mapping");
    }
    window.bytes_mut()[..32].copy_from_slice(b"IFR3-write-through-frame-bytes!!");
    let fd = file.into_raw_fd();

    let mut driver = make_driver();
    let mut pool = BufferPool::new(4, 1024);
    // SAFETY: `window` outlives the reap loop below; the op is terminal
    // before this function returns.
    let data = unsafe { StableBytes::new(&window.bytes()[..4096]) };
    driver.push(IoOp::LogWrite {
        fd,
        offset: 4096,
        data,
        token: wtoken(7),
        barrier: WriteBarrier::WriteThrough,
    });
    let out = reap(&mut driver, &mut pool, 1);
    assert_eq!(out.len(), 1, "write-through is one completion: {out:?}");
    assert!(
        matches!(
            (&out[0].result, out[0].token.class()),
            (CompletionResult::LogWritten, TokenClass::LogWrite)
        ),
        "{out:?}"
    );
    // Nothing else arrives: no Synced rides a write-through frame.
    let mut extra = Vec::new();
    driver
        .submit_and_reap(
            &mut pool,
            Wait::Park { timeout: Some(std::time::Duration::from_millis(20)) },
            &mut extra,
        )
        .expect("submit");
    assert!(extra.is_empty(), "unexpected completions: {extra:?}");

    let written = std::fs::read(&path).unwrap();
    assert_eq!(&written[4096..4096 + 32], b"IFR3-write-through-frame-bytes!!");
    assert!(written[4096 + 32..8192].iter().all(|&b| b == 0));
    drop(window);
    // SAFETY: fd came from into_raw_fd above; closing it exactly once.
    unsafe { libc::close(fd) };
    std::fs::remove_file(&path).ok();
}
