//! The plane's task table (ADR-0177 D2): a task lives as long as the owner it serves. One class
//! per future the plane runs: `conn` (a connection's pump, made at its admission), the three
//! peer pumps (made at plane start, before the cell serves, never ending) and the two MAINTAIN
//! reads (made under a permit, ending with their read). Each class is registered and reserved
//! here from the same function item, so a `Foreign` answer is this module's pairing gone wrong.

use inf_foundation::bounded::{AssemblyFull, Cap, CapError};
use inf_runtime::{CellExecutor, ExecutorLayout, TaskAdmission, TaskClass, TaskId, TaskPermit};

use super::*;
use crate::limits::{GATE_UNITS_PER_CONN_TASK, PeerTaskCaps, PlaneTaskCaps};

/// The six classes' handles and their caps, one field of `Shared`.
pub(super) struct PlaneTasks {
    conn: (TaskClass, Cap),
    peers: Option<PeerTasks>,
    compact: (TaskClass, Cap),
    shadow: (TaskClass, Cap),
}

/// The three peer pumps' classes, at a cell with peers.
struct PeerTasks {
    ns_apply: (TaskClass, Cap),
    owner_pub: (TaskClass, Cap),
    gated: (TaskClass, Cap),
}

/// The long-lived peer pumps' ids, which their owners wake by (ADR-0177 D2): the
/// `ns-apply` pump of each peer in peer order, the `owner-pub` and the `gated` pump.
pub(super) struct PeerPumps {
    ns_apply: Vec<TaskId>,
    pub(super) owner_pub: TaskId,
    pub(super) gated: TaskId,
}

impl PeerPumps {
    /// The `ns-apply` pump that serves `origin`, a peer of `cell`.
    pub(super) fn ns_apply(&self, cell: CellId, origin: CellId) -> TaskId {
        self.ns_apply[peer_index(cell, origin)]
    }
}

/// `origin`'s place among `cell`'s peers: the cells in order, `cell` left out.
fn peer_index(cell: CellId, origin: CellId) -> usize {
    debug_assert_ne!(cell, origin, "a cell is not its own peer");
    let origin = usize::from(origin.0);
    if origin > usize::from(cell.0) { origin - 1 } else { origin }
}

impl PlaneTasks {
    /// Registers the six classes on `layout`, each from the function item it runs.
    pub(super) fn register<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        layout: &mut ExecutorLayout,
        caps: &PlaneTaskCaps,
    ) -> Result<PlaneTasks, CapError> {
        #[cfg(not(inf_canary_conn_slots_doubled))]
        let conn = layout.class(&pump::<O, F>, caps.conn)?;
        // Canary: the class is built over `(1 + k) C` slots, `k` from its own stride.
        #[cfg(inf_canary_conn_slots_doubled)]
        let conn = layout.class(
            &pump::<O, F>,
            crate::limits::conn_slots_doubled(
                caps.conn,
                ExecutorLayout::slot_shape(&pump::<O, F>).0,
            )?,
        )?;
        let peers = match &caps.peers {
            None => None,
            Some(PeerTaskCaps { ns_apply, owner_pub, gated }) => Some(PeerTasks {
                ns_apply: (layout.class(&ns_apply_pump::<O, F>, *ns_apply)?, *ns_apply),
                owner_pub: (layout.class(&owner_pub_pump::<O, F>, *owner_pub)?, *owner_pub),
                gated: (layout.class(&gated_pump::<O, F>, *gated)?, *gated),
            }),
        };
        let compact = (layout.class(&compact_pump::<O, F>, caps.compact)?, caps.compact);
        let shadow = (layout.class(&shadow_pump::<O, F>, caps.shadow)?, caps.shadow);
        Ok(PlaneTasks { conn: (conn, caps.conn), peers, compact, shadow })
    }

    /// The `conn` tasks held (alive and Retired), the Retired among them, and
    /// the class's slots, the cell's share `C`: O1's first owner and its bound.
    pub(super) fn conn_tasks(&self, executor: &CellExecutor) -> (u32, u32, u32) {
        let (class, cap) = self.conn;
        (executor.class_tasks(class), executor.class_retired(class), cap.entries_max())
    }

    /// Compaction chains MAINTAIN may still start this pass.
    pub(super) fn compact_room(&self, executor: &CellExecutor) -> u32 {
        room(executor, self.compact)
    }

    /// Shadow reads MAINTAIN may still start this pass.
    pub(super) fn shadow_room(&self, executor: &CellExecutor) -> u32 {
        room(executor, self.shadow)
    }
}

/// The slots of `class` no task holds.
fn room(executor: &CellExecutor, (class, cap): (TaskClass, Cap)) -> u32 {
    // Canary: the room reads short at every pass, none free, so MAINTAIN paces
    // every namespace with work.
    if cfg!(inf_canary_maint_room_short) {
        return 0;
    }
    cap.entries_max().saturating_sub(executor.class_tasks(class))
}

