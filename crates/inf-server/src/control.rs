//! The control thread (M2-S08, ADR-0015 D3) — the slow plane the
//! architecture always reserved (§4). Its first job: **single writer of
//! the node catalog `META` file**. Cells must never block on file I/O, so
//! DDL persists asynchronously: the origin cell's pump sends a request
//! over a bounded channel and parks on a cell-local waitlist; this thread
//! performs the write-new + fsync + rename + dir-fsync swap (`inf-log`'s
//! `meta` protocol) and publishes a monotone **persist epoch**; every
//! cell's MAINTAIN observes the epoch and wakes its parked pumps.
//!
//! The thread also owns namespace-id allocation (ids are node-unique,
//! allocated once, never reused — ADR-0015 D2): a shared `AtomicU32`
//! seeded from the catalog's `next_id` at boot. One `fetch_add` per DDL is
//! control-plane traffic; L1's no-shared-atomics rule binds the data plane.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use inf_log::fs::StdSegmentFs;
use inf_log::meta::{read_meta, write_meta};
use inf_store::{FIRST_NAMED_NS_ID, NsCatalog};

/// One cell's boot-recovery slot on the [`RecoveryBoard`] (M2-S15).
/// Single writer (the owning cell, relaxed stores); readers are the
/// control thread's progress printer and any cell's `INFO persistence`
/// aggregation. Control-plane observability, not shared data-plane state
/// (the L1 carve-out class the park board documents).
#[derive(Debug, Default)]
pub struct CellRecoverySlot {
    /// 0 = recovering, 1 = ready (recovery complete, cell serving).
    state: AtomicU8,
    bytes_total: AtomicU64,
    bytes_done: AtomicU64,
    segments_total: AtomicU64,
    segments_done: AtomicU64,
    /// Records recovered (checkpoint + tail) — the ready log line.
    records: AtomicU64,
    /// Packed torn-truncation LSN + 1 (0 = no torn tail) for the boot line.
    torn_at: AtomicU64,
    /// Recovery phase about to run (M2.5-S01): published *before* each
    /// step so a step stalled inside the kernel names itself — the
    /// ADR-0022 D7 wedge was invisible precisely because nothing was
    /// published around the blocking section. 0 = not started; codes per
    /// `Recovery::phase_code`.
    phase: AtomicU8,
}

impl CellRecoverySlot {
    /// Publishes one progress sample (owning cell only).
    pub fn publish(&self, bytes_done: u64, bytes_total: u64, segs_done: u64, segs_total: u64) {
        self.bytes_done.store(bytes_done, Ordering::Relaxed);
        self.bytes_total.store(bytes_total, Ordering::Relaxed);
        self.segments_done.store(segs_done, Ordering::Relaxed);
        self.segments_total.store(segs_total, Ordering::Relaxed);
    }

    /// Marks this cell recovered (owning cell only, once).
    pub fn mark_ready(&self, records: u64, torn_truncated_at: Option<inf_log::Lsn>) {
        debug_assert_eq!(self.state.load(Ordering::Relaxed), 0, "mark_ready called twice");
        self.records.store(records, Ordering::Relaxed);
        self.torn_at.store(torn_truncated_at.map_or(0, |lsn| lsn.to_u64() + 1), Ordering::Relaxed);
        self.state.store(1, Ordering::Release);
    }

    /// Publishes the phase the next recovery step will run (owning cell).
    pub fn publish_phase(&self, code: u8) {
        self.phase.store(code, Ordering::Relaxed);
    }

    /// Last published phase code (0 = recovery never stepped).
    pub fn phase(&self) -> u8 {
        self.phase.load(Ordering::Relaxed)
    }

