//! The tier cell's lane tests: the round's groups under a real device
//! budget, the cell's turn, the cold window's bound, the lanes.

use super::*;
use crate::durable::ack_class::{AckClass, AckClasses};
use inf_foundation::bounded::CapCensus;
use inf_foundation::fault::FaultSpec;
use inf_foundation::time::Nanos;
use inf_log::TIER_FRAME_BYTES;
use inf_log::fs::TierIoMode;
use inf_log::fs::mem::MemFs;
use inf_log::fs::sim::SimDisk;
use inf_runtime::{ClassSlice, DeviceBudget, DeviceModel, IoClass};
use inf_store::{AddressSpaceConfig, DemotionConfig, KeyHasher};
use inf_store::{Keyspace, StoreConfig, TierSpec};

fn spec() -> TierSpec {
    TierSpec::for_budget(64 << 20)
}

/// A cell's cap census for a tier cell under test.
fn census() -> Rc<CapCensus> {
    CapCensus::new(inf_foundation::CellId(0)).0
}

/// No durable plane: MAINTAIN ticks no reclaim mark.
const NO_DURABLE: Option<&crate::durable::DurableCell<SimDisk>> = None;

/// The op index has 8 token bits, so a wider index is refused, never
/// masked. Pre-fix `round_slot(1, 256)` was `(1 << 8) | 0x100` — lane 1's op
/// 0 — a second op carrying the same completion token.
#[test]
#[should_panic(expected = "round op index is 8-bit")]
fn round_slot_refuses_an_aliasing_index() {
    assert_ne!(round_slot(1, 0), round_slot(1, 256));
}

/// Flush lanes recycle once their namespace has no op in flight, and
/// the reuser continues the round
/// sequence. Before the fix `next_lane` was monotone: with the counter
/// at the width this test starts from, the second `CREATE` was the
/// release assert `tier flush lanes exhausted` (a DDL count over the
/// cell's lifetime as a node kill).
#[test]
fn dropped_namespace_lane_recycles_with_its_round_sequence() {
    let mut cell = TierCell::new(MemFs::new(), 0, PathBuf::from("/shard-0"), census());
    cell.next_lane = inf_runtime::MAX_SLOT >> 8;
    cell.create_ns(NsId(16), &spec());
    let first = &cell.namespaces[0];
    assert_eq!(first.lane, inf_runtime::MAX_SLOT >> 8, "the last fresh lane");
    cell.namespaces[0].round_seq = 7;
    // A keyspace without the namespace: MAINTAIN's reconcile drops it
    // (no round in flight → the lane recycles immediately).
    let ks = Keyspace::new(StoreConfig::default());
    let mut releases = vec![(NsId(16), 1)];
    cell.sync_namespaces(&ks, &mut releases);
    assert!(cell.namespaces.is_empty());
    assert_eq!(cell.free_lanes, vec![(inf_runtime::MAX_SLOT >> 8, 7)]);
    // The next tiered namespace takes the recycled lane — pre-fix this
    // call took lane `MAX_SLOT >> 8 + 1` and panicked.
    cell.create_ns(NsId(17), &spec());
    let second = &cell.namespaces[0];
    assert_eq!(second.lane, inf_runtime::MAX_SLOT >> 8);
    assert_eq!(second.round_seq, 7, "the round sequence continues past the last holder's");
    assert!(cell.free_lanes.is_empty());
    assert_eq!(cell.next_lane, (inf_runtime::MAX_SLOT >> 8) + 1, "no fresh lane was minted");
}

/// Namespaces are kept ascending by id whatever order they are created
/// in: the turn's rotation is the vector's order.
#[test]
fn namespaces_are_kept_in_id_order() {
    let mut cell = TierCell::new(MemFs::new(), 0, PathBuf::from("/shard-0"), census());
    for id in [40, 16, 32, 17] {
        cell.create_ns(NsId(id), &spec());
    }
    let ids: Vec<u32> = cell.namespaces.iter().map(|t| t.ns.0).collect();
    assert_eq!(ids, vec![16, 17, 32, 40]);
    assert_eq!(cell.rotation_start(None), 0);
    assert_eq!(cell.rotation_start(Some(NsId(17))), 2, "the first id above 17");
    assert_eq!(cell.rotation_start(Some(NsId(20))), 2, "a dropped id is passed in the order");
    assert_eq!(cell.rotation_start(Some(NsId(40))), 0, "wrapping past the last");
}

/// A tiered keyspace on `NsId(16)` filled past its demotion threshold
/// and sealed `seal_steps` times, nothing staged. Seal marks sit at
/// page boundaries and the demotion slice is one 4 KiB page, so each
/// step — one flush cut — is one page's records, about 4 KiB.
fn sealed_backlog(seal_steps: u32) -> Keyspace {
    const PAGE: u64 = 4 << 10;
    let demote = DemotionConfig::for_budget(1 << 20, PAGE);
    let reserve_bytes = demote.ring_reserve_bytes().expect("valid budget");
    let space = AddressSpaceConfig {
        reserve_bytes,
        page_bytes: PAGE as usize,
        life_origin: LogicalAddr::ZERO,
    };
    let mut ks = Keyspace::new(StoreConfig::default());
    assert!(ks.materialize_tiered(NsId(16), space, demote, 2048).is_ok());
    let table = ks.tiered_store_mut(NsId(16)).expect("materialized");
    let hasher = KeyHasher::default();
    let mut index = 0u32;
    let mut sealed = 0;
    while sealed < seal_steps {
        let key = format!("round:{index:06}").into_bytes();
        table.insert(&key, &[0x5A; 200], hasher.hash(&key)).expect("fits the window");
        index += 1;
        assert!(index < 100_000, "the fill reaches the demotion threshold");
        if table.demote_due() && table.seal_slice() > 0 {
            sealed += 1;
        }
    }
    ks
}

/// Materializes `ns` on `ks` with a `budget`-byte window of 4 KiB pages
/// and seals at least `sealed_bytes` behind the ro-boundary, one seal
/// step per page, nothing staged: 200 B values under 12 B keys. Returns
/// every inserted record's address in insertion order.
fn sealed_backlog_on(
    ks: &mut Keyspace,
    ns: NsId,
    budget: u64,
    sealed_bytes: u64,
) -> Vec<LogicalAddr> {
    const PAGE: u64 = 4 << 10;
    let demote = DemotionConfig::for_budget(budget, PAGE);
    let reserve_bytes = demote.ring_reserve_bytes().expect("valid budget");
    let space = AddressSpaceConfig {
        reserve_bytes,
        page_bytes: PAGE as usize,
        life_origin: LogicalAddr::ZERO,
    };
    assert!(ks.materialize_tiered(ns, space, demote, 65_536).is_ok());
    fill_sealed(ks.tiered_store_mut(ns).expect("materialized"), sealed_bytes)
}

/// Inserts 200 B values under 12 B keys into `table` and seals as it
/// goes until at least `sealed_bytes` lie behind the ro-boundary; returns
/// every inserted record's address in insertion order.
fn fill_sealed(table: &mut inf_store::TieredTable, sealed_bytes: u64) -> Vec<LogicalAddr> {
    let hasher = KeyHasher::default();
    let mut addrs = Vec::new();
    let mut sealed = 0u64;
    while sealed < sealed_bytes {
        let key = format!("round:{:06}", addrs.len()).into_bytes();
        let addr = table.insert(&key, &[0x5A; 200], hasher.hash(&key)).expect("fits");
        addrs.push(addr);
        assert!(addrs.len() < 1_000_000, "the fill reaches the demotion threshold");
        if table.demote_due() {
            sealed += table.seal_slice();
        }
    }
    addrs
}

