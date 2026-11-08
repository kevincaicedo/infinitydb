//! The thread marks of the cell executor's wake bridge (ADR-0003 A1.2.3).
//!
//! A standard `Waker` is `Send + Sync`, so a waker operation may run on any thread. A cell's
//! executor tells its own thread from every other by a mark: a `const` thread local, 0 until the
//! thread claims one when it builds an executor, then a process-unique value that is neither 0
//! nor [`MARK_CLOSED`]. The executor's wake headers carry their executor's mark, so comparing it
//! with [`ThreadMark::current`] tells the owner lane from the foreign one.

use core::cell::Cell;

// ---- Thread marks --------------------------------------------------------------------------

/// The owner mark of a closed header: no thread's, since the mark counter never issues it.
pub const MARK_CLOSED: u64 = u64::MAX;

/// The next mark to claim. A `core` atomic under Loom too: a `static` cannot hold Loom's atomics,
/// which are not `const`-constructible, and a mark's uniqueness needs the read-modify-write
/// alone, no ordering (ADR-0003 A1.2.8). Width: marks 1 to `MARK_CLOSED − 1` are issued, one per
/// executor-building thread of the process (INF 584 years at 10⁹ claims a second); past them a
/// claim aborts the process, so no mark is issued twice, and 0 and `MARK_CLOSED` never.
static NEXT_MARK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

std::thread_local! {
    /// This thread's mark: 0 until [`ThreadMark::claim`]. `const` and with no `Drop`, so it is
    /// readable during thread-local destruction: a waker another thread local's destructor drops
    /// reads it as any other does.
    static THREAD_MARK: Cell<u64> = const { Cell::new(0) };
}

/// A thread's mark, claimed by the thread that builds an executor: process-unique, never 0 and
/// never [`MARK_CLOSED`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ThreadMark(u64);

impl ThreadMark {
    /// The calling thread's mark word: 0 on a thread that has claimed none, so it matches no
    /// bridge.
    #[inline(always)]
    #[must_use]
    pub fn current() -> u64 {
        THREAD_MARK.with(Cell::get)
    }

    /// The calling thread's mark, claimed from the process-wide counter on its first call. Two
    /// executors on one thread share it, so each takes the owner lane on that thread.
    #[must_use]
    pub fn claim() -> ThreadMark {
        let held = ThreadMark::current();
        if held != 0 {
            return ThreadMark(held);
        }
        let issued = NEXT_MARK.fetch_update(
            core::sync::atomic::Ordering::Relaxed,
            core::sync::atomic::Ordering::Relaxed,
            |next| (next < MARK_CLOSED).then_some(next + 1),
        );
        // The counter stops at `MARK_CLOSED`: every claim past the last mark aborts.
        let Ok(mark) = issued else { std::process::abort() };
        THREAD_MARK.with(|slot| slot.set(mark));
        ThreadMark(mark)
    }

    /// The mark's value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    //! The thread marks: one per thread, claimed once, never 0 or `MARK_CLOSED`.

    use std::sync::mpsc;

    use super::*;

    #[test]
    fn two_threads_claim_distinct_marks_neither_zero_nor_closed() {
        let here = ThreadMark::claim();
        assert_eq!(ThreadMark::claim(), here, "a thread claims its mark once");
        assert_eq!(ThreadMark::current(), here.get());
        let there = std::thread::spawn(|| {
            assert_eq!(ThreadMark::current(), 0, "a thread that claimed nothing reads 0");
            ThreadMark::claim()
        })
        .join()
        .expect("the other thread claims a mark");
        assert_ne!(here, there, "two threads, two marks");
        for mark in [here, there] {
            assert_ne!(mark.get(), 0, "0 names a thread with no mark");
            assert_ne!(mark.get(), MARK_CLOSED, "MARK_CLOSED is no thread's");
        }
    }

    /// A waker dropped by another thread local's destructor reads the thread's mark: the mark is a
    /// `const` thread local with no `Drop`, so it outlives every destructor of its thread.
    #[test]
    fn a_mark_is_readable_while_thread_locals_are_destroyed() {
        struct ReportOnDrop(Option<mpsc::Sender<u64>>);
        impl Drop for ReportOnDrop {
            fn drop(&mut self) {
                if let Some(to) = self.0.take() {
                    let _ = to.send(ThreadMark::current());
                }
            }
        }
        std::thread_local! {
            static REPORT: Cell<Option<ReportOnDrop>> = const { Cell::new(None) };
        }
        let (to, from) = mpsc::channel();
        let claimed = std::thread::spawn(move || {
            let mark = ThreadMark::claim();
            REPORT.with(|slot| slot.set(Some(ReportOnDrop(Some(to)))));
            mark
        })
        .join()
        .expect("the thread claims a mark and exits");
        let read = from.recv().expect("the destructor reports the mark it read");
        assert_eq!(read, claimed.get(), "the mark read during destruction is the thread's own");
    }
}
