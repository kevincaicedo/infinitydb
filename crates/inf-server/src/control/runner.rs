//! The control thread (ADR-0015 D3): its spawn, its receive side and its
//! loop, in which every iteration is the retirement worker's (ADR-0156 D3):
//! at most one catalog message, the wait on the one-slot wake only after a
//! whole empty circuit of the worker's lanes, then a pass.

use std::path::{Path, PathBuf};
use std::sync::Arc;
// denylist-allow: the control thread's channel and one-slot wake (ADR-0015 D3), received here only.
use std::sync::mpsc;
use std::time::Duration;

use inf_log::fs::SegmentFs;
use inf_store::NsCatalog;

use super::ControlBootRefused;
use super::{
    CatalogWriter, CellRecoverySlot, CellWake, CkptSpace, ControlHandle, ControlMsg, ControlParts,
    DROPPED_NS_MAX, FinalClose, RecoveredResidue, RecoveryBoard, RetireWorker, SpawnedControl,
    WorkerInbox,
};

/// [`spawn`](super::spawn) over an explicit issuance space — the A1.5 headroom seam for
/// tests. Returns the handle, the boot's checkpoint issuers and the cells'
/// parts.
///
/// # Errors
/// As [`spawn`](super::spawn).
pub fn spawn_in<F: SegmentFs + Send + 'static>(
    fs: F,
    data_dir: PathBuf,
    seed: Option<&NsCatalog>,
    space: CkptSpace,
    start_unix_ms: u64,
    cell_wake: Option<CellWake>,
) -> Result<SpawnedControl<F::File>, ControlBootRefused>
where
    F::File: FinalClose + Send,
{
    let cells = space.cells.get();
    let parts = ControlHandle::new_parts::<F::File>(seed, space, start_unix_ms)?;
    let ControlParts { handle, rx, wake, writer, issuers, mut worker, parts } = parts;
    let booked = writer.tombstones.len();
    if booked > DROPPED_NS_MAX {
        eprintln!(
            "control: {booked} drop tombstones at boot, over the cap of {DROPPED_NS_MAX}: a \
             durable CREATE or DROP waits until the book drains below it (ceiling {booked})"
        );
    }
    if let Some(cell_wake) = cell_wake {
        worker.set_cell_wake(cell_wake);
    }
    let board = Arc::clone(&handle.recovery);
    let mut inbox = ThreadInbox { rx, wake, booting: true, gone: false };
    // denylist-allow: the control thread itself (ADR-0015 D3) - the slow plane, never a cell.
    std::thread::Builder::new()
        .name("inf-control".into())
        .spawn(move || {
            // This thread is detached (the JoinHandle drops below): a
            // swallowed unwind would leave cells serving with catalog
            // persistence, checkpoint epochs, and delegated unlinks
            // silently dead — the ADR-0026 D2 class. Same boundary as
            // the cell threads (M2.5-S01/S16): panic ⇒ loud process exit.
            let body = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let mut loop_state = ControlLoop { writer, worker };
                control_main(&fs, &data_dir, &board, &mut inbox, cells, &mut loop_state);
            }));
            if body.is_err() {
                // The default hook already printed the panic.
                eprintln!("infinityd: control thread panicked — fail-stop");
                std::process::exit(101);
            }
        })
        .expect("spawn control thread");
    Ok((handle, issuers, parts))
}

/// The control thread's receive side (ADR-0156 D3): the channel's messages
/// and the worker's one-slot wake, waited on only by the worker's turn,
/// with a timeout while the boot narrates.
struct ThreadInbox {
    rx: mpsc::Receiver<ControlMsg>,
    wake: mpsc::Receiver<()>,
    booting: bool,
    gone: bool,
}

impl WorkerInbox for ThreadInbox {
    type Msg = ControlMsg;

    fn try_recv(&mut self) -> Option<ControlMsg> {
        match self.rx.try_recv() {
            Ok(msg) => Some(msg),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.gone = true;
                None
            }
        }
    }

    fn wait(&mut self) {
        if self.gone {
            return;
        }
        if self.booting {
            // The boot's narration keeps its 100 ms cadence.
            let _ = self.wake.recv_timeout(Duration::from_millis(100));
        } else if self.wake.recv().is_err() {
            self.gone = true;
        }
    }

    fn disconnected(&self) -> bool {
        self.gone
    }
}

/// What the control thread's loop owns: the catalog writer and the
/// retirement worker.
struct ControlLoop<J> {
    writer: CatalogWriter,
    worker: RetireWorker<J>,
}