/// Registers `name` on `ks` as a durable tiered namespace of `id` with
/// `tier` (the registry entry `sync_namespaces` reads, so the namespace
/// can later be dropped by name) and materializes its table on 1 MiB
/// commit pages.
fn register_tiered(ks: &mut Keyspace, id: NsId, name: &[u8], tier: TierSpec) {
    let spec = inf_store::NsSpec {
        id,
        name: name.to_vec(),
        mode: inf_store::NsMode::Durable,
        fsync: None,
        policy: None,
        maxmemory: None,
        tier: Some(tier),
    };
    ks.ns_create(spec).expect("a fresh tiered namespace");
}

/// A spec whose rounds are 8 MiB (ten writes, three barriers) and whose
/// first seal comes early: a 16 MiB budget, a tenth of it mutable.
fn eight_mib_rounds() -> TierSpec {
    TierSpec {
        mutable_permille: 100,
        maintain_slice_bytes: 8 << 20,
        ..TierSpec::for_budget(16 << 20)
    }
}

/// A sealed keyspace staged into `cell`'s own `SimDisk` pipeline: a
/// real reactor round, the new file's header write first (the
/// `tiered_flush_reactor` recipe). `MemFs` cannot stage one — it has
/// no fds.
fn staged_round(cell: &mut TierCell<SimDisk>) -> Keyspace {
    let mut ks = sealed_backlog(1);
    let table = ks.tiered_store_mut(NsId(16)).expect("materialized");
    let flush = &mut cell.namespaces[0].flush;
    table.stage_flush_round(flush).expect("stage");
    ks
}

/// The tier class's budget as the plane wires it: a real
/// `DeviceBudget`, offered each group at its exact size.
struct BudgetAdmission(DeviceBudget);

impl FlushAdmission for BudgetAdmission {
    fn admit(&mut self, bytes: u64, ops: u64) -> Issue {
        self.0.offer(IoClass::TierFlush, bytes, ops)
    }
}

/// Admission with no durable plane: every offer `Now`, nothing metered.
struct NoBudget;

impl FlushAdmission for NoBudget {
    fn admit(&mut self, _bytes: u64, _ops: u64) -> Issue {
        Issue::Now
    }
}

/// Records every offer made through it, then asks `inner`.
struct Recording<A: FlushAdmission> {
    inner: A,
    offers: Vec<(u64, u64)>,
}

impl<A: FlushAdmission> FlushAdmission for Recording<A> {
    fn admit(&mut self, bytes: u64, ops: u64) -> Issue {
        self.offers.push((bytes, ops));
        self.inner.admit(bytes, ops)
    }
}

/// Answers each offer from a script: `Now` where the script says so.
struct Scripted<S: FnMut(usize) -> bool> {
    grant: S,
    offers: usize,
}

impl<S: FnMut(usize) -> bool> FlushAdmission for Scripted<S> {
    fn admit(&mut self, _bytes: u64, _ops: u64) -> Issue {
        let index = self.offers;
        self.offers += 1;
        if (self.grant)(index) { Issue::Now } else { Issue::NotThisSlice }
    }
}

/// A device budget over `model`'s share at `cells`, with the tier
/// class's declared slice (one group's bytes, the token op bound), as
/// the durable cell builds it.
fn plane_budget(model: DeviceModel, cells: u16) -> BudgetAdmission {
    let mut slices = [ClassSlice { bytes: 0, ops: 0 }; IoClass::COUNT];
    slices[IoClass::TierFlush.index()] =
        ClassSlice { bytes: TIER_GROUP_BYTES_MAX, ops: TIER_ROUND_MAX_OPS };
    BudgetAdmission(DeviceBudget::new(model.share(cells), slices, 2, Nanos(0)))
}

/// The reference-shape model of the binary's budget tests.
fn reference_model() -> DeviceModel {
    DeviceModel {
        write_bytes_per_s: 40_000_000,
        write_ops_per_s: 40_000,
        read_bytes_per_s: 40_000_000,
        read_ops_per_s: 40_000,
    }
}

/// One MAINTAIN's tier pass on `cell`: every round advanced on its
/// completions, then the cell's turn. Returns the ops pushed.
fn pass(
    cell: &mut TierCell<SimDisk>,
    ks: &mut Keyspace,
    now_us: u64,
    admission: &mut impl FlushAdmission,
) -> Vec<IoOp> {
    let stats = &mut cell.flush_stats;
    for t in &mut cell.namespaces {
        let table = ks.tiered_store_mut(t.ns).expect("materialized");
        advance_round(table, t, stats, now_us).expect("no barrier failed");
    }
    let mut ops = Vec::new();
    cell.turn(ks, now_us, &mut ops, admission).expect("the plan is not refused");
    ops
}

/// Executes `ops` on the cell's sim disk as the driver would and feeds
/// each completion back: a write lands whole, a barrier flushes; the op
/// at `fail` (an index into `ops`) fails with `EIO` and lands nothing.
/// Returns the device bytes the disk was handed, attempts included.
fn complete_ops_failing(cell: &mut TierCell<SimDisk>, ops: Vec<IoOp>, fail: Option<usize>) -> u64 {
    let disk = cell.fs.clone();
    let mut bytes = 0u64;
    for (at, op) in ops.into_iter().enumerate() {
        match op {
            IoOp::LogWrite { fd, offset, token, data: _, barrier: _ } => {
                let lane = token.slot() >> 8;
                let index = (token.slot() & 0xFF) as usize;
                let t = cell.namespaces.iter().find(|t| t.lane == lane).expect("lane");
                let view = t.flush.round_op(index);
                bytes += view.bytes.len() as u64;
                if fail == Some(at) {
                    cell.on_flush_completion(token, Some(EIO));
                    continue;
                }
                disk.driver_write_at(fd, offset, view.bytes).expect("sim write");
                cell.on_flush_completion(token, None);
            }
            IoOp::Fdatasync { fd, token } => {
                disk.driver_fdatasync(fd).expect("sim barrier");
                cell.on_flush_completion(token, None);
            }
            IoOp::AcceptArm { .. }
            | IoOp::RecvArm { .. }
            | IoOp::RecvDisarm { .. }
            | IoOp::Send { .. }
            | IoOp::Close { .. }
            | IoOp::TierRead { .. } => {
                panic!("a flush round pushes writes and barriers only")
            }
        }
    }
    bytes
}

fn complete_ops(cell: &mut TierCell<SimDisk>, ops: Vec<IoOp>) -> u64 {
    complete_ops_failing(cell, ops, None)
}

/// The round op index an op's token carries.
fn op_index(op: &IoOp) -> usize {
    match op {
        IoOp::LogWrite { token, .. } | IoOp::Fdatasync { token, .. } => {
            (token.slot() & 0xFF) as usize
        }
        IoOp::AcceptArm { .. }
        | IoOp::RecvArm { .. }
        | IoOp::RecvDisarm { .. }
        | IoOp::Send { .. }
        | IoOp::Close { .. }
        | IoOp::TierRead { .. } => panic!("a flush round pushes writes and barriers only"),
    }
}

/// The lane of each op pushed, in order.
fn lanes_of(ops: &[IoOp]) -> Vec<u32> {
    ops.iter()
        .map(|op| match op {
            IoOp::LogWrite { token, .. } | IoOp::Fdatasync { token, .. } => token.slot() >> 8,
            IoOp::AcceptArm { .. }
            | IoOp::RecvArm { .. }
            | IoOp::RecvDisarm { .. }
            | IoOp::Send { .. }
            | IoOp::Close { .. }
            | IoOp::TierRead { .. } => panic!("a flush round pushes writes and barriers only"),
        })
        .collect()
}

