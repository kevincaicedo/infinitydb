//! Runtime bounds owned by the server. Every crossing names its behavior here.

use core::num::NonZeroU32;

use inf_foundation::bounded::{Cap, CapError, CapFill};
use inf_foundation::time::Nanos;

/// Reclaim marks one tiered namespace holds on its cell: ADR-0061 D5's
/// `(seq, epoch)` marks, one per ack class (ADR-0163 D2). After MAINTAIN
/// merges each pair of adjacent marks that one barrier releases, a mark
/// stands for each class, so the marks are at most `F + C + 2`: `F` frames
/// behind the written prefix (at most the frames in flight while writes land
/// in order, the reorder window's 16 behind a late front write), `C`
/// coverage points in the unfolded fsync ledger, one trailing written ack
/// entry and the staging builder. The cap binds only behind a wedge: a late
/// front write, or a FLUSH-class front entry held while write-through frames
/// complete behind it. Owner: the namespace's `TierNs`, one deque per tiered
/// namespace per cell, sized at its construction. Crossing: coalesce — at 16
/// classes the back mark takes the later pair whole, so its extent deaths
/// wait for the later class's barrier: reclaim is delayed by at most one
/// barrier, never early, and each such merge counts as the row's crossing in
/// `INFO caps`.
pub const EPOCH_MARKS_MAX: u32 = 16;

/// The reclaim marks' cap: `EPOCH_MARKS_MAX` entries under the census name
/// `epoch-marks`, filled while the cell serves.
pub const EPOCH_MARKS: Cap = Cap::entries::<EPOCH_MARKS_MAX>("epoch-marks", CapFill::Serving);

/// Ack points one cell's durable plane holds: a frame's exclusive end and
/// the last durable seq it carries, for every frame queued and not yet
/// covered by the fsync watermark. Coalescing keeps a written point only
/// where an unfolded ledger entry covers up to it, so the map holds at most
/// the frames behind the written prefix, one point per unfolded ledger entry
/// and one more: `REORDER_WINDOW_FRAMES + FSYNC_LEDGER_ENTRIES + 1`. Owner:
/// the cell's `DurableCell`, one map built with it. Crossing: pacing — the
/// LOG step asks the map's admission before the frame's plan and holds the
/// frame while it is full (derived never to occur; `frame_waits_ack_map`
/// counts it once per held frame).
#[cfg(not(inf_canary_ack_map_window_only))]
pub const ACK_MAP_ENTRIES: u32 =
    inf_log::limits::REORDER_WINDOW_FRAMES + inf_log::limits::FSYNC_LEDGER_ENTRIES + 1;
/// Canary: the ack map sized as the frame queue alone, so it fills while
/// unfolded ledger entries keep their points.
#[cfg(inf_canary_ack_map_window_only)]
pub const ACK_MAP_ENTRIES: u32 = inf_log::limits::REORDER_WINDOW_FRAMES;

/// The ack map's cap: `ACK_MAP_ENTRIES` entries under the census name
/// `ack-map`, filled while the cell serves.
pub const ACK_MAP: Cap = Cap::entries::<ACK_MAP_ENTRIES>("ack-map", CapFill::Serving);

/// Watermark waits one cell's durable plane wakes between two of its LOG steps (ADR-0177
/// A2.1): the first batch at a sync's completion, each later one at the head of the LOG step,
/// one call of the ack gate's `wake_covered` each, drawn from one count the LOG step refills.
/// Its reason is a batch's time, a sift a wake: 1,024 woken read 0.13 to 0.41 ms with up to
/// 50,000 waits queued. At least one by its type, as a zero budget would strand every
/// acknowledgment; at most the loop's default `exec_budget`, as waking more than one EXECUTE
/// pass polls runs no task sooner (a test holds both). The durable config carries it, so a
/// test may lower it. Unit: waits between two LOG steps of a cell. Owner: the cell's
/// `DurableCell`. Crossing: pacing, the later LOG steps waking the rest in LSN order.
// 1,024 built from the least non-zero count: a construction with no refusal to handle.
pub const ACK_WAKES_PER_TURN: NonZeroU32 = NonZeroU32::MIN.saturating_add(1_024 - 1);

/// Timer owner positions one server cell holds (ADR-0154 D1, D4): its roles'
/// count, `TimerRole::COUNT` (one listener's accept retry, everysec, fill
/// hold, group hold), so the cap is derived from the producers and a role
/// added is a position added. Owner: `ServerTimers::build`, the cell's one
/// constructor of its set and its owners, inside `ServerPlane::new`.
/// `CapFill::Assembly`: the build reserves every position before the cell's
/// loop exists, and no role closes its owner, so the `timer-owners` row is
/// full at the serve mark and takes no publish after it (the census counts
/// any in `cap_assembly_violations`). Crossing: a reserve refused at the
/// assembly fails it before anything serves (`TimerAssemblyError`); derived
/// never, as the cap is the role count.
#[cfg(not(inf_canary_timer_owners_short))]
pub const SERVER_TIMER_OWNERS: Cap =
    Cap::entries::<{ crate::TimerRole::COUNT }>("timer-owners", CapFill::Assembly);
/// Canary: a cap one short of the roles, so every assembly's last reserve
/// is refused.
#[cfg(inf_canary_timer_owners_short)]
pub const SERVER_TIMER_OWNERS: Cap =
    Cap::entries::<{ crate::TimerRole::COUNT - 1 }>("timer-owners", CapFill::Assembly);

/// Checkpoint-board slots one cell's sweep visits per MAINTAIN turn
/// (ADR-0159 D4, A1.4). Owner: the cell's `BoardSweep`, charged as one
/// Maintenance unit per turn as MAINTAIN's first budgeted step. Crossing:
/// pacing — the sweep resumes at its cursor next turn, so a board of `N`
/// cells completes in `ceil(N / 64)` turns (256 at 16,384 cells) and a
/// publication is observed within two completed sweeps.
pub const CKPT_BOARD_VISITS_PER_TURN: usize = 64;

/// Tiered namespace lifetimes a node holds at once (ADR-0148 D1, A1.1), the
/// one value `inf_foundation::limits` declares with its unit and crossing:
/// the catalog writer's grant book refuses a tiered `CREATE` past it, and a
/// boot whose catalog names more live tiered definitions is refused before
/// any cell.
pub use inf_foundation::limits::TIERED_NAMESPACE_LIFETIMES_PER_NODE_MAX;

