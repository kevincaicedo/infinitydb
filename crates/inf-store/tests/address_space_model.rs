//! M4-S01 AC: the address space vs a shadow model — random
//! alloc/advance/resolve/rewrite sequences must never mislocate a record
//! across watermark moves, and the byte-exact accounting identities must
//! hold after every op (allocated = tail − origin; dead = seal holes;
//! committed = page window). The ADR-0052 arithmetic (ring-top seal, page
//! commit/decommit windows) is replicated here as *specification*, so a
//! divergence is an implementation bug, not a tautology.
//!
//! Campaign row: `cargo test -p inf-store --release --test
//! address_space_model` runs the 10⁶-op storm (the AC number); the
//! proptest wrapper fuzzes seeds at CI scale.

use inf_store::{AddrClass, AddressSpace, AddressSpaceConfig, LogicalAddr};
use proptest::prelude::*;

const RING: u64 = 1 << 20;
const PAGE: u64 = 1 << 12;

/// One live-or-cold record the model knows about (kept sorted by addr —
/// allocation order is address order).
struct Rec {
    addr: u64,
    len: u64,
    stamp: u8,
    /// Present while the record is RAM-resident (addr ≥ head).
    bytes: Option<Vec<u8>>,
}

/// The shadow model: watermarks, page-window arithmetic, and record bytes.
struct Model {
    origin: u64,
    head: u64,
    flushed: u64,
    ro_boundary: u64,
    tail: u64,
    commit_floor: u64,
    commit_top: u64,
    hole_bytes: u64,
    hole_count: u64,
    records: Vec<Rec>,
    /// Index of the first record with `addr ≥ head` (monotone cursor).
    first_ram: usize,
}

impl Model {
    fn new(origin: u64) -> Model {
        Model {
            origin,
            head: origin,
            flushed: origin,
            ro_boundary: origin,
            tail: origin,
            commit_floor: 0,
            commit_top: 0,
            hole_bytes: 0,
            hole_count: 0,
            records: Vec::new(),
            first_ram: 0,
        }
    }

    fn page_floor(rel: u64) -> u64 {
        rel & !(PAGE - 1)
    }

    fn page_ceil(rel: u64) -> u64 {
        (rel + PAGE - 1) & !(PAGE - 1)
    }

    /// ADR-0052 D2 seal arithmetic — the spec the space must match.
    fn expected_alloc(&self, len: u64) -> (u64, u64) {
        let rel_tail = self.tail - self.origin;
        let ring_offset = rel_tail & (RING - 1);
        let hole = if ring_offset + len > RING { RING - ring_offset } else { 0 };
        (self.tail + hole, hole)
    }

    /// ADR-0052 D1/D3 window arithmetic: would `len` exceed the ring?
    fn alloc_would_overflow(&self, len: u64) -> bool {
        let (addr, _) = self.expected_alloc(len);
        let new_top = Self::page_ceil(addr + len - self.origin).max(self.commit_top);
        new_top - self.commit_floor > RING
    }

    fn classify(&self, addr: u64) -> AddrClass {
        if addr >= self.ro_boundary {
            AddrClass::Mutable
        } else if addr >= self.head {
            AddrClass::ReadOnly
        } else {
            AddrClass::Cold
        }
    }

    /// Record-start advancement candidates inside `[from, to]`, plus `to`.
    fn boundary_in(&self, from: u64, to: u64, roll: u64) -> u64 {
        let lo = self.records.partition_point(|r| r.addr < from);
        let hi = self.records.partition_point(|r| r.addr <= to);
        let choices = (hi - lo) + 1; // + the range top itself
        let pick = (roll % choices as u64) as usize;
        if pick == choices - 1 { to } else { self.records[lo + pick].addr }
    }

    fn drop_bytes_below_head(&mut self) {
        while self.first_ram < self.records.len() && self.records[self.first_ram].addr < self.head {
            self.records[self.first_ram].bytes = None;
            self.first_ram += 1;
        }
    }
}

/// Deterministic fill pattern: a record's bytes are a function of its
/// address and mutation stamp, so verification needs no extra state.
fn fill(bytes: &mut [u8], addr: u64, stamp: u8) {
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (addr as u8) ^ (stamp.wrapping_mul(31)) ^ (i as u8);
    }
}

fn check_identities(space: &AddressSpace, model: &Model) {
    assert_eq!(space.head().to_raw(), model.head, "head drift");
    assert_eq!(space.flushed().to_raw(), model.flushed, "flushed drift");
    assert_eq!(space.ro_boundary().to_raw(), model.ro_boundary, "ro_boundary drift");
    assert_eq!(space.tail().to_raw(), model.tail, "tail drift");
    let report = space.report();
    assert_eq!(report.allocated_bytes, model.tail - model.origin, "allocated identity");
    assert_eq!(report.dead_bytes, model.hole_bytes, "dead = seal holes (S01 scope)");
    assert_eq!(
        report.committed_bytes,
        model.commit_top - model.commit_floor,
        "committed page-window identity"
    );
    let counters = space.counters();
    assert_eq!(counters.seal_holes, model.hole_count, "seal count drift");
    assert_eq!(counters.seal_hole_bytes, model.hole_bytes, "seal bytes drift");
}

