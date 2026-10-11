//! The empty-node rows: idle RSS, the empty data directory's allocated
//! bytes, warm boot to the first `+PONG`, and idle CPU — the small side
//! that bounds every preallocation decision.
//!
//! Each row is an instrument before it is a number. The estimator is a
//! read the harness parses itself (`/proc/<pid>`, `lstat`, the wire's
//! first `+PONG`); a same-binary control set A′ runs interleaved with A
//! and bounds the spread; a planted red per row runs in every run before
//! any value is published. A plant that reads green or never engaged is
//! VACUOUS and keeps its row unset. The thresholds live only in
//! `docs/milestones/m1-gates.toml`, and the plants read them from there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cli::Flags;
use crate::gaterun::proc::read_memlock_limit;
use crate::gaterun::serving::{EMPTY_BOOT_DEADLINE_S, Serving, wait_pong};
use crate::gaterun::{
    CLOCK_TICKS_PER_S, CleanStop, GRACEFUL_STOP_DEADLINE_S, Measurements, ProcReadError,
    ProcSample, ServerGuard, SourceValue, launch_infinityd, read_proc,
};
use crate::gates::Gate;

#[cfg(test)]
mod tests;
mod walk;

use walk::{DirWalk, walk_allocated};

/// The rows' topology, the shipped default. Another cell count withholds
/// the rows (a note): every bill is written for four cells.
const EMPTY_NODE_CELLS: u16 = 4;

/// Seconds from the first `+PONG` to the window's first sample. Fixed:
/// boot work still running then falls into the window and is measured.
const EMPTY_NODE_SETTLE_S: u64 = 5;

/// Seconds between the window's two samples. Fixed.
const EMPTY_NODE_IDLE_WINDOW_S: u64 = 10;

/// infinityd's receive pool per cell: `--buffers` (default) buffers of
/// `--buf-size` (default) bytes, one page each.
const RECEIVE_BUFFERS_DEFAULT: u64 = 4096;
const RECEIVE_BUFFER_BYTES: u64 = 4096;

/// Pages one registered buffer of at most a page can span; io_uring
/// accounts every page an iovec touches, so N − 1 registered pools pin at
/// most this many pages per buffer. VmPin over that floor proves all N.
const PIN_SPAN_PAGES_MAX: u64 = 2;

/// The RSS plant's pool, `--buffers 8192`: N × 16 MiB over the default.
const PLANT_RECEIVE_BUFFERS: u64 = 8192;

/// The RSS plant's rise must reach this fraction of its extra pool.
const RSS_PLANT_RISE_FRACTION: f64 = 0.9;

/// The CPU plant's park, `--park-us 20`, against the default 500 µs.
const PLANT_PARK_US: u64 = 20;
const DEFAULT_PARK_US: u64 = 500;

/// The bill's cost of one idle wake, µs (the idle CPU bill's unit).
const IDLE_WAKE_COST_US: f64 = 5.0;

/// The CPU plant's rise must reach this fraction of its own bill.
const CPU_PLANT_RISE_FRACTION: f64 = 0.2;

/// The boot plant (the probing first boot: four FLUSH rows of 1 s, so
/// ≥ 4 s) must read at least this much over the median warm boot, ms.
const BOOT_PLANT_DELTA_MS_MIN: f64 = 2000.0;

/// The largest filesystem block the dir bill counts, bytes. Over: a note,
/// and the dir row stays unset.
const FS_BLOCK_BYTES_MAX: u64 = 4096;

/// The durable legs' data root when `--data-root` is not given.
const DATA_ROOT_DEFAULT: &str = ".artifacts/m1/empty-node-data";

const MIB: f64 = 1024.0 * 1024.0;

/// The four rows, one table: every per-row decision is a column here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Row {
    IdleRss,
    DataDir,
    WarmBoot,
    IdleCpu,
}

impl Row {
    const ALL: [Row; 4] = [Row::IdleRss, Row::DataDir, Row::WarmBoot, Row::IdleCpu];

    /// The measurement key the row's `source` names in `m1-gates.toml`.
    fn key(self) -> &'static str {
        match self {
            Row::IdleRss => "loadgen:empty_idle_rss_mib",
            Row::DataDir => "loadgen:empty_data_dir_kib",
            Row::WarmBoot => "loadgen:empty_warm_boot_ms",
            Row::IdleCpu => "loadgen:empty_idle_cpu_pct",
        }
    }

    /// The A/A′ spread budget, a fraction of median(A). Crossing: the run
    /// fails and the row stays unset; widening one needs A/A evidence.
    fn spread_budget(self) -> f64 {
        match self {
            Row::IdleRss => 0.02,
            Row::DataDir => 0.01,
            Row::WarmBoot => 0.20,
            Row::IdleCpu => 0.10,
        }
    }

    /// One resolution step in the row's unit — the control's floor: VmRSS
    /// moves in 4 KiB pages, `st_blocks` in 4 KiB blocks, the boot in
    /// `SERVE_POLL_MS`, CPU in one tick over the window (0.1 %).
    fn resolution(self) -> f64 {
        match self {
            Row::IdleRss => 4.0 / 1024.0,
            Row::DataDir => 4.0,
            Row::WarmBoot => 1.0,
            Row::IdleCpu => 0.1,
        }
    }

    /// The loaded row: the plants' comparator and threshold. There is no
    /// default: a gates file without the row makes the plant VACUOUS.
    fn gate(self, gates: &[Gate]) -> Option<&Gate> {
        gates.iter().find(|gate| gate.source == self.key())
    }
}