    /// Human name for a phase code — the stuck-cell narration. Codes
    /// 1–6 are `Recovery::phase_code`; 10+ are the assembly's setup
    /// steps (published by `cell_main` so a pre-loop stall names itself —
    /// the 500-cycle storm caught exactly that class).
    #[must_use]
    pub fn phase_name(code: u8) -> &'static str {
        match code {
            0 => "spawned",
            1 => "start",
            2 => "checkpoint",
            3 => "replay",
            4 => "audit",
            5 => "finish",
            6 => "complete",
            10 => "setup:listen",
            11 => "setup:pool",
            12 => "setup:driver",
            13 => "setup:register",
            14 => "setup:keyspace",
            15 => "setup:plane",
            16 => "setup:loop",
            _ => "unknown",
        }
    }

    pub fn ready(&self) -> bool {
        self.state.load(Ordering::Acquire) == 1
    }

    pub fn bytes(&self) -> (u64, u64) {
        (self.bytes_done.load(Ordering::Relaxed), self.bytes_total.load(Ordering::Relaxed))
    }

    pub fn segments(&self) -> (u64, u64) {
        (self.segments_done.load(Ordering::Relaxed), self.segments_total.load(Ordering::Relaxed))
    }

    /// Records recovered (checkpoint + tail), valid once [`ready`](Self::ready).
    pub fn records(&self) -> u64 {
        self.records.load(Ordering::Relaxed)
    }

    /// `Some(lsn)` when this cell truncated a torn final write (M2-S14).
    pub fn torn_truncated_at(&self) -> Option<inf_log::Lsn> {
        match self.torn_at.load(Ordering::Relaxed) {
            0 => None,
            packed => Some(inf_log::Lsn::from_u64(packed - 1)),
        }
    }
}

/// Node-wide boot-recovery progress (M2-S15): one slot per cell, created
/// before any cell thread spawns. `-LOADING` gates on [`all_ready`]
/// (Redis semantics: the node loads until *every* cell is live); `INFO
/// persistence` and the control thread's progress lines aggregate it.
///
/// [`all_ready`]: RecoveryBoard::all_ready
#[derive(Debug)]
pub struct RecoveryBoard {
    cells: Vec<CellRecoverySlot>,
    /// Unix ms at board creation (boot start) — `loading_start_time`/ETA.
    start_unix_ms: u64,
}

impl RecoveryBoard {
    #[must_use]
    pub fn new(cells: u16, start_unix_ms: u64) -> RecoveryBoard {
        RecoveryBoard {
            cells: (0..cells).map(|_| CellRecoverySlot::default()).collect(),
            start_unix_ms,
        }
    }

    /// This cell's slot (single writer: the owning cell).
    ///
    /// # Panics
    /// Out-of-range cell id (assembly bug).
    #[must_use]
    pub fn slot(&self, cell: u16) -> &CellRecoverySlot {
        &self.cells[usize::from(cell)]
    }

    /// True once every cell finished recovery — the `-LOADING` gate drops.
    #[must_use]
    pub fn all_ready(&self) -> bool {
        self.cells.iter().all(CellRecoverySlot::ready)
    }

    #[must_use]
    pub fn ready_cells(&self) -> u64 {
        self.cells.iter().filter(|slot| slot.ready()).count() as u64
    }

    #[must_use]
    pub fn cell_count(&self) -> u64 {
        self.cells.len() as u64
    }

    /// Aggregate (done, total) bytes across cells.
    #[must_use]
    pub fn bytes(&self) -> (u64, u64) {
        self.cells.iter().fold((0, 0), |(d, t), slot| {
            let (done, total) = slot.bytes();
            (d + done, t + total)
        })
    }

    #[must_use]
    pub fn start_unix_ms(&self) -> u64 {
        self.start_unix_ms
    }
}

/// One cell's checkpoint request/publication slot (M2-S20, ADR-0021 D6).
/// Same L1 control-plane carve-out class as the recovery board: the
/// request side is written by command programs (any cell's pump), the
/// publication side by the owning cell when the MANIFEST swap's
/// dir-fsync commits (the durability point — ADR-0017). Readers are
/// `INF.CKPT WAIT` pollers, `LASTSAVE`, and `INFO` aggregation.
#[derive(Debug, Default)]
pub struct CkptSlot {
    /// Highest requested epoch (cells edge-detect this in MAINTAIN).
    req: AtomicU64,
    /// Highest epoch whose checkpoint reached a *durable* MANIFEST.
    published: AtomicU64,
    /// The published checkpoint id / unix ms (the `LASTSAVE` currency).
    ckpt_id: AtomicU64,
    unix_ms: AtomicU64,
}