fn run_storm(seed: u64, ops: usize) {
    let origin = if seed.is_multiple_of(3) { 0 } else { (seed % (1 << 30)) & !(PAGE - 1) };
    let mut space = AddressSpace::new(AddressSpaceConfig {
        reserve_bytes: RING as usize,
        page_bytes: PAGE as usize,
        life_origin: LogicalAddr::from_raw(origin).expect("origin fits"),
    })
    .expect("reservation");
    let mut model = Model::new(origin);
    let mut x = seed | 1;
    let mut rand = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for op in 0..ops {
        match rand() % 100 {
            // Tail allocation + pattern fill.
            0..=44 => {
                let len = match rand() % 16 {
                    0 => 1 + rand() % (64 * 1024), // page-spanning tail
                    1..=3 => 1 + rand() % 8192,
                    _ => 1 + rand() % 400,
                };
                if model.alloc_would_overflow(len) {
                    assert!(space.alloc(len as usize).is_none(), "op {op}: overflow not refused");
                    continue;
                }
                let (want_addr, hole) = model.expected_alloc(len);
                let addr = space.alloc(len as usize).expect("model says it fits");
                assert_eq!(addr.to_raw(), want_addr, "op {op}: alloc address diverged");
                let mut bytes = vec![0u8; len as usize];
                fill(&mut bytes, want_addr, 0);
                space.bytes_mut(addr, len as usize).copy_from_slice(&bytes);
                model.hole_bytes += hole;
                model.hole_count += u64::from(hole > 0);
                model.tail = want_addr + len;
                model.commit_top =
                    Model::page_ceil(model.tail - model.origin).max(model.commit_top);
                model.records.push(Rec { addr: want_addr, len, stamp: 0, bytes: Some(bytes) });
            }
            // Resolve + content verification of a random known record.
            45..=69 => {
                if model.records.is_empty() {
                    continue;
                }
                let rec = &model.records[(rand() % model.records.len() as u64) as usize];
                let addr = LogicalAddr::from_raw(rec.addr).expect("fits");
                assert_eq!(space.resolve(addr), model.classify(rec.addr), "op {op}: mislocated");
                if let Some(want) = &rec.bytes {
                    assert_eq!(
                        space.bytes(addr, rec.len as usize),
                        want.as_slice(),
                        "op {op}: RAM bytes diverged"
                    );
                }
            }
            // Seal: advance the ro-boundary to a record boundary.
            70..=79 => {
                let to = model.boundary_in(model.ro_boundary, model.tail, rand());
                space.advance_ro_boundary(LogicalAddr::from_raw(to).expect("fits"));
                model.ro_boundary = to;
            }
            // Flush progress (§3.1: never above the ro-boundary).
            80..=85 => {
                let to = model.boundary_in(model.flushed, model.ro_boundary, rand());
                space.advance_flushed(LogicalAddr::from_raw(to).expect("fits"));
                model.flushed = to;
            }
            // Release below flushed (§3.1: never above).
            86..=91 => {
                let to = model.boundary_in(model.head, model.flushed, rand());
                space.advance_head(LogicalAddr::from_raw(to).expect("fits"));
                model.head = to;
                model.commit_floor =
                    Model::page_floor(model.head - model.origin).max(model.commit_floor);
                model.drop_bytes_below_head();
            }
            // In-place rewrite — mutable region only (M4-S05's contract).
            _ => {
                let lo = model.records.partition_point(|r| r.addr < model.ro_boundary);
                if lo == model.records.len() {
                    continue;
                }
                let pick = lo + (rand() % (model.records.len() - lo) as u64) as usize;
                let rec = &mut model.records[pick];
                rec.stamp = rec.stamp.wrapping_add(1);
                let bytes = rec.bytes.as_mut().expect("mutable region is RAM");
                fill(bytes, rec.addr, rec.stamp);
                let addr = LogicalAddr::from_raw(rec.addr).expect("fits");
                space.bytes_mut(addr, rec.len as usize).copy_from_slice(bytes);
            }
        }
        check_identities(&space, &model);
    }
    // Final sweep: every record still classifies and reads correctly.
    for rec in &model.records {
        let addr = LogicalAddr::from_raw(rec.addr).expect("fits");
        assert_eq!(space.resolve(addr), model.classify(rec.addr), "final classify");
        if let Some(want) = &rec.bytes {
            assert_eq!(space.bytes(addr, rec.len as usize), want.as_slice(), "final bytes");
        }
    }
}

proptest! {
    /// Seed-fuzzed storms at CI scale (256 cases × 3 000 ops ≈ 768k ops
    /// per run); the named 10⁶-op storm below is the AC row.
    #[test]
    fn address_space_matches_shadow_model(seed: u64) {
        run_storm(seed, 3_000);
    }
}

/// The M4-S01 AC storm: 10⁶ random alloc/resolve/advance/rewrite ops vs
/// the shadow model, one seed, deterministic.
#[test]
fn address_space_storm_million_ops() {
    let ops = if cfg!(miri) { 3_000 } else { 1_000_000 };
    run_storm(0xC0FFEE, ops);
}
