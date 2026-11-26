//! The tier-file transport's crate tests. The transport: a ninth offer
//! deferred with eight jobs out and again with eight results unconsumed, a
//! position freed only by its result's consumption, every owner consumed
//! once by identity, each outcome, the stop's rows, the cut's custody, the
//! wakes landing between a cell's final check and its park, and on Linux
//! the native close and the cell's park ended by the worker's result wake.
//! The worker: its pass's fairness from its cursor under continuous,
//! last-only, alternating and empty readiness and with one lane failing
//! every attempt, its wait only after a whole empty circuit, and a
//! message's turn, a full channel's included. The expected identities, the
//! drop census and the waits are the tests' own, kept outside the
//! candidate.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;

use inf_foundation::fault::{self, FaultSpec};
use inf_foundation::grants::{DirectoryWriter, GrantPos};
use inf_log::fs::mem::{MemFile, MemFs};
use inf_log::fs::sim::{SimDisk, SimFile};
use inf_log::fs::{SegmentFile, SegmentFs, StdSegmentFile, StdSegmentFs, TierIoMode};
use inf_log::{NsId, SealReason, TierFileMeta};
use inf_store::LogicalAddr;

use super::*;
use crate::control::{ControlMsg, ControlSender, RetireWake};

/// A job's identity as the test keeps it: cell, namespace, file id.
type Key = (u16, u32, u32);

/// A transport of `cells` cells over one test thread: the worker, every
/// part, the board's wake, and the identities offered and consumed. Its
/// drop cuts every part with its lane.
struct Rig<J: SegmentFile + FinalClose> {
    board: Arc<RetireBoard>,
    writer: DirectoryWriter,
    tier: DetachedTier<J>,
    parts: Vec<CellPart<J>>,
    wake: RetireWake,
    _control: mpsc::Receiver<ControlMsg>,
    _wakes: mpsc::Receiver<()>,
    offered: Vec<Key>,
    consumed: Vec<(Key, RetireOutcome)>,
}

impl<J: SegmentFile + FinalClose> Rig<J> {
    fn new(cells: u16) -> Rig<J> {
        let TierBoot { shared, writer, worker, parts } =
            TierLifetimeBoot::reserve::<J>(cells).expect("the reservation");
        let (control_tx, control) = mpsc::sync_channel(256);
        let (wake_tx, wakes) = mpsc::sync_channel(1);
        let wake = RetireWake::over(wake_tx, &control_tx);
        Rig {
            board: shared.board,
            writer,
            tier: DetachedTier::new(worker, Vec::new()),
            parts,
            wake,
            _control: control,
            _wakes: wakes,
            offered: Vec::new(),
            consumed: Vec::new(),
        }
    }

    fn pos(&self, index: usize) -> GrantPos {
        self.writer.positions().nth(index).expect("a grant position")
    }

    /// One turn's offer of one owner on `cell`: `false`, the owner kept,
    /// when the batch has no room. A published offer rings the bell.
    fn offer(&mut self, cell: u16, ns: u32, meta: TierFileMeta, handle: Option<J>) -> bool {
        let pos = self.pos(usize::from(cell) % 64);
        let part = &mut self.parts[usize::from(cell)];
        let mut batch = part.offers();
        if !batch.room() {
            return false;
        }
        let file = meta.id;
        let offered = batch.offer(pos, NsId(ns), JobKind::Retired, meta, handle).is_ok();
        if batch.publish() > 0 {
            self.board.bell().ring(&self.wake);
        }
        if offered {
            self.offered.push((cell, ns, file));
        }
        offered
    }

    /// Every ready result of `cell`, consumed; returns how many.
    fn consume(&mut self, cell: u16) -> usize {
        let consumed = &mut self.consumed;
        let (taken, stale) = self.parts[usize::from(cell)].consume(|c| {
            consumed.push(((cell, c.ns.0, c.meta.id), c.outcome));
        });
        assert_eq!(stale, 0, "every result names an Out position");
        taken
    }

    /// The worker's iterations, as the control loop runs them, until two in
    /// a row run no job: every published job is served.
    fn serve<F: SegmentFs<File = J>>(&mut self, fs: &F) {
        let mut idle = 0;
        for _ in 0..1_000 {
            let before = self.board.worker_jobs();
            self.tier.iterate(fs);
            idle = if self.board.worker_jobs() == before { idle + 1 } else { 0 };
            if idle == 2 {
                return;
            }
        }
        panic!("the worker's iterations never ran dry");
    }

    /// The census: every offered owner consumed exactly once, by identity.
    fn assert_census(&self, at: &str) {
        let consumed: Vec<Key> = self.consumed.iter().map(|(key, _)| *key).collect();
        let unique: BTreeSet<Key> = consumed.iter().copied().collect();
        assert_eq!(unique.len(), consumed.len(), "{at}: an owner consumed twice: {consumed:?}");
        let offered: BTreeSet<Key> = self.offered.iter().copied().collect();
        assert!(offered.len() == self.offered.len(), "{at}: the test offered one identity twice");
        let missing: Vec<&Key> = offered.difference(&unique).collect();
        assert!(missing.is_empty(), "OWNER_DROPPED: {at}: offered, never consumed: {missing:?}");
    }
}

impl<J: SegmentFile + FinalClose> Drop for Rig<J> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        for part in &mut self.parts {
            self.tier.cut(part);
        }
    }
}

fn meta(id: u32, path: PathBuf) -> TierFileMeta {
    TierFileMeta { id, base: LogicalAddr::ZERO, data_len: 0, reason: SealReason::Capacity, path }
}