impl CkptSlot {
    /// Highest requested epoch (the MAINTAIN edge-detection input).
    pub fn req(&self) -> u64 {
        self.req.load(Ordering::Relaxed)
    }

    /// Publication (owning cell, at the swap's dir-fsync commit).
    pub fn publish(&self, epoch: u64, ckpt_id: u64, unix_ms: u64) {
        self.ckpt_id.store(ckpt_id, Ordering::Relaxed);
        self.unix_ms.store(unix_ms, Ordering::Relaxed);
        self.published.fetch_max(epoch, Ordering::Release);
    }

    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }

    pub fn last_unix_ms(&self) -> u64 {
        self.unix_ms.load(Ordering::Relaxed)
    }

    pub fn last_ckpt_id(&self) -> u64 {
        self.ckpt_id.load(Ordering::Relaxed)
    }
}

/// Node-wide checkpoint board: one slot per cell.
#[derive(Debug)]
pub struct CkptBoard {
    cells: Vec<CkptSlot>,
}

impl CkptBoard {
    /// This cell's slot.
    ///
    /// # Panics
    /// Out-of-range cell id (assembly bug).
    #[must_use]
    pub fn slot(&self, cell: u16) -> &CkptSlot {
        &self.cells[usize::from(cell)]
    }

    /// The lowest published epoch across cells — `INF.CKPT WAIT` (all
    /// cells) completes when this covers its request epoch.
    #[must_use]
    pub fn min_published(&self) -> u64 {
        self.cells.iter().map(CkptSlot::published).min().unwrap_or(0)
    }

    /// Sum of published epochs — a cheap any-cell-published change signal
    /// (the MAINTAIN wake edge for parked waiters).
    #[must_use]
    pub fn published_sum(&self) -> u64 {
        self.cells.iter().map(CkptSlot::published).sum()
    }

    /// Newest durable MANIFEST publication time across cells (unix ms) —
    /// `LASTSAVE`/`rdb_last_save_time` (0 before the first publication).
    #[must_use]
    pub fn max_unix_ms(&self) -> u64 {
        self.cells.iter().map(CkptSlot::last_unix_ms).max().unwrap_or(0)
    }
}

/// One cell's memory-gauge publication payload — and, field-for-field,
/// the node-wide fold `MemoryBoard::totals` returns.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MemoryGauges {
    /// The cell's `used_memory` contribution (attributed keyspace bytes +
    /// wire buffers + connection state).
    pub used_bytes: u64,
    pub docs_live: u64,
    pub doc_tape_bytes: u64,
    pub doc_arena_bytes: u64,
    pub doc_resident_bytes: u64,
    pub doc_intern_bytes: u64,
    pub doc_slack_bytes: u64,
    pub doc_scratch_bytes: u64,
    pub doc_path_cache_bytes: u64,
}

/// Per-cell memory publication slot. Same L1 control-plane carve-out
/// class as the recovery/checkpoint boards: the owning cell writes its
/// slot (MAINTAIN cadence, plus fresh at its own `INFO` render); readers
/// fold across slots. Gauges are advisory — a peer's value lags its
/// publisher by at most one publish period, like every other INFO gauge.
#[derive(Debug, Default)]
pub struct MemorySlot {
    used_bytes: AtomicU64,
    docs_live: AtomicU64,
    doc_tape_bytes: AtomicU64,
    doc_arena_bytes: AtomicU64,
    doc_resident_bytes: AtomicU64,
    doc_intern_bytes: AtomicU64,
    doc_slack_bytes: AtomicU64,
    doc_scratch_bytes: AtomicU64,
    doc_path_cache_bytes: AtomicU64,
}