/// Drives passes with the budget refilled every `pass_ns` until the
/// cell holds no round, completing each pass's ops; returns the device
/// bytes and ops handed to the disk. Panics past `passes_max`.
fn flush_to_idle(
    cell: &mut TierCell<SimDisk>,
    ks: &mut Keyspace,
    admission: &mut Recording<BudgetAdmission>,
    pass_ns: u64,
    passes_max: u64,
) -> (u64, u64) {
    let (mut device_bytes, mut device_ops) = (0u64, 0u64);
    for pass_index in 0..passes_max {
        admission.inner.0.refill(Nanos(pass_index * pass_ns));
        let ops = pass(cell, ks, pass_index * pass_ns / 1_000, admission);
        device_ops += ops.len() as u64;
        device_bytes += complete_ops(cell, ops);
        if cell.namespaces.iter().all(|t| t.round.is_none()) && pass_index > 0 {
            return (device_bytes, device_ops);
        }
    }
    panic!("the rounds did not complete in {passes_max} passes");
}

/// Every tier offer is at most one group's bytes and ops, so at most the
/// class cap, and a staged round of 8 MiB (nine batch windows) issues in
/// groups, never as one burst: the budget never reads an unattainable
/// offer, and what it spent is what the disk was handed (ADR-0178 D3's
/// tier row; the oracle's equality at this tier).
#[test]
fn a_round_above_the_class_cap_issues_in_groups_of_at_most_the_cap() {
    const SLICE: u64 = 8 << 20;
    let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
    cell.create_ns(NsId(16), &TierSpec { maintain_slice_bytes: SLICE, ..spec() });
    let mut ks = Keyspace::new(StoreConfig::default());
    sealed_backlog_on(&mut ks, NsId(16), 16 << 20, SLICE);
    let mut admission = Recording { inner: plane_budget(reference_model(), 2), offers: Vec::new() };
    let cap = admission.inner.0.cap(IoClass::TierFlush);
    let (device_bytes, device_ops) =
        flush_to_idle(&mut cell, &mut ks, &mut admission, 10_000_000, 1_000);
    let t = &cell.namespaces[0];
    assert!(cell.flush_stats.rounds >= 1, "engagement: the round completed");
    assert!(t.flush.sealed().is_empty() || t.flush.active().is_some());
    assert!(
        admission.offers.len() >= 9,
        "engagement: nine batch windows among {} offers",
        admission.offers.len()
    );
    for &(bytes, ops) in &admission.offers {
        assert!(bytes <= TIER_GROUP_BYTES_MAX, "an offer of {bytes} B is at most one group");
        assert!(ops <= TIER_GROUP_OPS_MAX as u64, "an offer of {ops} ops is at most one group");
        assert!(bytes <= cap.bytes && ops <= cap.ops, "an offer is at most the class cap");
    }
    let counters = admission.inner.0.counters(IoClass::TierFlush);
    assert_eq!(counters.unattainable, 0, "no tier offer is above the class cap");
    assert_eq!(counters.overrun_bytes, 0);
    assert_eq!(counters.spent_bytes, device_bytes, "spent is what the disk was handed");
    assert_eq!(counters.spent_ops, device_ops);
    assert!(counters.deferrals >= 1, "engagement: the class refilled between groups");
}

/// Two namespaces on one cell over a real budget whose refill per pass
/// is below one 64 KiB slice: the first has a sustained backlog, the
/// second one page. Under the turn the second's ready group is issued
/// within `n × T_c` of its first offer, the bound computed from the
/// model's share (`T_c = (B + 3) / r_c + 2Δ`, `r_c` the class's floor
/// share, `Δ` the pass interval), while the first's group was refused
/// beside it (the engagement the index-order visit of the design's DV-1
/// never gave).
#[test]
fn a_second_namespace_is_granted_within_its_bound_beside_a_backlog() {
    const SLICE: u64 = 64 << 10;
    const PASS_NS: u64 = 10_000_000;
    let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
    let spec = TierSpec { maintain_slice_bytes: SLICE, ..spec() };
    cell.create_ns(NsId(16), &spec);
    cell.create_ns(NsId(17), &spec);
    let mut ks = Keyspace::new(StoreConfig::default());
    sealed_backlog_on(&mut ks, NsId(16), 16 << 20, 6 << 20);
    sealed_backlog_on(&mut ks, NsId(17), 1 << 20, 4 << 10);
    let model = DeviceModel {
        write_bytes_per_s: 2_000_000,
        write_ops_per_s: 100_000,
        read_bytes_per_s: 2_000_000,
        read_ops_per_s: 100_000,
    };
    let mut admission = plane_budget(model, 1);
    // The boot credit (the class holds its cap at boot) is spent as a
    // prior round's: both namespaces start from an empty credit.
    let cap = admission.0.cap(IoClass::TierFlush);
    assert!(matches!(admission.0.offer(IoClass::TierFlush, cap.bytes, 1), Issue::Now));
    // n × T_c on the byte axis, in passes: r_c = share × 4 / (8 × 10).
    let share = model.share(1).write_bytes_per_s;
    let r_c = share * 4 / 80;
    let t_c_ns = (SLICE + 3) * 1_000_000_000 / r_c + 2 * PASS_NS;
    let bound_passes = 2 * t_c_ns / PASS_NS;
    let second_lane = cell.namespaces[1].lane;
    let mut first_offer_pass: Option<u64> = None;
    for pass_index in 0..(bound_passes * 3) {
        admission.0.refill(Nanos(pass_index * PASS_NS));
        let ops = pass(&mut cell, &mut ks, pass_index * PASS_NS / 1_000, &mut admission);
        if first_offer_pass.is_none() && cell.namespaces[1].round.is_some() {
            first_offer_pass = Some(pass_index);
        }
        if lanes_of(&ops).contains(&second_lane) {
            let first = first_offer_pass.expect("offered before granted");
            let stats = &cell.flush_stats;
            assert!(stats.turn_waits >= 1, "engagement: a group was refused");
            assert!(stats.turn_contended >= 1, "engagement: both namespaces were ready in a pass");
            assert!(
                pass_index - first <= bound_passes,
                "the second namespace waited {} passes, past n × T_c = {bound_passes}",
                pass_index - first
            );
            return;
        }
        complete_ops(&mut cell, ops);
    }
    panic!("the second namespace was never granted in {} passes", bound_passes * 3);
}