/// The rows' published values. The map is private to this module and its
/// one writer, [`publish`](Self::publish), consumes a [`ControlChecked`]:
/// both control sets, the control within budget, the row's own plant red.
/// The report reads the rows' sources only from here
/// ([`Measurements::measured`]), so no other path publishes them.
#[derive(Debug, Default)]
pub(crate) struct EmptyNodeValues {
    values: BTreeMap<Row, f64>,
}

impl EmptyNodeValues {
    /// Whether `source` is one of the rows, and its value if published.
    pub(crate) fn lookup(&self, source: &str) -> SourceValue {
        match Row::ALL.into_iter().find(|row| row.key() == source) {
            Some(row) => SourceValue::Proven(self.values.get(&row).copied()),
            None => SourceValue::Plain,
        }
    }

    fn publish(&mut self, checked: ControlChecked) {
        self.values.insert(checked.row, checked.value);
    }
}

/// Whether the run binds (`--reference-box`): an unproven precondition
/// is then a failure, not a note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Dev,
    Binding,
}

struct Context<'a> {
    infinityd: &'a str,
    replicates: usize,
    gates: &'a [Gate],
}

/// Runs the empty-node rows. `Err` only when a binding run's data root
/// is refused before any row; a leg failure is `m.fail` and the rows
/// stay unset while the other m1 rows run.
pub(super) fn run(
    flags: &Flags,
    gates: &[Gate],
    m: &mut Measurements,
    infinityd: &str,
    cells: u16,
    reference_box: bool,
) -> Result<(), String> {
    println!("\n== rows: empty node ==");
    if !cfg!(target_os = "linux") {
        m.note("empty-node rows withheld: the instruments read /proc and st_blocks (Linux)");
        return Ok(());
    }
    if cells != EMPTY_NODE_CELLS {
        m.note(format!(
            "empty-node rows withheld: --cells {cells}; the bills are for {EMPTY_NODE_CELLS}"
        ));
        return Ok(());
    }
    let root = PathBuf::from(flags.str_or("data-root", DATA_ROOT_DEFAULT));
    let fstype = crate::gaterun::admit_device_root(flags, &root, reference_box)?;
    let admission = if crate::gaterun::is_memory_fs(&fstype) {
        Err(format!("data root {} is {fstype}", root.display()))
    } else {
        Ok(())
    };
    let tier = if reference_box { Tier::Binding } else { Tier::Dev };
    let ctx = Context { infinityd, replicates: flags.usize_or("replicates", 3)?, gates };
    environment_notes(m, &ctx, &root, &fstype);
    match measure(&ctx, &root, admission, m) {
        Ok(outcome) => publish_rows(m, tier, &outcome),
        Err(e) => m.fail(format!("empty-node leg failed, rows unset: {e}")),
    }
    Ok(())
}

fn environment_notes(m: &mut Measurements, ctx: &Context<'_>, root: &Path, fstype: &str) {
    let version = std::process::Command::new(ctx.infinityd)
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())
        .map(|out| String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").to_string());
    let thp = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
        .map(|text| text.trim().to_string());
    m.note(format!(
        "empty-node: infinityd {} · THP {} · data root {} ({fstype}) · {} replicates per set",
        version.unwrap_or_else(|e| format!("version unknown ({e})")),
        thp.unwrap_or_else(|e| format!("unreadable ({e})")),
        root.display(),
        ctx.replicates
    ));
}

// ---- one leg -----------------------------------------------------------

/// A memory leg's RSS on a node whose every cell registered its receive
/// pool (VmPin over [`pin_proven_floor`]). Built only by `prove`.
#[derive(Clone, Copy, Debug)]
struct PinnedRss {
    rss_bytes: u64,
}

/// VmPin at or under the floor: registration degraded on some cell
/// (`RLIMIT_MEMLOCK`, an old kernel) and its pool was never touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PinUnproven {
    pinned_bytes: u64,
    floor_bytes: u64,
}

impl std::fmt::Display for PinUnproven {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fixed-buffer registration unproven: VmPin {} B at or under the {} B floor",
            self.pinned_bytes, self.floor_bytes
        )
    }
}

/// 2 (N − 1) · b · s: the most VmPin N − 1 registered pools can account.
fn pin_proven_floor(cells: u64, buffers: u64) -> Option<u64> {
    PIN_SPAN_PAGES_MAX
        .checked_mul(cells.checked_sub(1)?)?
        .checked_mul(buffers)?
        .checked_mul(RECEIVE_BUFFER_BYTES)
}

impl PinnedRss {
    fn prove(sample: &ProcSample, cells: u64, buffers: u64) -> Result<PinnedRss, PinUnproven> {
        let floor_bytes = pin_proven_floor(cells, buffers).unwrap_or(u64::MAX);
        if sample.pinned_bytes() > floor_bytes {
            Ok(PinnedRss { rss_bytes: sample.rss_bytes() })
        } else {
            Err(PinUnproven { pinned_bytes: sample.pinned_bytes(), floor_bytes })
        }
    }

    fn mib(self) -> f64 {
        self.rss_bytes as f64 / MIB
    }
}

/// The two samples of one idle window, a window apart.
#[derive(Debug)]
struct IdleWindow {
    first: ProcSample,
    last: ProcSample,
    elapsed: Duration,
}