impl MemorySlot {
    pub fn publish(&self, g: MemoryGauges) {
        self.used_bytes.store(g.used_bytes, Ordering::Relaxed);
        self.docs_live.store(g.docs_live, Ordering::Relaxed);
        self.doc_tape_bytes.store(g.doc_tape_bytes, Ordering::Relaxed);
        self.doc_arena_bytes.store(g.doc_arena_bytes, Ordering::Relaxed);
        self.doc_resident_bytes.store(g.doc_resident_bytes, Ordering::Relaxed);
        self.doc_intern_bytes.store(g.doc_intern_bytes, Ordering::Relaxed);
        self.doc_slack_bytes.store(g.doc_slack_bytes, Ordering::Relaxed);
        self.doc_scratch_bytes.store(g.doc_scratch_bytes, Ordering::Relaxed);
        self.doc_path_cache_bytes.store(g.doc_path_cache_bytes, Ordering::Relaxed);
    }

    fn read(&self) -> MemoryGauges {
        MemoryGauges {
            used_bytes: self.used_bytes.load(Ordering::Relaxed),
            docs_live: self.docs_live.load(Ordering::Relaxed),
            doc_tape_bytes: self.doc_tape_bytes.load(Ordering::Relaxed),
            doc_arena_bytes: self.doc_arena_bytes.load(Ordering::Relaxed),
            doc_resident_bytes: self.doc_resident_bytes.load(Ordering::Relaxed),
            doc_intern_bytes: self.doc_intern_bytes.load(Ordering::Relaxed),
            doc_slack_bytes: self.doc_slack_bytes.load(Ordering::Relaxed),
            doc_scratch_bytes: self.doc_scratch_bytes.load(Ordering::Relaxed),
            doc_path_cache_bytes: self.doc_path_cache_bytes.load(Ordering::Relaxed),
        }
    }
}

/// Node-wide memory board: one slot per cell (M3-S25 finding — `INFO`
/// rendered the *serving cell's* keyspace attribution beside
/// *process-wide* RSS, so `docs_live` read 17,801 on a 71,200-document
/// node and `mem_fragmentation_ratio` mixed scopes. The memory section
/// now folds this board — §16's "control thread aggregates on scrape").
#[derive(Debug)]
pub struct MemoryBoard {
    cells: Vec<MemorySlot>,
}

impl MemoryBoard {
    #[must_use]
    pub fn new(cells: u16) -> MemoryBoard {
        MemoryBoard { cells: (0..cells).map(|_| MemorySlot::default()).collect() }
    }

    /// This cell's slot.
    ///
    /// # Panics
    /// Out-of-range cell id (assembly bug).
    #[must_use]
    pub fn slot(&self, cell: u16) -> &MemorySlot {
        &self.cells[usize::from(cell)]
    }

    /// Field-wise sum across every cell's last publication.
    #[must_use]
    pub fn totals(&self) -> MemoryGauges {
        let mut t = MemoryGauges::default();
        for slot in &self.cells {
            let g = slot.read();
            t.used_bytes += g.used_bytes;
            t.docs_live += g.docs_live;
            t.doc_tape_bytes += g.doc_tape_bytes;
            t.doc_arena_bytes += g.doc_arena_bytes;
            t.doc_resident_bytes += g.doc_resident_bytes;
            t.doc_intern_bytes += g.doc_intern_bytes;
            t.doc_slack_bytes += g.doc_slack_bytes;
            t.doc_scratch_bytes += g.doc_scratch_bytes;
            t.doc_path_cache_bytes += g.doc_path_cache_bytes;
        }
        t
    }
}

/// One catalog snapshot to persist (the origin cell's post-apply export).
struct PersistReq {
    catalog: NsCatalog,
    /// The epoch value published once this snapshot is durable.
    epoch: u64,
}

