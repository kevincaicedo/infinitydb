//! Observe actual plane submissions with delayed and refused cache constructors.

use super::*;
use core::num::NonZeroU16;
use inf_alloc::{BufferPool, CountingAllocator};
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::time::VirtualClock;
use inf_runtime::{BackendDriver, Capabilities, CellLoop, LoopConfig, SubmitStats, Wait};
use inf_store::StoreConfig;

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

#[derive(Default)]
struct AcceptObserver {
    arms: u64,
}

impl BackendDriver for AcceptObserver {
    fn push(&mut self, op: IoOp) {
        if let IoOp::AcceptArm { .. } = op {
            self.arms += 1;
        }
    }

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
            backend: "accept-observer",
            multishot_accept: true,
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

type TestLoop = CellLoop<AcceptObserver, VirtualClock>;

fn submit_two_iterations(reactor: &mut TestLoop, plane: &mut impl CellPlane) {
    // Plane operations reach the driver on the following reactor iteration.
    for _ in 0..2 {
        reactor.run_iteration(plane).unwrap();
    }
}

fn permits() -> (crate::CacheBootPermit, crate::CacheBootPermit) {
    let mut group = crate::CacheBootGroup::new(NonZeroU16::new(2).unwrap());
    (group.next().unwrap(), group.next().unwrap())
}

fn plane(permit: crate::CacheBootPermit) -> (TestLoop, ServerPlane) {
    let node = NodeInfo::try_default().unwrap().into_boot_group(permit);
    let fabric =
        Mesh::new(2, MeshConfig { ring_capacity: 8, data_credits: 4 }).into_iter().next().unwrap();
    let plane = ServerPlane::new(
        CellId(0),
        2,
        7,
        Keyspace::new(StoreConfig::default()),
        fabric,
        node,
        NoopObserver,
        false,
    );
    let reactor = CellLoop::new(
        AcceptObserver::default(),
        VirtualClock::default(),
        BufferPool::new(4, 4096),
        LoopConfig::default(),
    );
    (reactor, plane)
}

#[test]
fn delayed_cache_never_arms_a_peer_listener() {
    let (first, second) = permits();
    let (mut reactor, mut plane) = plane(first);
    for _ in 0..16 {
        reactor.run_iteration(&mut plane).unwrap();
        assert_eq!(reactor.driver().arms, 0, "peer cache is still pending");
        assert!(plane.take_boot_error().is_none());
    }
    let _peer = NodeInfo::try_default().unwrap().into_boot_group(second);
    for _ in 0..4 {
        reactor.run_iteration(&mut plane).unwrap();
    }
    assert_eq!(reactor.driver().arms, 1, "the ready group must arm exactly once");
}

#[cfg(feature = "doc")]
#[test]
fn cache_refusal_never_arms_a_peer_listener() {
    for skip in 0..2 {
        let (first, second) = permits();
        let (mut reactor, mut plane) = plane(first);
        let config = crate::ConfigStore::default();
        let guard = ALLOC.refuse_after(skip).unwrap();
        let result = NodeInfo::try_new(config).map(|node| node.into_boot_group(second));
        let triggered = guard.was_triggered();
        drop(guard);
        assert!(triggered, "cache reservation was not reached");
        assert!(result.is_err(), "cache refusal must prevent group admission");
        submit_two_iterations(&mut reactor, &mut plane);
        assert_eq!(reactor.driver().arms, 0, "failed peer cache must prevent accepts");
        let error = plane.take_boot_error().expect("peer cache failure must reach boot");
        assert!(error.to_string().contains("cache construction"));
    }
}

struct RetryTimer<'a>(&'a mut ServerPlane);

impl CellPlane for RetryTimer<'_> {
    fn on_completion(&mut self, cx: &mut LoopCx<'_>, completion: Completion) {
        self.0.on_completion(cx, completion);
    }

    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        self.0.on_timer(cx, ACCEPT_RETRY_TIMER_KEY);
        self.0.parse_execute(cx);
    }

    fn respond(&mut self, cx: &mut LoopCx<'_>) {
        self.0.respond(cx);
    }
}

#[test]
fn premature_retry_timer_cannot_bypass_cache_admission() {
    let (first, second) = permits();
    let (mut reactor, mut plane) = plane(first);
    submit_two_iterations(&mut reactor, &mut RetryTimer(&mut plane));
    assert_eq!(reactor.driver().arms, 0, "retry timer cannot admit a pending group");
    drop(second);
    submit_two_iterations(&mut reactor, &mut RetryTimer(&mut plane));
    assert_eq!(reactor.driver().arms, 0, "retry timer cannot admit a failed group");
}