impl IdleWindow {
    fn cpu_pct(&self) -> Result<f64, String> {
        let ticks = self
            .last
            .cpu_ticks()
            .checked_sub(self.first.cpu_ticks())
            .ok_or("CPU ticks went backwards within one process")?;
        let seconds = self.elapsed.as_secs_f64();
        if seconds <= 0.0 {
            return Err("an empty idle window".into());
        }
        Ok(ticks as f64 / CLOCK_TICKS_PER_S as f64 / seconds * 100.0)
    }
}

/// Settles, then samples twice a window apart. `serving`'s connection
/// is borrowed for the whole wait and nothing is sent on it: the node's
/// one idle client. The second read must be the same process.
fn idle_window<S>(
    serving: &Serving,
    settle: Duration,
    window: Duration,
    mut sample: S,
) -> Result<IdleWindow, ProcReadError>
where
    S: FnMut(Option<u64>) -> Result<ProcSample, ProcReadError>,
{
    let _idle_client = serving.connection();
    std::thread::sleep(settle);
    let first = sample(None)?;
    let started = Instant::now();
    std::thread::sleep(window);
    let last = sample(Some(first.start_ticks()))?;
    Ok(IdleWindow { first, last, elapsed: started.elapsed() })
}

fn idle_durations() -> (Duration, Duration) {
    (Duration::from_secs(EMPTY_NODE_SETTLE_S), Duration::from_secs(EMPTY_NODE_IDLE_WINDOW_S))
}

fn serve(guard: &mut ServerGuard, spawned_at: Instant) -> Result<Serving, String> {
    let port = guard.port;
    let deadline = Duration::from_secs(EMPTY_BOOT_DEADLINE_S);
    wait_pong(port, spawned_at, deadline, || guard.try_exited())
        .map_err(|e| format!("boot on port {port}: {e}"))
}

/// The memory plants: one flag each over the shipped default.
#[derive(Clone, Copy, Debug)]
enum MemoryPlant {
    /// `--buffers 8192`: the RSS row's planted red.
    Buffers,
    /// `--park-us 20`: the CPU row's planted red.
    Park,
}

/// Which memory node a leg boots: the control, or a plant.
#[derive(Clone, Copy, Debug)]
enum MemoryArm {
    Control,
    Plant(MemoryPlant),
}

impl MemoryArm {
    fn buffers(self) -> u64 {
        match self {
            MemoryArm::Control | MemoryArm::Plant(MemoryPlant::Park) => RECEIVE_BUFFERS_DEFAULT,
            MemoryArm::Plant(MemoryPlant::Buffers) => PLANT_RECEIVE_BUFFERS,
        }
    }

    /// The shipped default (no data dir; the probe default named so the
    /// harness does not turn it off), plus the plant's one flag.
    fn args(self) -> Vec<String> {
        let mut args = vec!["--device-probe".to_string(), "auto".to_string()];
        match self {
            MemoryArm::Control => {}
            MemoryArm::Plant(MemoryPlant::Buffers) => {
                args.extend(["--buffers".to_string(), PLANT_RECEIVE_BUFFERS.to_string()]);
            }
            MemoryArm::Plant(MemoryPlant::Park) => {
                args.extend(["--park-us".to_string(), PLANT_PARK_US.to_string()]);
            }
        }
        args
    }
}

/// A control leg: the shipped default node. [`boot`](Self::boot) is its
/// only constructor and a [`LegSet`] holds nothing else, so a planted
/// node's facts never reach a row value.
#[derive(Debug)]
struct ControlLeg(MemoryLeg);

impl ControlLeg {
    fn boot(ctx: &Context<'_>) -> Result<ControlLeg, String> {
        memory_leg(ctx, MemoryArm::Control).map(ControlLeg)
    }
}

/// A planted leg: its facts feed its plant's verdict and the notes only.
#[derive(Debug)]
struct PlantLeg(MemoryLeg);

impl PlantLeg {
    fn boot(ctx: &Context<'_>, plant: MemoryPlant) -> Result<PlantLeg, String> {
        memory_leg(ctx, MemoryArm::Plant(plant)).map(PlantLeg)
    }
}

/// One memory leg's facts, wrapped as a [`ControlLeg`] or a [`PlantLeg`].
#[derive(Debug)]
struct MemoryLeg {
    rss: Result<PinnedRss, PinUnproven>,
    rss_bytes: u64,
    pinned_bytes: u64,
    cpu_pct: f64,
    boot: Duration,
    loading_replies: u64,
    memlock: String,
    /// The server's own INFO fields, per cell, raw (after the window).
    attribution: String,
}

/// Spawn → first `+PONG` → settle → window → SIGKILL (memory-only:
/// nothing to keep).
fn memory_leg(ctx: &Context<'_>, arm: MemoryArm) -> Result<MemoryLeg, String> {
    let args = arm.args();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (mut guard, spawned_at) = launch_infinityd(ctx.infinityd, EMPTY_NODE_CELLS, &refs)?;
    let serving = serve(&mut guard, spawned_at)?;
    let pid = guard.pid();
    let memlock = read_memlock_limit(pid).unwrap_or_else(|e| format!("unreadable ({e})"));
    let (settle, window) = idle_durations();
    let idle = idle_window(&serving, settle, window, |expected| read_proc(pid, expected))
        .map_err(|e| format!("{arm:?} memory leg: {e}"))?;
    Ok(MemoryLeg {
        rss: PinnedRss::prove(&idle.last, u64::from(EMPTY_NODE_CELLS), arm.buffers()),
        rss_bytes: idle.last.rss_bytes(),
        pinned_bytes: idle.last.pinned_bytes(),
        cpu_pct: idle.cpu_pct().map_err(|e| format!("{arm:?} memory leg: {e}"))?,
        boot: serving.boot(),
        loading_replies: serving.loading_replies(),
        memlock,
        attribution: info_attribution(guard.port, &MEMORY_ATTRIBUTION_FIELDS),
    })
}