fn mem_path(id: u32) -> PathBuf {
    Path::new("/shard-0/ns-16/cold").join(inf_log::tier_file_name(id))
}

/// The transport's population: at 1, 2, 8 and 64 cells, eight jobs a
/// cell fill its positions and a ninth is deferred with nothing built;
/// after the worker took every job and published every result, unconsumed,
/// a ninth is deferred again (a dequeue frees nothing); one consumed result
/// admits exactly one owner; at the end every owner was consumed once, the
/// same file numbers on every cell under different namespaces.
#[test]
fn a_position_frees_only_at_its_results_consumption() {
    for cells in [1u16, 2, 8, 64] {
        let fs = MemFs::new();
        let mut rig = Rig::<MemFile>::new(cells);
        for cell in 0..cells {
            let ns = 16 + u32::from(cell);
            for id in 0..8 {
                assert!(rig.offer(cell, ns, meta(id, mem_path(id)), None), "{cells}: job {id}");
            }
            assert!(!rig.offer(cell, ns, meta(8, mem_path(8)), None), "{cells}: a 9th is deferred");
            assert_eq!(rig.parts[usize::from(cell)].jobs_out(), 8);
        }
        rig.serve(&fs);
        for cell in 0..cells {
            let ninth = rig.offer(cell, 16 + u32::from(cell), meta(8, mem_path(8)), None);
            assert!(!ninth, "NINTH_ADMITTED: {cells} cells: eight results unconsumed on {cell}");
        }
        for cell in 0..cells {
            let ns = 16 + u32::from(cell);
            let mut one = 0;
            let consumed = &mut rig.consumed;
            rig.parts[usize::from(cell)].consume(|c| {
                consumed.push(((cell, c.ns.0, c.meta.id), c.outcome));
                one += 1;
            });
            assert_eq!(one, 8, "every result was published");
            assert!(rig.offer(cell, ns, meta(8, mem_path(8)), None), "a consumed result frees one");
        }
        rig.serve(&fs);
        for cell in 0..cells {
            assert_eq!(rig.consume(cell), 1);
        }
        rig.assert_census(&format!("{cells} cells"));
        assert!(rig.consumed.iter().all(|(_, outcome)| *outcome == RetireOutcome::AlreadyAbsent));
    }
}

/// The transport's reuse: twenty full rounds on one cell, each with fresh file ids,
/// every identity consumed once and matched against the round's own set.
#[test]
fn twenty_rounds_of_reuse_keep_every_identity() {
    let fs = MemFs::new();
    let mut rig = Rig::<MemFile>::new(1);
    for round in 0..20u32 {
        let ids: Vec<u32> = (0..8).map(|i| round * 8 + i).collect();
        for &id in &ids {
            assert!(rig.offer(0, 16, meta(id, mem_path(id)), None), "round {round}: {id}");
        }
        rig.serve(&fs);
        let before = rig.consumed.len();
        assert_eq!(rig.consume(0), 8);
        let got: Vec<u32> = rig.consumed[before..].iter().map(|((_, _, id), _)| *id).collect();
        assert_eq!(got, ids, "round {round}: the results return in order, by identity");
    }
    rig.assert_census("reuse");
}

/// Each outcome over the in-memory tier: a present file is `Unlinked`,
/// an absent one `AlreadyAbsent`, an injected unlink failure `Retry` with no
/// OS errno (the operator's class), and that owner, offered again with no
/// handle, is unlinked: no owner is dropped unconsumed.
#[test]
fn each_outcome_returns_its_owner() {
    let fs = MemFs::new();
    fs.create_dir_all(Path::new("/shard-0/ns-16/cold")).expect("dirs");
    for id in [0, 1] {
        drop(fs.create_tier(&mem_path(id), TierIoMode::Buffered).expect("a tier file"));
    }
    let mut rig = Rig::<MemFile>::new(1);
    assert!(rig.offer(0, 16, meta(0, mem_path(0)), None));
    assert!(rig.offer(0, 16, meta(1, mem_path(1)), None));
    assert!(rig.offer(0, 16, meta(2, mem_path(2)), None));
    fault::arm(inf_log::fault::TIER_UNLINK_FAIL, FaultSpec::Nth(1));
    rig.serve(&fs);
    let fired = fault::fired(inf_log::fault::TIER_UNLINK_FAIL);
    fault::disarm_all();
    assert_eq!(fired, 1, "engagement: the first job's unlink failed");
    let taken = rig.consume(0);
    rig.assert_census("outcomes");
    assert_eq!(taken, 3);
    let outcomes: Vec<RetireOutcome> = rig.consumed.iter().map(|(_, o)| *o).collect();
    let RetireOutcome::Retry(cause) = outcomes[0] else { panic!("Retry first: {outcomes:?}") };
    assert!(matches!(cause, RetryCause::Unlink { errno: None, .. }), "{cause:?}");
    assert!(!cause.transient(), "an injected point is the operator's class");
    assert_eq!(&outcomes[1..], [RetireOutcome::Unlinked, RetireOutcome::AlreadyAbsent]);
    assert!(fs.open_read(&mem_path(0)).is_ok(), "a failed unlink left the file");
    rig.offered.clear();
    rig.consumed.clear();
    assert!(rig.offer(0, 16, meta(0, mem_path(0)), None), "the retried owner, no handle");
    rig.serve(&fs);
    assert_eq!(rig.consume(0), 1);
    assert_eq!(rig.consumed[0].1, RetireOutcome::Unlinked);
    assert!(fs.open_read(&mem_path(0)).is_err(), "the retry unlinked it");
}

