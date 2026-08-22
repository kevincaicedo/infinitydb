//! The M2-S18 simulated disk (ADR-0020 D6, master plan §17.1): a
//! [`SegmentFs`] whose un-fsynced state **loses, tears, and reorders** on
//! a power cut — fsync is the only barrier honored, conservatively
//! modeling what real filesystems permit (POSIX; ext4/XFS journaling
//! semantics as studied in Pillai et al., OSDI '14).
//!
//! Two layers per file:
//! - the **OS view** — what reads, `list_dir`, `file_size` see: durable
//!   state plus every pending effect applied in issue order (the page
//!   cache outlives nothing);
//! - the **durable image** — what survives a power cut.
//!
//! The model:
//! - **Data** is volatile until `sync_data` on that file (fdatasync
//!   class — persists the file's data *and* length). Files are inodes:
//!   an open handle follows its inode across renames, and syncing
//!   through a handle syncs the inode (POSIX fd semantics).
//! - **Metadata** (create / rename / remove) is a per-directory pending
//!   op queue, volatile until `sync_dir` on the parent. A power cut
//!   keeps a seeded **prefix** of each directory's queue — journal
//!   commits are atomic and ordered within a directory — while
//!   directories are mutually independent (POSIX promises no cross-file
//!   ordering without fsync). A file whose create did not survive
//!   vanishes with all its data, fdatasync'd or not (the classic
//!   fsync-the-directory lesson); a remove that did not survive
//!   resurrects the file (ADR-0017's boot-GC re-collection).
//! - `create_segment`'s prealloc extent is durable-if-the-name-survives
//!   (`StdSegmentFs` syncs the file at create); `create_meta` carries no
//!   durability side effects (ADR-0017).
//! - **Power cut** ([`SimDisk::power_cut`]): per surviving inode, each
//!   pending write is split into **sectors** (`sector_bytes` grid,
//!   absolute file offsets); each sector independently survives a
//!   seeded coin, and surviving pieces apply in a seeded **permutation**
//!   of the writes (reorder — an *older* version of a range may survive
//!   a newer one: writeback may have flushed the old bytes before the
//!   overwrite, and the cut lands between). Unwritten gaps read as
//!   zeros — exactly the `AllZero`/`Garbage` residue the M2-S14
//!   taxonomy classifies. All draws come from one seeded `SplitMix64`
//!   over canonical (`BTreeMap`) iteration: same seed over the same op
//!   sequence ⇒ byte-identical surviving image (L7 — the S18 CI
//!   assert).
//! - **Cut scheduling**: [`SimDisk::cut_after_ops`] arms a dead switch —
//!   after `n` further mutating ops every operation fails with a named
//!   error until `power_cut` materializes the surviving image and
//!   revives the disk (the `MemFs::fail_after_ops` shape). Chosen-LSN /
//!   boundary schedules compose from this plus the named fault points
//!   (ADR-0019: the S19 sweep's currency).
//!
//! Recorded simplifications (ADR-0020): directories themselves are
//! durable at creation (file *entries* are the modeled thing); the
//! per-sector survival coin is fixed at 1/2 (sweep diversity comes from
//! seeds); no capacity/ENOSPC modeling (`MemFs` owns that discipline);
//! renames are same-directory only (every caller's protocol is —
//! cross-directory renames are rejected loudly).
//!
//! Driver tier: [`SimDisk`] hands out fake fds (`raw_fd`, high base) so
//! the `inf-sim` driver executes `LogWrite`/`Fdatasync` against the same
//! disk ([`SimDisk::driver_write_at`] / [`SimDisk::driver_fdatasync`] —
//! ADR-0020 D7). A `SimDisk` must only ever pair with the sim driver: a
//! real driver on a fake fd fails loudly with `EBADF`, never corrupts.
//!
//! **Write-through** (M4.5-S34, ADR-0086 D8): [`SimDisk::driver_write_
//! through`] models a FUA-class write — the bytes reach the durable image
//! at completion, earlier pending writes overlapping the range are
//! superseded (a device cannot resurrect a cached write over a later
//! FUA-acknowledged one to the same sectors), and later plain writes to
//! the range are ordinary pending writes again. `Direct`-mode segments are
//! created **empty** with a preallocation target so the rotor's driver
//! zero-fill, its barrier, and the lost-barrier reopen all run here.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};

use super::{SegmentFile, SegmentFs, SegmentIoMode};

/// Fake-fd base for inode handles (dir handles sit above it). High
/// enough that no simulated socket fd space collides.
const FILE_FD_BASE: i64 = 0x4000_0000;
const DIR_FD_BASE: i64 = 0x6000_0000;

/// Sector granularity of power-cut tears.
pub const DEFAULT_SECTOR_BYTES: u32 = 512;

#[derive(Copy, Clone, Debug)]
pub struct SimDiskConfig {
    /// Tear granularity: pending writes survive per-sector on this grid.
    pub sector_bytes: u32,
}

impl Default for SimDiskConfig {
    fn default() -> Self {
        SimDiskConfig { sector_bytes: DEFAULT_SECTOR_BYTES }
    }
}