/// INFO fields that attribute an idle-RSS excess (the server's own reads).
const MEMORY_ATTRIBUTION_FIELDS: [&str; 3] =
    ["used_memory", "used_memory_rss", "wire_buffers_bytes"];

/// INFO fields that attribute a warm-boot or directory excess by phase.
const BOOT_ATTRIBUTION_FIELDS: [&str; 8] = [
    "recover_start_us",
    "recover_ckpt_us",
    "recover_replay_us",
    "recover_audit_us",
    "recover_finish_us",
    "recover_total_us",
    "zero_fill_bytes",
    "rotations_unzeroed",
];

/// The server's own INFO fields, one value per cell in cell order, taken
/// after the window: cross-check notes that attribute an excess, never an
/// estimator. A missing field reads `absent`, never a 0.
fn info_attribution(port: u16, fields: &[&str]) -> String {
    let infos = match crate::gaterun::scrape_cells(port, EMPTY_NODE_CELLS) {
        Ok(infos) => infos,
        Err(e) => return format!("INFO unavailable ({e})"),
    };
    let per_field = fields.iter().map(|field| {
        let values: Vec<&str> =
            infos.iter().map(|info| info.get(*field).map_or("absent", String::as_str)).collect();
        format!("{field}={}", values.join("/"))
    });
    per_field.collect::<Vec<_>>().join(" ")
}

/// A fresh directory under the data root, removed on drop (success or
/// failure). It removes only itself, never the root.
struct RunDir {
    path: PathBuf,
}

impl RunDir {
    fn create(root: &Path, label: &str) -> Result<RunDir, String> {
        let path = root.join(format!("empty-node-{}-{label}", std::process::id()));
        if path.exists() {
            std::fs::remove_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        std::fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(RunDir { path })
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() && self.path.exists() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// A data directory whose last stop exited 0: the only directory a warm
/// boot may use.
struct WarmDir<'a> {
    dir: &'a RunDir,
    _clean: CleanStop,
}

/// What a durable boot starts from.
enum BootOf<'a> {
    /// A directory no node has used: a first boot, not walked.
    Fresh(&'a RunDir),
    /// A directory a node stopped cleanly: a warm boot, walked.
    Warm(WarmDir<'a>),
}

impl<'a> BootOf<'a> {
    fn dir(&self) -> &'a RunDir {
        match self {
            BootOf::Fresh(dir) => dir,
            BootOf::Warm(warm) => warm.dir,
        }
    }
}

/// One durable boot's facts; `walk` is set for warm boots only.
struct DurableBoot {
    boot: Duration,
    loading_replies: u64,
    cpu_pct: f64,
    walk: Option<DirWalk>,
    /// The server's own INFO fields, per cell, raw (after the window).
    attribution: String,
}

impl DurableBoot {
    fn boot_ms(&self) -> f64 {
        self.boot.as_secs_f64() * 1000.0
    }
}

/// Spawn → first `+PONG` → settle → window (→ walk, warm boots) →
/// SIGTERM → exit 0, which makes the directory warm for the next boot.
fn durable_boot<'a>(
    ctx: &Context<'_>,
    of: BootOf<'a>,
) -> Result<(DurableBoot, WarmDir<'a>), String> {
    let dir = of.dir();
    let path = dir.path.to_str().ok_or("the data root is not UTF-8")?;
    let args = ["--data-dir", path, "--device-probe", "auto"];
    let (mut guard, spawned_at) = launch_infinityd(ctx.infinityd, EMPTY_NODE_CELLS, &args)?;
    let serving = serve(&mut guard, spawned_at)?;
    let pid = guard.pid();
    let (settle, window) = idle_durations();
    let idle = idle_window(&serving, settle, window, |expected| read_proc(pid, expected))
        .map_err(|e| format!("durable leg on {path}: {e}"))?;
    let walk = match of {
        BootOf::Fresh(_) => None,
        BootOf::Warm(_) => {
            Some(walk_allocated(&dir.path, EMPTY_NODE_CELLS).map_err(|e| format!("{path}: {e}"))?)
        }
    };
    let facts = DurableBoot {
        boot: serving.boot(),
        loading_replies: serving.loading_replies(),
        cpu_pct: idle.cpu_pct()?,
        walk,
        attribution: info_attribution(guard.port, &BOOT_ATTRIBUTION_FIELDS),
    };
    drop(serving);
    let clean = guard
        .stop_graceful(Duration::from_secs(GRACEFUL_STOP_DEADLINE_S))
        .map_err(|e| format!("stop on {path}: {e}"))?;
    Ok((facts, WarmDir { dir, _clean: clean }))
}

/// Proof that D and D′ ran one io configuration: their
/// `io-properties.toml` files are byte-equal. Built only by `compare`.
#[derive(Debug)]
struct SameIoProperties(());

#[derive(Debug)]
struct IoPropertiesDiffer(String);

impl SameIoProperties {
    fn compare(d: &Path, d_prime: &Path) -> Result<SameIoProperties, IoPropertiesDiffer> {
        let read = |dir: &Path| {
            let path = dir.join(IO_PROPERTIES_FILE);
            std::fs::read(&path).map_err(|e| IoPropertiesDiffer(format!("{}: {e}", path.display())))
        };
        if read(d)? == read(d_prime)? {
            Ok(SameIoProperties(()))
        } else {
            Err(IoPropertiesDiffer(format!("{IO_PROPERTIES_FILE} differs: D′ reprobed")))
        }
    }
}

/// infinityd's device model file (the first boot's probe writes it).
const IO_PROPERTIES_FILE: &str = "io-properties.toml";

// ---- the run -----------------------------------------------------------

/// One warm boot's row facts.
#[derive(Debug)]
struct WarmLeg {
    boot_ms: f64,
    walk: DirWalk,
}

/// One control set's complete legs (A or A′): control legs only.
#[derive(Debug, Default)]
struct LegSet {
    memory: Vec<ControlLeg>,
    warm: Vec<WarmLeg>,
}

/// The durable half of the run.
#[derive(Debug)]
enum Durable {
    /// A declared withholding (a memory-filesystem data root, dev tier).
    Withheld(String),
    /// D and D′ ran; `io` says whether they ran one io configuration.
    Ran { io: Result<SameIoProperties, IoPropertiesDiffer> },
}

/// A plant's verdict: only `Red` lets its row publish.
#[derive(Debug)]
enum PlantVerdict {
    Red(String),
    /// The plant ran and the instrument did not see it.
    Green(String),
    /// The plant's leg never engaged (a spawn failure, an unproven pin, a
    /// missing row).
    Unengaged(String),
}

impl std::fmt::Display for PlantVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlantVerdict::Red(detail) => write!(f, "RED — {detail}"),
            PlantVerdict::Green(why) => write!(f, "GREEN (VACUOUS) — {why}"),
            PlantVerdict::Unengaged(why) => write!(f, "NOT ENGAGED (VACUOUS) — {why}"),
        }
    }
}