/// The control thread's message loop + boot narration (M2-S15/M2.5-S01),
/// extracted so [`spawn_in`] can wrap it in the panic fail-stop boundary. Each
/// iteration is the retirement worker's ([`RetireWorker::iterate`]): at
/// most one message, the wait only after a whole empty circuit, a pass.
fn control_main<F: SegmentFs>(
    fs: &F,
    data_dir: &Path,
    board: &Arc<RecoveryBoard>,
    inbox: &mut ThreadInbox,
    cells: u16,
    state: &mut ControlLoop<F::File>,
) where
    F::File: FinalClose,
{
    {
        let ControlLoop { writer, worker } = state;
        // Boot narration (M2-S15): until every cell reports ready, the
        // worker's wait times out at 100 ms and each iteration prints
        // per-cell ready lines + a periodic aggregate progress line
        // (segments/bytes/ETA). Log-line clocks are ambient wall time —
        // control-plane narration only, never oracle input (those ride the
        // injected clocks).
        let mut announced = vec![false; usize::from(cells)];
        #[allow(
            clippy::disallowed_methods,
            reason = "clock: control-thread boot narration, never oracle input (ADR-0015 D3)"
        )]
        // denylist-allow: boot-narration wall clock on the control thread, never oracle input.
        let boot_started = std::time::Instant::now();
        let mut next_progress = boot_started + Duration::from_secs(1);
        while !board.all_ready() {
            if worker.iterate(inbox, fs, |msg| super::handle_msg(writer, data_dir, msg)) {
                return;
            }
            for (cell, seen) in announced.iter_mut().enumerate() {
                if !*seen && board.slot(cell as u16).ready() {
                    *seen = true;
                    announce_ready(board, cell);
                }
            }
            #[allow(
                clippy::disallowed_methods,
                reason = "clock: control-thread boot narration, never oracle input (ADR-0015 D3)"
            )]
            // denylist-allow: boot-narration wall clock on the control thread, never oracle input.
            let now = std::time::Instant::now();
            if now >= next_progress && !board.all_ready() {
                next_progress = now + Duration::from_secs(1);
                narrate_progress(board, cells, boot_started);
            }
        }
        #[allow(
            clippy::disallowed_methods,
            reason = "clock: control-thread boot narration, never oracle input (ADR-0015 D3)"
        )]
        let boot_ms = boot_started.elapsed().as_millis();
        eprintln!(
            "control: recovery complete — {} cells serving ({boot_ms} ms)",
            board.cell_count()
        );
        inbox.booting = false;
        while !worker.iterate(inbox, fs, |msg| super::handle_msg(writer, data_dir, msg)) {}
        // Channel closed = node shutdown; nothing to flush (every
        // acked DDL already persisted before its reply).
    }
}

/// One cell's recovered line (M2-S15): its segments, bytes, records, its
/// torn tail and recycled residue, and its phases' times.
fn announce_ready(board: &RecoveryBoard, cell: usize) {
    let slot = board.slot(cell as u16);
    let (_, total) = slot.bytes();
    let (segs, _) = slot.segments();
    let torn = slot
        .torn_truncated_at()
        .map_or(String::new(), |lsn| format!(", torn tail truncated at {lsn} (M2-S14)"));
    let residue = slot.residue();
    let recycled = if residue == RecoveredResidue::default() {
        String::new()
    } else {
        format!(
            ", recycled residue: {} segment stops, {} slacks (ADR-0090)",
            residue.segment_residue_stops, residue.recycled_residue_slacks
        )
    };
    let (ns, bytes) = slot.phases();
    let ms = |i: usize| ns[i] as f64 / 1e6;
    let unknown = match slot.records_skipped_unknown_ns() {
        0 => String::new(),
        n => format!(
            ", {n} records of untombstoned unknown namespaces skipped \
             (foreign log or corruption — ADR-0103 D4)"
        ),
    };
    eprintln!(
        "control: cell {cell} recovered ({segs} segments, {total} bytes, {} \
         records{torn}{recycled}{unknown}; phases ms: start {:.1}, ckpt {:.1} \
         [{} B], replay {:.1} [{} B], audit {:.1} [{} B], finish {:.1}, \
         total {:.1})",
        slot.records(),
        ms(0),
        ms(1),
        bytes[0],
        ms(2),
        bytes[1],
        ms(3),
        bytes[2],
        ms(4),
        ns.iter().sum::<u64>() as f64 / 1e6,
    );
}

/// The boot's aggregate progress line (segments/bytes/ETA) and, after 5 s,
/// each cell not yet ready with the phase it published.
fn narrate_progress(board: &RecoveryBoard, cells: u16, boot_started: std::time::Instant) {
    let (done, total) = board.bytes();
    #[allow(
        clippy::disallowed_methods,
        reason = "clock: boot narration, never oracle input (ADR-0015 D3)"
    )]
    let elapsed = boot_started.elapsed().as_secs_f64();
    let eta = if done > 0 {
        (elapsed * (total.saturating_sub(done)) as f64 / done as f64).ceil()
    } else {
        0.0
    };
    eprintln!(
        "control: recovery {}/{} cells ready, {done}/{total} bytes ({:.1}%), eta \
             {eta:.0}s",
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
                    "control: cell {cell} not ready — in {} ({done}/{total} bytes) for \
                         {elapsed:.0}s",
                    CellRecoverySlot::phase_name(slot.phase()),
                );
            }
        }
    }
}
