//! The tier lifetimes' boot reservation (ADR-0148 D4), read by the
//! counting allocator on the test's thread. The success leg requests
//! exactly the bill this file computes from each type's size and the
//! tables' lengths, allocates its three `Arc` headers last, and frees every
//! byte at its retirement; at 1, 2 and 16,384 cells the bill grows by
//! exactly the per-cell term a cell. Each allocation before the headers is
//! refused in turn, each in a child of this binary, through the control
//! plane's assembly: the boot answers a typed refusal naming the term, no
//! control plane is built, and every granted byte comes back. A child that
//! dies by a signal fails the parent's test.

#![allow(clippy::disallowed_methods, reason = "test target: a child's environment and its process")]
#![allow(clippy::disallowed_types, reason = "test target: plain collections")]

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};

use inf_alloc::CountingAllocator;
use inf_foundation::CellCount;
use inf_foundation::grants::{GrantDirectory, POSITIONS, ReceiptBoard, RetireBoard};
use inf_server::{
    BootTerm, CellPart, CkptSpace, ControlBootRefused, ControlHandle, RetireJob, RetireLane,
    RetireResult, StdSegmentFile, StdSegmentFs, TierLifetimeBoot,
};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::new();

/// The cells of each refusal leg: every allocation refused in turn.
const REFUSAL_CELLS: [u16; 2] = [1, 2];

/// The parent's test name, which each child re-runs alone.
const NAME: &str = "boot_reservation_refusals_are_typed";

/// A child's leg, `cells:ordinal` (the allocation it refuses).
const LEG: &str = "INF_TIER_BOOT_LEG";

/// `SIGABRT`'s number on every supported host.
const SIGABRT: i32 = 6;

type J = StdSegmentFile;

/// This thread's (allocations, requested bytes, freed bytes).
fn counters() -> (u64, u64, u64) {
    (ALLOCATOR.thread_allocations(), ALLOCATOR.thread_bytes(), ALLOCATOR.thread_freed_bytes())
}

/// The bytes one retained ring of `T` requests (its slot block and header).
fn ring_bytes<T>() -> usize {
    let mut ring = inf_fabric::RetainedRing::<T>::admit().expect("within the bound");
    let bytes = ring.requested_bytes();
    ring.cancel();
    let phase = ring.retire_step(&mut inf_fabric::RingSteps::new(1), drop);
    assert!(matches!(phase, inf_fabric::RingPhase::Terminal(_)), "an unbuilt ring retires");
    bytes
}

/// The bill this test computes, `(node, per cell)`: the directory's slots;
/// a cell's receipts, its writer flag, its `closing`, its lane, its part and
/// its two rings.
fn bill() -> (usize, usize) {
    let node = POSITIONS * size_of::<AtomicU64>();
    let cell = POSITIONS * size_of::<AtomicU32>()
        + size_of::<AtomicBool>()
        + size_of::<AtomicBool>()
        + size_of::<RetireLane<J>>()
        + size_of::<CellPart<J>>()
        + ring_bytes::<RetireJob<J>>()
        + ring_bytes::<RetireResult>();
    (node, cell)
}

/// The three `Arc` headers' bytes, made last: each its strong and weak
/// counts and its value.
fn headers() -> [usize; 3] {
    let counts = 2 * size_of::<usize>();
    [
        counts + size_of::<GrantDirectory>(),
        counts + size_of::<ReceiptBoard>(),
        counts + size_of::<RetireBoard>(),
    ]
}

/// The success leg at `cells`: requested bytes equal the bill plus the
/// headers, `6 + 4 × cells` refusable allocations then the three headers,
/// and every byte freed once the host retires the transport. Returns the
/// bytes the reservation requested.
fn success(cells: u16) -> u64 {
    let before = counters();
    let boot = TierLifetimeBoot::reserve::<J>(cells).expect("the reservation");
    let built = counters();
    let (node, cell) = bill();
    let headers = headers();
    let want = node + usize::from(cells) * cell + headers.iter().sum::<usize>();
    let requested = built.1 - before.1;
    assert_eq!(
        requested, want as u64,
        "BILL_MISMATCH: {cells} cells requested {requested} bytes, the bill is {want}"
    );
    let allocations = built.0 - before.0;
    assert_eq!(allocations, 6 + 4 * u64::from(cells) + 3, "{cells} cells: allocations");
    assert_eq!(built.2, before.2, "{cells} cells: the reservation frees nothing");
    drop(boot.into_detached());
    let after = counters();
    assert_eq!(after.2 - before.2, requested, "{cells} cells: every byte freed at retirement");
    requested
}

/// The success leg at 1, 2 and 16,384 cells; the bill grows by exactly the
/// per-cell term a cell (the control), and the measured terms are reported
/// beside it.
#[test]
fn the_reservation_requests_exactly_its_bill() {
    let one = success(1);
    let two = success(2);
    let full = success(16_384);
    let (_, cell) = bill();
    assert_eq!(two - one, cell as u64, "a cell adds the per-cell bill");
    assert_eq!(full - one, 16_383 * cell as u64, "16,384 cells: the per-cell bill each");
    let terms = inf_server::tier_measured_terms();
    println!(
        "boot reservation: {one} B at 1 cell, {full} B at 16,384 ({cell} B a cell); position \
         table {} B a cell, writer book {} B a node",
        terms.position_table_bytes, terms.writer_book_bytes
    );
}