/// A close error on the simulated disk: the handle's final close
/// reports `tier_close_fail`'s `EIO` and no unlink is attempted; the retry,
/// with no handle, unlinks and closes nothing.
#[test]
fn a_failed_close_is_retried_by_its_unlink_alone() {
    let disk = SimDisk::new();
    let cold = Path::new("shard-0/ns-16/cold");
    disk.create_dir_all(cold).expect("dirs");
    let path = cold.join(inf_log::tier_file_name(0));
    let file = disk.create_tier(&path, TierIoMode::Buffered).expect("a tier file");
    let mut rig = Rig::<SimFile>::new(1);
    assert!(rig.offer(0, 16, meta(0, path.clone()), Some(file)));
    fault::arm(inf_log::fault::TIER_CLOSE_FAIL, FaultSpec::Nth(1));
    rig.serve(&disk);
    let fired = fault::fired(inf_log::fault::TIER_CLOSE_FAIL);
    fault::disarm_all();
    assert_eq!(fired, 1, "engagement: the close failed");
    assert_eq!(rig.consume(0), 1);
    let RetireOutcome::Retry(cause) = rig.consumed[0].1 else { panic!("a Retry") };
    assert!(matches!(cause, RetryCause::Close { errno: Some(5), .. }), "EIO: {cause:?}");
    assert!(disk.open_read(&path).is_ok(), "no unlink after a failed close");
    rig.offered.clear();
    rig.consumed.clear();
    assert!(rig.offer(0, 16, meta(0, path.clone()), None));
    rig.serve(&disk);
    assert_eq!(rig.consume(0), 1);
    assert_eq!(rig.consumed[0].1, RetireOutcome::Unlinked);
    let closes = disk
        .logged_calls(0)
        .iter()
        .filter(|call| call.call == inf_log::fs::sim::DiskCall::FinalClose)
        .count();
    assert_eq!(closes, 1, "the retry closed nothing");
}

/// The stop: jobs queued, in the worker's hand and with results
/// unconsumed when the stop begins. Offers end; results are consumed each
/// turn; the cell releases its endpoints only once every position is Free,
/// the worker releases its lane on reading `closing`, and the backings
/// retire a block a step: Retired only after every owner was consumed. The
/// worker sits out the first two turns, so the release is tried with jobs
/// out.
#[test]
fn the_stop_releases_only_with_every_position_free() {
    let fs = MemFs::new();
    let mut rig = Rig::<MemFile>::new(1);
    for id in 0..4 {
        assert!(rig.offer(0, 16, meta(id, mem_path(id)), None));
    }
    rig.serve(&fs);
    for id in 4..8 {
        assert!(rig.offer(0, 16, meta(id, mem_path(id)), None));
    }
    let board = Arc::clone(&rig.board);
    let mut phases = Vec::new();
    let mut tried_with_jobs_out = false;
    for turn in 0..64 {
        rig.consume(0);
        let closing = rig.parts[0].phase() == TransportPhase::Closing;
        tried_with_jobs_out |= closing && rig.parts[0].jobs_out() > 0;
        let phase = rig.parts[0].stop_step(&board, &rig.wake);
        if phase == TransportPhase::Released || phase == TransportPhase::Joined {
            assert_eq!(
                rig.consumed.len(),
                rig.offered.len(),
                "OWNER_UNCONSUMED: the endpoints were released with a result unconsumed"
            );
        }
        phases.push(phase);
        if phase == TransportPhase::Retired {
            break;
        }
        assert!(!rig.offer(0, 16, meta(99, mem_path(99)), None), "no offer once stopping");
        if turn >= 2 {
            rig.tier.iterate(&fs);
        }
    }
    assert!(tried_with_jobs_out, "VACUOUS: the release was never tried with a job out");
    assert_eq!(phases.last(), Some(&TransportPhase::Retired), "the stop's rows end: {phases:?}");
    assert!(phases.contains(&TransportPhase::Closing));
    rig.assert_census("stop");
    assert_eq!(rig.parts[0].jobs_out(), 0);
}

/// A real file whose final close closes its descriptor, then reports
/// `EIO` (Linux releases the number even on `EIO`): the close error over a
/// native descriptor.
struct FailingClose(StdSegmentFile);

impl FinalClose for FailingClose {
    fn final_close(self) -> io::Result<()> {
        self.0.final_close()?;
        Err(io::Error::from_raw_os_error(5))
    }
}

impl SegmentFile for FailingClose {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.0.write_at(offset, data)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read_at(offset, buf)
    }
    fn file_size(&self) -> io::Result<u64> {
        self.0.file_size()
    }
    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.0.truncate(len)
    }
    fn sync_data(&mut self) -> io::Result<()> {
        self.0.sync_data()
    }
    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        self.0.raw_fd()
    }
    fn fully_allocated(&self) -> io::Result<bool> {
        self.0.fully_allocated()
    }
}

/// The real filesystem, its files [`FailingClose`].
#[derive(Clone, Copy)]
struct FailingCloseFs;