/// Device service-time model (M2.5-S14): base heavy-tailed fsync latency
/// plus seeded stall episodes, drawn from one `SplitMix64` over the
/// deterministic per-fsync call order. Closes the model gap the S22
/// campaign exposed — sim fsyncs completed in zero virtual time, so the
/// group-commit batching pathology only expressed on a real 5 ms-fsync
/// device (commit.rs, ADR-0022 D3). A disk without a model armed is the
/// legacy instant device: every existing trace stays byte-identical.
#[derive(Clone, Debug)]
pub struct StallConfig {
    /// Floor service time every fsync pays (ns). ~50–200 µs models a
    /// warm NVMe fdatasync; 0 recovers the instant device.
    pub base_ns: u64,
    /// Heavy tail: with probability `tail_permille/1000` an fsync pays
    /// an extra `0..base_ns * tail_mult` ns (a second seeded draw).
    /// Models GC pauses / write-cache flush spikes.
    pub tail_permille: u32,
    pub tail_mult: u64,
    /// Stall episodes: roughly every `episode_gap_ns` (± gap/4 seeded
    /// jitter) the device wedges for `episode_ms_min..=episode_ms_max`;
    /// every fsync whose service window straddles the episode inherits
    /// the remaining stall (the device drains its queue in order).
    pub episode_gap_ns: u64,
    pub episode_ms_min: u64,
    pub episode_ms_max: u64,
    /// Write-through (FUA-class) service time (ns, M4.5-S34 / ADR-0086
    /// D8): a per-write barrier that does **not** queue on the serial
    /// flush timeline — the modeled difference between the classes. Same
    /// heavy tail as fsyncs (`tail_permille`/`tail_mult` over this base).
    /// 0 completes inline (every pre-S34 trace stays byte-identical).
    pub through_base_ns: u64,
    /// Plain (barrier-less) write service time (ns, M4.5-S35 / ADR-0087
    /// D7): the page-cache copy, or under `O_DIRECT` the device write
    /// without a barrier. Drawn independently per write, never on the
    /// serial flush timeline — so with K frames in flight plain writes
    /// complete in any order and a write can land **after** an fdatasync
    /// issued later, the shape the linked-sync drain rule must survive.
    /// Same heavy tail and stall-episode inheritance as the other
    /// classes. 0 completes inline (every pre-S35 trace byte-identical).
    pub write_base_ns: u64,
    /// Device bandwidth (M4.5-S36, ADR-0088 D8): every write — plain,
    /// write-through, zero-fill, checkpoint, tier — and every `TierRead`
    /// takes `len × 1e9 / rate` on a **shared per-disk byte timeline**
    /// (FIFO) in addition to its own off-timeline base draw; completion
    /// = max(base due, timeline finish). Large background writes now
    /// delay the foreground frame behind them — the contention the
    /// device budget exists for. 0 = unlimited (every existing trace
    /// byte-identical; reads stay inline).
    pub write_bytes_per_s: u64,
    pub read_bytes_per_s: u64,
}

impl Default for StallConfig {
    fn default() -> Self {
        // The instant device — byte-identical to the pre-S14 sim.
        // Durable scenarios opt in with concrete numbers.
        StallConfig {
            base_ns: 0,
            tail_permille: 0,
            tail_mult: 1,
            episode_gap_ns: u64::MAX,
            episode_ms_min: 0,
            episode_ms_max: 0,
            through_base_ns: 0,
            write_base_ns: 0,
            write_bytes_per_s: 0,
            read_bytes_per_s: 0,
        }
    }
}

/// The armed model: one serial completion timeline (`device_free_at`)
/// keeps fsyncs FIFO — a stalled head delays everything queued behind it.
#[derive(Debug)]
struct StallModel {
    cfg: StallConfig,
    rng: SplitMix64,
    /// Serial device timeline: the next fsync starts no earlier than this.
    device_free_at: u64,
    /// The byte timeline (ADR-0088 D8): the next transfer starts no
    /// earlier than this; shared by every write and read class.
    bandwidth_free_at: u64,
    /// Next scheduled stall episode starts here...
    next_episode_at: u64,
    /// ...and lasts this long (memoized with the start, one draw each).
    episode_dur_ns: u64,
}

impl StallModel {
    fn new(cfg: StallConfig, seed: u64) -> StallModel {
        let mut model = StallModel {
            cfg,
            rng: SplitMix64::new(seed),
            device_free_at: 0,
            bandwidth_free_at: 0,
            next_episode_at: 0,
            episode_dur_ns: 0,
        };
        model.arm_next_episode();
        model
    }

    /// One fsync's absolute completion time (virtual ns). Pure function
    /// of the seeded stream, the call order, and the injected `now_ns` —
    /// no ambient time or entropy (L7).
    fn schedule(&mut self, now_ns: u64) -> u64 {
        let start = now_ns.max(self.device_free_at);
        let mut service_ns = self.cfg.base_ns;
        if self.cfg.tail_permille > 0
            && self.rng.next_below(1_000) < u64::from(self.cfg.tail_permille)
        {
            service_ns += self.rng.next_below(self.cfg.base_ns.max(1) * self.cfg.tail_mult);
        }
        // Advance fully-elapsed episodes into the present (the schedule
        // exists whether or not anyone fsynced through it), then let a
        // straddling fsync inherit the episode's remainder.
        while self.next_episode_at.saturating_add(self.episode_dur_ns) <= start {
            self.arm_next_episode();
        }
        if start.saturating_add(service_ns) >= self.next_episode_at {
            let episode_end = self.next_episode_at.saturating_add(self.episode_dur_ns);
            service_ns = service_ns.max(episode_end.saturating_sub(start));
            self.arm_next_episode();
        }
        let finish = start + service_ns;
        debug_assert!(finish >= self.device_free_at, "device timeline is monotone (FIFO)");
        self.device_free_at = finish;
        finish
    }

    /// One write-through barrier's absolute completion time (virtual ns,
    /// ADR-0086 D8). Drawn on the same seeded stream in call order; never
    /// advances `device_free_at` — a FUA write persists itself without
    /// the flush unit. A stall episode still wedges it (the whole device
    /// is wedged), which is what keeps the model honest about tails.
    fn schedule_through(&mut self, now_ns: u64, len: u64) -> u64 {
        let due = self.schedule_off_timeline(now_ns, self.cfg.through_base_ns);
        due.max(self.schedule_transfer(now_ns, len, self.cfg.write_bytes_per_s))
    }

    /// One plain write's absolute completion time (ADR-0087 D7): the same
    /// off-timeline draw as write-through over `write_base_ns`, then the
    /// byte timeline (ADR-0088 D8).
    fn schedule_write(&mut self, now_ns: u64, len: u64) -> u64 {
        let due = self.schedule_off_timeline(now_ns, self.cfg.write_base_ns);
        due.max(self.schedule_transfer(now_ns, len, self.cfg.write_bytes_per_s))
    }