#[derive(Debug)]
struct Plants {
    rss: PlantVerdict,
    cpu: PlantVerdict,
    boot: PlantVerdict,
    dir: PlantVerdict,
}

impl Plants {
    fn of(&self, row: Row) -> &PlantVerdict {
        match row {
            Row::IdleRss => &self.rss,
            Row::IdleCpu => &self.cpu,
            Row::WarmBoot => &self.boot,
            Row::DataDir => &self.dir,
        }
    }
}

#[derive(Debug)]
struct Outcome {
    a: LegSet,
    a_prime: LegSet,
    durable: Durable,
    plants: Plants,
}

/// D and D′ after their first boots.
struct DurablePair<'a> {
    d: WarmDir<'a>,
    d_prime: WarmDir<'a>,
    probe_boot_ms: f64,
    io: Result<SameIoProperties, IoPropertiesDiffer>,
}

/// Every leg of A and A′ (interleaved), then every plant.
fn measure(
    ctx: &Context<'_>,
    root: &Path,
    admission: Result<(), String>,
    m: &mut Measurements,
) -> Result<Outcome, String> {
    let dirs = match &admission {
        Ok(()) => {
            std::fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
            Some((RunDir::create(root, "d")?, RunDir::create(root, "d-prime")?))
        }
        Err(_) => None,
    };
    let pair = match &dirs {
        Some((d, d_prime)) => Some(first_boots(ctx, d, d_prime, m)?),
        None => None,
    };
    let mut log = String::new();
    let (a, a_prime, pair) = replicates(ctx, pair, &mut log)?;
    let (durable, boot, dir) = match (pair, admission) {
        (Some(pair), _) => durable_plants(ctx, pair, &a, m),
        (None, reason) => {
            let reason = reason.err().unwrap_or_else(|| "no durable legs ran".into());
            let not_run = || PlantVerdict::Unengaged(format!("durable legs withheld: {reason}"));
            (Durable::Withheld(reason.clone()), not_run(), not_run())
        }
    };
    let rss = rss_plant(ctx, set_rss_median(&a), &mut log);
    let cpu = cpu_plant(ctx, median_of(&set_values(&a, |leg| leg.cpu_pct)), &mut log);
    m.raw_section("empty-node legs", &log);
    Ok(Outcome { a, a_prime, durable, plants: Plants { rss, cpu, boot, dir } })
}

/// D's first boot probes the device; D′ is seeded with D's
/// `io-properties.toml`, so its first boot reads it and does not probe.
fn first_boots<'a>(
    ctx: &Context<'_>,
    d: &'a RunDir,
    d_prime: &'a RunDir,
    m: &mut Measurements,
) -> Result<DurablePair<'a>, String> {
    let (probe, d_warm) = durable_boot(ctx, BootOf::Fresh(d))?;
    let seed = (d.path.join(IO_PROPERTIES_FILE), d_prime.path.join(IO_PROPERTIES_FILE));
    std::fs::copy(&seed.0, &seed.1).map_err(|e| format!("seed D′ {}: {e}", seed.1.display()))?;
    let (seeded, d_prime_warm) = durable_boot(ctx, BootOf::Fresh(d_prime))?;
    let class = d.path.to_str().map_or("unknown".into(), crate::m45rows::s42_file_class);
    m.note(format!(
        "empty-node first boots (never row values): D probing {:.1} ms ({} -LOADING), D′ \
         seeded {:.1} ms ({} -LOADING); barrier class {class}",
        probe.boot_ms(),
        probe.loading_replies,
        seeded.boot_ms(),
        seeded.loading_replies
    ));
    let attribution = format!(
        "D (probing): INFO {}\nD′ (seeded): INFO {}\n",
        probe.attribution, seeded.attribution
    );
    m.raw_section("empty-node first boots", &attribution);
    Ok(DurablePair {
        d: d_warm,
        d_prime: d_prime_warm,
        probe_boot_ms: probe.boot_ms(),
        io: SameIoProperties::compare(&d.path, &d_prime.path),
    })
}