/// Control-thread work (single receiver, bounded — L3).
enum ControlMsg {
    Persist(PersistReq),
    /// Blocking unlink delegated by a cell (M2-S11/S12, ADR-0017):
    /// freeing a truncated segment's or stale checkpoint's pages is
    /// O(file size) in the kernel — a measured multi-ms stall when done
    /// on the loop. The named file is already outside every recovery
    /// unit (below the durable manifest's floor / not the named `.ick`),
    /// so any thread may delete it; failure is re-collected by boot GC.
    Unlink(PathBuf),
}

/// Shared handle the assembly wires into every cell's plane.
pub struct ControlHandle {
    tx: mpsc::SyncSender<ControlMsg>,
    next_ns_id: AtomicU32,
    next_epoch: AtomicU64,
    persisted_epoch: Arc<AtomicU64>,
    /// Manual-checkpoint request epoch (M2-S10, ADR-0016 D7): bumping it
    /// asks every durable cell to checkpoint; cells edge-detect it in
    /// MAINTAIN (one relaxed load — the persisted-epoch pattern).
    /// `INF.CKPT`/`BGSAVE` ride this at S20 (per-cell targeting refines
    /// there).
    ckpt_epoch: AtomicU64,
    /// Per-cell checkpoint request/publication slots (M2-S20).
    ckpt_board: Arc<CkptBoard>,
    /// Boot-recovery progress + readiness (M2-S15).
    recovery: Arc<RecoveryBoard>,
    /// Per-cell memory gauges for node-scope `INFO` (M3-S25 fix).
    memory_board: Arc<MemoryBoard>,
}

impl ControlHandle {
    /// Allocates one namespace id (never reused — ADR-0015 D2).
    pub fn alloc_ns_id(&self) -> u32 {
        self.next_ns_id.fetch_add(1, Ordering::Relaxed)
    }

    /// The id the allocator would hand out next (catalog `next_id`).
    pub fn next_ns_id(&self) -> u32 {
        self.next_ns_id.load(Ordering::Relaxed)
    }

    /// Queues `catalog` for a durable swap; returns the epoch to await.
    /// The bounded channel makes DDL admission explicit: a full queue
    /// blocks the *sender* briefly (DDL-rate traffic, never the data path
    /// of other connections — the pump yields between commands).
    pub fn request_persist(&self, catalog: NsCatalog) -> u64 {
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        self.tx
            .send(ControlMsg::Persist(PersistReq { catalog, epoch }))
            .expect("control thread alive (fail-stop)");
        epoch
    }

    /// Delegates one blocking unlink to the control thread (M2-S11/S12).
    /// Non-blocking: `false` = queue full — the caller keeps the path and
    /// retries next MAINTAIN slice (bounded local queue, never a stall).
    pub fn request_unlink(&self, path: PathBuf) -> bool {
        self.tx.try_send(ControlMsg::Unlink(path)).is_ok()
    }

    /// True once every persist up to `epoch` is durable. Cells poll this
    /// from MAINTAIN (one relaxed load) and wake their parked DDL pumps.
    pub fn persisted(&self, epoch: u64) -> bool {
        self.persisted_epoch.load(Ordering::Acquire) >= epoch
    }

    /// The highest durable epoch (the MAINTAIN edge-detection input).
    pub fn persisted_epoch(&self) -> u64 {
        self.persisted_epoch.load(Ordering::Acquire)
    }

    /// Requests a checkpoint on every durable cell (M2-S10/S20 — the
    /// `INF.CKPT`/`BGSAVE` surface). Returns the request epoch; `WAIT`
    /// completes once every slot's published epoch covers it.
    pub fn request_ckpt_all(&self) -> u64 {
        let epoch = self.ckpt_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        for cell in &self.ckpt_board.cells {
            cell.req.fetch_max(epoch, Ordering::Relaxed);
        }
        epoch
    }

    /// Requests a checkpoint on one cell (`INF.CKPT CELL k`, M2-S20).
    /// Epochs come from the same allocator, so cross-target `WAIT`s
    /// compose (published epochs are monotone per slot).
    pub fn request_ckpt_cell(&self, cell: u16) -> u64 {
        let epoch = self.ckpt_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        self.ckpt_board.slot(cell).req.fetch_max(epoch, Ordering::Relaxed);
        epoch
    }