impl SegmentFs for FailingCloseFs {
    type File = FailingClose;
    type DirStream = <StdSegmentFs as SegmentFs>::DirStream;

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        StdSegmentFs.create_dir_all(dir)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        StdSegmentFs.sync_dir(dir)
    }
    fn open_listing(&self, dir: &Path) -> io::Result<inf_log::fs::OpenedDir<Self::DirStream>> {
        StdSegmentFs.open_listing(dir)
    }
    fn create_segment(&self, path: &Path, prealloc_bytes: u64) -> io::Result<FailingClose> {
        StdSegmentFs.create_segment(path, prealloc_bytes).map(FailingClose)
    }
    fn create_meta(&self, path: &Path) -> io::Result<FailingClose> {
        StdSegmentFs.create_meta(path).map(FailingClose)
    }
    fn create_meta_direct(&self, path: &Path) -> io::Result<FailingClose> {
        StdSegmentFs.create_meta_direct(path).map(FailingClose)
    }
    fn open_dir(&self, dir: &Path) -> io::Result<FailingClose> {
        StdSegmentFs.open_dir(dir).map(FailingClose)
    }
    fn open_write(&self, path: &Path) -> io::Result<FailingClose> {
        StdSegmentFs.open_write(path).map(FailingClose)
    }
    fn open_read(&self, path: &Path) -> io::Result<FailingClose> {
        StdSegmentFs.open_read(path).map(FailingClose)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        StdSegmentFs.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        StdSegmentFs.remove_file(path)
    }
}

/// A fresh scratch directory of the test's own.
#[allow(clippy::disallowed_methods, reason = "test-only: a real filesystem fixture")]
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "inf-tier-retire-{name}-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    StdSegmentFs.create_dir_all(&dir).expect("scratch dir");
    dir
}