/// A₁ A′₁ … A_R A′_R: each replicate runs a memory leg per set and, when
/// the durable legs run, a warm boot of D (A) and of D′ (A′).
fn replicates<'a>(
    ctx: &Context<'_>,
    mut pair: Option<DurablePair<'a>>,
    log: &mut String,
) -> Result<(LegSet, LegSet, Option<DurablePair<'a>>), String> {
    let (mut a, mut a_prime) = (LegSet::default(), LegSet::default());
    for rep in 0..ctx.replicates {
        for (set, label) in [(&mut a, "A"), (&mut a_prime, "A′")] {
            let leg = ControlLeg::boot(ctx)?;
            log_memory(log, &format!("{label} memory rep{rep}"), &leg.0);
            set.memory.push(leg);
        }
        if let Some(current) = pair.take() {
            let (leg_a, d) = warm_leg(ctx, current.d, &format!("A warm rep{rep} (D)"), log)?;
            let (leg_ap, d_prime) =
                warm_leg(ctx, current.d_prime, &format!("A′ warm rep{rep} (D′)"), log)?;
            a.warm.push(leg_a);
            a_prime.warm.push(leg_ap);
            pair = Some(DurablePair { d, d_prime, ..current });
        }
    }
    Ok((a, a_prime, pair))
}

fn warm_leg<'a>(
    ctx: &Context<'_>,
    warm: WarmDir<'a>,
    label: &str,
    log: &mut String,
) -> Result<(WarmLeg, WarmDir<'a>), String> {
    let (facts, warm) = durable_boot(ctx, BootOf::Warm(warm))?;
    let walk = facts.walk.ok_or("a warm boot returned no walk")?;
    log.push_str(&format!(
        "{label}: boot {:.2} ms ({} -LOADING), dir {} B allocated ({} entries, block ≤ {} B), \
         CPU {:.2} %\n",
        facts.boot.as_secs_f64() * 1000.0,
        facts.loading_replies,
        walk.allocated_bytes,
        walk.entries.len(),
        walk.block_bytes_max,
        facts.cpu_pct
    ));
    log.push_str(&format!("  INFO {}\n", facts.attribution));
    Ok((WarmLeg { boot_ms: facts.boot.as_secs_f64() * 1000.0, walk }, warm))
}

fn log_memory(log: &mut String, label: &str, leg: &MemoryLeg) {
    let pin = match &leg.rss {
        Ok(_) => "proven".to_string(),
        Err(unproven) => unproven.to_string(),
    };
    log.push_str(&format!(
        "{label}: boot {:.2} ms ({} -LOADING), VmRSS {} B, VmPin {} B ({pin}), CPU {:.2} %, \
         memlock {}\n",
        leg.boot.as_secs_f64() * 1000.0,
        leg.loading_replies,
        leg.rss_bytes,
        leg.pinned_bytes,
        leg.cpu_pct,
        leg.memlock
    ));
    log.push_str(&format!("  INFO {}\n", leg.attribution));
}

/// The boot and dir plants (D is stopped and never booted again after
/// its plant), and the durable rows' io check.
fn durable_plants(
    ctx: &Context<'_>,
    pair: DurablePair<'_>,
    a: &LegSet,
    m: &mut Measurements,
) -> (Durable, PlantVerdict, PlantVerdict) {
    let warm_median = median_of(&a.warm.iter().map(|leg| leg.boot_ms).collect::<Vec<_>>());
    let boot = match warm_median {
        Some(median) => judge(
            Row::WarmBoot.gate(ctx.gates),
            pair.probe_boot_ms,
            median,
            BOOT_PLANT_DELTA_MS_MIN,
        ),
        None => PlantVerdict::Unengaged("no warm boot median".into()),
    };
    if let Some(last) = a.warm.last() {
        let listing: String =
            last.walk.entries.iter().map(|(name, bytes)| format!("{bytes:>8} {name}\n")).collect();
        m.raw_section("empty-node D's last warm walk (allocated bytes per entry)", &listing);
    }
    let dir = dir_plant_on(pair.d, ctx.gates);
    (Durable::Ran { io: pair.io }, boot, dir)
}

/// The dir plant consumes D's warm-directory proof: D is never booted
/// again after its plant.
fn dir_plant_on(warm: WarmDir<'_>, gates: &[Gate]) -> PlantVerdict {
    walk::dir_plant(&warm.dir.path, Row::DataDir.gate(gates), |path| {
        walk_allocated(path, EMPTY_NODE_CELLS).map(|walk| walk.allocated_bytes)
    })
}

/// A plant is red only when its reading fails the loaded row's own
/// comparator **and** rises over the control median by `rise_min`.
fn judge(gate: Option<&Gate>, reading: f64, baseline: f64, rise_min: f64) -> PlantVerdict {
    let Some(gate) = gate else {
        return PlantVerdict::Unengaged("the gates file has no such row".into());
    };
    let rise = reading - baseline;
    let detail = format!(
        "reads {reading:.3} over control median {baseline:.3} (rise {rise:.3}, needs ≥ \
         {rise_min:.3}); row {} {} {}",
        gate.comparator, gate.threshold, gate.unit
    );
    if rise.is_nan() || rise < rise_min {
        PlantVerdict::Green(format!("rise under the plant's floor: {detail}"))
    } else if gate.passes(reading) {
        PlantVerdict::Green(format!("within the bill: {detail}"))
    } else {
        PlantVerdict::Red(detail)
    }
}