    /// The checkpoint board (request/publication slots per cell).
    #[must_use]
    pub fn ckpt_board(&self) -> &Arc<CkptBoard> {
        &self.ckpt_board
    }

    /// The boot-recovery board (M2-S15): cells publish their slot, the
    /// plane gates `-LOADING` on `all_ready`, `INFO` aggregates it.
    #[must_use]
    pub fn recovery_board(&self) -> &Arc<RecoveryBoard> {
        &self.recovery
    }

    /// The memory board (M3-S25): cells publish their slot from MAINTAIN
    /// and at their own `INFO` render; the memory section folds it.
    #[must_use]
    pub fn memory_board(&self) -> &Arc<MemoryBoard> {
        &self.memory_board
    }
}

/// Reads the catalog at boot (`None` = fresh node). Corruption is a typed
/// error — the node must refuse to start, never guess (§8.4).
pub fn load_catalog(data_dir: &Path) -> std::io::Result<Option<NsCatalog>> {
    load_catalog_from(&StdSegmentFs, data_dir)
}

/// [`load_catalog`] over an injected filesystem tier (M2-S19: the sim
/// boots its catalog from the `SimDisk` surviving image).
///
/// # Errors
/// Envelope or schema corruption — fail-stop, never guess (§8.4).
pub fn load_catalog_from<F: inf_log::fs::SegmentFs>(
    fs: &F,
    data_dir: &Path,
) -> std::io::Result<Option<NsCatalog>> {
    let Some(payload) = read_meta(fs, data_dir)? else {
        return Ok(None);
    };
    NsCatalog::decode(&payload)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// The receiving end of a detached control plane (M2-S19, ADR-0021 D2):
/// the sim cannot spawn the control thread (single-thread determinism),
/// so [`ControlHandle::detached`] hands the message queue back and the
/// harness drains it inline once per scheduler step — catalog META swaps
/// over the *injected* fs and delegated unlinks, exactly the thread's
/// message loop. DDL, checkpoint epochs, and truncation therefore all
/// run inside the deterministic loop.
pub struct ControlInbox {
    rx: mpsc::Receiver<ControlMsg>,
    handle: Arc<ControlHandle>,
}

impl std::fmt::Debug for ControlInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlInbox").finish_non_exhaustive()
    }
}

impl ControlInbox {
    /// Drains queued control work against `fs`. A failed catalog swap is
    /// an error (the control thread's fail-stop analog — the caller owns
    /// the verdict); failed unlinks are ignored (boot GC re-collects).
    ///
    /// # Errors
    /// The catalog META swap failed — a DDL was acked against it (§8.4).
    pub fn drain<F: inf_log::fs::SegmentFs>(&self, fs: &F, data_dir: &Path) -> std::io::Result<()> {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                ControlMsg::Persist(req) => {
                    let mut catalog = req.catalog;
                    catalog.next_id = catalog.next_id.max(self.handle.next_ns_id());
                    write_meta(fs, data_dir, &catalog.encode())?;
                    self.handle.persisted_epoch.store(req.epoch, Ordering::Release);
                }
                ControlMsg::Unlink(path) => {
                    let _ = fs.remove_file(&path);
                }
            }
        }
        Ok(())
    }
}

impl ControlHandle {
    /// A control plane without the thread (M2-S19): the caller drains the
    /// returned [`ControlInbox`] inline. Production nodes use
    /// [`spawn`]; the sim's determinism forbids the thread.
    #[must_use]
    pub fn detached(cells: u16, start_unix_ms: u64) -> (Arc<ControlHandle>, ControlInbox) {
        let (handle, rx) = ControlHandle::new_parts(FIRST_NAMED_NS_ID, cells, start_unix_ms);
        let inbox = ControlInbox { rx, handle: Arc::clone(&handle) };
        (handle, inbox)
    }