    /// One tier read's absolute completion time (ADR-0088 D8): the byte
    /// timeline alone (reads have no base draw — with the rate at 0 the
    /// driver keeps completing them inline).
    fn schedule_read(&mut self, now_ns: u64, len: u64) -> u64 {
        self.schedule_transfer(now_ns, len, self.cfg.read_bytes_per_s)
    }

    /// The shared byte timeline: `len × 1e9 / rate` of transfer, FIFO
    /// behind whatever is already queued. `now_ns` when the rate is 0.
    fn schedule_transfer(&mut self, now_ns: u64, len: u64, rate: u64) -> u64 {
        if rate == 0 {
            return now_ns;
        }
        let transfer_ns =
            u64::try_from(u128::from(len) * 1_000_000_000 / u128::from(rate)).unwrap_or(u64::MAX);
        let start = now_ns.max(self.bandwidth_free_at);
        let finish = start.saturating_add(transfer_ns);
        self.bandwidth_free_at = finish;
        finish
    }

    /// A per-op service time that does not queue on the serial flush
    /// timeline: base + the seeded heavy tail, inheriting a straddled
    /// stall episode's remainder (the device wedges for every class).
    fn schedule_off_timeline(&mut self, now_ns: u64, base_ns: u64) -> u64 {
        let mut service_ns = base_ns;
        if self.cfg.tail_permille > 0
            && self.rng.next_below(1_000) < u64::from(self.cfg.tail_permille)
        {
            service_ns += self.rng.next_below(base_ns.max(1) * self.cfg.tail_mult);
        }
        while self.next_episode_at.saturating_add(self.episode_dur_ns) <= now_ns {
            self.arm_next_episode();
        }
        if now_ns.saturating_add(service_ns) >= self.next_episode_at {
            let episode_end = self.next_episode_at.saturating_add(self.episode_dur_ns);
            service_ns = service_ns.max(episode_end.saturating_sub(now_ns));
        }
        now_ns + service_ns
    }

    fn arm_next_episode(&mut self) {
        let gap = self.cfg.episode_gap_ns;
        // gap ± gap/4: `gap - gap/4 + uniform(0 .. gap/2)`.
        let jitter = if gap >= 4 { self.rng.next_below(gap / 2) } else { 0 };
        self.next_episode_at =
            self.next_episode_at.saturating_add(gap - gap / 4).saturating_add(jitter);
        let span_ms = self.cfg.episode_ms_max.saturating_sub(self.cfg.episode_ms_min);
        let dur_ms = self.cfg.episode_ms_min
            + if span_ms > 0 { self.rng.next_below(span_ms + 1) } else { 0 };
        self.episode_dur_ns = dur_ms.saturating_mul(1_000_000);
    }
}

#[derive(Debug)]
struct PendingWrite {
    offset: u64,
    data: Vec<u8>,
}

#[derive(Debug, Default)]
struct Inode {
    /// Survives any power cut (fsync-covered bytes + length).
    durable: Vec<u8>,
    /// The OS view: durable + pending applied in issue order.
    os: Vec<u8>,
    /// Un-fsynced writes, issue order.
    pending: Vec<PendingWrite>,
    /// Preallocation target of a `Direct`-mode segment (ADR-0086 D8):
    /// `fully_allocated` ⇔ the OS length reached it. 0 for every other
    /// file (always fully allocated — no sparse concept).
    prealloc_target: u64,
    /// Opened `O_DIRECT` (segments and v3 checkpoints): every driver
    /// write must be [`crate::ckpt::ICK_BLOCK_ALIGN`]-aligned in offset
    /// and length, asserted here so the simulator catches what tmpfs
    /// swallows (ADR-0088 D3).
    direct: bool,
}

impl Inode {
    fn write(&mut self, offset: u64, data: &[u8]) {
        let offset_usize = usize::try_from(offset).expect("offset fits usize");
        let end = offset_usize + data.len();
        if end > self.os.len() {
            self.os.resize(end, 0);
        }
        self.os[offset_usize..end].copy_from_slice(data);
        self.pending.push(PendingWrite { offset, data: data.to_vec() });
    }

    /// Write-through (ADR-0086 D8): durable at completion, and any earlier
    /// pending write overlapping the range is trimmed — on media the
    /// FUA-acknowledged bytes are the newest content of those sectors and
    /// a superseded cached write cannot land over them later.
    fn write_through(&mut self, offset: u64, data: &[u8]) {
        let offset_usize = usize::try_from(offset).expect("offset fits usize");
        let end = offset_usize + data.len();
        if end > self.os.len() {
            self.os.resize(end, 0);
        }
        self.os[offset_usize..end].copy_from_slice(data);
        if end > self.durable.len() {
            self.durable.resize(end, 0);
        }
        self.durable[offset_usize..end].copy_from_slice(data);
        let range_end = offset + data.len() as u64;
        let earlier = std::mem::take(&mut self.pending);
        for write in earlier {
            let write_end = write.offset + write.data.len() as u64;
            if write_end <= offset || write.offset >= range_end {
                self.pending.push(write);
                continue;
            }
            // Keep the non-overlapping head and tail pieces (either may be
            // empty); the overlapped middle is superseded.
            if write.offset < offset {
                let keep = (offset - write.offset) as usize;
                self.pending
                    .push(PendingWrite { offset: write.offset, data: write.data[..keep].to_vec() });
            }
            if write_end > range_end {
                let from = (range_end - write.offset) as usize;
                self.pending
                    .push(PendingWrite { offset: range_end, data: write.data[from..].to_vec() });
            }
        }
    }

    fn sync(&mut self) {
        self.durable = self.os.clone();
        self.pending.clear();
    }