/// `--buffers 8192` adds N × 4096 × 4 KiB = 64 MiB of pinned pool: the
/// reading must rise by 0.9 of it and exceed the RSS bill.
fn rss_plant(ctx: &Context<'_>, control_mib: Option<f64>, log: &mut String) -> PlantVerdict {
    let PlantLeg(leg) = match PlantLeg::boot(ctx, MemoryPlant::Buffers) {
        Ok(leg) => leg,
        Err(e) => return PlantVerdict::Unengaged(format!("plant leg: {e}")),
    };
    log_memory(log, "RSS plant (--buffers 8192)", &leg);
    let rss = match leg.rss {
        Ok(rss) => rss,
        Err(unproven) => return PlantVerdict::Unengaged(format!("plant leg: {unproven}")),
    };
    let Some(baseline) = control_mib else {
        return PlantVerdict::Unengaged("no control RSS median".into());
    };
    let extra_pool_bytes = (PLANT_RECEIVE_BUFFERS - RECEIVE_BUFFERS_DEFAULT)
        * RECEIVE_BUFFER_BYTES
        * u64::from(EMPTY_NODE_CELLS);
    let rise_min = RSS_PLANT_RISE_FRACTION * extra_pool_bytes as f64 / MIB;
    judge(Row::IdleRss.gate(ctx.gates), rss.mib(), baseline, rise_min)
}

/// The CPU plant's own bill, % of one core: N × (10⁶/20 − 10⁶/500)
/// wakes/s × 5 µs = 96 %.
fn cpu_plant_bill_pct() -> f64 {
    let wakes_per_s = 1e6 / PLANT_PARK_US as f64 - 1e6 / DEFAULT_PARK_US as f64;
    f64::from(EMPTY_NODE_CELLS) * wakes_per_s * IDLE_WAKE_COST_US / 1e6 * 100.0
}

fn cpu_plant(ctx: &Context<'_>, control_pct: Option<f64>, log: &mut String) -> PlantVerdict {
    let PlantLeg(leg) = match PlantLeg::boot(ctx, MemoryPlant::Park) {
        Ok(leg) => leg,
        Err(e) => return PlantVerdict::Unengaged(format!("plant leg: {e}")),
    };
    log_memory(log, "CPU plant (--park-us 20)", &leg);
    let Some(baseline) = control_pct else {
        return PlantVerdict::Unengaged("no control CPU median".into());
    };
    let rise_min = CPU_PLANT_RISE_FRACTION * cpu_plant_bill_pct();
    judge(Row::IdleCpu.gate(ctx.gates), leg.cpu_pct, baseline, rise_min)
}

// ---- control and publication -------------------------------------------

/// The upper median (the harness's `median` convention), `None` when
/// empty; the total order keeps it defined for every input.
fn median_of(values: &[f64]) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted.get(sorted.len() / 2).copied()
}

fn set_values(set: &LegSet, value: impl Fn(&MemoryLeg) -> f64) -> Vec<f64> {
    set.memory.iter().map(|ControlLeg(leg)| value(leg)).collect()
}

/// The set's RSS median in MiB, only when every leg's pin is proven.
fn set_rss_median(set: &LegSet) -> Option<f64> {
    let values: Option<Vec<f64>> = set
        .memory
        .iter()
        .map(|ControlLeg(leg)| leg.rss.as_ref().ok().map(|rss| rss.mib()))
        .collect();
    median_of(&values?)
}

/// Why a row is withheld, which decides what it means per tier.
#[derive(Debug)]
enum Withholding {
    /// A declared scope limit (a memory-filesystem data root, a block over
    /// the bill's): a note in every tier.
    Declared(String),
    /// A precondition the run did not prove (a pin, D′'s io
    /// configuration): a note on the dev tier, a failure when binding.
    PreconditionUnproven(String),
}

/// A row's values from both sets, or why the row is withheld.
#[derive(Debug)]
enum RowInput {
    Measured { a: Vec<f64>, a_prime: Vec<f64> },
    Withheld(Withholding),
}

/// The two rows the durable legs feed.
#[derive(Clone, Copy, Debug)]
enum DurableRow {
    WarmBoot,
    DataDir,
}

fn row_input(row: Row, a: &LegSet, a_prime: &LegSet, durable: &Durable) -> RowInput {
    match row {
        Row::IdleRss => rss_input(a, a_prime),
        Row::IdleCpu => RowInput::Measured {
            a: set_values(a, |leg| leg.cpu_pct),
            a_prime: set_values(a_prime, |leg| leg.cpu_pct),
        },
        Row::WarmBoot => durable_input(DurableRow::WarmBoot, a, a_prime, durable),
        Row::DataDir => durable_input(DurableRow::DataDir, a, a_prime, durable),
    }
}

fn durable_input(row: DurableRow, a: &LegSet, a_prime: &LegSet, durable: &Durable) -> RowInput {
    match durable {
        Durable::Withheld(reason) => RowInput::Withheld(Withholding::Declared(reason.clone())),
        Durable::Ran { io: Err(IoPropertiesDiffer(reason)) } => {
            RowInput::Withheld(Withholding::PreconditionUnproven(reason.clone()))
        }
        Durable::Ran { io: Ok(same) } => warm_input(row, a, a_prime, same),
    }
}