/// A cold read planned below the active file's readable length is
/// served while a later round's writes are held: its window lies inside
/// the bytes the file holds, so the driver fills it. Round 1 completes;
/// round 2 stages and its group is pushed but never completed (the staged
/// length moves, nothing is written); a record in the last three full
/// frames below the confirmable length is read through the plan.
#[test]
fn a_cold_read_below_the_confirmable_length_fills_its_window_while_a_round_is_held() {
    const SLICE: u64 = 64 << 10;
    let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
    cell.create_ns(
        NsId(16),
        &TierSpec { maintain_slice_bytes: SLICE, tier_io_mode: TierIoMode::Buffered, ..spec() },
    );
    let mut ks = Keyspace::new(StoreConfig::default());
    let addrs = sealed_backlog_on(&mut ks, NsId(16), 1 << 20, 160 << 10);
    let mut admission = NoBudget;
    // Round 1: plan and writes, then barriers, each completed; the third
    // turn completes round 1 and, in the same turn, plans round 2 and
    // pushes its first group, which is held: nothing of it is written.
    for _ in 0..2 {
        let ops = pass(&mut cell, &mut ks, 0, &mut admission);
        complete_ops(&mut cell, ops);
    }
    let held = pass(&mut cell, &mut ks, 0, &mut admission);
    let t = &cell.namespaces[0];
    assert_eq!(cell.flush_stats.rounds, 1, "engagement: round 1 completed");
    let (_, base, data_len2, durable_len, _) = t.flush.active().expect("an active file");
    assert!(durable_len > 0, "engagement: round 1 is durable");
    assert!(
        data_len2 > durable_len && !held.is_empty(),
        "engagement: round 2's writes are held ({} ops; staged {data_len2} B, durable \
         {durable_len} B)",
        held.len()
    );
    let data = inf_log::TIER_FRAME_DATA as u64;
    let confirmable = durable_len / data * data;
    let frames_written = durable_len.div_ceil(data);
    // The record: starts in the last three full frames below the
    // confirmable length and ends inside it.
    let base_raw = base.to_raw();
    let frames_confirmable = confirmable / data;
    let table = ks.tiered_store(NsId(16)).expect("materialized");
    let (addr, len) = addrs
        .iter()
        .rev()
        .map(|&addr| (addr, table.record(addr).encoded_len as u64))
        .find(|&(addr, len)| {
            let start = addr.to_raw() - base_raw;
            start / data + 3 >= frames_confirmable && start + len <= confirmable
        })
        .expect("a record in the last three full frames");
    let first_frame = (addr.to_raw() - base_raw) / data;
    let (fd, _, offset, frames, _) = t.plan_cold_read(addr, len as usize).expect("planned");
    let mut buf = vec![0u8; frames as usize * TIER_FRAME_BYTES];
    let offset = FileOffset::new(offset).expect("a file offset");
    let n = cell.fs.driver_read_at(fd, offset, &mut buf).expect("sim read");
    assert_eq!(
        n,
        buf.len(),
        "the window is filled: a read of a record in frame {first_frame}, below the confirmable \
         length ({confirmable} B, {frames_confirmable} frames), planned {frames} frames while \
         the file holds {frames_written} written frames and {} staged",
        data_len2.div_ceil(data)
    );
}

/// The budget's tier spend is the device bytes and ops the disk was
/// handed, retries and barriers included: a write that fails is unsent
/// again once every sent op is terminal, its retry group is offered and
/// charged like any group, and both attempts count.
#[test]
fn a_retry_group_is_offered_and_charged_like_any_group() {
    let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
    cell.create_ns(NsId(16), &TierSpec { maintain_slice_bytes: 64 << 10, ..spec() });
    let mut ks = Keyspace::new(StoreConfig::default());
    sealed_backlog_on(&mut ks, NsId(16), 1 << 20, 64 << 10);
    let mut admission = Recording { inner: plane_budget(reference_model(), 1), offers: Vec::new() };
    // The first group: the header, the 16-frame batch and the tail frame
    // of a 64 KiB round, one offer. The batch write fails.
    let first = pass(&mut cell, &mut ks, 0, &mut admission);
    assert!(first.len() >= 3, "engagement: the first group holds several writes");
    assert_eq!(admission.offers.len(), 1, "one group, one offer");
    let mut pushed_ops = first.len() as u64;
    let mut device_bytes = complete_ops_failing(&mut cell, first, Some(1));
    for pass_index in 1..200u64 {
        admission.inner.0.refill(Nanos(pass_index * 10_000_000));
        let ops = pass(&mut cell, &mut ks, pass_index * 10_000, &mut admission);
        pushed_ops += ops.len() as u64;
        device_bytes += complete_ops(&mut cell, ops);
        if cell.namespaces[0].round.is_none() {
            break;
        }
    }
    assert!(cell.namespaces[0].round.is_none(), "the round completed");
    assert_eq!(cell.flush_stats.write_retries, 1, "engagement: one retry");
    let retry_offer = admission.offers[1];
    assert_eq!(retry_offer.1, 1, "the retry group is the one failed write: {retry_offer:?}");
    let counters = admission.inner.0.counters(IoClass::TierFlush);
    assert_eq!(counters.spent_bytes, device_bytes, "spent counts both attempts' device bytes");
    assert_eq!(counters.spent_ops, pushed_ops, "spent counts every attempt's ops");
    let offered_ops: u64 = admission.offers.iter().map(|o| o.1).sum();
    assert_eq!(offered_ops, pushed_ops, "every pushed op was offered");
}

/// A plan that fails after staging part of its round (the pipeline
/// rotates at a 4 KiB file capacity and the second file's first
/// directory hold is refused — `tier_dir_open_fail`'s third firing, two
/// holds per creation) keeps the first chunk and its file's seal; those
/// ops are offered and issued like any group, so the spend is exactly
/// what the disk was handed, and the error is not fatal.
#[test]
fn a_failed_plan_issues_what_it_staged() {
    let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
    cell.create_ns(NsId(16), &TierSpec { maintain_slice_bytes: 6 << 10, ..spec() });
    let config = flush_config(&cell, 4 << 10, 6 << 10);
    let fs = cell.fs.clone();
    let t = &mut cell.namespaces[0];
    t.flush = TierFlush::new(fs, config, 0);
    t.flush.set_drive(TierDrive::Reactor);
    let mut ks = sealed_backlog(3);
    let mut admission = Recording { inner: plane_budget(reference_model(), 1), offers: Vec::new() };
    inf_foundation::fault::arm(inf_log::fault::TIER_DIR_OPEN_FAIL, FaultSpec::Nth(3));
    let mut ops = Vec::new();
    let refused = cell.turn(&mut ks, 0, &mut ops, &mut admission);
    inf_foundation::fault::disarm_all();
    let err = refused.expect_err("the second file's creation is refused");
    assert!(!err.is_fatal(), "a refused creation retries next turn: {err}");
    assert!(cell.namespaces[0].round.is_some(), "engagement: the partial round opened");
    assert!(!ops.is_empty(), "the partial round's first group was issued");
    let device_bytes = complete_ops(&mut cell, ops);
    let counters = admission.inner.0.counters(IoClass::TierFlush);
    assert_eq!(counters.spent_bytes, device_bytes, "the issued chunk's device bytes are spent");
}

/// The pipeline `create_ns` builds for `NsId(16)` on `cell`, at a file
/// capacity of `file_capacity` data bytes and `slice_bytes`.
fn flush_config(cell: &TierCell<SimDisk>, file_capacity: u64, slice_bytes: u64) -> TierFlushConfig {
    TierFlushConfig {
        shard_dir: cell.shard_dir.join("ns-16"),
        cell: cell.cell,
        ns: NsId(16),
        mode: spec().tier_io_mode,
        file_capacity,
        slice_bytes,
    }
}