    /// Power cut: seeded permutation over pending writes, per-sector
    /// survival coin, surviving pieces applied over the durable image.
    fn cut(&mut self, rng: &mut SplitMix64, sector: u64) {
        let mut order: Vec<usize> = (0..self.pending.len()).collect();
        for i in (1..order.len()).rev() {
            let j = rng.next_below(i as u64 + 1) as usize;
            order.swap(i, j);
        }
        let mut image = std::mem::take(&mut self.durable);
        for idx in order {
            let write = &self.pending[idx];
            let mut at = write.offset;
            let end = write.offset + write.data.len() as u64;
            while at < end {
                let sector_end = ((at / sector) + 1) * sector;
                let piece_end = sector_end.min(end);
                if rng.next_below(2) == 1 {
                    let src_from = (at - write.offset) as usize;
                    let src_to = (piece_end - write.offset) as usize;
                    let dst_from = usize::try_from(at).expect("offset fits usize");
                    let dst_to = usize::try_from(piece_end).expect("offset fits usize");
                    if dst_to > image.len() {
                        image.resize(dst_to, 0);
                    }
                    image[dst_from..dst_to].copy_from_slice(&write.data[src_from..src_to]);
                }
                at = piece_end;
            }
        }
        self.durable = image;
        self.os = self.durable.clone();
        self.pending.clear();
    }
}

/// Per-directory pending metadata ops (issue order — the journal queue).
#[derive(Clone, Debug)]
enum MetaOp {
    Create { name: PathBuf, ino: u64 },
    Rename { from: PathBuf, to: PathBuf },
    Remove { name: PathBuf },
}

fn apply_meta(names: &mut BTreeMap<PathBuf, u64>, op: &MetaOp) {
    match op {
        MetaOp::Create { name, ino } => {
            names.insert(name.clone(), *ino);
        }
        MetaOp::Rename { from, to } => {
            if let Some(ino) = names.remove(from) {
                names.insert(to.clone(), ino);
            }
        }
        MetaOp::Remove { name } => {
            names.remove(name);
        }
    }
}

#[derive(Debug, Default)]
struct DiskState {
    cfg: SimDiskConfig,
    /// Simplification: directories are durable at creation.
    dirs: BTreeSet<PathBuf>,
    /// OS-view namespace: name → inode.
    os_names: BTreeMap<PathBuf, u64>,
    /// Dir-fsync-committed namespace (what a cut starts from).
    durable_names: BTreeMap<PathBuf, u64>,
    /// Per-directory pending metadata ops, issue order.
    pending_meta: BTreeMap<PathBuf, Vec<MetaOp>>,
    inodes: BTreeMap<u64, Inode>,
    next_ino: u64,
    /// Dir-handle fd → directory (driver dir-fsync routing).
    dir_fds: BTreeMap<i64, PathBuf>,
    next_dir_fd: i64,
    /// `Some(n)`: n more mutating ops succeed, then everything fails
    /// until [`SimDisk::power_cut`] (the dead switch).
    ops_until_cut: Option<u64>,
    /// `create_meta_direct` answers `Unsupported` — a filesystem without
    /// `O_DIRECT` (ADR-0088 D3 as amended): the checkpoint's probed
    /// buffered fallback under the reactor tier.
    refuse_direct_meta: bool,
    /// Cumulative **blocking** `SegmentFs::sync_dir` calls (M2.5-S01):
    /// the boot-storm oracle's observable — the ready path must issue
    /// zero of these (driver-ridden barrier syncs on dir handles do not
    /// count; they cannot block a reactor).
    sync_dir_calls: u64,
    /// Device service-time model (M2.5-S14). `None` = instant fsyncs,
    /// the pre-S14 behavior — every legacy trace stays byte-identical.
    stall: Option<StallModel>,
}

impl DiskState {
    fn tick_op(&mut self) -> io::Result<()> {
        match self.ops_until_cut {
            None => Ok(()),
            Some(0) => Err(io::Error::other("injected fault: power lost (sim disk dead)")),
            Some(ref mut n) => {
                *n -= 1;
                Ok(())
            }
        }
    }

    fn dead_check(&self) -> io::Result<()> {
        if self.ops_until_cut == Some(0) {
            return Err(io::Error::other("injected fault: power lost (sim disk dead)"));
        }
        Ok(())
    }

    fn ino_of(&self, path: &Path) -> io::Result<u64> {
        self.os_names.get(path).copied().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("no file {}", path.display()))
        })
    }
}

/// The simulated disk (shared handle — clones share state, like `MemFs`).
#[derive(Clone, Default)]
pub struct SimDisk {
    state: Rc<RefCell<DiskState>>,
}

impl std::fmt::Debug for SimDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.borrow();
        f.debug_struct("SimDisk")
            .field("files", &state.os_names.keys().collect::<Vec<_>>())
            .field("pending_dirs", &state.pending_meta.len())
            .finish()
    }
}

impl SimDisk {
    #[must_use]
    pub fn new() -> SimDisk {
        SimDisk::default()
    }

    #[must_use]
    pub fn with_config(cfg: SimDiskConfig) -> SimDisk {
        let disk = SimDisk::default();
        disk.state.borrow_mut().cfg = cfg;
        disk
    }

    /// Arms the device service-time model (M2.5-S14). `stall_seed` is
    /// derived from the scenario seed so the device timeline replays
    /// byte-identically (L7).
    #[must_use]
    pub fn with_stall(cfg: SimDiskConfig, stall: StallConfig, stall_seed: u64) -> SimDisk {
        let disk = SimDisk::with_config(cfg);
        disk.state.borrow_mut().stall = Some(StallModel::new(stall, stall_seed));
        disk
    }

    /// Draws one fsync's absolute completion time (virtual ns) from the
    /// service-time model. `None` when no model is armed — the caller
    /// completes inline (the legacy instant-device path).
    #[must_use]
    pub fn schedule_fsync(&self, now_ns: u64) -> Option<u64> {
        self.state.borrow_mut().stall.as_mut().map(|model| model.schedule(now_ns))
    }

    /// Draws one write-through barrier's absolute completion time
    /// (ADR-0086 D8). `None` when no model is armed **or** the model's
    /// `through_base_ns` is 0 — inline completion, byte-identical traces.
    #[must_use]
    pub fn schedule_write_through(&self, now_ns: u64, len: u64) -> Option<u64> {
        let mut state = self.state.borrow_mut();
        let model = state.stall.as_mut()?;
        if model.cfg.through_base_ns == 0 && model.cfg.write_bytes_per_s == 0 {
            return None;
        }
        Some(model.schedule_through(now_ns, len))
    }

