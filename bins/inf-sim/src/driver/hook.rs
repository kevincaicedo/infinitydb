//! The panic hook that names the running seed.
//!
//! A panic is not caught: it leaves the fault registry and the simulated
//! disk in an unknown state, and a later seed would run on it. Before each
//! run the drivers record the running seed in a thread-local (the
//! simulator is one thread, and the hook runs on the panicking thread).
//! The hook, installed once per process in front of the previous one,
//! writes `inf-sim: seed <seed>: panic: <message>` to stderr and, when a
//! sweep shard writes results, appends `<seed> VIOLATION panic: <message>`
//! to them; the results lines before it were written unbuffered, so its
//! line is the file's last. When the shard had held failing seeds back from
//! stderr, the hook also prints the line that counts them and names the
//! results file. Then the previous hook runs and the process dies as
//! before. On a thread with no recorded seed the hook only chains.
//!
//! Its writes are best-effort: it drops their errors and calls nothing
//! that can panic (`eprintln!` panics when stderr fails, so it writes to
//! the stderr handle), and its message goes through the line writer, which
//! cannot panic.

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::Write as _;
use std::sync::Once;

use super::line::write_text;

thread_local! {
    /// The seed running on this thread.
    static RUNNING: Cell<Option<u64>> = const { Cell::new(None) };
    /// The running shard's results and its stderr bound's state.
    static SHARD: RefCell<Option<HookShard>> = const { RefCell::new(None) };
}

/// What the hook writes of a sweep shard: a handle on its results, their
/// path, and the failing seeds held back from stderr so far.
struct HookShard {
    results: File,
    results_path: String,
    withheld: u64,
}

static HOOK: Once = Once::new();

/// Installs the seed-naming hook once per process, in front of the
/// previous hook, which still runs.
pub(super) fn install() {
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            name_the_seed(info);
            previous(info);
        }));
    });
}

/// Records the running seed until it drops.
pub(super) struct Running(());

impl Running {
    pub(super) fn seed(seed: u64) -> Running {
        RUNNING.with(|running| running.set(Some(seed)));
        Running(())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = RUNNING.try_with(|running| running.set(None));
    }
}

/// Lends the hook a handle on a shard's results until it drops. A handle
/// that cannot be duplicated leaves the hook without the results line; its
/// stderr line still names the seed.
pub(super) struct LentResults(());

impl LentResults {
    pub(super) fn lend(results: &File, results_path: String) -> LentResults {
        let shard = results.try_clone().ok().map(|results| HookShard {
            results,
            results_path,
            withheld: 0,
        });
        SHARD.with(|slot| *slot.borrow_mut() = shard);
        LentResults(())
    }

    /// The shard held one more failing seed back from stderr.
    pub(super) fn withheld_one(&self) {
        SHARD.with(|slot| {
            if let Some(shard) = slot.borrow_mut().as_mut() {
                shard.withheld += 1;
            }
        });
    }
}

impl Drop for LentResults {
    fn drop(&mut self) {
        let _ = SHARD.try_with(|slot| slot.try_borrow_mut().map(|mut slot| *slot = None));
    }
}

/// The hook's own part: on a thread with a running seed, its stderr line,
/// its results line and the held-back count. Best-effort throughout.
fn name_the_seed(info: &std::panic::PanicHookInfo<'_>) {
    let Ok(Some(seed)) = RUNNING.try_with(Cell::get) else {
        return;
    };
    let mut message = String::new();
    write_text(&mut message, info.payload_as_str().unwrap_or("a panic whose payload is not text"));
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "inf-sim: seed {seed:#x}: panic: {message}");
    let _ = SHARD.try_with(|slot| {
        let Ok(slot) = slot.try_borrow() else {
            return;
        };
        let Some(shard) = slot.as_ref() else {
            return;
        };
        let line = format!("{seed:#x} VIOLATION panic: {message}\n");
        let _ = (&shard.results).write_all(line.as_bytes());
        if shard.withheld > 0 {
            let _ = writeln!(
                stderr,
                "inf-sim: {} more failing seeds not printed; every seed's line is in {}",
                shard.withheld, shard.results_path
            );
        }
    });
}
