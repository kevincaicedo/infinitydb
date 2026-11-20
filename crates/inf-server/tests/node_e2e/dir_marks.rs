//! The boot's check on a data directory's previous life. A node's end marks
//! its directory, and the next boot of that directory takes the mark. A
//! crash-equivalent end (`Node::stop` with no drain before it, the kill, a
//! node dropped with its cells running) that leaves a cell's log above its
//! durable watermark marks the directory undurable: an `everysec`
//! acknowledgment promises nothing there. The next boot of the directory
//! then fails `VACUOUS`, unless the test stated before that end, with a
//! reason, that what it reads next does not depend on the loss
//! (`Node::expect_crash_loss`). A drained end marks the directory clean.
//!
//! The types carry the rule. Only the judge makes a [`Judged`] and a `Node`
//! holds one, so every node took its directory's mark at its boot. A cell's
//! reading is its thread's return value, and only [`CellThreads::join`]
//! ends a node's threads, which marks the directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use super::*;

/// How a node's life ended, for the mark it leaves on its directory.
#[derive(Clone, Copy)]
pub(super) enum NodeEnd {
    /// The graceful or synced stop: every cell drained to its final sync.
    Drained,
    /// The cells stopped where they were.
    Crash,
}

/// One durable cell's log after its loop's last iteration.
#[derive(Clone, Copy)]
struct CellTail {
    cell: u16,
    /// The LSN distance queued above the durable watermark, over LSNs
    /// packed `segment << 32 | offset`: a byte count within one segment.
    watermark_lag_lsn: u64,
    /// Records staged and not yet sealed, which the lag omits.
    records_staged: u64,
}

impl CellTail {
    /// An executed effect lies above the fsync watermark. The check reads
    /// the watermark, not the file: in-process the page cache outlives a
    /// crash stop, so a frame written but not fsynced survives it, and a
    /// test relying on that would read red only where a write lagged.
    fn undurable(&self) -> bool {
        self.watermark_lag_lsn > 0 || self.records_staged > 0
    }
}

/// What a cell thread returns when its loop ends.
pub(super) struct CellEnd {
    /// The tiered namespaces the cell ran at the product's
    /// `TAIL-STALL-TIMEOUT` default ([`note_product_tail_stall`]).
    at_default: NsNames,
    /// `None` on a volatile node, and on a cell whose recovery had not
    /// finished: its durable plane is installed when recovery completes.
    tail: Option<CellTail>,
}

impl CellEnd {
    /// Cell `cell`'s end, read from its plane's counters after the loop's
    /// last iteration.
    pub(super) fn read(
        cell: u16,
        stats: Option<inf_server::DurableStats>,
        at_default: NsNames,
    ) -> CellEnd {
        let tail = stats.map(|stats| CellTail {
            cell,
            watermark_lag_lsn: stats.watermark_lag_lsn,
            records_staged: stats.records_staged,
        });
        CellEnd { at_default, tail }
    }
}

/// The mark a node's end leaves on its data directory.
enum DirMark {
    Clean,
    /// A crash-equivalent end over the cells in `tails`, each above its
    /// durable watermark; `stated` is the test's reason that the next life
    /// does not depend on what they held.
    Undurable {
        tails: Vec<CellTail>,
        stated: Option<&'static str>,
    },
}

/// Every data directory's mark, by canonical path, until a boot takes it.
// nextest-in-process: one test a process under nextest; under cargo test, one key a directory
static MARKS: Mutex<BTreeMap<PathBuf, DirMark>> = Mutex::new(BTreeMap::new());

/// The registry, whole after a test panicked holding it: each write is one
/// insert or one remove.
fn marks() -> MutexGuard<'static, BTreeMap<PathBuf, DirMark>> {
    MARKS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One key for every spelling of a directory (`dir`, `dir/../<name>`): its