    /// Draws one tier read's absolute completion time (ADR-0088 D8).
    /// `None` when no model is armed or `read_bytes_per_s` is 0 — inline
    /// completion, byte-identical traces.
    #[must_use]
    pub fn schedule_read(&self, now_ns: u64, len: u64) -> Option<u64> {
        let mut state = self.state.borrow_mut();
        let model = state.stall.as_mut()?;
        if model.cfg.read_bytes_per_s == 0 {
            return None;
        }
        Some(model.schedule_read(now_ns, len))
    }

    /// Draws one plain write's absolute completion time (ADR-0087 D7).
    /// `None` when no model is armed **or** `write_base_ns` is 0 — inline
    /// completion, byte-identical traces.
    #[must_use]
    pub fn schedule_write(&self, now_ns: u64, len: u64) -> Option<u64> {
        let mut state = self.state.borrow_mut();
        let model = state.stall.as_mut()?;
        if model.cfg.write_base_ns == 0 && model.cfg.write_bytes_per_s == 0 {
            return None;
        }
        Some(model.schedule_write(now_ns, len))
    }

    /// Model a filesystem without `O_DIRECT`: every `create_meta_direct`
    /// answers `Unsupported` from now on (ADR-0088 D3 as amended).
    pub fn refuse_direct_meta(&self) {
        self.state.borrow_mut().refuse_direct_meta = true;
    }

    /// Arms the dead switch: `n` more mutating ops succeed, then every
    /// operation fails with a named error until [`Self::power_cut`].
    pub fn cut_after_ops(&self, n: u64) {
        self.state.borrow_mut().ops_until_cut = Some(n);
    }

    /// Cumulative blocking `sync_dir` calls (M2.5-S01 boot-storm oracle):
    /// a recovery ready-path window must show a zero delta.
    #[must_use]
    pub fn sync_dir_calls(&self) -> u64 {
        self.state.borrow().sync_dir_calls
    }

    /// The power cut (ADR-0020 D6): per directory, a seeded prefix of
    /// its pending metadata ops survives; per surviving inode, pending
    /// writes lose/tear/reorder at sector granularity. Clears the dead
    /// switch — the disk then serves the surviving image (the reboot).
    pub fn power_cut(&self, seed: u64) {
        let mut state = self.state.borrow_mut();
        let sector = u64::from(state.cfg.sector_bytes.max(1));
        let mut rng = SplitMix64::new(seed);

        // 1. Metadata: seeded per-directory prefix (journal truncation).
        let pending = std::mem::take(&mut state.pending_meta);
        for (_dir, ops) in pending {
            let keep = rng.next_below(ops.len() as u64 + 1) as usize;
            for op in &ops[..keep] {
                apply_meta(&mut state.durable_names, op);
            }
        }

        // 2. Data: only inodes reachable from the surviving namespace;
        //    everything else vanishes with its create.
        let reachable: BTreeSet<u64> = state.durable_names.values().copied().collect();
        let inodes = std::mem::take(&mut state.inodes);
        state.inodes = inodes
            .into_iter()
            .filter(|(ino, _)| reachable.contains(ino))
            .map(|(ino, mut inode)| {
                inode.cut(&mut rng, sector);
                (ino, inode)
            })
            .collect();

        // 3. The OS view IS the durable image now.
        state.os_names = state.durable_names.clone();
        state.dir_fds.clear();
        state.ops_until_cut = None;

        // 4. The rebooted device starts idle, but the seeded episode
        //    stream continues — the stall RNG never resets across a cut
        //    (documented determinism: one seed, one device timeline).
        if let Some(stall) = &mut state.stall {
            stall.device_free_at = 0;
        }
    }

    /// Canonical surviving image: `(path, bytes)` in path order — the
    /// determinism-assert currency (byte-identical across same-seed runs).
    #[must_use]
    pub fn image(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let state = self.state.borrow();
        state
            .os_names
            .iter()
            .map(|(path, ino)| (path.clone(), state.inodes[ino].os.clone()))
            .collect()
    }

    /// Chained `hash64` over [`Self::image`].
    #[must_use]
    pub fn image_digest(&self) -> u64 {
        let mut acc = 0xD15C_0BAD_5EED_0001;
        for (path, bytes) in self.image() {
            acc = hash64(path.as_os_str().as_encoded_bytes(), acc);
            acc = hash64(&bytes, acc);
        }
        acc
    }

    /// OS-view contents of a file (test assertions).
    #[must_use]
    pub fn contents(&self, path: &Path) -> Option<Vec<u8>> {
        let state = self.state.borrow();
        let ino = state.os_names.get(path)?;
        Some(state.inodes[ino].os.clone())
    }

