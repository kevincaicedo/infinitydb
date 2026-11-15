//! A scripted node for plane tests that drive real cells through their
//! loops on one thread. Each cell's driver completes what the test hands it
//! and nothing else: an accept, a receive's bytes, the peer's end of stream
//! or a reset arrive when the test delivers them; every `Send` completes at
//! the next reap with its bytes kept per descriptor; every `Close` completes
//! at the next reap. A cell the test freezes runs no iteration, so the
//! fabric operations sent to it stay unanswered until it is thawed.

use std::cell::RefCell;

use super::*;
use inf_alloc::BufferPool;
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::time::VirtualClock;
use inf_runtime::{BackendDriver, Capabilities, CellLoop, LoopConfig, SubmitStats, Wait};
use inf_store::StoreConfig;

/// Linux's `ECONNRESET`: the receive error of a peer that reset its socket.
const ECONNRESET: i32 = 104;

/// The buffers of a cell's pool and their size.
const POOL_BUFFERS: usize = 256;
const BUFFER_BYTES: usize = 4096;

/// A completion the cell's next reap delivers.
enum Delivery {
    Accepted(RawFd),
    /// Bytes on the descriptor's armed receive, at most one buffer.
    Data(RawFd, Vec<u8>),
    /// A zero-length receive: the peer's end of stream.
    Eof(RawFd),
    /// A receive error: the peer reset the connection.
    Reset(RawFd),
}

#[derive(Default)]
struct Script {
    deliver: Vec<Delivery>,
    /// The receive arm the plane pushed for each descriptor, latest last.
    armed: Vec<(RawFd, CompletionToken)>,
    /// Sends pushed since the last reap.
    sends: Vec<(RawFd, BufferId, u32, CompletionToken)>,
    /// Every byte the plane sent, per descriptor.
    written: Vec<(RawFd, Vec<u8>)>,
    /// Closes pushed since the last reap.
    closes: Vec<(RawFd, CompletionToken)>,
    /// Every descriptor whose `Closed` was delivered, in order.
    closed: Vec<RawFd>,
}

impl Script {
    fn recv_token(&self, fd: RawFd) -> CompletionToken {
        let armed = self.armed.iter().rev().find(|(armed, _)| *armed == fd);
        armed.map(|(_, token)| *token).expect("the fixture delivers only to an armed receive")
    }

    fn written_mut(&mut self, fd: RawFd) -> &mut Vec<u8> {
        let at = match self.written.iter().position(|(written, _)| *written == fd) {
            Some(at) => at,
            None => {
                self.written.push((fd, Vec::new()));
                self.written.len() - 1
            }
        };
        &mut self.written[at].1
    }
}

#[derive(Default)]
pub(super) struct ScriptDriver {
    script: RefCell<Script>,
}

impl BackendDriver for ScriptDriver {
    fn push(&mut self, op: IoOp) {
        let script = self.script.get_mut();
        match op {
            IoOp::RecvArm { fd, token } => script.armed.push((fd, token)),
            IoOp::Send { fd, buf, len, token } => script.sends.push((fd, buf, len, token)),
            IoOp::Close { fd, token } => script.closes.push((fd, token)),
            _ => {}
        }
    }