/// canonical path, else the path as given.
fn dir_key(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// A node's data directory as its boot judged it (`None`: a volatile
/// node). Only [`judge_previous_end`] makes one.
pub(super) struct Judged {
    dir: Option<PathBuf>,
}

impl Judged {
    pub(super) fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
}

/// The boot's check, before anything opens the directory: a directory
/// whose last life ended crash-equivalent with a cell's log above its
/// durable watermark boots only when that life stated the loss. The mark is
/// taken either way, so a later boot of the directory boots.
pub(super) fn judge_previous_end(dir: Option<PathBuf>) -> Judged {
    let mark = dir.as_deref().and_then(|dir| marks().remove(&dir_key(dir)));
    match mark {
        Some(DirMark::Undurable { tails, stated: None }) => panic!("{}", vacuous(&tails)),
        None | Some(DirMark::Clean) | Some(DirMark::Undurable { stated: Some(_), .. }) => {
            Judged { dir }
        }
    }
}

/// The refusal: every undurable cell's reading, and the three ways out.
fn vacuous(tails: &[CellTail]) -> String {
    let cells: Vec<String> = tails
        .iter()
        .map(|tail| {
            format!(
                "cell {}: watermark_lag_lsn {}, records_staged {}",
                tail.cell, tail.watermark_lag_lsn, tail.records_staged
            )
        })
        .collect();
    format!(
        "VACUOUS: the previous life's crash-equivalent stop left its log above the durable \
         watermark ({}), so what it acknowledged may be lost: end that life with stop_synced \
         (it also completes a checkpoint in flight), write through an FSYNC always namespace, \
         or call expect_crash_loss(reason) when the next life reads nothing the loss can change",
        cells.join("; ")
    )
}

/// A node's cell threads. Only [`CellThreads::join`] ends them all, and it
/// marks the node's directory, so no end joins its cells unmarked.
pub(super) struct CellThreads {
    running: Vec<JoinHandle<CellEnd>>,
    /// The ends a refused boot joined early, marked with the rest.
    joined: Vec<CellEnd>,
}

impl CellThreads {
    pub(super) fn new(running: Vec<JoinHandle<CellEnd>>) -> CellThreads {
        CellThreads { running, joined: Vec::new() }
    }

    /// No thread is left running: the node ended.
    pub(super) fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    pub(super) fn any_finished(&self) -> bool {
        self.running.iter().any(JoinHandle::is_finished)
    }

    /// Joins one cell thread that has exited and says why it exited: a
    /// refused boot's fail-stop message. `None` while every thread runs.
    pub(super) fn join_finished(&mut self) -> Option<String> {
        let at = self.running.iter().position(JoinHandle::is_finished)?;
        Some(match self.running.remove(at).join() {
            Ok(end) => {
                self.joined.push(end);
                "a cell thread exited during recovery".to_owned()
            }
            Err(payload) => {
                panic_text(payload.as_ref()).unwrap_or_else(|| "a cell thread panicked".to_owned())
            }
        })
    }

    /// Joins every cell thread (the caller set `stop`), marks `judged`'s
    /// directory with how the life ended and what the test `stated`, and
    /// returns each cell's tiered namespaces at the product's
    /// `TAIL-STALL-TIMEOUT` default: the ends a refused boot joined early
    /// first, then the rest in cell order.
    pub(super) fn join(
        &mut self,
        end: NodeEnd,
        judged: &Judged,
        stated: Option<&'static str>,
    ) -> Vec<NsNames> {
        let mut ends = std::mem::take(&mut self.joined);
        ends.extend(self.running.drain(..).map(|handle| handle.join().expect("cell thread")));
        let mark = mark_of(&ends, end, stated);
        if let Some(dir) = judged.dir() {
            marks().insert(dir_key(dir), mark);
        }
        ends.into_iter().map(|end| end.at_default).collect()
    }
}

/// The mark of an end over its cells' readings. A drained end is clean
/// whatever its tail reads: no connection is left after the drain's
/// all-Quiet barrier, so only a MAINTAIN effect stages after the final sync
/// (ADR-0199 D3), and nothing the life acknowledged lies above the
/// watermark. So a drained end refuses a statement of loss.
fn mark_of(ends: &[CellEnd], end: NodeEnd, stated: Option<&'static str>) -> DirMark {
    match end {
        NodeEnd::Drained => {
            assert!(stated.is_none(), "a stated crash loss on a node that drained: {stated:?}");
            DirMark::Clean
        }
        NodeEnd::Crash => {
            let tails: Vec<CellTail> =
                ends.iter().filter_map(|end| end.tail).filter(CellTail::undurable).collect();
            if tails.is_empty() { DirMark::Clean } else { DirMark::Undurable { tails, stated } }
        }
    }
}

/// A panic's text, when its payload carries one (`panic!` with or without
/// format arguments).
pub(super) fn panic_text(payload: &(dyn std::any::Any + Send)) -> Option<String> {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_owned()))
}

/// The everysec life the check's tests share: a one-cell durable node, an
/// `everysec` namespace, every cell clock held, then one `SET`. A tick due
/// at the held instant still fires once, at the first iteration that reads
/// the held clock, so a `PING` round trip runs that iteration first; after
/// it no tick fires while the clock is held, and no fsync covers the frame.
pub(super) fn crash_loss_life(dir: &Path) -> (Node, TcpStream) {
    let node = Node::start_durable(1, dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"e", b"MODE", b"durable", b"FSYNC", b"everysec"]))
        .expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"e"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    node.clock_gates.iter().for_each(|gate| gate.hold());
    c.write_all(&cmd(&[b"PING"])).expect("write");
    read_exactly(&mut c, b"+PONG\r\n");
    c.write_all(&cmd(&[b"SET", b"k", b"v"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    (node, c)
}

/// Boots `dir` and reads `k` back from namespace `e`.
fn boot_and_serve(dir: &Path) {
    let node = Node::start_durable(1, dir);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"USE", b"e"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"GET", b"k"])).expect("write");
    read_exactly(&mut c, b"$1\r\nv\r\n");
    drop(c);
    node.stop_synced();
}