/// A class's permit, or `None` at `Full`, the crossing its host answers.
fn granted<Fut>(admission: TaskAdmission<'_, Fut>) -> Option<TaskPermit<'_, Fut>> {
    match admission {
        TaskAdmission::Granted(permit) => Some(permit),
        TaskAdmission::Full => None,
        // This module registers and reserves each class from one function item, so another
        // future type or another layout's class here is an assembly defect.
        TaskAdmission::Foreign => panic!("a plane task class reserved for another future"),
        // A slot retires after 2^64 - 1 uses: the cell's fail-stop (ADR-0177 D7).
        TaskAdmission::Exhausted => panic!("a plane task class retired every slot"),
    }
}

/// Admits a connection and makes its pump in one step (ADR-0123 A1, ADR-0177 D2): the `conn`
/// permit first, so a full class refuses the accept before anything is installed; then the
/// task's gate units (ADR-0177 D4), which leave and return with it; then the slot, whose key
/// the pump serves; the pump's first poll finds no command and parks. `None` when the class,
/// the gate's units or the slab is full: nothing was installed or made.
pub(super) fn admit_conn<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    executor: &mut CellExecutor,
    shared: &Rc<Shared<O, F>>,
    conn: impl FnOnce(TaskId) -> Conn,
) -> Option<ConnKey> {
    let permit = granted(executor.reserve(shared.tasks.conn.0))?;
    let units = shared.gate.borrow().units(GATE_UNITS_PER_CONN_TASK)?;
    shared.conns.borrow_mut().insert_with(|key| {
        let pump_shared = Rc::clone(shared);
        conn(permit.spawn_local(move || pump(pump_shared, key, units)))
    })
}

/// Plane start (ADR-0177 D2): the peer pumps, made in the cell's setup before its loop's first
/// iteration, so each `Assembly` class is full at the serve mark. Their first polls find their
/// queues empty and park. A cell with no peers makes none.
pub(super) fn start_peer_pumps<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    executor: &mut CellExecutor,
    shared: &Rc<Shared<O, F>>,
) -> Result<Option<PeerPumps>, AssemblyFull> {
    let Some(peers) = &shared.tasks.peers else {
        return Ok(None);
    };
    let (ns_class, ns_cap) = peers.ns_apply;
    let mut ns_apply = Vec::with_capacity(usize::from(shared.cells) - 1);
    for origin in (0..shared.cells).filter(|&origin| origin != shared.cell.0) {
        let permit = granted(executor.reserve(ns_class)).ok_or(AssemblyFull::new(ns_cap))?;
        let pump_shared = Rc::clone(shared);
        ns_apply.push(permit.spawn_local(move || ns_apply_pump(pump_shared, origin)));
    }
    // Canary: plane start makes one `ns-apply` pump more than the class's `N - 1` slots.
    #[cfg(inf_canary_ns_apply_pump_extra)]
    {
        let permit = granted(executor.reserve(ns_class)).ok_or(AssemblyFull::new(ns_cap))?;
        let (pump_shared, own) = (Rc::clone(shared), shared.cell.0);
        ns_apply.push(permit.spawn_local(move || ns_apply_pump(pump_shared, own)));
    }
    // The owner pump's units: one leg toward each peer (ADR-0177 D2).
    let owner_units = || {
        let units = shared.gate.borrow().units(u32::from(shared.cells) - 1);
        units.ok_or(AssemblyFull::new(peers.owner_pub.1))
    };
    let units = owner_units()?;
    let owner_pub =
        start_one(executor, peers.owner_pub, || owner_pub_pump(Rc::clone(shared), units))?;
    // Canary: plane start makes a second `owner-pub` pump.
    #[cfg(inf_canary_owner_pub_pump_extra)]
    {
        let units = owner_units()?;
        start_one(executor, peers.owner_pub, || owner_pub_pump(Rc::clone(shared), units))?;
    }
    let gated = start_one(executor, peers.gated, || gated_pump(Rc::clone(shared)))?;
    // Canary: plane start makes a second `gated` pump.
    #[cfg(inf_canary_gated_pump_extra)]
    start_one(executor, peers.gated, || gated_pump(Rc::clone(shared)))?;
    Ok(Some(PeerPumps { ns_apply, owner_pub, gated }))
}

/// One plane-start pump of `class`, or the assembly's refusal naming its row.
fn start_one<Fut: Future<Output = ()> + 'static>(
    executor: &mut CellExecutor,
    (class, cap): (TaskClass, Cap),
    make: impl FnOnce() -> Fut,
) -> Result<TaskId, AssemblyFull> {
    let permit = granted(executor.reserve::<Fut>(class)).ok_or(AssemblyFull::new(cap))?;
    Ok(permit.spawn_local(make))
}

