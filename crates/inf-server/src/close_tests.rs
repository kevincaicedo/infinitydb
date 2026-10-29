//! A connection's descriptor is closed once. A close pushed for a slot
//! already closing reaches `close(2)` again after the first has returned the
//! number, which the next accept or file open may already hold. The fixture
//! drives the real plane through the loop with a driver that records what it
//! is handed and completes nothing it is not told to: a `Close` stays in
//! flight until a test delivers its `Closed`.

use std::cell::RefCell;

use super::*;
use inf_alloc::{BufferPool, LeaseKind};
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::time::VirtualClock;
use inf_runtime::{BackendDriver, Capabilities, CellLoop, LoopConfig, SubmitStats, Wait};
use inf_store::StoreConfig;

/// The one accepted descriptor of every case.
const FD: RawFd = 41;

/// A completion the next reap delivers.
enum Delivery {
    Accepted(RawFd),
    /// A zero-length receive: the peer's EOF.
    Eof(CompletionToken),
}

#[derive(Default)]
struct Script {
    deliver: Vec<Delivery>,
    /// The receive arm the plane pushed for each descriptor.
    armed: Vec<(RawFd, CompletionToken)>,
    /// Every `Close` the plane pushed, in order.
    closes: Vec<RawFd>,
}

#[derive(Default)]
struct RecordingDriver {
    script: RefCell<Script>,
}

impl BackendDriver for RecordingDriver {
    fn push(&mut self, op: IoOp) {
        let script = self.script.get_mut();
        match op {
            IoOp::RecvArm { fd, token } => script.armed.push((fd, token)),
            IoOp::Close { fd, .. } => script.closes.push(fd),
            _ => {}
        }
    }

    fn submit_and_reap(
        &mut self,
        pool: &mut BufferPool,
        _: Wait,
        out: &mut Vec<Completion>,
    ) -> std::io::Result<usize> {
        for delivery in self.script.get_mut().deliver.drain(..) {
            out.push(match delivery {
                Delivery::Accepted(fd) => Completion {
                    token: CompletionToken::new(TokenClass::Accept, 0, 0),
                    result: CompletionResult::Accepted { fd },
                },
                Delivery::Eof(token) => Completion {
                    token,
                    result: CompletionResult::Recv {
                        buf: pool
                            .try_lease(LeaseKind::Recv)
                            .expect("the fixture pool has a buffer"),
                        len: 0,
                    },
                },
            });
        }
        Ok(out.len())
    }

    fn register_pool(&mut self, _: &mut BufferPool) -> std::io::Result<()> {
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "recording",
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

/// One cell with one admitted connection: its client id and its receive
/// token.
struct Rig {
    reactor: CellLoop<RecordingDriver, VirtualClock>,
    plane: ServerPlane,
    id: u64,
    recv: CompletionToken,
}

impl Rig {
    fn admitted() -> Rig {
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
        let mut reactor = CellLoop::new(
            RecordingDriver::default(),
            VirtualClock::default(),
            BufferPool::new(4, 4096),
            LoopConfig::default(),
            serve_mark,
        );
        reactor.driver().script.borrow_mut().deliver.push(Delivery::Accepted(FD));
        for _ in 0..2 {
            reactor.run_iteration(&mut plane).expect("the recording driver never fails");
        }
        let ids: Vec<u64> = plane.shared.node.clients.borrow().iter().map(|(id, _)| id).collect();
        assert_eq!(ids.len(), 1, "the accept registers one client");
        let armed = reactor.driver().script.borrow().armed.clone();
        assert_eq!(armed.len(), 1, "the accept arms one receive");
        assert_eq!(armed[0].0, FD);
        Rig { reactor, plane, id: ids[0], recv: armed[0].1 }
    }

    fn deliver(&self, delivery: Delivery) {
        self.reactor.driver().script.borrow_mut().deliver.push(delivery);
    }

    /// `CLIENT KILL ID`'s mark, which the next MAINTAIN sweeps.
    fn kill(&self) {
        let marked = self.plane.shared.node.clients.borrow_mut().request_kill(self.id);
        assert!(marked, "the id resolves until `Closed`");
    }

    /// Ops a plane pushes reach the driver at the next iteration's submit.
    fn run(&mut self, iterations: usize) {
        for _ in 0..iterations {
            self.reactor.run_iteration(&mut self.plane).expect("the recording driver never fails");
        }
    }

    fn closes(&self) -> Vec<RawFd> {
        self.reactor.driver().script.borrow().closes.clone()
    }
}

/// Control: two marks of one id before one sweep are one mark (the
/// registry's flag), so the sweep closes the connection once.
#[test]
fn two_kills_of_one_id_before_one_sweep_close_it_once() {
    let mut rig = Rig::admitted();
    rig.kill();
    rig.kill();
    rig.run(2);
    assert_eq!(rig.closes(), [FD], "one sweep closes a killed connection once");
}

/// A second kill of one id, swept while its first close is in flight, pushes
/// no second `Close`.
#[test]
fn a_kill_swept_while_its_close_is_in_flight_closes_nothing_again() {
    let mut rig = Rig::admitted();
    rig.kill();
    rig.run(2);
    assert_eq!(rig.closes(), [FD], "the first sweep closes the connection");
    rig.kill();
    rig.run(2);
    assert_eq!(rig.closes(), [FD], "a connection already closing was closed twice");
}

/// A kill swept after the peer's EOF closed the connection, before its
/// `Closed`, pushes no second `Close`.
#[test]
fn a_kill_after_the_eof_close_before_closed_closes_nothing_again() {
    let mut rig = Rig::admitted();
    rig.deliver(Delivery::Eof(rig.recv));
    rig.run(2);
    assert_eq!(rig.closes(), [FD], "the EOF closes the connection");
    rig.kill();
    rig.run(2);
    assert_eq!(rig.closes(), [FD], "a connection already closing was closed twice");
}