/// The namespace-id counter's spent value (ADR-0148 D4). Unit: namespace
/// ids per data directory, issued from `FIRST_NAMED_NS_ID` up to
/// `u32::MAX − 1`, once each (ADR-0015 D2). Owner: the catalog writer, the
/// counter's one mover. Crossing: a counter at this value issues none, so
/// every `INF.NS CREATE` answers `IdentityExhausted(Namespace)` before any
/// effect, for the data directory's life.
pub const NS_IDS_EXHAUSTED: u32 = u32::MAX;

/// The recovery step budget's price of one byte a tiered boot replay
/// machine wrote to a tier file (ADR-0174 R10; D6's gauge reads the
/// largest step's charge): one budget byte, the device bytes the step
/// moved. Owner: the recovery driver (`RecoverConfig::step_bytes` is the
/// budget it charges). Crossing: a step whose charge — the bytes its cursor
/// passed and its priced boot work — reaches the budget yields at the next
/// record or entry, inside a log frame or a checkpoint section (ADR-0174
/// A1), after an index-sidecar section, after the settle step that reached
/// it, or at the hand-over's next unit. The bytes a demote step's seal walk
/// and the end settle pass, and their reads, are priced by the store, whose
/// walks yield on them
/// ([`ReplayWork::settle_charge_bytes`](inf_store::ReplayWork::settle_charge_bytes)).
pub const REPLAY_TIER_BYTE_CHARGE: u64 = 1;

/// The recovery step budget's price (ADR-0174 R10) of one boot barrier
/// (a demote step's flush, a gap or capacity seal, the hand-over's
/// drain): 4 MiB, about 4 ms of the flush barrier a device that moves
/// 1 GiB/s pays. Owner and crossing: as [`REPLAY_TIER_BYTE_CHARGE`].
pub const REPLAY_BARRIER_CHARGE_BYTES: u64 = 4 << 20;

/// Bytes read by the process supervisor for one CPU sample. An oversized
/// procfs record is refused and the read board retains its preceding sample.
pub const PROCESS_STAT_BYTES_MAX: u64 = 16 * 1024;

/// Flush rounds one cell plans per MAINTAIN turn, whatever the number of
/// its tiered namespaces (ADR-0178 D3's tier row, ADR-0155 D1): one span
/// of at most `max(MAINTAIN-SLICE, the largest inline record)` bytes, a
/// gap, or a refusal. Owner: the cell's tier turn, which mints one plan
/// permit per turn and spends it on the first namespace after the last
/// planned one, in id order, that is idle with work at its cursor;
/// charged to Maintenance. Crossing: pacing — every other namespace with
/// work stays idle and plans in a later turn, in id order, so one plans
/// within `n − 1` further turns; `tier_plan_waits` counts each one passed
/// over.
pub const TIER_PLANS_PER_TURN: usize = 1;

/// Round-op descriptors the cell's issue pass visits per MAINTAIN turn
/// (ADR-0155 D4): the ops of every group it builds, and the first op of
/// the group that did not fit. Owner: the cell's tier turn; charged to
/// Maintenance. Crossing: pacing — the pass ends, and the next turn's
/// visit starts at the namespace whose group was refused, else after the
/// one served last.
pub const TIER_ISSUE_OPS_PER_TURN: usize = 64;

/// Bytes of the longest reply root `JSON.SET` and `JSON.MERGE`'s root
/// create write after their store call: `+OK\r\n`, or the NX/XX null
/// (`$-1\r\n`; RESP3 `_\r\n`). A command whose reply budget has fewer
/// bytes left answers `ERR reply too large` before the store call
/// (ADR-0099 A1).
#[cfg(feature = "doc")]
pub const JSON_STATUS_REPLY_BYTES_MAX: usize = 5;

/// Bytes of the reply a root `JSON.DEL`/`JSON.FORGET` writes after its
/// delete: `:0\r\n` or `:1\r\n`. Below it the command refuses before the
/// delete (ADR-0099 A1).
#[cfg(feature = "doc")]
pub const JSON_COUNT_REPLY_BYTES_MAX: usize = 4;

/// Bytes of the longest reply the in-place `JSON.TOGGLE` writes after its
/// patch: legacy `$5\r\nfalse\r\n`; `$` mode `*1\r\n$-1\r\n`, `*1\r\n:1\r\n`
/// or `*0\r\n`. Below it the command refuses before the probe, and so does
/// its general path, which the probe decides (ADR-0099 A1).
#[cfg(feature = "doc")]
pub const JSON_TOGGLE_REPLY_BYTES_MAX: usize = 11;

/// The fixed-shape column's maximum, and the `Number` row: the in-place
/// `JSON.NUMINCRBY`/`JSON.NUMMULTBY` reply, a RESP3 `*1\r\n` and one
/// double (the RESP2 bulk is at most 41 B). Two crossings (ADR-0099 A1):
/// a fixed-shape command whose reply budget has fewer bytes left than its
/// shape's maximum answers `ERR reply too large` before its effect; and a
/// `doc-max-reply-bytes` setter, when one exists, refuses a value below
/// this constant, because below it a numeric patch can never be served.
#[cfg(feature = "doc")]
pub const JSON_FIXED_REPLY_BYTES_MAX: usize = 4 + inf_wire::limits::DOUBLE_REPLY_BYTES_MAX;