    /// A detached control plane seeded from a recovered catalog (the
    /// reboot path: ids never regress across restart — ADR-0015 D2).
    #[must_use]
    pub fn detached_with_catalog(
        seed: Option<&NsCatalog>,
        cells: u16,
        start_unix_ms: u64,
    ) -> (Arc<ControlHandle>, ControlInbox) {
        let next_id = seed.map_or(FIRST_NAMED_NS_ID, |c| c.next_id.max(FIRST_NAMED_NS_ID));
        let (handle, rx) = ControlHandle::new_parts(next_id, cells, start_unix_ms);
        let inbox = ControlInbox { rx, handle: Arc::clone(&handle) };
        (handle, inbox)
    }

    fn new_parts(
        next_id: u32,
        cells: u16,
        start_unix_ms: u64,
    ) -> (Arc<ControlHandle>, mpsc::Receiver<ControlMsg>) {
        let (tx, rx) = mpsc::sync_channel::<ControlMsg>(256);
        let handle = Arc::new(ControlHandle {
            tx,
            next_ns_id: AtomicU32::new(next_id),
            next_epoch: AtomicU64::new(0),
            persisted_epoch: Arc::new(AtomicU64::new(0)),
            ckpt_epoch: AtomicU64::new(0),
            ckpt_board: Arc::new(CkptBoard {
                cells: (0..cells).map(|_| CkptSlot::default()).collect(),
            }),
            recovery: Arc::new(RecoveryBoard::new(cells, start_unix_ms)),
            memory_board: Arc::new(MemoryBoard::new(cells)),
        });
        (handle, rx)
    }
}

/// Spawns the control thread. `seed` is the boot-loaded catalog (its
/// `next_id` seeds the allocator; a fresh node starts at the named floor).
///
/// The thread fail-stops the process on a failed swap: the catalog is
/// durability metadata and a lost DDL after its `+OK` would be a §8.2
/// violation — same rule class as fsync failure.
pub fn spawn(
    data_dir: PathBuf,
    seed: Option<&NsCatalog>,
    cells: u16,
    start_unix_ms: u64,
) -> Arc<ControlHandle> {
    let next_id = seed.map_or(FIRST_NAMED_NS_ID, |c| c.next_id.max(FIRST_NAMED_NS_ID));
    let (handle, rx) = ControlHandle::new_parts(next_id, cells, start_unix_ms);
    let allocator = Arc::clone(&handle);
    let persisted = Arc::clone(&handle.persisted_epoch);
    let board = Arc::clone(&handle.recovery);
    std::thread::Builder::new()
        .name("inf-control".into())
        .spawn(move || {
            // This thread is detached (the JoinHandle drops below): a
            // swallowed unwind would leave cells serving with catalog
            // persistence, checkpoint epochs, and delegated unlinks
            // silently dead — the ADR-0026 D2 class. Same boundary as
            // the cell threads (M2.5-S01/S16): panic ⇒ loud process exit.
            let body = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                control_main(&data_dir, &allocator, &persisted, &board, &rx, cells);
            }));
            if body.is_err() {
                // The default hook already printed the panic.
                eprintln!("infinityd: control thread panicked — fail-stop");
                std::process::exit(101);
            }
        })
        .expect("spawn control thread");
    handle
}