fn rss_input(a: &LegSet, a_prime: &LegSet) -> RowInput {
    let unproven = a.memory.iter().chain(&a_prime.memory).find_map(|ControlLeg(leg)| leg.rss.err());
    if let Some(unproven) = unproven {
        return RowInput::Withheld(Withholding::PreconditionUnproven(unproven.to_string()));
    }
    let mib = |set: &LegSet| -> Vec<f64> {
        set.memory.iter().filter_map(|ControlLeg(leg)| leg.rss.ok()).map(PinnedRss::mib).collect()
    };
    RowInput::Measured { a: mib(a), a_prime: mib(a_prime) }
}

/// The durable rows read D and D′ only with the proof that they ran one
/// io configuration.
fn warm_input(row: DurableRow, a: &LegSet, a_prime: &LegSet, _same: &SameIoProperties) -> RowInput {
    let block = a.warm.iter().chain(&a_prime.warm).map(|leg| leg.walk.block_bytes_max).max();
    let over_block = block.is_some_and(|bytes| bytes > FS_BLOCK_BYTES_MAX);
    if matches!(row, DurableRow::DataDir) && over_block {
        let reason = format!("filesystem block {block:?} B over the bill's {FS_BLOCK_BYTES_MAX} B");
        return RowInput::Withheld(Withholding::Declared(reason));
    }
    let value = |leg: &WarmLeg| match row {
        DurableRow::DataDir => walk::bytes_to_kib(leg.walk.allocated_bytes),
        DurableRow::WarmBoot => leg.boot_ms,
    };
    RowInput::Measured {
        a: a.warm.iter().map(value).collect(),
        a_prime: a_prime.warm.iter().map(value).collect(),
    }
}

/// A row value whose control passed and whose own plant read red: the
/// only input [`EmptyNodeValues::publish`] takes.
#[derive(Debug)]
struct ControlChecked {
    row: Row,
    value: f64,
    detail: String,
}

/// Why a row stays unset.
#[derive(Debug)]
enum Refusal {
    Withheld(Withholding),
    Spread(String),
    Vacuous(String),
}

impl ControlChecked {
    /// The row's proof, from the outcome's own input and plant for that
    /// row: no caller can pair a row with another row's plant or values.
    fn check(row: Row, outcome: &Outcome) -> Result<ControlChecked, Refusal> {
        let input = row_input(row, &outcome.a, &outcome.a_prime, &outcome.durable);
        let (value, detail) = control(row, input, outcome.plants.of(row))?;
        Ok(ControlChecked { row, value, detail })
    }
}

/// The row's value, median(A), when its plant read red and its A/A′
/// control is within budget; with the detail for the note.
fn control(row: Row, input: RowInput, plant: &PlantVerdict) -> Result<(f64, String), Refusal> {
    let (a, a_prime) = match input {
        RowInput::Measured { a, a_prime } => (a, a_prime),
        RowInput::Withheld(withholding) => return Err(Refusal::Withheld(withholding)),
    };
    match plant {
        PlantVerdict::Red(_) => {}
        PlantVerdict::Green(why) | PlantVerdict::Unengaged(why) => {
            return Err(Refusal::Vacuous(why.clone()));
        }
    }
    let (Some(median_a), Some(median_ap)) = (median_of(&a), median_of(&a_prime)) else {
        return Err(Refusal::Spread("a control set has no legs".into()));
    };
    let allowed = (row.spread_budget() * median_a.abs()).max(row.resolution());
    let delta = (median_a - median_ap).abs();
    let detail = format!(
        "median(A) {median_a:.3}, median(A′) {median_ap:.3}, |Δ| {delta:.3} ≤ {allowed:.3}"
    );
    if delta.is_nan() || delta > allowed {
        return Err(Refusal::Spread(detail.replace('≤', "over")));
    }
    Ok((median_a, detail))
}

fn publish(m: &mut Measurements, checked: ControlChecked) {
    m.note(format!("empty-node {}: {}", checked.row.key(), checked.detail));
    m.empty_node.publish(checked);
}

/// An unproven precondition fails a binding run; every other withholding
/// is a note.
fn withhold(m: &mut Measurements, row: Row, withholding: Withholding, tier: Tier) {
    match (withholding, tier) {
        (Withholding::PreconditionUnproven(reason), Tier::Binding) => {
            m.fail(format!("empty-node {} precondition unproven: {reason}", row.key()));
        }
        (Withholding::PreconditionUnproven(reason), Tier::Dev)
        | (Withholding::Declared(reason), Tier::Dev | Tier::Binding) => {
            m.note(format!("empty-node {} withheld: {reason}", row.key()));
        }
    }
}

fn publish_rows(m: &mut Measurements, tier: Tier, outcome: &Outcome) {
    for row in Row::ALL {
        m.note(format!("empty-node plant {}: {}", row.key(), outcome.plants.of(row)));
        match (ControlChecked::check(row, outcome), tier) {
            (Ok(checked), Tier::Dev | Tier::Binding) => publish(m, checked),
            (Err(Refusal::Withheld(withholding)), Tier::Dev | Tier::Binding) => {
                withhold(m, row, withholding, tier);
            }
            (Err(Refusal::Spread(detail)), Tier::Dev | Tier::Binding) => {
                m.fail(format!("empty-node instrument spread {}: {detail}", row.key()));
            }
            (Err(Refusal::Vacuous(why)), Tier::Dev | Tier::Binding) => {
                m.fail(format!("VACUOUS canary {}: {why}", row.key()));
            }
        }
    }
}