/// Declares [`FixedShape`], [`FixedShape::ALL`] and
/// [`FixedShape::reply_bytes_max`] from one list of rows, so a new shape is
/// a row of `ALL` by construction and the column assertion below sees it.
macro_rules! fixed_shapes {
    ($($(#[$row:meta])* $shape:ident => $reply_bytes_max:expr,)+) => {
        /// The shapes of a `JSON.*` reply known only after its effect. The
        /// command reserves the shape's maximum before the effect and then
        /// makes its one write (ADR-0099 A1).
        #[cfg(feature = "doc")]
        #[derive(Copy, Clone, PartialEq, Eq, Debug)]
        pub enum FixedShape {
            $($(#[$row])* $shape,)+
        }

        #[cfg(feature = "doc")]
        impl FixedShape {
            /// Every row, for the column's assertion.
            pub const ALL: &'static [FixedShape] = &[$(FixedShape::$shape,)+];

            /// The row's maximum reply bytes, M(shape).
            pub const fn reply_bytes_max(self) -> usize {
                match self {
                    $(FixedShape::$shape => $reply_bytes_max,)+
                }
            }
        }
    };
}

fixed_shapes! {
    /// Root `SET`, `MERGE`'s root create.
    Status => JSON_STATUS_REPLY_BYTES_MAX,
    /// Root `DEL`/`FORGET`.
    Count => JSON_COUNT_REPLY_BYTES_MAX,
    /// In-place `TOGGLE`.
    Toggle => JSON_TOGGLE_REPLY_BYTES_MAX,
    /// In-place `NUMINCRBY`/`NUMMULTBY`.
    Number => JSON_FIXED_REPLY_BYTES_MAX,
}

#[cfg(feature = "doc")]
const _: () = {
    let mut row = 0;
    while row < FixedShape::ALL.len() {
        let fits = FixedShape::ALL[row].reply_bytes_max() <= JSON_FIXED_REPLY_BYTES_MAX;
        assert!(fits, "every fixed shape fits the column's maximum");
        row += 1;
    }
    assert!(JSON_FIXED_REPLY_BYTES_MAX == 348, "the fixed column's maximum is 348 bytes");
};

/// Legs of one cross-cell `JSON.MGET` gather in flight at once (ADR-0173
/// D4): 3, the owner's value, which keeps a gather over every cell of a
/// four-cell node at one round trip. Owner: the gather's FIFO of leg
/// waiters, built per gather on the cell's census; every gather of the cell
/// shares the `json-gather-legs` row. Crossing: pacing — with this many legs
/// in flight the next leg waits for the oldest leg's reply (the
/// connection's pump with it), and the row counts a crossing in `INFO caps`.
/// A larger value needs a decision record that first accounts for the
/// memory a connection's queued requests hold: one refused gather can hold
/// (this value + 1) × `doc-max-reply-bytes`.
#[cfg(feature = "doc")]
pub const JSON_GATHER_LEGS_IN_FLIGHT_MAX: u32 = 3;

/// The gather window's cap: `JSON_GATHER_LEGS_IN_FLIGHT_MAX` entries under
/// the census name `json-gather-legs`, filled while the cell serves.
#[cfg(feature = "doc")]
pub const JSON_GATHER_LEGS: Cap =
    Cap::entries::<JSON_GATHER_LEGS_IN_FLIGHT_MAX>("json-gather-legs", CapFill::Serving);

/// Bytes of the longest frame header a `JSON.MGET` leg's reply carries: a
/// type byte, the decimal of a `u64` length (20 digits at most), CRLF — the
/// most the reply writer emits for one element. Owner: the gather's leg
/// decoder, which looks for a header's CRLF within it. Crossing: a header
/// longer than this is a malformed success, answered
/// `ERR cross-cell program reply malformed`.
#[cfg(feature = "doc")]
pub const JSON_LEG_FRAME_HEADER_BYTES_MAX: usize = 1 + decimal_digits(u64::MAX) + 2;

#[cfg(feature = "doc")]
const _: () = assert!(JSON_LEG_FRAME_HEADER_BYTES_MAX == 23, "`$`, 20 digits and CRLF");

/// Decimal digits of `value`.
#[cfg(feature = "doc")]
const fn decimal_digits(mut value: u64) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

/// The recovery step budget's price (ADR-0174 A1) of one boot file
/// operation outside replay — in the start step's tier map, an open of an
/// existing tier file, a truncate, a removal, a directory listing's open
/// and each batch it reads (at most 256 names and 8 KiB, ADR-0202 D2); at
/// the hand-over, the rename that brings a quarantined extent the replayed
/// map references back under its name (ADR-0096 D3): 128 KiB, about 100 µs
/// of a device that moves 1 GiB/s, as a settle read is priced; a cold
/// listing batch reads a few directory blocks. An estimate the
/// reference-box run calibrates, never a measurement. Owner: the recovery
/// driver, which charges it per operation and, at the hand-over, runs as
/// many a turn as its budget left buys, at least one. Crossing: the step
/// yields once its charge reaches the budget, at the next unit, and the
/// next step resumes at its phase's cursor.
pub const BOOT_FILE_OP_CHARGE_BYTES: u64 = 128 << 10;

/// The accept retry's window, in injected time: after the driver parks a
/// listener's accept arm on an exhaustion or a broken-listener failure, the
/// plane's accept-retry timer owner re-arms it this long after the failure.
/// Descriptors freed outside the driver (segment files, checkpoint handles),
/// and a listener parked as broken, which no `Close` resumes, let queued
/// clients in by this retry alone. Owner: the plane's accept-retry owner,
/// armed from Idle by a window's first failure. Crossing: pacing. Failures
/// inside a window coalesce into its one arm, which falls due at most one
/// window and the deadline's rounding millisecond after the window's first
/// failure (ADR-0154 D3). While the failure persists, each retry fails
/// again, counts one error and arms one more window: one `accept(2)` a
/// window, never a spin (ADR-0118 D2), and a queued client waits one more
/// window for each failed retry.
pub const ACCEPT_RETRY: Nanos = Nanos::from_millis(100);

/// `--maxclients`'s default, the boot capacity `B` of a node assembled
/// without one (ADR-0123 A1): connections per node. Owner: the assembly,
/// which checks it with every boot value through `BootCapacity::new`.
/// Crossing: none of its own; `CONFIG SET maxclients` above the node's `B`
/// is refused whole with `BOOT_CAPACITY_REFUSAL`.
pub const MAXCLIENTS_BOOT_DEFAULT: u32 = 10_000;

/// Connection slots one cell holds at most (ADR-0123 A1): the ceiling of
/// `C = max(1, B / cells)`, the connection token's slot range (2^24 − 1).
/// Unit: connection slots, and from ADR-0177 D2 `conn` tasks, per cell.
/// Owner: `BootCapacity::new`. Crossing: a boot value whose `C` passes it
/// is refused before any cell is built.
pub const CONN_TASKS_MAX: u32 = inf_runtime::MAX_SLOT;

/// Data credits the plane keeps free toward each peer for the owner pump's
/// publish fan (ADR-0192 D2): a general send stages only while more than
/// this many credits toward its peer are free, so at least one stays free
/// after it, and a fan leg, one in flight toward each peer, may take it.
/// Unit: data credits per directed cell pair. Owner: the plane's
/// `PlaneFabric`. Crossing: a general send with this many or fewer free
/// gets `NoCredit` and parks for a returned credit, as at the window's end:
/// the client sees latency, never a refusal.
pub const FAN_RESERVE_PER_PEER: u32 = 1;

/// Data credits per directed cell pair of the shipped binary's mesh; the
/// general window is this less [`FAN_RESERVE_PER_PEER`]. Unit: data
/// credits. Owner: `infinityd`'s mesh constant, built through
/// `checked_mesh_config`. Crossing: a send past the window parks for a
/// returned credit.
pub const FABRIC_CREDITS_PER_PEER: u32 = 1024;

/// Data credits per directed cell pair at most (ADR-0177 D6): the ceiling
/// of `d`, the credits a mesh gives each peer, which bounds the work one
/// cell parks for one peer. Unit: data credits per directed cell pair.
/// Owner: `checked_mesh_config`, the one check of a mesh, and the three
/// per-origin deques' caps below. Crossing: a mesh with more is refused when
/// it is checked (a `const` mesh fails the build), and a plane built over
/// one refuses its assembly.
pub const FABRIC_CREDITS_PER_PEER_MAX: u32 = 1_536;

/// Bytes one entry of a per-origin deque takes at most (ADR-0177 D6): the
/// request or reply bytes sit behind one pointer, so `d` entries of a peer
/// fit one deque's backing at the ceiling of credits. Unit: bytes per
/// entry. Owner: the three entry types, each held to it by a const
/// assertion. Crossing: none at run time; a wider entry fails the build.
pub const PARKED_ENTRY_BYTES_MAX: usize = 40;

const _: () = assert!(
    FABRIC_CREDITS_PER_PEER_MAX as usize * PARKED_ENTRY_BYTES_MAX
        <= inf_foundation::limits::DEQUE_BACKING_BYTES_MAX,
    "a peer's entries at the ceiling of credits fit one deque's backing"
);

/// Namespace applies one cell parks for one origin (ADR-0177 D6): FABRIC-IN
/// diverts or parks an apply to its origin's pump. The ceiling of the
/// per-origin cap `d` (`per_origin`), under the census name `ns-applies`,
/// filled while serving. Unit: entries per origin. Owner: the plane's
/// `ns_applies`. Crossing: before any effect the op is answered `Refused`
/// (`ERR cross-cell execution failed` to its client) and
/// `fabric_parked_refused` counts it; derived never, as an origin holds at
/// most `d` un-replied ops.
pub const NS_APPLIES: Cap =
    Cap::entries::<FABRIC_CREDITS_PER_PEER_MAX>("ns-applies", CapFill::Serving);

/// `INF.PUB`s one cell parks for one origin's publishers (ADR-0177 D6),
/// fanned out by the owner pump in rotation over the origins: the ceiling of
/// `d`, census name `owner-pubs`. Unit, owner (`owner_pubs`) and crossing:
/// as [`NS_APPLIES`].
pub const OWNER_PUBS: Cap =
    Cap::entries::<FABRIC_CREDITS_PER_PEER_MAX>("owner-pubs", CapFill::Serving);

/// `always` verdicts one cell holds for one origin until its fsync
/// watermark covers them (ADR-0177 D3, D6): a placeholder before the
/// apply's first effect, filled after it, published by the `gated` task.
/// The ceiling of `d`, census name `gated`. Unit, owner (`gated`) and
/// crossing: as [`NS_APPLIES`].
pub const GATED_VERDICTS: Cap =
    Cap::entries::<FABRIC_CREDITS_PER_PEER_MAX>("gated", CapFill::Serving);

/// One per-origin deque's cap over a mesh's `data_credits` (ADR-0177 D1,
/// D6): `ceiling`'s name, `data_credits` entries.
///
/// # Errors
/// `CapError::Layout` when `data_credits` is 0 or above the ceiling.
pub(crate) const fn per_origin(ceiling: &Cap, data_credits: u32) -> Result<Cap, CapError> {
    #[cfg(not(any(inf_canary_parked_deque_of_one, inf_canary_parked_cap_short)))]
    let entries = data_credits;
    // Canary: one entry an origin, under the credits a peer may have
    // un-replied, so a second parked op is refused.
    #[cfg(inf_canary_parked_deque_of_one)]
    let entries = {
        let _ = data_credits;
        1
    };
    // Canary: two entries short of the credits, one under the general
    // window an origin may fill.
    #[cfg(inf_canary_parked_cap_short)]
    let entries = data_credits.saturating_sub(2);
    Cap::derived(ceiling, entries)
}

/// `conn` tasks one cell holds (ADR-0177 D2): one per connection slot that
/// is not vacant, made in the step that admits its connection and ending
/// when that slot retires. The ceiling of the class's cap `C`, the cell's
/// connection share (`conn_tasks`), census name `conn-tasks`, filled while
/// serving. Unit: tasks per cell. Owner: the cell's executor, through the
/// plane's task table (`plane/tasks.rs`). Crossing: the accept is refused
/// with Redis's `maxclients` frame and a close, before anything ran.
pub const CONN_TASKS: Cap = Cap::entries::<CONN_TASKS_MAX>("conn-tasks", CapFill::Serving);

/// The doubled-`conn` canary's plant: the class over `(1 + k) C` slots,
/// `k = ⌈512 KiB ÷ (C × s)⌉` from the class's stride `s`, so its rise passes
/// the idle-resident tool's 256 KiB control a cell whatever the stride.
#[cfg(inf_canary_conn_slots_doubled)]
pub(crate) fn conn_slots_doubled(cap: Cap, stride: usize) -> Result<Cap, CapError> {
    let share = u64::from(cap.entries_max());
    let bytes = share * u64::try_from(stride).map_err(|_| CapError::Layout)?;
    let k = (512 * 1024u64).div_ceil(bytes.max(1));
    let entries = u32::try_from(share * (1 + k)).map_err(|_| CapError::Layout)?;
    Cap::derived(&CONN_TASKS, entries)
}

/// Cells one node runs at most: the key slots one cell owns at least one
/// of. Unit: cells per node. Owner: `CellCount::new`, which refuses more.
pub const CELLS_MAX: u32 = inf_foundation::SLOT_COUNT as u32;

/// `ns-apply` tasks one cell holds (ADR-0177 D2): one long-lived pump per
/// peer, made at plane start, before the cell serves, and never ending. The
/// ceiling of `N − 1`, census name `ns-apply-tasks`, filled whole at
/// assembly. Unit: tasks per cell. Owner: the plane's task table. Crossing:
/// none while serving; a plane start that asks past it ends the assembly
/// with `ns-apply-tasks-full-at-assembly`.
pub const NS_APPLY_TASKS: Cap =
    Cap::entries::<{ CELLS_MAX - 1 }>("ns-apply-tasks", CapFill::Assembly);

/// `owner-pub` tasks one cell holds (ADR-0177 D2): the one long-lived owner
/// publish pump of a cell with peers, made at plane start. Census name
/// `owner-pub-tasks`, filled whole at assembly. Unit, owner and crossing:
/// as [`NS_APPLY_TASKS`].
pub const OWNER_PUB_TASKS: Cap = Cap::entries::<1>("owner-pub-tasks", CapFill::Assembly);

/// `gated` tasks one cell holds (ADR-0177 D2, D3): the one long-lived task
/// that publishes the cell's `always` verdicts, made at plane start. Census
/// name `gated-tasks`, filled whole at assembly. Unit, owner and crossing:
/// as [`NS_APPLY_TASKS`].
pub const GATED_TASKS: Cap = Cap::entries::<1>("gated-tasks", CapFill::Assembly);

/// Tiered namespaces whose compaction one cell runs at once (ADR-0177 D2's
/// `K`, ADR-0148 D1's lifetimes): 64. Unit: compaction chains per cell.
/// Owner: MAINTAIN, which takes a namespace's compaction read only while
/// the class has room. Crossing: pace — the read is not taken, nothing is
/// marked in flight, `tier_maint_task_waits` counts it, and the next
/// MAINTAIN starts at the first namespace it paced.
pub const TIER_MAINT_NAMESPACES: u32 = 64;

/// `compact` tasks one cell holds (ADR-0177 D2): one read chain per tiered
/// namespace, made by MAINTAIN under a permit and ending at the chain's end.
/// Census name `compact-tasks`, filled while serving. Unit: tasks per cell.
/// Owner and crossing: [`TIER_MAINT_NAMESPACES`]'s.
pub const COMPACT_TASKS: Cap =
    Cap::entries::<TIER_MAINT_NAMESPACES>("compact-tasks", CapFill::Serving);

/// `shadow` tasks one cell holds (ADR-0177 D2): four shadow reads in flight
/// per tiered namespace (`inf_store::SHADOW_READS_IN_FLIGHT`), each made by
/// MAINTAIN under a permit and ending at its read's end. Census name
/// `shadow-tasks`, filled while serving. Unit: tasks per cell. Owner and
/// crossing: [`TIER_MAINT_NAMESPACES`]'s.
pub const SHADOW_TASKS: Cap =
    Cap::entries::<{ 4 * TIER_MAINT_NAMESPACES }>("shadow-tasks", CapFill::Serving);

const _: () = assert!(
    inf_store::SHADOW_READS_IN_FLIGHT as u32 * TIER_MAINT_NAMESPACES == SHADOW_TASKS.entries_max(),
    "the shadow class holds every tiered namespace's reads in flight"
);

/// The plane's task classes' caps on one cell (ADR-0177 D2): `conn` at the
/// cell's connection share; the three peer pumps' at `cells − 1`, 1 and 1,
/// none at one cell; `compact` and `shadow` at their consts.
pub(crate) struct PlaneTaskCaps {
    pub(crate) conn: Cap,
    pub(crate) peers: Option<PeerTaskCaps>,
    pub(crate) compact: Cap,
    pub(crate) shadow: Cap,
}

/// The peer pumps' caps of a cell with peers.
pub(crate) struct PeerTaskCaps {
    pub(crate) ns_apply: Cap,
    pub(crate) owner_pub: Cap,
    pub(crate) gated: Cap,
}

/// The caps of a cell holding `conn_share` connection slots of a node of
/// `cells` cells (ADR-0177 D1: each a derived `Cap` under its ceiling).
///
/// # Errors
/// `CapError::Layout` when the share is 0 or above `CONN_TASKS_MAX`, or the
/// cells are 0 or above `CELLS_MAX`.
pub(crate) const fn plane_task_caps(
    conn_share: u32,
    cells: u16,
) -> Result<PlaneTaskCaps, CapError> {
    let conn = match Cap::derived(&CONN_TASKS, conn_share) {
        Ok(conn) => conn,
        Err(refused) => return Err(refused),
    };
    let peers = match cells {
        0 => return Err(CapError::Layout),
        1 => None,
        cells => {
            let ns_apply = match Cap::derived(&NS_APPLY_TASKS, cells as u32 - 1) {
                Ok(cap) => cap,
                Err(refused) => return Err(refused),
            };
            Some(PeerTaskCaps { ns_apply, owner_pub: OWNER_PUB_TASKS, gated: GATED_TASKS })
        }
    };
    Ok(PlaneTaskCaps { conn, peers, compact: COMPACT_TASKS, shadow: SHADOW_TASKS })
}

/// Every task class's ceiling, as one cell of the most cells and the largest share holds them:
/// the caps [`WAIT_NODES_MAX`] sums.
const TASK_CEILINGS: PlaneTaskCaps = PlaneTaskCaps {
    conn: CONN_TASKS,
    peers: Some(PeerTaskCaps {
        ns_apply: NS_APPLY_TASKS,
        owner_pub: OWNER_PUB_TASKS,
        gated: GATED_TASKS,
    }),
    compact: COMPACT_TASKS,
    shadow: SHADOW_TASKS,
};

/// The task slots of a cell of `caps`: every class's cap, each named, so a class the struct
/// gains is a compile error here and in [`TASK_CEILINGS`] until it is counted. Each cap is at
/// most its ceiling, so the sum is at most [`WAIT_NODES_MAX`]'s, which the compiler evaluates.
const fn task_slots(caps: &PlaneTaskCaps) -> u32 {
    let PlaneTaskCaps { conn, peers, compact, shadow } = caps;
    let mut slots = conn.entries_max() + compact.entries_max() + shadow.entries_max();
    if let Some(PeerTaskCaps { ns_apply, owner_pub, gated }) = peers {
        slots += ns_apply.entries_max() + owner_pub.entries_max() + gated.entries_max();
    }
    slots
}

/// Wait nodes one cell holds at most (ADR-0177 D5): one for each live task's one parked wait, so
/// the task slots of every class at its ceiling. Unit: nodes per cell.
pub const WAIT_NODES_MAX: u32 = task_slots(&TASK_CEILINGS);

/// The cell's pool of wait nodes (ADR-0177 D5): `T`, one node for each task slot of the cell's
/// classes, which every wait list of the cell takes. The ceiling of the derived cap
/// ([`wait_nodes`]), census name `wait-nodes`, filled while serving. Unit: nodes per cell.
/// Owner: the plane, which builds the pool before the cell serves. Crossing: derived never, as
/// a task holds at most one parked wait.
pub const WAIT_NODES: Cap = Cap::entries::<WAIT_NODES_MAX>("wait-nodes", CapFill::Serving);

/// The pool of a cell of `caps` (ADR-0177 D5): its task slots, `T`.
///
/// # Errors
/// `CapError::Layout` when `T` passes [`WAIT_NODES_MAX`]; derived never.
pub(crate) const fn wait_nodes(caps: &PlaneTaskCaps) -> Result<Cap, CapError> {
    let nodes = task_slots(caps);
    // Canary: the pool is built over (1 + k) T nodes, and the gate's entry array with it, k
    // the fewest whose nodes' and entries' bytes pass ten times the idle-resident tool's
    // 256 KiB control a cell, so the idle node's resident rise is one the tool gives a verdict
    // on.
    #[cfg(inf_canary_wait_nodes_oversized)]
    let nodes = nodes * (1 + wait_nodes_oversized_k(nodes));
    Cap::derived(&WAIT_NODES, nodes)
}

/// The oversized-pool canary's `k` at `nodes`: the fewest counts of `nodes` whose bytes, a
/// node's slot and the watermark gate's entry each, pass ten times the idle-resident tool's
/// 256 KiB control a cell, 2,621,440 B, at the strides the build reads. The entry array is
/// built at the pool's cap (ADR-0177 D5), so the plant grows both.
#[cfg(any(all(test, feature = "doc"), inf_canary_wait_nodes_oversized))]
pub(crate) const fn wait_nodes_oversized_k(nodes: u32) -> u32 {
    let stride =
        (inf_runtime::WaitNodes::NODE_SLOT_BYTES + inf_runtime::WatermarkGate::ENTRY_BYTES) as u64;
    let per_count = nodes as u64 * stride;
    (WAIT_NODES_PLANT_FLOOR_BYTES / per_count + 1) as u32
}

/// The bytes a cell the oversized-pool canary must pass: ten times the idle-resident tool's
/// 256 KiB control a cell, the least resident rise the tool separates from its control.
#[cfg(any(all(test, feature = "doc"), inf_canary_wait_nodes_oversized))]
pub(crate) const WAIT_NODES_PLANT_FLOOR_BYTES: u64 = 10 * 262_144;

/// Keys one cell's credit wait list holds (ADR-0177 D5): one per peer, `cells − 1`, each held
/// from the list's build, before the cell serves; a cell of a node of one cell holds none and
/// builds no map. The ceiling of `cells − 1`, census name `credit-waiter-keys`, filled whole at
/// assembly. Unit: keys per cell. Owner: the plane's credit list. Crossing: none while serving;
/// a build past it ends the assembly with `credit-waiter-keys-full-at-assembly`.
pub const CREDIT_WAITER_KEYS: Cap =
    Cap::entries::<{ CELLS_MAX - 1 }>("credit-waiter-keys", CapFill::Assembly);

/// Keys the DDL wait list holds (ADR-0177 D5): the one key its pumps wait on, held from the
/// list's build. Census name `ddl-waiter-keys`, filled whole at assembly. Unit, owner and
/// crossing: as [`CREDIT_WAITER_KEYS`], the plane's DDL list.
pub const DDL_WAITER_KEYS: Cap = Cap::entries::<1>("ddl-waiter-keys", CapFill::Assembly);

/// Keys the checkpoint wait list holds (ADR-0177 D5): the one key its pumps wait on, held from
/// the list's build. Census name `ckpt-waiter-keys`, filled whole at assembly. Unit, owner and
/// crossing: as [`CREDIT_WAITER_KEYS`], the plane's checkpoint list.
pub const CKPT_WAITER_KEYS: Cap = Cap::entries::<1>("ckpt-waiter-keys", CapFill::Assembly);

/// The credit list's keys of a node of `cells` cells: `cells − 1`, `None` at one cell.
///
/// # Errors
/// `CapError::Layout` when the cells are 0 or above `CELLS_MAX`.
pub(crate) const fn credit_waiter_keys(cells: u16) -> Result<Option<Cap>, CapError> {
    match cells {
        0 => Err(CapError::Layout),
        1 => Ok(None),
        cells => match Cap::derived(&CREDIT_WAITER_KEYS, cells as u32 - 1) {
            Ok(cap) => Ok(Some(cap)),
            Err(refused) => Err(refused),
        },
    }
}

/// A pool of wait nodes for a test's own gates and lists.
#[cfg(test)]
pub(crate) const TEST_WAIT_NODES: Cap = Cap::entries::<64>("test-wait-nodes", CapFill::Serving);

/// Live holders one connection task may have on its cell's fabric gate
/// (ADR-0177 D2, D4): its remote ops in flight. Replies that land out of
/// order park in their holders until emitted, so this also bounds a
/// connection's parked replies. Unit: gate units per `conn` task. Owner:
/// the fabric gate, which issues them with the task at its admission.
/// Crossing: pace — a pump dispatches a command only while a unit is free,
/// and a fan with every unit out settles its oldest leg before it sends
/// the next, so a fan wider than this runs in windows.
pub const GATE_UNITS_PER_CONN_TASK: u32 = 32;

/// The units a cell's fabric gate issues to its tasks (ADR-0177 D2, D4):
/// `Σ T_j × W_j`, 32 a `conn` slot and `cells − 1` for the `owner-pub`
/// pump; the other classes send nothing. Every class is named, so a class
/// the caps gain is a compile error here until its units are counted.
pub(crate) const fn gate_units(caps: &PlaneTaskCaps) -> u32 {
    let PlaneTaskCaps { conn, peers, compact: _, shadow: _ } = caps;
    let conn = conn.entries_max() * GATE_UNITS_PER_CONN_TASK;
    match peers {
        None => conn,
        // The `owner-pub` pump's one unit per peer, its own slots aside: the
        // `ns-apply` class's `cells − 1` slots. `gated` sends nothing.
        Some(PeerTaskCaps { ns_apply, owner_pub: _, gated: _ }) => conn + ns_apply.entries_max(),
    }
}

/// A cell's fabric gate holders (ADR-0177 D4): `H = Σ T_j × W_j + (cells −
/// 1) × d`, the units its tasks hold and one orphan for each op a peer may
/// leave unanswered; `None` at one cell, which sends no request.
///
/// # Errors
/// `CapError::Layout` when `H` passes `GATE_HOLDERS_MAX`, before the cell
/// serves.
pub(crate) const fn gate_holders(
    caps: &PlaneTaskCaps,
    cells: u16,
    data_credits: u32,
) -> Result<Option<Cap>, CapError> {
    if cells <= 1 {
        return Ok(None);
    }
    let holders = gate_units(caps) as u64 + (cells as u64 - 1) * data_credits as u64;
    // Canary: the gate is built over twice its holders, which the idle
    // node's resident set must show.
    #[cfg(inf_canary_gate_holders_doubled)]
    let holders = 2 * holders;
    if holders > u32::MAX as u64 {
        return Err(CapError::Layout);
    }
    match Cap::derived(&inf_runtime::limits::GATE_HOLDERS, holders as u32) {
        Ok(cap) => Ok(Some(cap)),
        Err(refused) => Err(refused),
    }
}

/// Verdicts the `gated` task publishes in one poll (ADR-0177 D3). Unit:
/// replies per poll. Owner: the `gated` task, inside a poll, so charged to
/// Foreground. Crossing: the task yields and is polled again in ready
/// order, so an origin's verdicts never hold the loop past one poll.
pub const GATED_REPLIES_PER_POLL: usize = 1_024;

/// Ring slots per directed cell pair of the shipped binary's mesh: a power
/// of two with reply headroom over [`FABRIC_CREDITS_PER_PEER`]. Unit: ring
/// slots. Owner: `infinityd`'s mesh constant, built through
/// `checked_mesh_config`. Crossing: none at run time; a pair the check
/// refuses fails the build.
pub const FABRIC_RING_SLOTS_PER_PEER: usize = 4096;

#[cfg(test)]
mod tests {
    use super::ACK_WAKES_PER_TURN;

    /// The wake budget's two bounds (ADR-0177 A2.1): at most the loop's default EXECUTE budget,
    /// which polls that many tasks a pass, so waking more runs nothing sooner; and the value the
    /// limit's doc reasons from.
    #[test]
    fn the_wake_budget_is_at_most_one_execute_pass() {
        let exec_budget = inf_runtime::LoopConfig::default().exec_budget;
        let budget = usize::try_from(ACK_WAKES_PER_TURN.get()).expect("a u32 fits a usize here");
        assert!(
            budget <= exec_budget,
            "{budget} wakes a turn past an EXECUTE pass's {exec_budget}"
        );
        assert_eq!(budget, 1_024, "the budget the limit's doc reads its batch time at");
    }

    /// Every copy of the wake budget in the workspace: the crates below this one cannot reach
    /// the limit, so a source of theirs keeps its own `WAKE_BUDGET`. Every `.rs` file of each
    /// workspace member's tree, as the root manifest's `members` name them, is read (a build's
    /// output, by Cargo's `CACHEDIR.TAG`, and this file, whose planted strings name it, aside),
    /// and one that names `WAKE_BUDGET` declares it once, as a literal equal to the limit: a
    /// copy joins the tie by its name wherever it sits, a test module under `src/` among them,
    /// with no list of files to extend. A copy that drifts, is declared twice, `pub`, commented
    /// out or not a literal is red here, and so is a walk that finds no copy.
    #[test]
    #[allow(clippy::disallowed_methods, reason = "test-only: reads the workspace members' sources")]
    fn every_copy_of_the_wake_budget_in_the_workspace_is_the_limit() {
        let limit = u64::from(ACK_WAKES_PER_TURN.get());
        let sources = member_sources();
        let mut copies = 0_usize;
        for path in &sources {
            let source = std::fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            if let Some(copy) = wake_budget_copy(&source) {
                let file = path.display();
                assert_eq!(copy, Ok(limit), "{file}'s wake budget is not ACK_WAKES_PER_TURN");
                copies += 1;
            }
        }
        let scanned = sources.len();
        assert!(copies > 0, "VACUOUS no-copy: {scanned} sources name no WAKE_BUDGET");
    }

    /// The tie's planted reds: a copy one past the limit, a copy declared twice, a source with
    /// no copy, a copy not a literal, and a source that names `WAKE_BUDGET` with its one
    /// declaration `pub` or commented out are each refused, so the tie above can go red; a
    /// source that never names it is no copy.
    #[test]
    fn the_wake_budget_tie_refuses_a_drifted_doubled_hidden_or_missing_copy() {
        let limit = u64::from(ACK_WAKES_PER_TURN.get());
        let drifted = format!("const WAKE_BUDGET: usize = {};\n", limit + 1);
        assert_eq!(declared_wake_budget(&drifted), Ok(limit + 1), "the drifted value is read");
        assert_ne!(declared_wake_budget(&drifted), Ok(limit), "a drifted copy is refused");
        let doubled = "const WAKE_BUDGET: u64 = 1_024;\nconst WAKE_BUDGET: u64 = 1_024;\n";
        assert_eq!(declared_wake_budget(doubled), Err("declared twice"));
        assert_eq!(declared_wake_budget("const OTHER: u64 = 1;\n"), Err("not declared"));
        assert_eq!(declared_wake_budget("const WAKE_BUDGET: u64 = x;\n"), Err("not a number"));
        let public = "pub const WAKE_BUDGET: u64 = 1_024;\n";
        assert_eq!(wake_budget_copy(public), Some(Err("not declared")), "a pub copy is refused");
        let hidden = "// const WAKE_BUDGET: u64 = 1_024;\nfn f() { g(WAKE_BUDGET) }\n";
        assert_eq!(wake_budget_copy(hidden), Some(Err("not declared")), "a hidden copy");
        assert_eq!(wake_budget_copy("const BUDGET: u64 = 1_024;\n"), None, "no name, no copy");
    }

    /// A source's copy of the wake budget: none when it never names `WAKE_BUDGET`, else its
    /// one declaration's value, or why it has none.
    fn wake_budget_copy(source: &str) -> Option<Result<u64, &'static str>> {
        source.contains("WAKE_BUDGET").then(|| declared_wake_budget(source))
    }

    /// The workspace members the root manifest's one `members = [...]` line names: a literal
    /// path, or a `dir/*` glob for each directory under `dir` that holds a `Cargo.toml`. Any
    /// other glob, or no such line, fails the test, so the walk never reads less than the
    /// workspace without saying so.
    #[allow(
        clippy::disallowed_methods,
        reason = "test-only: reads the workspace's root manifest and lists its member globs"
    )]
    fn workspace_members(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
            .unwrap_or_else(|error| panic!("the root manifest: {error}"));
        let line = manifest
            .lines()
            .find_map(|line| line.trim().strip_prefix("members = ["))
            .expect("the root manifest's one-line `members = [...]`");
        let globs = line.strip_suffix(']').expect("the members on one line");
        let mut members = Vec::new();
        for glob in globs.split(',').map(str::trim).filter(|glob| !glob.is_empty()) {
            let glob = glob.trim_matches('"');
            let Some(parent) = glob.strip_suffix("/*") else {
                assert!(!glob.contains('*'), "a members glob the walk does not read: {glob}");
                members.push(root.join(glob));
                continue;
            };
            for (path, is_dir) in listed(&root.join(parent)) {
                if is_dir && path.join("Cargo.toml").is_file() {
                    members.push(path);
                }
            }
        }
        assert!(!members.is_empty(), "the root manifest names no member");
        members
    }

    /// Each entry of `dir` and whether it is a directory; a directory that cannot be listed
    /// fails the test.
    #[allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "test-only: lists the workspace members' directories"
    )]
    fn listed(dir: &std::path::Path) -> Vec<(std::path::PathBuf, bool)> {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display()));
        entries
            .map(|entry| {
                let entry = entry.unwrap_or_else(|error| panic!("{}: {error}", dir.display()));
                let kind = entry.file_type().unwrap_or_else(|error| panic!("{error}"));
                (entry.path(), kind.is_dir())
            })
            .collect()
    }

    /// Every `.rs` file of each workspace member's whole tree but this file, whose planted
    /// strings name the budget, and which the walk must meet exactly once.
    fn member_sources() -> Vec<std::path::PathBuf> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let this_file = root.join(file!());
        let (files, met) = rust_sources(workspace_members(&root), &this_file);
        assert_eq!(met, 1, "the walk met {} {met} times, where once", this_file.display());
        files
    }

    /// The `.rs` files under `trees` but `aside`, sorted, walked with a stack of directories,
    /// and how many times the walk met `aside`. A directory is a build's output, and skipped,
    /// only when it holds `CACHEDIR.TAG`, the cache-directory tag Cargo writes at the root of
    /// each target directory it makes, as a regular file: a link of that name tags nothing, as
    /// `find -type f` reads it in `scripts/source-files.sh`, whose rule this walk keeps itself
    /// because the test has no shell in its path. A name never skips a directory, so a source
    /// directory named `target` at any depth is read.
    #[allow(clippy::disallowed_methods, reason = "test-only: reads each directory's cache tag")]
    fn rust_sources(
        mut trees: Vec<std::path::PathBuf>,
        aside: &std::path::Path,
    ) -> (Vec<std::path::PathBuf>, usize) {
        let (mut files, mut met) = (Vec::new(), 0);
        while let Some(dir) = trees.pop() {
            for (path, is_dir) in listed(&dir) {
                if is_dir {
                    let tag = std::fs::symlink_metadata(path.join("CACHEDIR.TAG"));
                    if !tag.is_ok_and(|tag| tag.file_type().is_file()) {
                        trees.push(path);
                    }
                } else if path == aside {
                    met += 1;
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    files.push(path);
                }
            }
        }
        files.sort();
        (files, met)
    }

    /// The walk's build-output rule, on a scratch tree: a directory named `target` that holds
    /// no `CACHEDIR.TAG` is read, so its source is met; one named `target` that holds the tag,
    /// and one under another name that holds it, are skipped, so neither's source is; one whose
    /// `CACHEDIR.TAG` is a link to a tag is read, since a link tags nothing; the `aside` file is
    /// met once and left out. A rule that skips by the name `target` leaves the untagged
    /// directory's source unread, and one that follows the link leaves the linked one's.
    #[test]
    #[allow(clippy::disallowed_methods, reason = "test-only: builds the walk's scratch tree")]
    fn the_tie_walk_skips_a_tagged_directory_and_reads_an_untagged_target() {
        let root = std::env::temp_dir().join(format!("inf-limits-tie-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let member = root.join("member");
        let untagged = member.join("target");
        let (tagged, tagged_other) = (member.join("nested/target"), member.join("out"));
        let linked = member.join("linked");
        for dir in [&untagged, &tagged, &tagged_other, &linked] {
            std::fs::create_dir_all(dir)
                .unwrap_or_else(|error| panic!("{}: {error}", dir.display()));
        }
        let tag = "Signature: 8a477f597d28d172789f06886806bc55\n";
        let (read, read_linked) = (untagged.join("read.rs"), linked.join("read.rs"));
        let aside = member.join("aside.rs");
        let files = [
            (tagged.join("CACHEDIR.TAG"), tag),
            (tagged_other.join("CACHEDIR.TAG"), tag),
            (read.clone(), ""),
            (read_linked.clone(), ""),
            (tagged.join("skipped.rs"), ""),
            (tagged_other.join("skipped.rs"), ""),
            (aside.clone(), ""),
        ];
        for (file, contents) in &files {
            std::fs::write(file, contents)
                .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
        }
        std::os::unix::fs::symlink(tagged.join("CACHEDIR.TAG"), linked.join("CACHEDIR.TAG"))
            .unwrap_or_else(|error| panic!("the linked tag: {error}"));
        let (sources, met) = rust_sources(vec![member], &aside);
        let _ = std::fs::remove_dir_all(&root);
        let want = [read_linked, read];
        assert_eq!(sources, want, "the walk's sources, where the untagged and the linked ones");
        assert_eq!(met, 1, "the walk met the aside file {met} times, where once");
    }

    /// The value of the one `const WAKE_BUDGET: <type> = <digits>;` line of `source`.
    fn declared_wake_budget(source: &str) -> Result<u64, &'static str> {
        let mut lines = source.lines().filter_map(|line| {
            let rest = line.trim_start().strip_prefix("const WAKE_BUDGET:")?;
            let (_, value) = rest.split_once('=')?;
            Some(value.trim().trim_end_matches(';').replace('_', ""))
        });
        let value = lines.next().ok_or("not declared")?;
        if lines.next().is_some() {
            return Err("declared twice");
        }
        value.parse().map_err(|_| "not a number")
    }
}