/// The reading the message prints for `cell`: its `watermark_lag_lsn` and
/// `records_staged`.
fn reading_of(text: &str, cell: u16) -> Option<(u64, u64)> {
    let (_, rest) = text.split_once(&format!("cell {cell}: watermark_lag_lsn "))?;
    let (lag, rest) = rest.split_once(", records_staged ")?;
    let staged = rest.split(|c: char| !c.is_ascii_digit()).next()?;
    Some((lag.parse().ok()?, staged.parse().ok()?))
}

/// The planted violation: the shared life ends crash-equivalent with its
/// frame above the watermark and no statement. The next boot, through
/// another spelling of the directory (`dir/../<name>`, one key), is refused
/// before it opens anything, and the message names cell 0 with a reading
/// that is not zero.
#[test]
fn a_crash_stop_above_the_watermark_fails_the_next_boot_vacuous() {
    let dir = temp_data_dir("crash-loss-plant");
    let (node, c) = crash_loss_life(&dir);
    drop(c);
    node.stop();
    // `Path` compares by components and drops a `.` from them, so `dir/.`
    // equals `dir` with the key taken as given and never tests the canonical
    // key; a `..` stays, so only `canonicalize` maps this spelling to `dir`.
    let name = dir.file_name().expect("a temporary data directory has a name");
    let alias = dir.join("..").join(name);
    let text = match std::panic::catch_unwind(|| Node::start_durable(1, &alias)) {
        Ok(node) => {
            node.stop();
            panic!("the boot after an unstated loss was not refused");
        }
        Err(payload) => panic_text(payload.as_ref()).unwrap_or_default(),
    };
    assert!(text.starts_with("VACUOUS: the previous life's crash-equivalent stop"), "{text}");
    let (lag, staged) = reading_of(&text, 0).unwrap_or_else(|| panic!("no cell 0: {text}"));
    assert!(lag > 0 || staged > 0, "a zero reading refused the boot: {text}");
    std::fs::remove_dir_all(&dir).ok();
}

/// The predicate's control: the same life, the tick covering the write
/// before the crash stop, boots and serves it. `INFO persistence` reports
/// the last MAINTAIN's reading, and an iteration runs MAINTAIN before LOG,
/// so the lag is polled only after a round trip behind the `SET`; then the
/// held clock steps past the next tick, due at most a second after the
/// held instant (each tick arms its successor a second on).
#[test]
fn a_crash_stop_after_the_tick_covered_the_write_boots() {
    let dir = temp_data_dir("crash-loss-ticked");
    let (node, mut c) = crash_loss_life(&dir);
    c.write_all(&cmd(&[b"PING"])).expect("write");
    read_exactly(&mut c, b"+PONG\r\n");
    node.clock_gates.iter().for_each(|gate| gate.step(Duration::from_millis(1001)));
    let text = wait_persistence_field(&mut c, "watermark_lag_lsn", "0");
    assert!(text.contains("watermark_lag_lsn:0\r\n"), "the tick never covered the write: {text}");
    drop(c);
    node.stop();
    boot_and_serve(&dir);
    std::fs::remove_dir_all(&dir).ok();
}

/// The statement's control: the plant's life, its loss stated, boots.
#[test]
fn a_stated_crash_loss_boots() {
    let dir = temp_data_dir("crash-loss-stated");
    let (node, c) = crash_loss_life(&dir);
    drop(c);
    node.expect_crash_loss("the control: the next life reads nothing of the last");
    node.stop();
    let node = Node::start_durable(1, &dir);
    node.stop_synced();
    std::fs::remove_dir_all(&dir).ok();
}

/// The cure's control: the plant's life, ended by the synced stop, boots
/// and serves the write.
#[test]
fn a_synced_stop_over_the_same_write_boots_and_serves_it() {
    let dir = temp_data_dir("crash-loss-synced");
    let (node, c) = crash_loss_life(&dir);
    drop(c);
    node.clock_gates.iter().for_each(|gate| gate.open());
    node.stop_synced();
    boot_and_serve(&dir);
    std::fs::remove_dir_all(&dir).ok();
}

/// A statement needs a directory to lose.
#[test]
fn a_stated_crash_loss_on_a_volatile_node_panics() {
    let node = Node::start(1);
    let stated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        node.expect_crash_loss("a volatile node");
    }));
    let payload = stated.expect_err("a volatile node took a statement");
    let text = panic_text(payload.as_ref()).unwrap_or_default();
    assert!(text.contains("a volatile node has no directory to lose"), "{text}");
    node.stop();
}

/// A drained life loses nothing it acknowledged, so its end refuses a
/// statement.
#[test]
fn a_stated_crash_loss_on_a_drained_node_panics() {
    let dir = temp_data_dir("crash-loss-drained");
    let (node, c) = crash_loss_life(&dir);
    drop(c);
    node.clock_gates.iter().for_each(|gate| gate.open());
    node.expect_crash_loss("a drained node");
    let end = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| node.stop_synced()));
    let payload = end.expect_err("a drained end took a statement");
    let text = panic_text(payload.as_ref()).unwrap_or_default();
    assert!(text.contains("a stated crash loss on a node that drained"), "{text}");
    std::fs::remove_dir_all(&dir).ok();
}