    fn submit_and_reap(
        &mut self,
        pool: &mut BufferPool,
        _: Wait,
        out: &mut Vec<Completion>,
    ) -> std::io::Result<usize> {
        let script = self.script.get_mut();
        for (fd, buf, len, token) in core::mem::take(&mut script.sends) {
            let bytes = pool.bytes(buf)[..len as usize].to_vec();
            script.written_mut(fd).extend_from_slice(&bytes);
            out.push(Completion { token, result: CompletionResult::Sent { buf } });
        }
        for (fd, token) in core::mem::take(&mut script.closes) {
            script.closed.push(fd);
            out.push(Completion { token, result: CompletionResult::Closed });
        }
        for delivery in core::mem::take(&mut script.deliver) {
            out.push(match delivery {
                Delivery::Accepted(fd) => Completion {
                    token: CompletionToken::new(TokenClass::Accept, 0, 0),
                    result: CompletionResult::Accepted { fd },
                },
                Delivery::Data(fd, bytes) => {
                    let buf = pool.try_lease(LeaseKind::Recv).expect("the fixture pool has room");
                    pool.bytes_mut(buf)[..bytes.len()].copy_from_slice(&bytes);
                    let len = u32::try_from(bytes.len()).expect("one buffer");
                    Completion {
                        token: script.recv_token(fd),
                        result: CompletionResult::Recv { buf, len },
                    }
                }
                Delivery::Eof(fd) => Completion {
                    token: script.recv_token(fd),
                    result: CompletionResult::Recv {
                        buf: pool.try_lease(LeaseKind::Recv).expect("the fixture pool has room"),
                        len: 0,
                    },
                },
                Delivery::Reset(fd) => Completion {
                    token: script.recv_token(fd),
                    result: CompletionResult::Error { errno: ECONNRESET, buf: None },
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
            backend: "script",
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

pub(super) struct RigCell {
    pub(super) reactor: CellLoop<ScriptDriver, Rc<VirtualClock>>,
    pub(super) plane: ServerPlane,
    frozen: bool,
}

/// A node of `cells` cells on one fabric and one virtual clock.
pub(super) struct Rig {
    pub(super) cells: Vec<RigCell>,
    pub(super) clock: Rc<VirtualClock>,
}

impl Rig {
    /// Every cell built and run until its listener is armed.
    pub(super) fn new(cells: u16) -> Rig {
        let clock = Rc::new(VirtualClock::default());
        let fabrics = Mesh::new(cells, MeshConfig::default());
        let built = fabrics.into_iter().enumerate().map(|(cell, fabric)| {
            let node = Rc::new(NodeInfo::try_default().expect("fixture cache allocation"));
            let (plane, serve_mark, timer_set) = ServerPlane::new(
                CellId(u16::try_from(cell).expect("a u16 cell")),
                cells,
                -1,
                KeyspaceSeed::Uncataloged { cfg: StoreConfig::default(), specs: Vec::new() },
                fabric,
                node,
                NoopObserver,
                false,
            )
            .expect("the server's timer owners fit their cap");
            let reactor = CellLoop::new(
                ScriptDriver::default(),
                Rc::clone(&clock),
                BufferPool::new(POOL_BUFFERS, BUFFER_BYTES),
                LoopConfig::default(),
                serve_mark,
                timer_set,
            );
            RigCell { reactor, plane, frozen: false }
        });
        let mut rig = Rig { cells: built.collect(), clock };
        rig.rounds(2);
        rig
    }

    /// One iteration of every cell that is not frozen, `rounds` times.
    pub(super) fn rounds(&mut self, rounds: usize) {
        for _ in 0..rounds {
            for cell in self.cells.iter_mut().filter(|cell| !cell.frozen) {
                cell.reactor.run_iteration(&mut cell.plane).expect("the script never fails");
            }
        }
    }

    /// A frozen cell runs no iteration until it is thawed.
    pub(super) fn freeze(&mut self, cell: usize, frozen: bool) {
        self.cells[cell].frozen = frozen;
    }

    fn deliver(&self, cell: usize, delivery: Delivery) {
        self.cells[cell].reactor.driver().script.borrow_mut().deliver.push(delivery);
    }

    /// Delivers an accept of `fd` to `cell` and runs the node until its
    /// receive is armed.
    pub(super) fn accept(&mut self, cell: usize, fd: RawFd) {
        self.deliver(cell, Delivery::Accepted(fd));
        self.rounds(2);
        let armed = self.cells[cell].reactor.driver().script.borrow().armed.clone();
        assert!(armed.iter().any(|(armed, _)| *armed == fd), "the accept arms a receive");
    }

    /// Queues `bytes` on `fd`'s receive, one buffer a completion, for the
    /// cell's next reap; nothing runs.
    pub(super) fn send(&self, cell: usize, fd: RawFd, bytes: &[u8]) {
        for chunk in bytes.chunks(BUFFER_BYTES) {
            self.deliver(cell, Delivery::Data(fd, chunk.to_vec()));
        }
    }

    /// Queues the peer's end of stream on `fd` for the cell's next reap.
    pub(super) fn eof(&self, cell: usize, fd: RawFd) {
        self.deliver(cell, Delivery::Eof(fd));
    }

    /// Queues a receive error on `fd` for the cell's next reap: the peer is
    /// gone.
    pub(super) fn reset(&self, cell: usize, fd: RawFd) {
        self.deliver(cell, Delivery::Reset(fd));
    }

    /// Every byte the plane sent on `fd`.
    pub(super) fn written(&self, cell: usize, fd: RawFd) -> Vec<u8> {
        let script = self.cells[cell].reactor.driver().script.borrow();
        let written = script.written.iter().find(|(written, _)| *written == fd);
        written.map(|(_, bytes)| bytes.clone()).unwrap_or_default()
    }

    /// Every descriptor of `cell` whose `Closed` was delivered.
    pub(super) fn closed(&self, cell: usize) -> Vec<RawFd> {
        self.cells[cell].reactor.driver().script.borrow().closed.clone()
    }

    /// A key that `owner` owns, named after `stem`.
    pub(super) fn key_on(&self, owner: u16, stem: &str) -> Vec<u8> {
        let router = &self.cells[0].plane.shared.router;
        (0..10_000)
            .map(|n| format!("{stem}:{n}").into_bytes())
            .find(|key| router.cell_of(SlotRouter::slot_of(key)) == CellId(owner))
            .expect("every owner is reachable")
    }

    /// `count` distinct keys that `owner` owns, named after `stem`.
    pub(super) fn keys_on(&self, owner: u16, stem: &str, count: usize) -> Vec<Vec<u8>> {
        let router = &self.cells[0].plane.shared.router;
        let keys = (0..1_000_000)
            .map(|n| format!("{stem}:{n}").into_bytes())
            .filter(|key| router.cell_of(SlotRouter::slot_of(key)) == CellId(owner));
        keys.take(count).collect()
    }
}

/// `argv` as one RESP multibulk frame.
pub(super) fn frame(argv: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", argv.len()).into_bytes();
    for arg in argv {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}