/// The turn's two rules (T3): a refused group makes its namespace the
/// holder and ends the pass — no later namespace offers — and the holder
/// is visited first on the next pass; a granted group moves the visit on,
/// and the next pass starts after the namespace served last.
#[test]
fn a_refused_group_ends_the_pass_and_its_namespace_is_visited_first_next_turn() {
    let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
    cell.create_ns(NsId(16), &spec());
    cell.create_ns(NsId(17), &spec());
    let mut ks = Keyspace::new(StoreConfig::default());
    sealed_backlog_on(&mut ks, NsId(16), 4 << 20, 2 << 20);
    sealed_backlog_on(&mut ks, NsId(17), 4 << 20, 2 << 20);
    let (lane16, lane17) = (cell.namespaces[0].lane, cell.namespaces[1].lane);
    // Two turns under refusal: each plans one namespace (16, then 17) and
    // offers once — the first namespace's group, refused, holds the turn.
    let mut refuse = Scripted { grant: |_| false, offers: 0 };
    assert!(pass(&mut cell, &mut ks, 0, &mut refuse).is_empty());
    assert!(pass(&mut cell, &mut ks, 1, &mut refuse).is_empty());
    assert_eq!(refuse.offers, 2, "one offer per pass: the holder's; the second never offers");
    assert_eq!(cell.turn.holder, Some(NsId(16)));
    assert_eq!(cell.flush_stats.turn_waits, 1, "one refusal episode");
    assert_eq!(cell.flush_stats.turn_contended, 1, "both ready in the second pass");
    assert_eq!(cell.flush_stats.groups_deferred, 2);
    // Grant the first offer only: the holder is visited first and served;
    // the next namespace's group is refused after it.
    let mut grant_first = Scripted { grant: |index| index == 0, offers: 0 };
    let ops = pass(&mut cell, &mut ks, 2, &mut grant_first);
    assert!(!ops.is_empty() && lanes_of(&ops).iter().all(|&l| l == lane16), "the holder's group");
    assert_eq!(grant_first.offers, 2, "the holder, then the next namespace, refused");
    assert_eq!(cell.turn.holder, Some(NsId(17)));
    assert_eq!(cell.turn.served_last, Some(NsId(16)));
    // Grant everything: the holder (17) first, then the rotation goes on.
    let mut grant = Scripted { grant: |_| true, offers: 0 };
    let ops = pass(&mut cell, &mut ks, 3, &mut grant);
    let lanes = lanes_of(&ops);
    assert_eq!(lanes.first(), Some(&lane17), "the holder is visited first");
    assert!(
        lanes.contains(&lane16) || cell.namespaces[0].round.as_ref().is_some_and(|r| r.pending > 0)
    );
    assert_eq!(cell.turn.holder, None);
}

/// The plan rule (T13): the cell plans one round per turn, taken by the
/// first idle namespace with work after the one planned last, in id
/// order, so with three ready namespaces each plans within two further
/// turns and the passed-over ones are counted.
#[test]
fn the_cell_plans_one_round_per_turn_in_id_order() {
    let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
    let mut ks = Keyspace::new(StoreConfig::default());
    for id in [18, 16, 17] {
        cell.create_ns(NsId(id), &spec());
        sealed_backlog_on(&mut ks, NsId(id), 4 << 20, 1 << 20);
    }
    let mut refuse = Scripted { grant: |_| false, offers: 0 };
    let planned = |cell: &TierCell<SimDisk>| -> Vec<u32> {
        cell.namespaces.iter().filter(|t| t.round.is_some()).map(|t| t.ns.0).collect()
    };
    pass(&mut cell, &mut ks, 0, &mut refuse);
    assert_eq!(planned(&cell), vec![16], "the first id plans first");
    assert_eq!(cell.flush_stats.rounds_planned, 1);
    assert_eq!(cell.flush_stats.plan_waits, 2, "two namespaces with work were passed over");
    pass(&mut cell, &mut ks, 1, &mut refuse);
    assert_eq!(planned(&cell), vec![16, 17]);
    assert_eq!(cell.flush_stats.plan_waits, 3);
    pass(&mut cell, &mut ks, 2, &mut refuse);
    assert_eq!(planned(&cell), vec![16, 17, 18], "every namespace planned within n − 1 turns");
    assert_eq!(cell.flush_stats.rounds_planned, 3);
    assert_eq!(cell.flush_stats.plan_waits, 3, "nothing left to pass over");
    assert_eq!(cell.turn.planned_last, Some(NsId(18)));
}

/// The device-model row of the design's adversarial table: the model
/// absent, 1 B/s, the reference share at 1, 8 and 256 cells, an ops rate
/// of 0. Every offer is at most one group on both axes and never above
/// the class cap; with no model every offer is `Now`; at 1 B/s nothing
/// issues and every offer is deferred; with no ops rate the ops axis is
/// never unattainable.
#[test]
fn every_offer_fits_one_group_on_every_device_model() {
    let reference = DeviceModel {
        write_bytes_per_s: 489_000_000,
        write_ops_per_s: 40_000,
        read_bytes_per_s: 489_000_000,
        read_ops_per_s: 40_000,
    };
    let one_byte = DeviceModel {
        write_bytes_per_s: 1,
        write_ops_per_s: 1,
        read_bytes_per_s: 1,
        read_ops_per_s: 1,
    };
    let no_ops_rate = DeviceModel { write_ops_per_s: 0, read_ops_per_s: 0, ..reference };
    let rows: [(&str, Option<(DeviceModel, u16)>); 6] = [
        ("absent", None),
        ("1 B/s", Some((one_byte, 1))),
        ("the reference share at 1 cell", Some((reference, 1))),
        ("the reference share at 8 cells", Some((reference, 8))),
        ("the reference share at 256 cells", Some((reference, 256))),
        ("an ops rate of 0", Some((no_ops_rate, 8))),
    ];
    for (row, model) in rows {
        let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
        cell.create_ns(NsId(16), &TierSpec { maintain_slice_bytes: 8 << 20, ..spec() });
        let mut ks = Keyspace::new(StoreConfig::default());
        sealed_backlog_on(&mut ks, NsId(16), 16 << 20, 8 << 20);
        let mut recorded = Recording { inner: NoBudget, offers: Vec::new() };
        let mut budgeted =
            model.map(|(m, cells)| Recording { inner: plane_budget(m, cells), offers: Vec::new() });
        let mut pushed = 0usize;
        for pass_index in 0..3u64 {
            let ops = match &mut budgeted {
                None => pass(&mut cell, &mut ks, pass_index * 10_000, &mut recorded),
                Some(b) => {
                    b.inner.0.refill(Nanos(pass_index * 10_000_000));
                    pass(&mut cell, &mut ks, pass_index * 10_000, b)
                }
            };
            pushed += ops.len();
            complete_ops(&mut cell, ops);
        }
        let offers = budgeted.as_ref().map_or(&recorded.offers, |b| &b.offers);
        assert!(!offers.is_empty(), "{row}: engagement: offers were made");
        for &(bytes, ops) in offers {
            assert!(
                bytes <= TIER_GROUP_BYTES_MAX && ops <= TIER_GROUP_OPS_MAX as u64,
                "{row}: {bytes} B, {ops} ops"
            );
        }
        let offered_ops: u64 = offers.iter().map(|o| o.1).sum();
        match &budgeted {
            None => assert_eq!(offered_ops, pushed as u64, "{row}: every offer is Now"),
            Some(b) => {
                let cap = b.inner.0.cap(IoClass::TierFlush);
                let counters = b.inner.0.counters(IoClass::TierFlush);
                assert_eq!(counters.unattainable, 0, "{row}: no offer above the cap {cap:?}");
                for &(bytes, ops) in offers {
                    assert!(
                        bytes <= cap.bytes && ops <= cap.ops,
                        "{row}: {bytes} B, {ops} ops under {cap:?}"
                    );
                }
                if row == "1 B/s" {
                    // The class holds its cap at boot, so the boot credit
                    // issues one group; nothing refills after it.
                    assert_eq!(pushed, 1, "{row}: the boot credit issues one group, then nothing");
                    assert!(counters.deferrals >= 2, "{row}: every later offer is deferred");
                    assert!(counters.spent_bytes <= TIER_GROUP_BYTES_MAX, "{row}");
                } else {
                    assert!(pushed >= 1, "{row}: groups issue");
                }
            }
        }
    }
}

