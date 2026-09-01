//! Review harness: guard-page over-read audit of the `inf-simd` kernels.
//!
//! Goal and method: a 32-byte AVX2 load whose tail handling reads past the
//! end of its input is undefined behaviour even when the extra bytes are
//! masked off, and it faults for real when the input ends on a page
//! boundary. No existing test can see this — a `Vec` almost never ends at a
//! page boundary. This harness maps two pages, revokes all access to the
//! second, places the input so its last byte is the last byte of the first
//! page, and calls each kernel in a forked child. A child that dies on
//! SIGSEGV over-read; a child that exits 0 did not.
//!
//! Every length in `0..=PAGE` is exercised so both the block loop and the
//! tail path are hit at every alignment relative to the guard.

use std::process::abort;

const PAGE: usize = 4096;

/// Maps 2 pages, makes the second inaccessible, and returns a slice of
/// `len` bytes ending exactly at the guard boundary, filled by `fill`.
///
/// # Safety
/// The returned slice borrows a leaked mapping; the caller must not outlive
/// the process. Only used inside forked children that immediately exit.
unsafe fn guarded(len: usize, fill: impl Fn(usize) -> u8) -> &'static [u8] {
    assert!(len <= PAGE);
    // SAFETY: standard anonymous mapping of two pages; the pointer is
    // checked against MAP_FAILED before any dereference.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGE * 2,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED, "mmap failed");
    let base = base.cast::<u8>();
    // SAFETY: `base` is a live 2-page mapping; the second page is in range.
    let rc = unsafe { libc::mprotect(base.add(PAGE).cast(), PAGE, libc::PROT_NONE) };
    assert_eq!(rc, 0, "mprotect failed");
    let start = PAGE - len;
    // SAFETY: `start + len == PAGE`, wholly inside the first, writable page.
    let data = unsafe { std::slice::from_raw_parts_mut(base.add(start), len) };
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = fill(i);
    }
    // SAFETY: same region, reborrowed immutably for the kernel under test.
    unsafe { std::slice::from_raw_parts(base.add(start), len) }
}

/// Runs `body` in a forked child and reports the signal that killed it, if any.
fn signal_of(body: impl FnOnce()) -> Option<i32> {
    // SAFETY: `fork` in a test binary; the child only runs `body` and exits
    // via `_exit`/`abort`, never returning into the test harness.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        body();
        // SAFETY: immediate child exit without running atexit handlers.
        unsafe { libc::_exit(0) };
    }
    let mut status: libc::c_int = 0;
    // SAFETY: `status` is a valid out-pointer for this child pid.
    let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
    assert_eq!(rc, pid, "waitpid failed");
    if libc::WIFSIGNALED(status) { Some(libc::WTERMSIG(status)) } else { None }
}

/// Calls `kernel` on a guard-backed buffer of every length 0..=PAGE and
/// returns the lengths at which the child died, with the killing signal.
fn scan_lengths(name: &str, kernel: fn(&[u8])) -> Vec<(usize, i32)> {
    let mut faults = Vec::new();
    for len in 0..=PAGE {
        let sig = signal_of(|| {
            // SAFETY: the child exits immediately after the call.
            let data = unsafe { guarded(len, |i| (i % 251) as u8) };
            kernel(data);
        });
        if let Some(sig) = sig {
            faults.push((len, sig));
            if faults.len() >= 8 {
                break;
            }
        }
    }
    if !faults.is_empty() {
        eprintln!("{name}: faulted at lengths {faults:?}");
    }
    faults
}

fn k_crc32c(data: &[u8]) {
    std::hint::black_box(inf_simd::crc32c(data));
}
fn k_scan_crlf(data: &[u8]) {
    std::hint::black_box(inf_simd::scan_crlf(data).len());
}
fn k_find_crlf(data: &[u8]) {
    std::hint::black_box(inf_simd::find_crlf(data, 0));
}
fn k_utf8(data: &[u8]) {
    std::hint::black_box(inf_simd::utf8_is_valid(data));
}
fn k_json_structurals(data: &[u8]) {
    let mut out = Vec::new();
    std::hint::black_box(inf_simd::json_scan_structurals(data, &mut out));
}
fn k_json_classify(data: &[u8]) {
    let mut out = Vec::new();
    inf_simd::json_classify_blocks(data, &mut out);
    std::hint::black_box(out.len());
}
fn k_json_copy_unescaped(data: &[u8]) {
    let mut out = Vec::new();
    std::hint::black_box(inf_simd::json_copy_unescaped(data, &mut out));
}
fn k_swar(data: &[u8]) {
    std::hint::black_box(inf_simd::swar_parse_int(data));
}

#[test]
fn rv_simd_kernels_do_not_over_read_past_a_guard_page() {
    // Sanity: the guard itself must work, or the whole test proves nothing.
    let control = signal_of(|| {
        // SAFETY: child exits immediately; the read past the guard is the point.
        let data = unsafe { guarded(16, |_| 0) };
        let past = data.as_ptr().wrapping_add(64);
        // SAFETY: deliberately violated — this is the positive control that
        // proves the guard page faults; it runs only in the forked child.
        std::hint::black_box(unsafe { past.read_volatile() });
        abort();
    });
    assert_eq!(control, Some(libc::SIGSEGV), "guard page is not armed — results would be vacuous");

    type Kernel = (&'static str, fn(&[u8]));
    let kernels: &[Kernel] = &[
        ("crc32c", k_crc32c),
        ("scan_crlf", k_scan_crlf),
        ("find_crlf", k_find_crlf),
        ("utf8_is_valid", k_utf8),
        ("json_scan_structurals", k_json_structurals),
        ("json_classify_blocks", k_json_classify),
        ("json_copy_unescaped", k_json_copy_unescaped),
        ("swar_parse_int", k_swar),
    ];

    let mut report = Vec::new();
    for (name, kernel) in kernels {
        let faults = scan_lengths(name, *kernel);
        if !faults.is_empty() {
            report.push(format!("{name}: over-read at input lengths {faults:?}"));
        }
    }
    assert!(report.is_empty(), "SIMD kernels read past their input:\n{}", report.join("\n"));
}
