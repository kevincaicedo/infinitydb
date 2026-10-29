//! The census's `Stopping` mark has two owners, the cell's loop and its
//! plane, the two aggregates that own the cell's containers: each marks it
//! in its `Drop` before its fields drop, so a container drops after the
//! first of the two and the order between them is no one's. Each test
//! builds one cell, runs one iteration (the census serves), drops one of
//! the two, and reads the phase through its own clone of the census before
//! the other drops.

use super::*;
use inf_alloc::BufferPool;
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::bounded::CensusPhase;
use inf_foundation::time::VirtualClock;
use inf_runtime::{BackendDriver, Capabilities, CellLoop, LoopConfig, SubmitStats, Wait};
use inf_store::StoreConfig;

/// A driver that completes nothing: one iteration needs no I/O.
struct Idle;

impl BackendDriver for Idle {
    fn push(&mut self, _: IoOp) {}

    fn submit_and_reap(
        &mut self,
        _: &mut BufferPool,
        _: Wait,
        _: &mut Vec<Completion>,
    ) -> std::io::Result<usize> {
        Ok(0)
    }

    fn register_pool(&mut self, _: &mut BufferPool) -> std::io::Result<()> {
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "idle",
            multishot_accept: false,
            multishot_recv: false,
            provided_buffers: false,
            fixed_buffers: false,
            single_issuer: true,
            defer_taskrun: false,
            performance_tier: false,
        }
    }

    fn submit_stats(&self) -> SubmitStats {
        SubmitStats::default()
    }
}

/// One serving cell: its loop, its plane and the test's own census handle.
fn serving_cell() -> (CellLoop<Idle, VirtualClock>, ServerPlane, Rc<CapCensus>) {
    let fabric = Mesh::new(1, MeshConfig::default()).into_iter().next().expect("one cell");
    let (mut plane, serve_mark) = ServerPlane::new(
        CellId(0),
        1,
        -1,
        Keyspace::new(StoreConfig::default()),
        fabric,
        Rc::new(NodeInfo::try_default().expect("fixture cache allocation")),
        NoopObserver,
        false,
    );
    let census = Rc::clone(serve_mark.census());
    let mut reactor = CellLoop::new(
        Idle,
        VirtualClock::default(),
        BufferPool::new(4, 4096),
        LoopConfig::default(),
        serve_mark,
    );
    reactor.run_iteration(&mut plane).expect("the idle driver never fails");
    assert_eq!(census.phase(), CensusPhase::Serving, "the first iteration took the mark");
    (reactor, plane, census)
}

#[test]
fn the_plane_dropped_first_marks_the_census_stopping() {
    let (reactor, plane, census) = serving_cell();
    drop(plane);
    assert_eq!(census.phase(), CensusPhase::Stopping, "the plane's drop marks before its fields");
    drop(reactor);
    assert_eq!(census.phase(), CensusPhase::Stopping);
}

#[test]
fn the_loop_dropped_first_marks_the_census_stopping() {
    let (reactor, plane, census) = serving_cell();
    drop(reactor);
    assert_eq!(census.phase(), CensusPhase::Stopping, "the loop's drop marks before its fields");
    drop(plane);
    assert_eq!(census.phase(), CensusPhase::Stopping);
}