/// A writer parked on the tail stall is woken at every MAINTAIN visit of
/// its namespace, the visit whose plan is refused included (a tier file's
/// creation refused by the device), so the typed timeout is always
/// reachable: before this the visit left at the refused stage, ahead of
/// the wake, and a parked writer on a device that kept refusing the
/// file read no timeout at all.
#[test]
fn a_stalled_writer_is_woken_at_a_visit_whose_plan_is_refused() {
    let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
    cell.create_ns(NsId(16), &spec());
    let mut ks = sealed_backlog(2);
    let parked = cell.namespaces[0].stall_waiters.wait(());
    assert!(cell.namespaces[0].stall_waiters.any_waiting(), "engagement: a writer is parked");
    inf_foundation::fault::arm(inf_log::fault::TIER_DIR_OPEN_FAIL, FaultSpec::Nth(1));
    let visited = cell.maintain_ns(&mut ks, 0, NO_DURABLE, false, 0);
    let mut ops = Vec::new();
    let planned = cell.turn(&mut ks, 0, &mut ops, &mut NoBudget);
    inf_foundation::fault::disarm_all();
    assert!(visited.is_ok(), "the visit itself is not refused");
    let err = planned.expect_err("engagement: the plan pass's file creation was refused");
    assert!(!err.is_fatal(), "{err}");
    assert!(cell.namespaces[0].round.is_none(), "nothing staged: the namespace stays idle");
    assert_eq!(cell.namespaces[0].stall_waiters.waiting(), 0, "the parked writer was woken");
    drop(parked);
}

/// ADR-0167 D4: a refused write position opens nothing — no round, no
/// round sequence spent — and the error is fatal, carrying the value.
/// The refusal is planted on the last write, so a check that stopped at
/// the first good position would already have opened the round. The same
/// staged round, unplanted, opens with every op unsent.
#[test]
fn unaddressable_round_opens_nothing() {
    let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
    cell.create_ns(NsId(16), &spec());
    let _ks = staged_round(&mut cell);
    let t = &mut cell.namespaces[0];
    let writes = t.flush.round_write_count();
    assert!(writes >= 2, "a header and at least one data write: {writes}");
    let planted = i64::MAX.cast_unsigned() + 1;
    let seq_before = t.round_seq;
    let refused =
        open_round(
            t,
            7,
            |flush, index| {
                if index + 1 == writes { planted } else { flush.round_op(index).offset }
            },
        );
    let err = refused.expect_err("an unaddressable position refuses the round");
    assert!(err.is_fatal(), "fail-stop, not a retry: {err}");
    let TierFlushError::Unaddressable { offset_bytes, .. } = &err else { panic!("{err}") };
    assert_eq!(*offset_bytes, planted, "the refusal carries the value");
    assert!(t.round.is_none(), "no round opened");
    assert_eq!(t.round_seq, seq_before, "no round sequence spent");
    open_round(t, 7, |flush, index| flush.round_op(index).offset)
        .expect("the unplanted round opens");
    let round = t.round.as_ref().expect("the round is open");
    assert!(round.states.iter().all(|s| *s == OpState::Unsent), "nothing is sent at the open");
    assert_eq!(round.pending, 0);
}

/// A drained round returns its lane too (the `round_drain` path).
#[test]
fn drained_namespace_lane_recycles_at_maintain() {
    let mut cell = TierCell::new(MemFs::new(), 0, PathBuf::from("/shard-0"), census());
    cell.create_ns(NsId(16), &spec());
    cell.namespaces[0].round_seq = 3;
    // Park a finished round on it so the drop routes through the drain.
    cell.namespaces[0].round = Some(FlushRound::new(0, Vec::new(), 3, 0));
    let ks = Keyspace::new(StoreConfig::default());
    let mut releases = vec![(NsId(16), 1)];
    cell.sync_namespaces(&ks, &mut releases);
    assert_eq!(cell.round_drain.len(), 1, "parked until every op is terminal");
    assert!(cell.free_lanes.is_empty(), "not recycled while parked");
    cell.maintain_teardown(|_| false);
    assert!(cell.round_drain.is_empty());
    assert_eq!(cell.free_lanes, vec![(0, 3)]);
    cell.create_ns(NsId(17), &spec());
    assert_eq!(cell.namespaces[0].lane, 0);
    assert_eq!(cell.namespaces[0].round_seq, 3);
}

/// The design's hostile retry: a 64 MiB round (67 writes, 70 ops) whose
/// first write stays in flight while the 65 after it land and the last
/// write fails, then the first fails too. The retry makes writes 0 and
/// 66 unsent again with 65 done writes between them. Every writer of the
/// cursor places it at the first unsent op (or the phase's end), so the
/// group after the retry group starts at write 66 and the round ends.
/// Pre-fix the cursor stayed at write 1 after the retry group's emit and
/// the walk spent every turn's 64 visits on the done run: the round
/// never ended — no fail-stop, no counter, no wake — and the namespace's
/// writers parked on the tail stall to the typed timeout for the life.
#[test]
fn a_retry_with_a_done_run_between_its_failed_writes_completes() {
    let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
    cell.create_ns(NsId(16), &TierSpec { maintain_slice_bytes: 64 << 20, ..spec() });
    let mut ks = Keyspace::new(StoreConfig::default());
    sealed_backlog_on(&mut ks, NsId(16), 128 << 20, 64 << 20);
    let mut admission = NoBudget;
    // Turn 1: the first group. Write 0 stays in flight; the rest land.
    let first = pass(&mut cell, &mut ks, 0, &mut admission);
    let writes = cell.namespaces[0].flush.round_write_count();
    assert!(writes >= 66, "engagement: 64 or more done writes between two failed ones: {writes}");
    assert!(first.len() <= TIER_ISSUE_OPS_PER_TURN, "a turn pushes at most its visits");
    let (held, rest): (Vec<IoOp>, Vec<IoOp>) = first.into_iter().partition(|op| op_index(op) == 0);
    complete_ops(&mut cell, rest);
    // Later turns: write 0 is pending, so no retry fires; every other
    // write issues and lands, except the round's last write, which fails.
    let mut last_failed = false;
    for turn in 1..8u64 {
        let ops = pass(&mut cell, &mut ks, turn, &mut admission);
        if ops.is_empty() {
            break;
        }
        assert!(ops.len() <= TIER_ISSUE_OPS_PER_TURN, "a turn pushes at most its visits");
        let fail_at = ops.iter().position(|op| op_index(op) == writes - 1);
        last_failed |= fail_at.is_some();
        complete_ops_failing(&mut cell, ops, fail_at);
    }
    assert!(last_failed, "engagement: the last write was sent and failed");
    for op in held {
        let IoOp::LogWrite { token, .. } = op else { panic!("write 0 is a write") };
        cell.on_flush_completion(token, Some(EIO));
    }
    // Every sent op is terminal with writes 0 and `writes - 1` failed and
    // every write between them done: the retry fires at the next advance.
    for turn in 8..40u64 {
        let ops = pass(&mut cell, &mut ks, turn, &mut admission);
        assert!(ops.len() <= TIER_ISSUE_OPS_PER_TURN, "a turn pushes at most its visits");
        complete_ops(&mut cell, ops);
        if cell.namespaces[0].round.is_none() {
            assert_eq!(cell.flush_stats.write_retries, 1, "one retry of both failed writes");
            assert_eq!(cell.flush_stats.rounds, 1, "the round completed");
            return;
        }
    }
    let round = cell.namespaces[0].round.as_ref().expect("still open");
    let unsent: Vec<usize> =
        (0..round.states.len()).filter(|&index| round.states[index] == OpState::Unsent).collect();
    panic!(
        "the round never completed: retries {}, cursor {}, pending {}, unsent ops {unsent:?} of \
         {writes} writes",
        cell.flush_stats.write_retries, round.next_unsent, round.pending
    );
}