/// A compaction read chain under its permit, polled once in place; `false` at `Full`, its read
/// not made (MAINTAIN reads the room before it takes one, so derived never).
pub(super) fn spawn_compact<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    executor: &mut CellExecutor,
    shared: &Rc<Shared<O, F>>,
    read: crate::tier_cell::CompactRead,
) -> bool {
    let Some(permit) = granted(executor.reserve(shared.tasks.compact.0)) else {
        return false;
    };
    let shared = Rc::clone(shared);
    let _ = permit.poll_immediate(move || compact_pump(shared, read));
    true
}

/// A shadow read under its permit, polled once in place; `false` at `Full`, as
/// [`spawn_compact`].
pub(super) fn spawn_shadow<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    executor: &mut CellExecutor,
    shared: &Rc<Shared<O, F>>,
    ns: NsId,
    read: inf_store::ShadowRead,
) -> bool {
    let Some(permit) = granted(executor.reserve(shared.tasks.shadow.0)) else {
        return false;
    };
    let shared = Rc::clone(shared);
    let _ = permit.poll_immediate(move || shadow_pump(shared, ns, read));
    true
}

#[cfg(test)]
mod tests {
    //! The shipped classes' layout (ADR-0194 A1): each class slab at its chunk.

    use inf_foundation::bounded::CapCensus;
    use inf_log::fs::StdSegmentFs;

    use super::*;
    use crate::config::ConfigStore;

    type O = NoopObserver;
    type F = StdSegmentFs;

    /// The node's layout at 4 and 16 cells and at one cell of `C` = 10,000: every class
    /// registers, each at the chunk its slot's bytes take, `c(s)`. Each stride and chunk is
    /// printed for the ticket.
    #[test]
    fn every_class_registers_at_its_chunk() {
        let shapes = [
            ("conn", ExecutorLayout::slot_shape(&pump::<O, F>)),
            ("ns-apply", ExecutorLayout::slot_shape(&ns_apply_pump::<O, F>)),
            ("owner-pub", ExecutorLayout::slot_shape(&owner_pub_pump::<O, F>)),
            ("gated", ExecutorLayout::slot_shape(&gated_pump::<O, F>)),
            ("compact", ExecutorLayout::slot_shape(&compact_pump::<O, F>)),
            ("shadow", ExecutorLayout::slot_shape(&shadow_pump::<O, F>)),
        ];
        for (class, (stride, chunk)) in shapes {
            println!("every_class_registers_at_its_chunk: {class}: {stride} B, c {chunk}");
        }
        let config = ConfigStore::default();
        for cells in [4u16, 16, 1] {
            let caps = crate::limits::plane_task_caps(config.boot_share(cells), cells)
                .expect("the default caps");
            let (census, _mark) = CapCensus::new(CellId(0));
            let mut layout = ExecutorLayout::new(&census);
            let tasks = PlaneTasks::register::<O, F>(&mut layout, &caps)
                .unwrap_or_else(|error| panic!("{cells} cells: class-refused: {error}"));
            let executor = layout
                .build()
                .unwrap_or_else(|error| panic!("{cells} cells: class-refused: {error}"));
            assert_eq!(executor.class_tasks(tasks.conn.0), 0, "{cells} cells");
            println!(
                "every_class_registers_at_its_chunk: {cells} cells: conn {}, header array {} B",
                caps.conn.entries_max(),
                executor.header_array_bytes()
            );
            let holders = crate::limits::gate_holders(&caps, cells, 1_024)
                .expect("the default gate")
                .map_or(0, |holders| holders.entries_max());
            println!(
                "every_class_registers_at_its_chunk: {cells} cells: H {holders}, holder {} B",
                FabricGate::<OwnedOutcome>::HOLDER_SLOT_BYTES
            );
        }
    }

    /// The chunks and directories a slab of `cap` slots in chunks of `chunk` builds: ⌈cap ÷ c⌉
    /// chunks and, per directory level above them, ⌈chunks ÷ 256^k⌉ directories, none for one
    /// chunk (ADR-0194 A1).
    fn chunks_and_directories(cap: u32, chunk: usize) -> (u64, u64) {
        let chunks = u64::from(cap).div_ceil(chunk as u64);
        let (mut directories, mut level) = (0, chunks);
        while level > 1 {
            level = level.div_ceil(inf_foundation::limits::MAP_DIRECTORY_CHILDREN_MAX as u64);
            directories += level;
        }
        (chunks, directories)
    }

