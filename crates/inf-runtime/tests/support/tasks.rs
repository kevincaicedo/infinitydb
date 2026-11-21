//! A test executor of one task class whose future is boxed (ADR-0177 D2: no task without a
//! class): a test that makes tasks of many future types boxes each into the class's one type.
//! Each executor is built on a census of its own, so two of them are two cells' layouts.

#![allow(dead_code, reason = "each test target uses the helpers it needs")]

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_runtime::{
    CellExecutor, ExecutorLayout, ExecutorProgress, PollImmediate, TaskAdmission, TaskClass, TaskId,
};

/// A `run_ready` pass that polled `tasks` tasks and reclaimed no slot.
pub const fn tasks_polled(tasks: usize) -> ExecutorProgress {
    ExecutorProgress { tasks_polled: tasks, slots_reclaimed: 0 }
}

/// A `run_ready` pass that reclaimed `slots` Retired slots and polled no task.
pub const fn slots_reclaimed(slots: usize) -> ExecutorProgress {
    ExecutorProgress { tasks_polled: 0, slots_reclaimed: slots }
}

/// The class's one future type.
pub type Boxed = Pin<Box<dyn Future<Output = ()>>>;

/// Tasks a test executor's class holds at once: each slot and its header are built at the
/// executor's build, so the class stays small enough for Miri to build in every test.
pub const TEST_TASKS: Cap = Cap::entries::<256>("test-tasks", CapFill::Serving);
/// A class of one slot, which its next task can only reuse.
pub const TEST_ONE: Cap = Cap::entries::<1>("test-one", CapFill::Serving);

/// An executor of one boxed class and that class's handle; it derefs to the executor.
pub struct TestExecutor {
    executor: CellExecutor,
    class: TaskClass,
}

impl TestExecutor {
    /// On a census of its own.
    pub fn new() -> TestExecutor {
        TestExecutor::at(0)
    }

    /// On a census of its own, cell `cell`'s: two at distinct cells are two cells' executors.
    pub fn at(cell: u16) -> TestExecutor {
        let (census, _mark) = CapCensus::new(CellId(cell));
        TestExecutor::on(&census)
    }

    /// On `census`.
    pub fn on(census: &Rc<CapCensus>) -> TestExecutor {
        TestExecutor::of(census, TEST_TASKS)
    }

    /// Of one slot, on a census of its own.
    pub fn one() -> TestExecutor {
        let (census, _mark) = CapCensus::new(CellId(0));
        TestExecutor::of(&census, TEST_ONE)
    }

    fn of(census: &Rc<CapCensus>, cap: Cap) -> TestExecutor {
        let mut layout = ExecutorLayout::new(census);
        let class = layout.class(&|future: Boxed| future, cap).expect("one class");
        TestExecutor { executor: layout.build().expect("no class refuses"), class }
    }

    /// `future` polled once in place as a task of the class, or `None` when the class is full.
    pub fn try_poll_immediate(
        &mut self,
        future: impl Future<Output = ()> + 'static,
    ) -> Option<PollImmediate> {
        match self.executor.reserve::<Boxed>(self.class) {
            TaskAdmission::Granted(permit) => Some(permit.poll_immediate(|| Box::pin(future))),
            TaskAdmission::Full => None,
            TaskAdmission::Exhausted | TaskAdmission::Foreign => {
                panic!("the test class refused a task")
            }
        }
    }

    /// The class's handle.
    pub fn class(&self) -> TaskClass {
        self.class
    }

    /// The class's Retired count.
    pub fn retired(&self) -> u32 {
        self.executor.class_retired(self.class)
    }

    /// `future` polled once in place as a task of the class.
    pub fn poll_immediate(&mut self, future: impl Future<Output = ()> + 'static) -> PollImmediate {
        match self.executor.reserve::<Boxed>(self.class) {
            TaskAdmission::Granted(permit) => permit.poll_immediate(|| Box::pin(future)),
            TaskAdmission::Full | TaskAdmission::Exhausted | TaskAdmission::Foreign => {
                panic!("the test class refused a task")
            }
        }
    }

    /// `future` made a task of the class, polled by the next `run_ready`.
    pub fn spawn_local(&mut self, future: impl Future<Output = ()> + 'static) -> TaskId {
        match self.executor.reserve::<Boxed>(self.class) {
            TaskAdmission::Granted(permit) => permit.spawn_local(|| Box::pin(future)),
            TaskAdmission::Full | TaskAdmission::Exhausted | TaskAdmission::Foreign => {
                panic!("the test class refused a task")
            }
        }
    }

    /// The executor, by value.
    pub fn into_inner(self) -> CellExecutor {
        self.executor
    }
}

impl Default for TestExecutor {
    fn default() -> TestExecutor {
        TestExecutor::new()
    }
}

impl Deref for TestExecutor {
    type Target = CellExecutor;
    fn deref(&self) -> &CellExecutor {
        &self.executor
    }
}

impl DerefMut for TestExecutor {
    fn deref_mut(&mut self) -> &mut CellExecutor {
        &mut self.executor
    }
}

thread_local! {
    /// The class of the executor this thread's fixture loop was last built with
    /// ([`loop_executor`]), which a fixture plane spawns into ([`spawn`]).
    static LOOP_CLASS: core::cell::Cell<Option<TaskClass>> = const { core::cell::Cell::new(None) };
}

/// A fixture loop's executor on `census`: one boxed class, recorded for this thread's
/// [`spawn`].
pub fn loop_executor(census: &Rc<CapCensus>) -> CellExecutor {
    let executor = TestExecutor::on(census);
    LOOP_CLASS.with(|class| class.set(Some(executor.class())));
    executor.into_inner()
}

/// `future` made a task of the class this thread's fixture loop was built with.
pub fn spawn(executor: &mut CellExecutor, future: impl Future<Output = ()> + 'static) -> TaskId {
    let class = LOOP_CLASS.with(core::cell::Cell::get).expect("a fixture loop was built");
    match executor.reserve::<Boxed>(class) {
        TaskAdmission::Granted(permit) => permit.spawn_local(|| Box::pin(future)),
        TaskAdmission::Full | TaskAdmission::Exhausted | TaskAdmission::Foreign => {
            panic!("the fixture loop's class refused a task")
        }
    }
}