    /// Driver tier (ADR-0020 D7): execute a `LogWrite` payload against a
    /// fake file fd. Page-cache semantics — NOT durable.
    ///
    /// # Errors
    /// `EBADF`-class errors for unknown fds; the dead-switch error when
    /// the disk is dead.
    pub fn driver_write_at(&self, fd: i32, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        state.tick_op()?;
        let ino = file_fd_ino(fd)?;
        let inode = state
            .inodes
            .get_mut(&ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("bad sim fd {fd}")))?;
        assert_direct_aligned(inode, offset, data.len());
        inode.write(offset, data);
        Ok(())
    }

    /// Driver tier (M4.5-S34, ADR-0086 D8): execute a write-through
    /// `LogWrite` against a fake file fd — durable at completion, earlier
    /// overlapping pending writes superseded.
    ///
    /// # Errors
    /// As [`Self::driver_write_at`].
    pub fn driver_write_through(&self, fd: i32, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        state.tick_op()?;
        let ino = file_fd_ino(fd)?;
        let inode = state
            .inodes
            .get_mut(&ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("bad sim fd {fd}")))?;
        assert_direct_aligned(inode, offset, data.len());
        inode.write_through(offset, data);
        Ok(())
    }

    /// Driver tier (M4-S04): execute a `TierRead` against a fake file fd
    /// — reads the OS (page-cache) view, exactly what a live kernel
    /// serves. Returns the bytes copied; a read past EOF returns the
    /// available prefix (pread(2) semantics — the driver's op contract
    /// turns an incomplete fill into `EIO`).
    ///
    /// # Errors
    /// As [`Self::driver_write_at`].
    pub fn driver_read_at(&self, fd: i32, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self.state.borrow_mut();
        state.tick_op()?;
        let ino = file_fd_ino(fd)?;
        let inode = state
            .inodes
            .get(&ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("bad sim fd {fd}")))?;
        let offset = usize::try_from(offset).expect("offset fits usize");
        if offset >= inode.os.len() {
            return Ok(0);
        }
        let n = buf.len().min(inode.os.len() - offset);
        buf[..n].copy_from_slice(&inode.os[offset..offset + n]);
        Ok(n)
    }

    /// Driver tier: execute an `Fdatasync` against a fake fd — a file
    /// fd flushes the inode; a dir fd is the dir barrier.
    ///
    /// # Errors
    /// As [`Self::driver_write_at`].
    pub fn driver_fdatasync(&self, fd: i32) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        state.tick_op()?;
        if let Some(dir) = state.dir_fds.get(&i64::from(fd)).cloned() {
            let ops = state.pending_meta.remove(&dir).unwrap_or_default();
            for op in &ops {
                apply_meta(&mut state.durable_names, op);
            }
            return Ok(());
        }
        let ino = file_fd_ino(fd)?;
        let inode = state
            .inodes
            .get_mut(&ino)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("bad sim fd {fd}")))?;
        inode.sync();
        Ok(())
    }

    fn create_inode(&self, path: &Path, os: Vec<u8>, durable: Vec<u8>) -> io::Result<SimFile> {
        self.create_inode_full(path, os, durable, 0, false)
    }

    fn create_inode_targeted(
        &self,
        path: &Path,
        os: Vec<u8>,
        durable: Vec<u8>,
        prealloc_target: u64,
    ) -> io::Result<SimFile> {
        self.create_inode_full(path, os, durable, prealloc_target, true)
    }

    fn create_inode_full(
        &self,
        path: &Path,
        os: Vec<u8>,
        durable: Vec<u8>,
        prealloc_target: u64,
        direct: bool,
    ) -> io::Result<SimFile> {
        let mut state = self.state.borrow_mut();
        state.tick_op()?;
        let parent = parent_dir(path);
        if !state.dirs.contains(&parent) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no dir {}", parent.display()),
            ));
        }
        if state.os_names.contains_key(path) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists", path.display()),
            ));
        }
        state.next_ino += 1;
        let ino = state.next_ino;
        state
            .inodes
            .insert(ino, Inode { durable, os, pending: Vec::new(), prealloc_target, direct });
        state.os_names.insert(path.to_path_buf(), ino);
        state
            .pending_meta
            .entry(parent)
            .or_default()
            .push(MetaOp::Create { name: path.to_path_buf(), ino });
        Ok(SimFile { state: Rc::clone(&self.state), target: Target::Ino(ino) })
    }
}

/// A direct inode takes only aligned writes — the `O_DIRECT` contract
/// the kernel enforces with `EINVAL`, enforced here as an invariant so a
/// misaligned block is a sim failure, never a device-only one.
fn assert_direct_aligned(inode: &Inode, offset: u64, len: usize) {
    if inode.direct {
        let align = crate::ckpt::ICK_BLOCK_ALIGN as u64;
        assert_eq!(offset % align, 0, "direct write offset {offset} is not {align}-aligned");
        assert_eq!(len as u64 % align, 0, "direct write length {len} is not {align}-aligned");
    }
}

fn parent_dir(path: &Path) -> PathBuf {
    path.parent().unwrap_or_else(|| Path::new("")).to_path_buf()
}

fn file_fd_ino(fd: i32) -> io::Result<u64> {
    let fd = i64::from(fd);
    if (FILE_FD_BASE..DIR_FD_BASE).contains(&fd) {
        Ok((fd - FILE_FD_BASE) as u64)
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a sim file fd: {fd}")))
    }
}

/// One open handle: follows its inode across renames (POSIX fd
/// semantics); dir handles map `sync_data` to the directory barrier.
#[derive(Debug)]
pub struct SimFile {
    state: Rc<RefCell<DiskState>>,
    target: Target,
}

#[derive(Debug)]
enum Target {
    Ino(u64),
    Dir(PathBuf, i64),
}

impl SegmentFile for SimFile {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        state.tick_op()?;
        match &self.target {
            Target::Ino(ino) => {
                let inode = state.inodes.get_mut(ino).expect("open handle pins its inode");
                inode.write(offset, data);
                Ok(())
            }
            Target::Dir(dir, _) => {
                Err(io::Error::other(format!("write on dir handle {}", dir.display())))
            }
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let state = self.state.borrow();
        state.dead_check()?;
        match &self.target {
            Target::Ino(ino) => {
                let bytes = &state.inodes[ino].os;
                let offset = usize::try_from(offset).expect("offset fits usize");
                if offset >= bytes.len() {
                    return Ok(0);
                }
                let n = buf.len().min(bytes.len() - offset);
                buf[..n].copy_from_slice(&bytes[offset..offset + n]);
                Ok(n)
            }
            Target::Dir(..) => Ok(0),
        }
    }

    fn file_size(&self) -> io::Result<u64> {
        let state = self.state.borrow();
        state.dead_check()?;
        match &self.target {
            Target::Ino(ino) => Ok(state.inodes[ino].os.len() as u64),
            Target::Dir(..) => Ok(0),
        }
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        state.tick_op()?;
        match &self.target {
            Target::Ino(ino) => {
                let inode = state.inodes.get_mut(ino).expect("open handle pins its inode");
                let len = usize::try_from(len).expect("length fits usize");
                // OS view honors the new length immediately; the durable
                // image keeps the old tail until the next sync, and
                // retained pending writes may resurrect bytes beyond the
                // cut at power-cut time — real ftruncate physics, which
                // is why ADR-0056 D5 syncs before any new flush.
                inode.os.resize(len, 0);
                Ok(())
            }
            Target::Dir(dir, _) => {
                Err(io::Error::other(format!("truncate on dir handle {}", dir.display())))
            }
        }
    }