    /// One slab's resident bill at the build: every slot of each chunk and each 2 KiB
    /// directory written (ADR-0177 D8 with A1.1).
    fn slab_bill(cap: u32, stride: usize, chunk: usize) -> u64 {
        let (chunks, directories) = chunks_and_directories(cap, chunk);
        chunks * chunk as u64 * stride as u64 + directories * 2_048
    }

    /// The `--plant-bytes-cell` value of `cfg`'s row in the canary table.
    fn plant_bytes(table: &str, cfg: &str) -> u64 {
        let row = table
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("\"{cfg} ")))
            .unwrap_or_else(|| panic!("no canary row for {cfg}"));
        let mut words = row.split_whitespace();
        words.find(|word| *word == "--plant-bytes-cell");
        words.next().and_then(|bytes| bytes.parse().ok()).expect("a plant's bytes")
    }

    /// `empty_idle_rss`'s bill at 4 cells and the default `maxclients`, recomputed from the
    /// strides the build reads (ADR-0177 D8 with A1): the fixed terms, each task class's
    /// slab and the fabric gate's holders. The gate row's threshold and both `binary` canaries'
    /// planted bytes (`conn` slots and gate holders doubled) are these strides' products, so a
    /// stride that moves turns this red until the three move with it.
    #[test]
    fn the_idle_bill_and_its_plants_follow_the_strides() {
        const CELLS: u16 = 4;
        const MIB: f64 = 1_048_576.0;
        let config = ConfigStore::default();
        let caps = crate::limits::plane_task_caps(config.boot_share(CELLS), CELLS)
            .expect("the default caps");
        let peers = caps.peers.as_ref().expect("a cell with peers");
        let classes = [
            (caps.conn, ExecutorLayout::slot_shape(&pump::<O, F>)),
            (peers.ns_apply, ExecutorLayout::slot_shape(&ns_apply_pump::<O, F>)),
            (peers.owner_pub, ExecutorLayout::slot_shape(&owner_pub_pump::<O, F>)),
            (peers.gated, ExecutorLayout::slot_shape(&gated_pump::<O, F>)),
            (caps.compact, ExecutorLayout::slot_shape(&compact_pump::<O, F>)),
            (caps.shadow, ExecutorLayout::slot_shape(&shadow_pump::<O, F>)),
        ];
        let tasks: u64 = classes
            .iter()
            .map(|(cap, (stride, chunk))| slab_bill(cap.entries_max(), *stride, *chunk))
            .sum();
        let holders = crate::limits::gate_holders(&caps, CELLS, 1_024)
            .expect("the default gate")
            .expect("a gate at 4 cells")
            .entries_max();
        let holder_bytes = FabricGate::<OwnedOutcome>::HOLDER_SLOT_BYTES;
        let holder_chunk =
            inf_foundation::limits::chunk_slots_for(holder_bytes).expect("a holder's chunk");
        let gate = slab_bill(holders, holder_bytes, holder_chunk);
        // Pinned receive pools 64 MiB, mesh ring slots 3 MiB, SQ and CQ 1.5 MiB, the per-cell
        // remainder 16 MiB and the per-node remainder 16 MiB: the row's fixed terms.
        let fixed = 100.5 * MIB;
        let bill = (fixed + (u64::from(CELLS) * (tasks + gate)) as f64) / MIB;
        let threshold = (bill * 100.0).ceil() / 100.0;
        println!(
            "the_idle_bill_and_its_plants_follow_the_strides: tasks {tasks} B, gate {gate} B a \
             cell, bill {bill:.3} MiB, threshold {threshold:.2}"
        );
        let gates = include_str!("../../../../docs/milestones/m1-gates.toml");
        let row = gates.split("[[gate]]").find(|row| row.contains("id = \"empty_idle_rss\""));
        let written: f64 = row
            .and_then(|row| row.lines().find_map(|line| line.strip_prefix("threshold = ")))
            .and_then(|value| value.trim().parse().ok())
            .expect("empty_idle_rss's threshold");
        assert!(
            (written - threshold).abs() < 1e-9,
            "empty_idle_rss's threshold {written} is not the bill's {threshold} at these strides"
        );
        let sum = format!("Σ {bill:.3} MiB");
        assert!(gates.contains(&sum), "the bill's comment does not read `{sum}` at these strides");
        let canaries = include_str!("../../../../scripts/sim-canaries.sh");
        let conn_stride = ExecutorLayout::slot_shape(&pump::<O, F>).0 as u64;
        let conn_plant = u64::from(caps.conn.entries_max()) * conn_stride;
        assert_eq!(
            plant_bytes(canaries, "inf_canary_conn_slots_doubled"),
            conn_plant,
            "C x s_conn"
        );
        let gate_plant = u64::from(holders) * holder_bytes as u64;
        assert_eq!(plant_bytes(canaries, "inf_canary_gate_holders_doubled"), gate_plant, "H x s_h");
    }
}