/// The term the refusal of allocation `ordinal` names.
fn term_of(ordinal: u64) -> BootTerm {
    use inf_fabric::RingBlock;
    use inf_foundation::grants::GrantsPayload;
    match ordinal {
        0 => BootTerm::Payload(GrantsPayload::Directory),
        1 => BootTerm::Payload(GrantsPayload::Receipts),
        2 => BootTerm::Payload(GrantsPayload::ReceiptsTaken),
        3 => BootTerm::Payload(GrantsPayload::Closing),
        4 => BootTerm::Lanes,
        5 => BootTerm::Parts,
        ring => match (ring - 6) % 4 {
            0 => BootTerm::RequestRing(RingBlock::Slots),
            1 => BootTerm::RequestRing(RingBlock::Header),
            2 => BootTerm::ResultRing(RingBlock::Slots),
            _ => BootTerm::ResultRing(RingBlock::Header),
        },
    }
}

/// One child's leg: the boot over `cells` cells with allocation `ordinal`
/// of its reservation refused, through the control plane's assembly.
fn child(cells: u16, ordinal: u64) {
    let space = CkptSpace::full(CellCount::new(cells).expect("a valid cell count"));
    let before = counters();
    let refusal = ALLOCATOR.refuse_after(ordinal).expect("no refusal window open");
    let boot = ControlHandle::detached_in::<StdSegmentFs>(None, space, 0);
    assert!(refusal.was_triggered(), "VACUOUS: refusal {ordinal} never met an allocation");
    drop(refusal);
    let refused = match boot {
        Err(ControlBootRefused::TierReservation(refused)) => refused,
        Err(other) => panic!("leg {cells}:{ordinal}: refused for another reason: {other}"),
        Ok(_) => panic!(
            "CELL_BUILT_AFTER_REFUSAL: leg {cells}:{ordinal}: the assembly built its control \
             plane after a refused reservation"
        ),
    };
    assert_eq!(refused.term, term_of(ordinal), "leg {cells}:{ordinal}: the term named");
    let after = counters();
    let refused_bytes = refused.bytes.expect("a bounded request") as u64;
    let granted = after.1 - before.1 - refused_bytes;
    assert_eq!(after.2 - before.2, granted, "leg {cells}:{ordinal}: every granted byte back");
    println!("BOOT_RESERVATION_REFUSED: {cells}:{ordinal} {:?}", refused.term);
}

/// Each allocation of the reservation before its headers refused in turn,
/// at 1 and 2 cells, each in a child of this binary (and four named ones at
/// 16,384 cells): every child exits 0 having printed its refusal's term. A
/// child that dies by a signal (an allocation the reservation could not
/// refuse aborts) fails this test, naming its leg and the signal.
#[test]
#[cfg(unix)]
fn boot_reservation_refusals_are_typed() {
    use std::os::unix::process::ExitStatusExt as _;
    if let Ok(leg) = std::env::var(LEG) {
        let (cells, ordinal) = leg.split_once(':').expect("cells:ordinal");
        child(cells.parse().expect("cells"), ordinal.parse().expect("ordinal"));
        return;
    }
    let mut legs: Vec<(u16, u64)> = REFUSAL_CELLS
        .iter()
        .flat_map(|&cells| (0..6 + 4 * u64::from(cells)).map(move |ordinal| (cells, ordinal)))
        .collect();
    let last_header = 6 + 4 * 16_384 - 1;
    legs.extend([1, 4, 5, last_header].map(|ordinal| (16_384, ordinal)));
    for (cells, ordinal) in legs {
        let exe = std::env::current_exe().expect("the test binary's path");
        let output = std::process::Command::new(exe)
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(LEG, format!("{cells}:{ordinal}"))
            .output()
            .expect("the child test runs");
        let text = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        if let Some(signal) = output.status.signal() {
            let named = if signal == SIGABRT { "SIGABRT" } else { "a signal" };
            panic!(
                "CHILD_SIGNALED: leg {cells}:{ordinal} died by {named} ({signal}): an allocation \
                 the reservation could not refuse: {text}"
            );
        }
        assert!(output.status.success(), "leg {cells}:{ordinal} failed: {text}");
        let line = format!("BOOT_RESERVATION_REFUSED: {cells}:{ordinal}");
        assert!(text.contains(&line), "VACUOUS: leg {cells}:{ordinal} printed no refusal: {text}");
    }
}

/// A ring layout over the bound is refused before any ring allocation: the
/// reservation of a transport whose job is too large for 65,536 bytes of
/// slots names the request ring's slot block, and every payload allocated
/// before it is freed.
#[test]
fn a_ring_over_its_byte_bound_is_refused_before_it_allocates() {
    use inf_log::fs::SegmentFile;
    /// A file handle of 8 KiB, so eight of its jobs pass the ring's bound.
    struct Huge([u8; 8 << 10]);
    impl SegmentFile for Huge {
        fn write_at(&mut self, _: u64, _: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
        fn read_at(&self, _: u64, _: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
        fn file_size(&self) -> std::io::Result<u64> {
            Ok(self.0.len() as u64)
        }
        fn truncate(&mut self, _: u64) -> std::io::Result<()> {
            Ok(())
        }
        fn sync_data(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }
        fn fully_allocated(&self) -> std::io::Result<bool> {
            Ok(true)
        }
    }
    impl inf_server::FinalClose for Huge {
        fn final_close(self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let before = counters();
    let refused = TierLifetimeBoot::reserve::<Huge>(1).map(|boot| drop(boot.into_detached()));
    let refused = refused.expect_err("a slot block over the bound");
    assert_eq!(refused.term, BootTerm::RequestRing(inf_fabric::RingBlock::Slots));
    assert!(refused.bytes.is_some_and(|bytes| bytes > 65_536), "{refused:?}");
    let after = counters();
    assert_eq!(after.1 - before.1, after.2 - before.2, "every payload freed: {refused:?}");
    assert_eq!(after.0 - before.0, 6, "the six payloads and tables, no ring block");
}