/// On Linux, the native close: the job's descriptor is closed by its
/// final close (its `/proc/self/fd` entry gone or naming another file);
/// after a close that failed, released as Linux releases it, a sentinel
/// opened onto the reused number stays open through the retry, which
/// unlinks with no handle and closes nothing.
#[test]
#[cfg(target_os = "linux")]
#[cfg_attr(miri, ignore = "the test spawns a process, which Miri does not")]
#[allow(clippy::disallowed_methods, reason = "test-only: /proc, a child and a real filesystem")]
fn a_retry_after_a_close_error_never_closes_the_number_again() {
    // The leg runs alone in a child of this binary, so no other test's
    // open takes the number the failed close released.
    const CHILD: &str = "INF_TIER_RETIRE_SENTINEL_CHILD";
    if std::env::var(CHILD).is_ok() {
        sentinel_leg();
        return;
    }
    // The test's name from its module's path, so a moved module moves it.
    let (_, module) = module_path!().split_once("::").expect("a crate-rooted module path");
    let name = format!("{module}::a_retry_after_a_close_error_never_closes_the_number_again");
    let exe = std::env::current_exe().expect("the test binary's path");
    let output = std::process::Command::new(exe)
        .args(["--exact", name.as_str(), "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .expect("the child test runs");
    let text = String::from_utf8_lossy(&output.stderr).into_owned()
        + &String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "the sentinel leg failed in its child: {text}");
    assert!(text.contains("1 passed"), "VACUOUS: the child ran no sentinel leg: {text}");
}

/// The sentinel leg itself, alone in its process.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods, reason = "test-only: /proc and a real filesystem")]
fn sentinel_leg() {
    let dir = scratch("close");
    let path = dir.join(inf_log::tier_file_name(0));
    let file = FailingCloseFs.create_tier(&path, TierIoMode::Buffered).expect("a tier file");
    let number = file.raw_fd().expect("a real descriptor");
    let mut rig = Rig::<FailingClose>::new(1);
    assert!(rig.offer(0, 16, meta(0, path.clone()), Some(file)));
    rig.serve(&FailingCloseFs);
    assert_eq!(rig.consume(0), 1);
    assert!(matches!(rig.consumed[0].1, RetireOutcome::Retry(RetryCause::Close { .. })));
    let entry = format!("/proc/self/fd/{number}");
    assert_ne!(std::fs::read_link(&entry).ok().as_deref(), Some(path.as_path()), "closed");
    let sentinel_path = dir.join("sentinel");
    let sentinel = std::fs::File::create(&sentinel_path).expect("the sentinel");
    let reused = std::os::fd::AsRawFd::as_raw_fd(&sentinel);
    rig.offered.clear();
    rig.consumed.clear();
    assert!(rig.offer(0, 16, meta(0, path.clone()), None), "the retry, no handle");
    rig.serve(&FailingCloseFs);
    assert_eq!(rig.consume(0), 1);
    assert_eq!(rig.consumed[0].1, RetireOutcome::Unlinked);
    if reused == number {
        let held = std::fs::read_link(format!("/proc/self/fd/{reused}")).ok();
        assert_eq!(
            held.as_deref(),
            Some(sentinel_path.as_path()),
            "SENTINEL_CLOSED: the retry closed fd {number} again, which the sentinel reused"
        );
    }
    assert!(reused == number, "engagement: the sentinel reused the released number {number}");
    drop(sentinel);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The control thread's receive side in the threaded leg: a real channel
/// and the one-slot wake, each wait bounded by a failure deadline.
struct DeadlineInbox {
    rx: mpsc::Receiver<ControlMsg>,
    wake: mpsc::Receiver<()>,
    stop: Arc<AtomicBool>,
}

impl WorkerInbox for DeadlineInbox {
    type Msg = ControlMsg;

    fn try_recv(&mut self) -> Option<ControlMsg> {
        self.rx.try_recv().ok()
    }

    fn wait(&mut self) {
        let woke = self.wake.recv_timeout(std::time::Duration::from_secs(5));
        assert!(
            woke.is_ok() || self.stop.load(Ordering::Acquire),
            "WORKER_WAKE_MISSING: the worker waited 5 s with nothing to wake it"
        );
    }

    fn disconnected(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

/// The threaded leg (Linux): the worker on a thread of its own, the cell's
/// thread parked on its loop waker with no timer until its results arrive:
/// it announces its park, fences, checks its result ring, and only then
/// waits on its eventfd, each wait under a 5 s failure deadline. Two rounds
/// of eight jobs, every result consumed, then the stop retires the part.
#[test]
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods, reason = "test-only: real threads and a real filesystem")]
fn a_parked_cell_is_woken_by_its_results() {
    use std::io::Read as _;
    let dir = scratch("threaded");
    let TierBoot { shared, writer, worker, mut parts } =
        TierLifetimeBoot::reserve::<StdSegmentFile>(1).expect("the reservation");
    let mut part = parts.pop().expect("one part");
    let (control_tx, control_rx) = mpsc::sync_channel(256);
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    let wake = RetireWake::over(wake_tx, &control_tx);
    let (watch, waker) = inf_runtime::net::wake_pair().expect("an eventfd");
    let mut watch = std::fs::File::from(watch);
    let flags = Arc::new(vec![AtomicBool::new(false)]);
    let mut worker = worker;
    let waker_for_worker = waker.clone();
    worker.set_cell_wake(CellWake::new(Arc::clone(&flags), move |_| waker_for_worker.wake()));
    let stop = Arc::new(AtomicBool::new(false));
    let mut inbox = DeadlineInbox { rx: control_rx, wake: wake_rx, stop: Arc::clone(&stop) };
    let worker_thread = std::thread::spawn(move || {
        let mut worker = worker;
        while !worker.iterate(&mut inbox, &StdSegmentFs, drop) {}
        worker
    });
    let pos = writer.positions().next().expect("position 0");
    let mut consumed = 0usize;
    for round in 0..2u32 {
        let mut batch = part.offers();
        for i in 0..8 {
            let id = round * 8 + i;
            let path = dir.join(inf_log::tier_file_name(id));
            let file = StdSegmentFs.create_tier(&path, TierIoMode::Buffered).expect("a file");
            assert!(
                batch.offer(pos, NsId(16), JobKind::Retired, meta(id, path), Some(file)).is_ok()
            );
        }
        assert_eq!(batch.publish(), 8);
        shared.board.bell().ring(&wake);
        let want = (round as usize + 1) * 8;
        while consumed < want {
            flags[0].store(true, Ordering::Relaxed);
            fence(Ordering::SeqCst);
            if !part.results_ready() {
                // Parked: only the waker's write ends it (the eventfd is
                // non-blocking, so the park reads it under its deadline).
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let mut count = [0u8; 8];
                while watch.read(&mut count).is_err() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "RESULT_WAKE_MISSING: the parked cell slept 5 s past its results"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            flags[0].store(false, Ordering::Relaxed);
            let (taken, stale) = part.consume(|c| {
                assert!(matches!(c.outcome, RetireOutcome::Unlinked), "{:?}", c.outcome);
            });
            assert_eq!(stale, 0);
            consumed += taken;
        }
    }
    let mut phase = part.stop_step(&shared.board, &wake);
    for _ in 0..5_000 {
        if phase == TransportPhase::Retired {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
        phase = part.stop_step(&shared.board, &wake);
    }
    assert_eq!(phase, TransportPhase::Retired, "the stop joined the worker's lane");
    stop.store(true, Ordering::Release);
    drop(control_tx);
    shared.board.bell().ring(&wake);
    let _ = worker_thread.join().expect("the worker thread");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the worker --------------------------------------------------------

/// The worker legs' inbox: the real control channel and the one-slot wake, the
/// iteration count, and the wait's record. A wait with the wake's slot
/// empty while the test's own bookkeeping holds a published job or a queued
/// message is a wait nothing would end.
struct RecordingInbox {
    rx: mpsc::Receiver<ControlMsg>,
    wake: mpsc::Receiver<()>,
    pending_jobs: usize,
    queued: usize,
    waits: usize,
}

impl WorkerInbox for RecordingInbox {
    type Msg = ControlMsg;

    fn try_recv(&mut self) -> Option<ControlMsg> {
        let msg = self.rx.try_recv().ok();
        if matches!(msg, Some(ControlMsg::Unlink(_))) {
            self.queued -= 1;
        }
        msg
    }

    fn wait(&mut self) {
        self.waits += 1;
        if self.wake.try_recv().is_err() {
            assert!(
                self.pending_jobs == 0 && self.queued == 0,
                "WAIT_UNWOKEN: a wait with {} jobs published and {} messages queued and no wake",
                self.pending_jobs,
                self.queued
            );
        }
    }

    fn disconnected(&self) -> bool {
        false
    }
}

/// A worker rig of `lanes` lanes, its offers published and rung the
/// way a cell's turn does.
struct WorkerRig {
    rig: Rig<MemFile>,
    inbox: RecordingInbox,
    sender: ControlSender,
    fs: LaneFaultFs,
    /// Jobs taken by the worker so far, read from the consumed results.
    served: Vec<(u16, usize)>,
    /// The messages handled, in the order the worker handled them.
    handled: Vec<PathBuf>,
    next_id: u32,
}

/// The in-memory tier whose unlinks meet `tier_unlink_fail` only under
/// `failing`: one lane's files fail as the armed point says, every other
/// lane's are removed as if nothing were armed.
#[derive(Clone)]
struct LaneFaultFs {
    inner: MemFs,
    failing: Option<PathBuf>,
}

impl SegmentFs for LaneFaultFs {
    type File = MemFile;
    type DirStream = <MemFs as SegmentFs>::DirStream;

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        self.inner.create_dir_all(dir)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.inner.sync_dir(dir)
    }
    fn open_listing(&self, dir: &Path) -> io::Result<inf_log::fs::OpenedDir<Self::DirStream>> {
        self.inner.open_listing(dir)
    }
    fn create_segment(&self, path: &Path, prealloc_bytes: u64) -> io::Result<MemFile> {
        self.inner.create_segment(path, prealloc_bytes)
    }
    fn create_meta(&self, path: &Path) -> io::Result<MemFile> {
        self.inner.create_meta(path)
    }
    fn create_meta_direct(&self, path: &Path) -> io::Result<MemFile> {
        self.inner.create_meta_direct(path)
    }
    fn open_dir(&self, dir: &Path) -> io::Result<MemFile> {
        self.inner.open_dir(dir)
    }
    fn open_write(&self, path: &Path) -> io::Result<MemFile> {
        self.inner.open_write(path)
    }
    fn open_read(&self, path: &Path) -> io::Result<MemFile> {
        self.inner.open_read(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn unlink_tier(&self, path: &Path) -> io::Result<()> {
        match &self.failing {
            Some(dir) if !path.starts_with(dir) => self.inner.remove_file(path),
            _ => self.inner.unlink_tier(path),
        }
    }
}

impl WorkerRig {
    fn new(lanes: u16) -> WorkerRig {
        let mut rig = Rig::<MemFile>::new(lanes);
        let (control_tx, control_rx) = mpsc::sync_channel(256);
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        rig.wake = RetireWake::over(wake_tx, &control_tx);
        let sender = ControlSender::over(control_tx, rig.wake.clone());
        let inbox =
            RecordingInbox { rx: control_rx, wake: wake_rx, pending_jobs: 0, queued: 0, waits: 0 };
        let fs = LaneFaultFs { inner: MemFs::new(), failing: None };
        WorkerRig { rig, inbox, sender, fs, served: Vec::new(), handled: Vec::new(), next_id: 0 }
    }

    fn publish(&mut self, lane: u16) {
        let id = self.next_id;
        self.publish_at(lane, mem_path(id));
    }

    /// One job on `lane` for the file at `path`.
    fn publish_at(&mut self, lane: u16, path: PathBuf) {
        let id = self.next_id;
        self.next_id += 1;
        assert!(self.rig.offer(lane, 16, meta(id, path), None), "lane {lane} has room");
        self.inbox.pending_jobs += 1;
    }

    fn post(&mut self) {
        self.post_path(PathBuf::from("/nowhere"));
    }

    fn post_path(&mut self, path: PathBuf) {
        self.inbox.queued += 1;
        self.sender.post(ControlMsg::Unlink(path));
    }

    /// One iteration; returns the messages it handled.
    fn iterate(&mut self, iteration: usize) -> usize {
        let mut handled = 0;
        let Self { rig, inbox, fs, served, handled: order, .. } = self;
        let _ = rig.tier.worker().iterate(inbox, fs, |msg| {
            if let ControlMsg::Unlink(path) = msg {
                handled += 1;
                order.push(path);
            }
        });
        for lane in 0..rig.parts.len() as u16 {
            let taken = rig.consume(lane);
            inbox.pending_jobs -= taken;
            for _ in 0..taken {
                served.push((lane, iteration));
            }
        }
        handled
    }
}

/// Last-only readiness at 65 lanes. A job in the last lane, published and
/// rung before the clear while the worker's circuit since its last job is
/// still open, is served with no wait at all. After a whole empty circuit
/// the worker waits; a job then published and rung ends that wait at once
/// (the bell's wake) and is served. Then the worker waits within
/// ⌈65 / 64⌉ + 1 iterations of its last job.
#[test]
fn a_last_lanes_job_is_served_before_any_wait() {
    let mut w = WorkerRig::new(65);
    w.publish(0);
    let first = (0..4).find(|&i| {
        w.iterate(i);
        !w.served.is_empty()
    });
    assert!(first.is_some(), "lane 0's job was served");
    w.served.clear();
    let waits_before = w.inbox.waits;
    w.publish(64);
    let served_at = (0..8).find(|&i| {
        w.iterate(100 + i);
        !w.served.is_empty()
    });
    assert!(served_at.is_some(), "the 65th lane's job was served");
    assert_eq!(w.served[0].0, 64);
    assert_eq!(
        w.inbox.waits, waits_before,
        "WAIT_BEFORE_SERVE: the worker waited with the last lane's job published and rung"
    );
    w.served.clear();
    for iteration in 0..4 {
        w.iterate(200 + iteration);
    }
    assert!(w.inbox.waits > waits_before, "VACUOUS: no wait after a whole empty circuit");
    w.publish(64);
    let served_at = (0..8).find(|&i| {
        w.iterate(300 + i);
        !w.served.is_empty()
    });
    assert!(served_at.is_some(), "the job published during a wait was served");
    let mut waited = false;
    for i in 0..(65usize.div_ceil(64) + 1) {
        let before = w.inbox.waits;
        w.iterate(400 + i);
        waited |= w.inbox.waits > before;
    }
    assert!(waited, "NO_WAIT: the worker did not wait within 3 iterations of its last job");
}

/// Continuous readiness at 1, 2, 8, 64 and 65 lanes: every lane keeps a
/// job ready, and each lane is served within ⌈lanes / 8⌉ passes of the
/// worker's cursor reaching it, so no lane waits behind the earlier ones.
#[test]
fn every_ready_lane_is_served_within_its_share_of_passes() {
    for lanes in [1u16, 2, 8, 64, 65] {
        let mut w = WorkerRig::new(lanes);
        for lane in 0..lanes {
            w.publish(lane);
        }
        let bound = usize::from(lanes).div_ceil(8);
        let mut last_served = vec![None; usize::from(lanes)];
        for iteration in 0..(4 * bound + 4) {
            w.iterate(iteration);
            for &(lane, at) in &w.served {
                last_served[usize::from(lane)] = Some(at);
            }
            let served: Vec<(u16, usize)> = w.served.drain(..).collect();
            for (lane, _) in served {
                w.publish(lane);
            }
        }
        for (lane, at) in last_served.iter().enumerate() {
            let at = at.unwrap_or_else(|| panic!("LANE_UNSERVED: {lanes} lanes: lane {lane}"));
            assert!(at + bound + 1 >= 4 * bound + 3, "LANE_STARVED: {lanes} lanes: {lane}");
        }
    }
}

/// A steady sender under continuous readiness: the n-th message queued
/// is handled within n iterations of its post, the bell's rings taking no
/// message's turn.
#[test]
fn a_message_waits_no_turn_behind_the_bell() {
    let mut w = WorkerRig::new(8);
    for lane in 0..8 {
        w.publish(lane);
    }
    let mut ahead = 0usize;
    for iteration in 0..64 {
        w.post();
        ahead += 1;
        let handled = w.iterate(iteration);
        ahead -= handled;
        assert!(
            ahead <= 1,
            "MESSAGE_BEHIND_WAKE: {ahead} messages queued after iteration {iteration}"
        );
        let served: Vec<(u16, usize)> = w.served.drain(..).collect();
        for (lane, _) in served {
            w.publish(lane);
        }
    }
}

/// Empty readiness: with nothing published and nothing queued the
/// worker waits within ⌈lanes / 64⌉ + 1 iterations, at 1, 64 and 65 lanes.
#[test]
fn an_idle_worker_waits_after_one_empty_circuit() {
    for lanes in [1u16, 64, 65] {
        let mut w = WorkerRig::new(lanes);
        let within = usize::from(lanes).div_ceil(64) + 1;
        for iteration in 0..within {
            w.iterate(iteration);
        }
        assert!(w.inbox.waits > 0, "NO_WAIT: {lanes} lanes: no wait within {within} iterations");
    }
}

/// Alternating readiness at 8, 64 and 65 lanes: even lanes publish on even
/// iterations and odd lanes on odd ones, each only once its last job was
/// served; every job is served within ⌈lanes / 8⌉ + 1 iterations of its
/// publish, and no wait is left unwoken.
#[test]
fn alternating_lanes_are_each_served_within_their_share() {
    for lanes in [8u16, 64, 65] {
        let mut w = WorkerRig::new(lanes);
        let bound = usize::from(lanes).div_ceil(8) + 1;
        let mut published_at: Vec<Option<usize>> = vec![None; usize::from(lanes)];
        let mut served = 0usize;
        for iteration in 0..(8 * bound) {
            for lane in 0..lanes {
                let at = &mut published_at[usize::from(lane)];
                if usize::from(lane) % 2 == iteration % 2 && at.is_none() {
                    *at = Some(iteration);
                    w.publish(lane);
                }
            }
            w.iterate(iteration);
            for (lane, at) in w.served.drain(..).collect::<Vec<_>>() {
                let since = published_at[usize::from(lane)].take().expect("a published job");
                assert!(
                    at - since <= bound,
                    "LANE_STARVED: {lanes} lanes: lane {lane} published at {since}, served at {at}"
                );
                served += 1;
            }
        }
        assert!(served >= 4 * usize::from(lanes), "VACUOUS: {lanes} lanes: {served} jobs served");
    }
}

/// A full 256-message channel under continuous readiness at 8 lanes: every
/// message is handled in the order it was queued, the n-th within n
/// iterations, whatever the lanes' publishes ring.
#[test]
fn a_full_channel_is_handled_in_order_within_its_turns() {
    let mut w = WorkerRig::new(8);
    for lane in 0..8 {
        w.publish(lane);
    }
    let queued: Vec<PathBuf> = (0..256).map(|n| PathBuf::from(format!("/m/{n:03}"))).collect();
    for path in &queued {
        w.post_path(path.clone());
    }
    for iteration in 0..256 {
        w.iterate(iteration);
        assert!(
            w.handled.len() > iteration,
            "MESSAGE_BEHIND_WAKE: {} of 256 handled after iteration {iteration}",
            w.handled.len()
        );
        for (lane, _) in w.served.drain(..).collect::<Vec<_>>() {
            w.publish(lane);
        }
    }
    assert_eq!(w.handled, queued, "MESSAGE_ORDER: the backlog was handled out of its order");
}

/// One lane failing every attempt at 8 lanes, every lane ready: lane 0's
/// files lie where `tier_unlink_fail`, armed `Always`, meets each of its
/// unlinks, so each of its jobs returns `Retry` and is offered again,
/// holding at most its own 8 positions; every other lane's job is served
/// within ⌈8 / 8⌉ + 1 iterations of its publish, each terminal.
#[test]
fn a_lane_failing_every_attempt_holds_only_its_own_positions() {
    let mut w = WorkerRig::new(8);
    let failing = PathBuf::from("/failing");
    w.fs.failing = Some(failing.clone());
    fault::arm(inf_log::fault::TIER_UNLINK_FAIL, FaultSpec::Always);
    let bound = 2;
    for n in 0..8 {
        w.publish_at(0, failing.join(format!("f{n}")));
    }
    let mut published_at = [Some(0usize); 8];
    for lane in 1..8 {
        w.publish(lane);
    }
    let mut seen = 0usize;
    let mut retried = 0usize;
    for iteration in 0..64 {
        w.iterate(iteration);
        assert!(w.rig.parts[0].jobs_out() <= 8, "the failing lane holds only its own positions");
        for &((cell, _, _), outcome) in &w.rig.consumed[seen..] {
            if cell == 0 {
                assert!(
                    matches!(outcome, RetireOutcome::Retry(RetryCause::Unlink { .. })),
                    "the failing lane's attempt answered {outcome:?}"
                );
                retried += 1;
            } else {
                assert!(
                    matches!(outcome, RetireOutcome::Unlinked | RetireOutcome::AlreadyAbsent),
                    "LANE_FAILED: lane {cell}'s job answered {outcome:?}"
                );
            }
        }
        seen = w.rig.consumed.len();
        for (lane, at) in w.served.drain(..).collect::<Vec<_>>() {
            if lane == 0 {
                let id = w.next_id;
                w.publish_at(0, failing.join(format!("f{id}")));
                continue;
            }
            let since = published_at[usize::from(lane)].take().expect("a published job");
            assert!(
                at - since <= bound,
                "LANE_STARVED: lane {lane} published at {since}, served at {at}"
            );
            published_at[usize::from(lane)] = Some(iteration + 1);
            w.publish(lane);
        }
    }
    let fired = fault::fired(inf_log::fault::TIER_UNLINK_FAIL);
    fault::disarm_all();
    assert!(retried >= 64, "VACUOUS: the failing lane's attempts: {retried}");
    assert_eq!(fired as usize, retried, "every failing attempt met the armed point, no other");
}

/// The result wake landing between the cell's final check and its park, on
/// one thread: the cell announces its park, fences, and its check finds no
/// result; the worker then runs the job, publishes its result, fences and
/// reads the announcement, so it wakes the cell, whose park ends at once.
#[test]
fn a_result_published_between_the_check_and_the_park_wakes_the_cell() {
    let mut rig = Rig::<MemFile>::new(1);
    let flags = Arc::new(vec![AtomicBool::new(false)]);
    let woken = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&woken);
    rig.tier.worker().set_cell_wake(CellWake::new(Arc::clone(&flags), move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    assert!(rig.offer(0, 16, meta(0, mem_path(0)), None));
    flags[0].store(true, Ordering::Relaxed);
    fence(Ordering::SeqCst);
    assert!(!rig.parts[0].wake_pending(), "VACUOUS: a result before the worker ran");
    rig.tier.iterate(&MemFs::new());
    assert_eq!(
        woken.load(Ordering::SeqCst),
        1,
        "WAKE_IN_GAP_LOST: the worker published into the gap and woke no cell"
    );
    flags[0].store(false, Ordering::Relaxed);
    assert!(rig.parts[0].wake_pending(), "the result waits for the cell");
    assert_eq!(rig.consume(0), 1);
    rig.assert_census("the gap");
}

/// The lane release landing between a stopping cell's final check and its
/// park, on one thread: Released, the cell announces its park and its
/// check finds the worker's lane still held; the worker then reads
/// `closing`, releases the lane, fences and reads the announcement, so it
/// wakes the cell, and the check now answers that the stop can step.
#[test]
fn a_lane_released_between_the_check_and_the_park_wakes_the_cell() {
    let mut rig = Rig::<MemFile>::new(1);
    let flags = Arc::new(vec![AtomicBool::new(false)]);
    let woken = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&woken);
    rig.tier.worker().set_cell_wake(CellWake::new(Arc::clone(&flags), move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    let board = Arc::clone(&rig.board);
    assert_eq!(rig.parts[0].stop_step(&board, &rig.wake), TransportPhase::Closing);
    assert_eq!(rig.parts[0].stop_step(&board, &rig.wake), TransportPhase::Released);
    flags[0].store(true, Ordering::Relaxed);
    fence(Ordering::SeqCst);
    assert!(!rig.parts[0].wake_pending(), "VACUOUS: the lane released before the worker ran");
    rig.tier.iterate(&MemFs::new());
    assert_eq!(
        woken.load(Ordering::SeqCst),
        1,
        "RELEASE_WAKE_LOST: the worker released the lane into the gap and woke no cell"
    );
    flags[0].store(false, Ordering::Relaxed);
    assert!(rig.parts[0].wake_pending(), "the stop can step: the lane is released");
    for _ in 0..8 {
        if rig.parts[0].stop_step(&board, &rig.wake) == TransportPhase::Retired {
            return;
        }
    }
    panic!("the stop never retired after the lane's release");
}

/// The cut's custody: a part cut with two jobs queued for the worker and
/// three results unconsumed drops each, counted, and retires both rings.
#[test]
fn a_cut_drops_every_queued_value_counted() {
    let fs = MemFs::new();
    let mut rig = Rig::<MemFile>::new(1);
    for id in 0..3 {
        assert!(rig.offer(0, 16, meta(id, mem_path(id)), None));
    }
    rig.serve(&fs);
    for id in 3..5 {
        assert!(rig.offer(0, 16, meta(id, mem_path(id)), None));
    }
    let Rig { tier, parts, .. } = &mut rig;
    let custody = tier.cut(&mut parts[0]);
    assert_eq!(
        custody,
        CutCustody { jobs: 2, results: 3 },
        "CUT_CUSTODY: the cut found other values queued"
    );
    assert_eq!(parts[0].phase(), TransportPhase::Retired);
}
