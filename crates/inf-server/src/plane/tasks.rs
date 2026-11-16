//! The plane's task table (ADR-0177 D2): a task lives as long as the owner it serves. One class
//! per future the plane runs: `conn` (a connection's pump, made at its admission), the three
//! peer pumps (made at plane start, before the cell serves, never ending) and the two MAINTAIN
//! reads (made under a permit, ending with their read). Each class is registered and reserved
//! here from the same function item, so a `Foreign` answer is this module's pairing gone wrong.

use inf_foundation::bounded::{AssemblyFull, Cap, CapError};
use inf_runtime::{CellExecutor, ExecutorLayout, TaskAdmission, TaskClass, TaskId, TaskPermit};

use super::*;
use crate::limits::{PeerTaskCaps, PlaneTaskCaps};

/// The six classes' handles and their caps, one field of `Shared`.
pub(super) struct PlaneTasks {
    conn: TaskClass,
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
        let conn = layout.class(&pump::<O, F>, caps.conn)?;
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
        Ok(PlaneTasks { conn, peers, compact, shadow })
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
/// slot, whose key the pump serves; the pump's first poll finds no command and parks. `None`
/// when the class or the slab is full: nothing was installed or made.
pub(super) fn admit_conn<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    executor: &mut CellExecutor,
    shared: &Rc<Shared<O, F>>,
    conn: impl FnOnce(TaskId) -> Conn,
) -> Option<ConnKey> {
    let permit = granted(executor.reserve(shared.tasks.conn))?;
    shared.conns.borrow_mut().insert_with(|key| {
        let pump_shared = Rc::clone(shared);
        conn(permit.spawn_local(move || pump(pump_shared, key)))
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
    let owner_pub = start_one(executor, peers.owner_pub, || owner_pub_pump(Rc::clone(shared)))?;
    let gated = start_one(executor, peers.gated, || gated_pump(Rc::clone(shared)))?;
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