/// The control thread's message loop + boot narration (M2-S15/M2.5-S01),
/// extracted so [`spawn`] can wrap it in the panic fail-stop boundary.
fn control_main(
    data_dir: &Path,
    allocator: &Arc<ControlHandle>,
    persisted: &Arc<AtomicU64>,
    board: &Arc<RecoveryBoard>,
    rx: &mpsc::Receiver<ControlMsg>,
    cells: u16,
) {
    {
        let handle_msg = |msg: ControlMsg| match msg {
            ControlMsg::Persist(req) => {
                // The persisted next_id must always cover the
                // allocator so ids never regress across restart,
                // even for namespaces whose DDL raced this
                // snapshot.
                let mut catalog = req.catalog;
                catalog.next_id = catalog.next_id.max(allocator.next_ns_id());
                let payload = catalog.encode();
                if let Err(err) = write_meta(&StdSegmentFs, data_dir, &payload) {
                    // §8.4 fail-stop: a DDL was acked against this
                    // swap. `process::exit`, never `panic!` — this
                    // thread is detached (the JoinHandle is dropped),
                    // so an unwind would kill only the control plane
                    // and leave cells serving with catalog
                    // persistence, checkpoint epochs, and delegated
                    // unlinks silently dead (the ADR-0026 D2
                    // swallowed-death class; M2.5-S16 audit fix).
                    eprintln!("FATAL: catalog META swap failed (fail-stop, §8.4): {err}");
                    std::process::exit(crate::EXIT_DURABLE_FAILSTOP);
                }
                persisted.store(req.epoch, Ordering::Release);
            }
            ControlMsg::Unlink(path) => {
                // Never fatal: the file is outside every recovery
                // unit; a survivor is re-collected at boot.
                if let Err(err) = std::fs::remove_file(&path)
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    eprintln!("control: unlink {} failed: {err}", path.display());
                }
            }
        };
        // Boot narration (M2-S15): until every cell reports ready,
        // poll the channel with a timeout and print per-cell ready
        // lines + a periodic aggregate progress line (segments/bytes/
        // ETA). Log-line clocks are ambient wall time — control-plane
        // narration only, never oracle input (those ride the
        // injected clocks).
        let mut announced = vec![false; usize::from(cells)];
        let boot_started = std::time::Instant::now();
        let mut next_progress = boot_started + Duration::from_secs(1);
        while !board.all_ready() {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(msg) => handle_msg(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            for (cell, seen) in announced.iter_mut().enumerate() {
                let slot = board.slot(cell as u16);
                if !*seen && slot.ready() {
                    *seen = true;
                    let (_, total) = slot.bytes();
                    let (segs, _) = slot.segments();
                    let torn = slot.torn_truncated_at().map_or(String::new(), |lsn| {
                        format!(", torn tail truncated at {lsn} (M2-S14)")
                    });
                    eprintln!(
                        "control: cell {cell} recovered ({segs} segments, {total} bytes, {} records{torn})",
                        slot.records()
                    );
                }
            }
            let now = std::time::Instant::now();
            if now >= next_progress && !board.all_ready() {
                next_progress = now + Duration::from_secs(1);
                let (done, total) = board.bytes();
                let elapsed = boot_started.elapsed().as_secs_f64();
                let eta = if done > 0 {
                    (elapsed * (total.saturating_sub(done)) as f64 / done as f64).ceil()
                } else {
                    0.0
                };
                eprintln!(
                    "control: recovery {}/{} cells ready, {done}/{total} bytes ({:.1}%), eta {eta:.0}s",
                    board.ready_cells(),
                    board.cell_count(),
                    if total > 0 { done as f64 * 100.0 / total as f64 } else { 100.0 },
                );
                // Stuck-cell narration (M2.5-S01): after 5 s, name each
                // not-ready cell and the phase it published before its
                // current step — the ADR-0022 D7 wedge signature was a
                // silent cell; a stalled boot must say where it stalled.
                if elapsed >= 5.0 {
                    for cell in 0..cells {
                        let slot = board.slot(cell);
                        if !slot.ready() {
                            let (done, total) = slot.bytes();
                            eprintln!(
                                "control: cell {cell} not ready — in {} ({done}/{total} bytes) for {elapsed:.0}s",
                                CellRecoverySlot::phase_name(slot.phase()),
                            );
                        }
                    }
                }
            }
        }
        eprintln!(
            "control: recovery complete — {} cells serving ({} ms)",
            board.cell_count(),
            boot_started.elapsed().as_millis()
        );
        while let Ok(msg) = rx.recv() {
            handle_msg(msg);
        }
        // Channel closed = node shutdown; nothing to flush (every
        // acked DDL already persisted before its reply).
    }
}