    fn sync_data(&mut self) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        state.tick_op()?;
        match &self.target {
            Target::Ino(ino) => {
                state.inodes.get_mut(ino).expect("open handle pins its inode").sync();
                Ok(())
            }
            Target::Dir(dir, _) => {
                let ops = state.pending_meta.remove(dir).unwrap_or_default();
                for op in &ops {
                    apply_meta(&mut state.durable_names, op);
                }
                Ok(())
            }
        }
    }

    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        let fd = match &self.target {
            Target::Ino(ino) => FILE_FD_BASE + *ino as i64,
            Target::Dir(_, fd) => *fd,
        };
        Some(i32::try_from(fd).expect("sim fd fits i32"))
    }

    fn fully_allocated(&self) -> io::Result<bool> {
        let state = self.state.borrow();
        state.dead_check()?;
        match &self.target {
            Target::Ino(ino) => {
                let inode = &state.inodes[ino];
                Ok(inode.os.len() as u64 >= inode.prealloc_target)
            }
            Target::Dir(..) => Ok(true),
        }
    }
}

impl SegmentFs for SimDisk {
    type File = SimFile;

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        state.tick_op()?;
        let mut current = PathBuf::new();
        for part in dir.components() {
            current.push(part);
            state.dirs.insert(current.clone());
        }
        Ok(())
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        state.sync_dir_calls += 1;
        state.tick_op()?;
        if !state.dirs.contains(dir) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no dir {}", dir.display()),
            ));
        }
        let ops = state.pending_meta.remove(dir).unwrap_or_default();
        for op in &ops {
            apply_meta(&mut state.durable_names, op);
        }
        Ok(())
    }

    fn list_dir(&self, dir: &Path) -> io::Result<Vec<String>> {
        let state = self.state.borrow();
        state.dead_check()?;
        if !state.dirs.contains(dir) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no dir {}", dir.display()),
            ));
        }
        Ok(state
            .os_names
            .keys()
            .filter(|path| path.parent() == Some(dir))
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect())
    }

    fn create_segment(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File> {
        // StdSegmentFs syncs the file at create: the prealloc extent is
        // durable-if-the-name-survives; the name still needs the dir
        // barrier.
        let len = usize::try_from(prealloc_bytes).expect("prealloc fits usize");
        self.create_inode(path, vec![0; len], vec![0; len])
    }

    fn create_segment_direct(&self, path: &Path, prealloc_bytes: u64) -> io::Result<Self::File> {
        // Empty, volatile, with a target (ADR-0086 D8): the rotor's driver
        // zero-fill grows it and its barrier makes the length durable —
        // exactly the real sparse-then-written shape, so the not-ready
        // rotation and the lost-barrier reopen are reachable here.
        self.create_inode_targeted(path, Vec::new(), Vec::new(), prealloc_bytes)
    }

    fn create_meta(&self, path: &Path) -> io::Result<Self::File> {
        // No durability side effects (ADR-0017): all volatile.
        self.create_inode(path, Vec::new(), Vec::new())
    }

    fn create_meta_direct(&self, path: &Path) -> io::Result<Self::File> {
        if self.state.borrow().refuse_direct_meta {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SimDisk: O_DIRECT refused (refuse_direct_meta)",
            ));
        }
        // Volatile like `create_meta`, flagged direct: every driver write
        // to it is alignment-asserted (ADR-0088 D3).
        self.create_inode_full(path, Vec::new(), Vec::new(), 0, true)
    }

    fn open_dir(&self, dir: &Path) -> io::Result<Self::File> {
        let mut state = self.state.borrow_mut();
        state.dead_check()?;
        if !state.dirs.contains(dir) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no dir {}", dir.display()),
            ));
        }
        state.next_dir_fd += 1;
        let fd = DIR_FD_BASE + state.next_dir_fd;
        state.dir_fds.insert(fd, dir.to_path_buf());
        Ok(SimFile { state: Rc::clone(&self.state), target: Target::Dir(dir.to_path_buf(), fd) })
    }

    fn open_write(&self, path: &Path) -> io::Result<Self::File> {
        let state = self.state.borrow();
        state.dead_check()?;
        let ino = state.ino_of(path)?;
        Ok(SimFile { state: Rc::clone(&self.state), target: Target::Ino(ino) })
    }

    fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        self.open_write(path)
    }

    /// The reopened tail takes the mode of *this* open (ADR-0086 D4 as
    /// amended): `O_DIRECT` is a property of the open file description,
    /// not the inode, so a segment created direct and reopened `Buffered`
    /// (the FUA → FLUSH transition, packed frames at the v3 tail's
    /// aligned end) takes packed writes, and a buffered segment reopened
    /// `Direct` asserts alignment from here on.
    fn open_segment_append(&self, path: &Path, mode: SegmentIoMode) -> io::Result<Self::File> {
        let file = self.open_write(path)?;
        let mut state = self.state.borrow_mut();
        let ino = state.ino_of(path)?;
        let inode = state.inodes.get_mut(&ino).expect("ino_of resolved the inode");
        inode.direct = mode == SegmentIoMode::Direct;
        Ok(file)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        state.tick_op()?;
        let parent = parent_dir(to);
        if parent_dir(from) != parent {
            // Same-directory only: every caller's swap protocol is, and a
            // cross-directory rename would need two-journal ordering the
            // model deliberately does not define.
            return Err(io::Error::other(format!(
                "sim disk models same-directory renames only ({} -> {})",
                from.display(),
                to.display()
            )));
        }
        if !state.dirs.contains(&parent) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no dir {}", parent.display()),
            ));
        }
        let ino = state.os_names.remove(from).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("no file {}", from.display()))
        })?;
        // Replaces an existing destination atomically, like POSIX rename.
        state.os_names.insert(to.to_path_buf(), ino);
        state
            .pending_meta
            .entry(parent)
            .or_default()
            .push(MetaOp::Rename { from: from.to_path_buf(), to: to.to_path_buf() });
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        state.tick_op()?;
        if state.os_names.remove(path).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no file {}", path.display()),
            ));
        }
        let parent = parent_dir(path);
        state
            .pending_meta
            .entry(parent)
            .or_default()
            .push(MetaOp::Remove { name: path.to_path_buf() });
        Ok(())
    }
}