/// The design's plan row: a write error on the first, a middle and the
/// last group of a multi-group round (8 MiB: a group per batch write).
/// Each is a charged retry — the failed write alone is unsent again once
/// every sent op is terminal, offered at its exact size and charged like
/// any group — and the round completes with the spend equal to the
/// device bytes handed to the disk, both attempts counted.
#[test]
fn a_write_error_on_the_first_a_middle_and_the_last_group_is_a_charged_retry() {
    const SLICE: u64 = 8 << 20;
    #[derive(Debug)]
    enum Pick {
        First,
        Middle,
        Last,
    }
    for pick in [Pick::First, Pick::Middle, Pick::Last] {
        let row = format!("{pick:?} group");
        let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
        cell.create_ns(NsId(16), &TierSpec { maintain_slice_bytes: SLICE, ..spec() });
        let mut ks = Keyspace::new(StoreConfig::default());
        sealed_backlog_on(&mut ks, NsId(16), 16 << 20, SLICE);
        let mut admission =
            Recording { inner: plane_budget(reference_model(), 2), offers: Vec::new() };
        let mut ops = pass(&mut cell, &mut ks, 0, &mut admission);
        let writes = cell.namespaces[0].flush.round_write_count();
        assert!(writes >= 9, "{row}: engagement: a multi-group round ({writes} writes)");
        let target = match pick {
            Pick::First => 0,
            Pick::Middle => writes / 2,
            Pick::Last => writes - 1,
        };
        let target_bytes = cell.namespaces[0].flush.round_op(target).bytes.len() as u64;
        let (mut device_bytes, mut pushed_ops, mut failed) = (0u64, 0u64, false);
        for pass_index in 1..1_000u64 {
            pushed_ops += ops.len() as u64;
            let fail_at =
                if failed { None } else { ops.iter().position(|op| op_index(op) == target) };
            failed |= fail_at.is_some();
            device_bytes += complete_ops_failing(&mut cell, ops, fail_at);
            if cell.namespaces[0].round.is_none() {
                break;
            }
            admission.inner.0.refill(Nanos(pass_index * 10_000_000));
            ops = pass(&mut cell, &mut ks, pass_index * 10_000, &mut admission);
            assert!(
                ops.len() <= TIER_ISSUE_OPS_PER_TURN,
                "{row}: a turn pushes at most its visits"
            );
        }
        assert!(cell.namespaces[0].round.is_none(), "{row}: the round completed");
        assert!(failed, "{row}: engagement: write {target} was sent and failed");
        assert_eq!(cell.flush_stats.write_retries, 1, "{row}: one retry");
        assert!(
            admission.offers.contains(&(target_bytes, 1)),
            "{row}: the retry group is the failed write alone, offered at its size: {:?}",
            admission.offers
        );
        let counters = admission.inner.0.counters(IoClass::TierFlush);
        assert_eq!(counters.spent_bytes, device_bytes, "{row}: both attempts' device bytes");
        assert_eq!(counters.spent_ops, pushed_ops, "{row}: every attempt's ops");
    }
}

/// The design's plan row "a drop in each state": a namespace dropped
/// while its round is prepared (opened, nothing sent), in its writes (one
/// group in flight, the rest unsent) or in its barriers (in flight)
/// drains — no group of it is offered or pushed again, its in-flight ops
/// complete into the parked round, and once every op is terminal the
/// lane recycles and its files join the teardown queue; the unsent ops
/// are never sent.
#[test]
fn a_dropped_namespace_drains_from_every_round_state() {
    #[derive(Debug)]
    enum State {
        Prepared,
        Writes,
        Barriers,
    }
    for state in [State::Prepared, State::Writes, State::Barriers] {
        let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
        cell.create_ns(NsId(16), &TierSpec { maintain_slice_bytes: 8 << 20, ..spec() });
        let mut ks = Keyspace::new(StoreConfig::default());
        sealed_backlog_on(&mut ks, NsId(16), 16 << 20, 8 << 20);
        let in_flight = match state {
            State::Prepared => {
                let ops =
                    pass(&mut cell, &mut ks, 0, &mut Scripted { grant: |_| false, offers: 0 });
                assert!(ops.is_empty(), "{state:?}: nothing granted");
                ops
            }
            State::Writes => {
                let ops = pass(
                    &mut cell,
                    &mut ks,
                    0,
                    &mut Scripted { grant: |index| index == 0, offers: 0 },
                );
                assert!(!ops.is_empty(), "{state:?}: the first group is in flight");
                ops
            }
            State::Barriers => {
                let writes = pass(&mut cell, &mut ks, 0, &mut NoBudget);
                complete_ops(&mut cell, writes);
                let barriers = pass(&mut cell, &mut ks, 1, &mut NoBudget);
                assert!(!barriers.is_empty(), "{state:?}: the barrier group is in flight");
                barriers
            }
        };
        let (lane, seq) = (cell.namespaces[0].lane, cell.namespaces[0].round_seq);
        let round = cell.namespaces[0].round.as_ref().expect("the round is open");
        assert_eq!(round.pending as usize, in_flight.len(), "{state:?}: the in-flight count");
        let unsent = round.states.iter().filter(|s| **s == OpState::Unsent).count();
        assert_eq!(matches!(state, State::Barriers), unsent == 0, "{state:?}: unsent ops");
        // The drop: the keyspace no longer holds the namespace.
        let mut gone = Keyspace::new(StoreConfig::default());
        cell.sync_namespaces(&gone, &mut vec![(NsId(16), 1)]);
        assert!(cell.namespaces.is_empty(), "{state:?}: dropped");
        assert_eq!(cell.round_drain.len(), 1, "{state:?}: the round parks whole");
        let mut ops = Vec::new();
        cell.turn(&mut gone, 2, &mut ops, &mut NoBudget).expect("nothing to plan");
        assert!(ops.is_empty(), "{state:?}: no group of a parked round is pushed");
        cell.maintain_teardown(|_| true);
        let parked = usize::from(!in_flight.is_empty());
        assert_eq!(cell.round_drain.len(), parked, "{state:?}: parked while an op is in flight");
        for op in in_flight {
            let (IoOp::LogWrite { token, .. } | IoOp::Fdatasync { token, .. }) = op else {
                panic!("a flush round pushes writes and barriers only")
            };
            cell.on_flush_completion(token, None);
        }
        assert_eq!(cell.flush_stats.stale_completions, 0, "{state:?}: applied to the parked round");
        cell.maintain_teardown(|_| true);
        assert!(cell.round_drain.is_empty(), "{state:?}: drained once every op is terminal");
        assert_eq!(cell.free_lanes, vec![(lane, seq)], "{state:?}: the lane recycled");
        assert!(cell.teardown.iter().all(|tn| tn.files.is_empty()), "{state:?}: files unlinked");
    }
}