#[cfg(test)]
mod stall_tests {
    use super::*;

    fn armed_cfg() -> StallConfig {
        StallConfig {
            base_ns: 120_000,
            tail_permille: 30,
            tail_mult: 8,
            episode_gap_ns: 1_500_000_000,
            episode_ms_min: 50,
            episode_ms_max: 90,
            through_base_ns: 0,
            write_base_ns: 0,
            write_bytes_per_s: 0,
            read_bytes_per_s: 0,
        }
    }

    #[test]
    fn plain_writes_defer_only_when_armed_and_off_the_flush_timeline() {
        // ADR-0087 D7: `write_base_ns = 0` keeps plain writes inline
        // (every pre-S35 trace byte-identical); armed, each write draws
        // its own service time without advancing the serial flush
        // timeline, so a write issued after an fsync can be due before
        // or after it — the reorder the written prefix must survive.
        let off = SimDisk::with_stall(SimDiskConfig::default(), armed_cfg(), 7);
        assert_eq!(off.schedule_write(1_000, 4096), None);
        let mut cfg = armed_cfg();
        cfg.write_base_ns = 8_000;
        let on = SimDisk::with_stall(SimDiskConfig::default(), cfg, 7);
        let sync_due = on.schedule_fsync(1_000).expect("armed");
        let write_due = on.schedule_write(1_000, 4096).expect("armed");
        assert!(write_due >= 1_000 + 8_000, "base service time");
        assert!(write_due < sync_due, "a plain write does not queue behind the flush unit");
        let next_sync = on.schedule_fsync(1_000).expect("armed");
        assert!(next_sync >= sync_due, "the flush timeline stays serial");
    }

    /// ADR-0088 D8: with a byte rate armed, every write and read queues
    /// FIFO on one shared byte timeline by its length; with the rate at
    /// 0 nothing changes (reads stay inline, writes keep their draws).
    #[test]
    fn bandwidth_term_queues_every_class_on_one_byte_timeline() {
        let mut cfg = armed_cfg();
        cfg.write_bytes_per_s = 1_000_000_000; // 1 byte per ns
        cfg.read_bytes_per_s = 1_000_000_000;
        let disk = SimDisk::with_stall(SimDiskConfig::default(), cfg, 11);
        // A 1 MiB plain write takes ~1 ms of transfer (base is 0).
        let w1 = disk.schedule_write(1_000, 1 << 20).expect("rate armed");
        assert!(w1 >= 1_000 + (1 << 20), "transfer time at the rate");
        // A 4 KiB write-through issued at the same instant queues behind it.
        let w2 = disk.schedule_write_through(1_000, 4096).expect("rate armed");
        assert!(w2 >= w1 + 4096, "FIFO behind the in-flight transfer");
        // A read shares the same timeline.
        let r = disk.schedule_read(1_000, 8192).expect("read rate armed");
        assert!(r >= w2 + 8192);
        // Idle timeline: a later op starts at `now`.
        let later = disk.schedule_write(r + 1_000_000, 4096).expect("rate armed");
        assert_eq!(later, r + 1_000_000 + 4096);
        // The unarmed rate keeps reads inline.
        let off = SimDisk::with_stall(SimDiskConfig::default(), armed_cfg(), 11);
        assert_eq!(off.schedule_read(1_000, 8192), None);
    }

    #[test]
    fn unarmed_disk_schedules_nothing() {
        // The legacy instant device: no model, no draw, no deferral.
        let disk = SimDisk::new();
        assert_eq!(disk.schedule_fsync(1_000), None);
    }

    #[test]
    fn schedule_is_deterministic_and_fifo() {
        // Same seed over the same (now) sequence ⇒ identical completion
        // times (L7), and the serial timeline never goes backwards.
        let a = SimDisk::with_stall(SimDiskConfig::default(), armed_cfg(), 42);
        let b = SimDisk::with_stall(SimDiskConfig::default(), armed_cfg(), 42);
        let mut last = 0;
        for step in 0..2_000u64 {
            let now = step * 1_000_000;
            let due_a = a.schedule_fsync(now).expect("armed");
            let due_b = b.schedule_fsync(now).expect("armed");
            assert_eq!(due_a, due_b);
            assert!(due_a >= now + 120_000, "every fsync pays at least the base");
            assert!(due_a >= last, "device timeline is FIFO");
            last = due_a;
        }
    }

    #[test]
    fn episodes_wedge_the_device_for_tens_of_ms() {
        // Walking virtual time across several episode gaps must hit at
        // least one 50–90 ms stall (the S14 model's whole point).
        let disk = SimDisk::with_stall(SimDiskConfig::default(), armed_cfg(), 7);
        let mut worst_service = 0u64;
        for step in 0..6_000u64 {
            let now = step * 1_000_000;
            let due = disk.schedule_fsync(now).expect("armed");
            worst_service = worst_service.max(due.saturating_sub(now));
        }
        assert!(
            worst_service >= 50_000_000,
            "no episode engaged across ~6 sim-seconds (worst {worst_service} ns)"
        );
    }

    #[test]
    fn power_cut_resets_the_timeline_but_not_the_stream() {
        // Post-cut the device starts idle (`device_free_at` cleared); the
        // seeded episode stream continues so replay stays byte-identical.
        let disk = SimDisk::with_stall(SimDiskConfig::default(), armed_cfg(), 9);
        for step in 0..100u64 {
            let _ = disk.schedule_fsync(step * 1_000_000);
        }
        disk.power_cut(0xDEAD);
        let due = disk.schedule_fsync(200_000_000).expect("armed");
        assert!(due >= 200_000_000 + 120_000);
        // Idle device: base + tail only, unless an episode straddles.
        assert!(due <= 200_000_000 + 120_000 * 9 + 90_000_000);
    }
}