/// The design's turn table: a dropped holder releases the turn (holder
/// ← none), so the next issue pass starts after the namespace served
/// last, not at the first id above the dropped one. Namespace 16 is
/// served, 17 has no work, 18 is refused and holds the turn; then 17
/// gets work and 18 is dropped: the next pass issues 17's group first.
/// Pre-fix the holder stayed `Some(18)` and the pass started at the
/// first id above it, wrapping to 16, which was served again before 17.
#[test]
fn a_dropped_holder_releases_the_turn_to_the_namespace_after_the_one_served_last() {
    let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
    let mut ks = Keyspace::new(StoreConfig::default());
    for (id, name) in [(16, &b"t16"[..]), (17, b"t17"), (18, b"t18")] {
        register_tiered(&mut ks, NsId(id), name, eight_mib_rounds());
        cell.create_ns(NsId(id), &eight_mib_rounds());
    }
    for id in [16, 18] {
        fill_sealed(ks.tiered_store_mut(NsId(id)).expect("materialized"), 8 << 20);
    }
    let (lane16, lane17) = (cell.namespaces[0].lane, cell.namespaces[1].lane);
    // Turn 1: 16 plans and its whole round issues (served last: 16).
    let ops = pass(&mut cell, &mut ks, 0, &mut NoBudget);
    assert!(lanes_of(&ops).iter().all(|&lane| lane == lane16), "16's groups");
    complete_ops(&mut cell, ops);
    assert_eq!(cell.turn.served_last, Some(NsId(16)));
    // Turn 2: 16's writes are done (its barrier group is ready), 17 has
    // no work, 18 plans; the pass starts after 16, passes 17 and is
    // refused at 18, which holds the turn.
    assert!(pass(&mut cell, &mut ks, 1, &mut Scripted { grant: |_| false, offers: 0 }).is_empty());
    assert_eq!(cell.turn.holder, Some(NsId(18)), "engagement: 18 holds the turn");
    assert_eq!(cell.turn.served_last, Some(NsId(16)));
    // 17 gets work; 18 is dropped while it holds the turn.
    fill_sealed(ks.tiered_store_mut(NsId(17)).expect("materialized"), 8 << 20);
    ks.ns_drop(b"t18").expect("registered");
    cell.sync_namespaces(&ks, &mut vec![(NsId(18), 1)]);
    assert_eq!(cell.round_drain.len(), 1, "18's prepared round parks");
    assert_eq!(cell.turn.holder, None, "a dropped holder releases the turn");
    // Turn 3: 17 plans; the pass starts after 16 and serves 17 first.
    let ops = pass(&mut cell, &mut ks, 2, &mut NoBudget);
    assert_eq!(lanes_of(&ops).first(), Some(&lane17), "17, next after the one served last");
}

/// The design's namespaces row: a namespace dropped while it is
/// `planned_last` is passed in the order — the next plan goes to the
/// first id above it, wrapping — and the plan rule holds as before.
#[test]
fn a_dropped_planned_last_passes_the_plan_to_the_first_id_above_it() {
    let mut cell = TierCell::new(SimDisk::new(), 0, PathBuf::from("/shard-0"), census());
    let mut ks = Keyspace::new(StoreConfig::default());
    for (id, name) in [(16, &b"t16"[..]), (17, b"t17"), (18, b"t18")] {
        register_tiered(&mut ks, NsId(id), name, eight_mib_rounds());
        cell.create_ns(NsId(id), &eight_mib_rounds());
        fill_sealed(ks.tiered_store_mut(NsId(id)).expect("materialized"), 8 << 20);
    }
    let mut refuse = Scripted { grant: |_| false, offers: 0 };
    let planned = |cell: &TierCell<SimDisk>| -> Vec<u32> {
        cell.namespaces.iter().filter(|t| t.round.is_some()).map(|t| t.ns.0).collect()
    };
    pass(&mut cell, &mut ks, 0, &mut refuse);
    pass(&mut cell, &mut ks, 1, &mut refuse);
    assert_eq!(planned(&cell), vec![16, 17]);
    assert_eq!(cell.turn.planned_last, Some(NsId(17)));
    ks.ns_drop(b"t17").expect("registered");
    cell.sync_namespaces(&ks, &mut vec![(NsId(17), 1)]);
    assert_eq!(cell.round_drain.len(), 1, "17's prepared round parks");
    pass(&mut cell, &mut ks, 2, &mut refuse);
    assert_eq!(planned(&cell), vec![16, 18], "the first id above the dropped one plans");
    assert_eq!(cell.turn.planned_last, Some(NsId(18)));
    assert_eq!(cell.flush_stats.rounds_planned, 3);
    pass(&mut cell, &mut ks, 3, &mut refuse);
    assert_eq!(cell.flush_stats.rounds_planned, 3, "every live namespace holds a round");
}

/// A durable plane as MAINTAIN's reclaim marks read it: the staged seq, a
/// watermark held at 0, and each seq's ack class from `class_of`.
struct HeldWatermark {
    staged: u64,
    class_of: fn(u64, u64) -> AckClass,
}

impl AckClasses for HeldWatermark {
    fn staged_seq(&self) -> u64 {
        self.staged
    }

    fn durable_seq(&self) -> u64 {
        0
    }

    fn ack_class(&self, seq: u64) -> AckClass {
        (self.class_of)(seq, self.staged)
    }
}

/// A fresh cell with tiered namespace 16 materialized: `(cell, keyspace)`.
fn marks_fixture() -> (TierCell<MemFs>, Keyspace) {
    const PAGE: u64 = 4 << 10;
    let demote = DemotionConfig::for_budget(1 << 20, PAGE);
    let reserve_bytes = demote.ring_reserve_bytes().expect("valid budget");
    let space = AddressSpaceConfig {
        reserve_bytes,
        page_bytes: PAGE as usize,
        life_origin: LogicalAddr::ZERO,
    };
    let mut ks = Keyspace::new(StoreConfig::default());
    assert!(ks.materialize_tiered(NsId(16), space, demote, 64).is_ok());
    let mut cell = TierCell::new(MemFs::new(), 0, PathBuf::from("/shard-0"), census());
    cell.create_ns(NsId(16), &spec());
    (cell, ks)
}

/// `ticks` MAINTAIN ticks, each after the namespace staged one record (its
/// WAL epoch moves), with the watermark held; returns the marks held.
fn tick_marks(class_of: fn(u64, u64) -> AckClass, ticks: u64) -> (TierCell<MemFs>, usize) {
    let (mut cell, mut ks) = marks_fixture();
    let mut ring = inf_log::StagingRing::new(inf_log::StagingConfig::with_capacity(1 << 20));
    for staged in 1..=ticks {
        let table = ks.tiered_store_mut(NsId(16)).expect("materialized");
        table
            .stage_wal(&mut ring, &inf_log::MutationEffect::Delete { ns: NsId(16), key: b"k" })
            .expect("room");
        let durable = HeldWatermark { staged, class_of };
        cell.maintain_ns(&mut ks, 0, Some(&durable), false, staged).expect("maintain");
    }
    let marks = cell.namespaces[0].epoch_marks.len();
    (cell, marks)
}

/// 1,000 ticks inside one barrier period: every earlier write sealed into a
/// written frame with no barrier between them, one ack class, and the
/// tick's own write in the builder. Uncapped, the namespace held a mark per
/// tick, 1,000; one mark per class holds two.
#[test]
fn a_thousand_ticks_inside_one_barrier_period_keep_at_most_two_marks() {
    let written_run = |seq: u64, staged: u64| {
        if seq < staged { AckClass::Frame(staged - 1) } else { AckClass::Builder }
    };
    let (cell, marks) = tick_marks(written_run, 1_000);
    assert!(marks <= 2, "{marks} reclaim marks after 1,000 ticks inside one barrier period");
    let row = cell.census.row("epoch-marks").expect("the marks' row");
    assert_eq!(row.crossings, 0, "every merge was exact");
}

/// 17 ticks, each sealed into a frame with a barrier of its own, the
/// watermark held: 17 ack classes. Uncapped, 17 marks; capped, the cap's 16
/// and one crossing, the row at its cap.
#[test]
fn seventeen_sealed_ticks_keep_the_cap_and_count_one_crossing() {
    let own_frame =
        |seq: u64, staged: u64| if seq < staged { AckClass::Frame(seq) } else { AckClass::Builder };
    let (cell, marks) = tick_marks(own_frame, 17);
    assert!(marks <= 16, "{marks} reclaim marks after 17 sealed ticks with the watermark held");
    let row = cell.census.row("epoch-marks").expect("the marks' row");
    assert_eq!((row.high_water, row.crossings), (16, 1));
}
