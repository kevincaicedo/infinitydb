//! The simulated network + [`SimDriver`]: per-cell in-memory connections
//! behind the real `BackendDriver` contract. Deterministic by construction:
//! `BTreeMap` iteration order, seeded chunk sizes, no wall clock, no real
//! syscalls.

use core::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::rc::Rc;

use inf_alloc::{BufferPool, LeaseKind};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_runtime::{
    BackendDriver, Capabilities, Completion, CompletionResult, CompletionToken, IoOp, RawFd,
    SubmitStats, Wait,
};

/// Fault plants (armed per scenario, fire on seeded draws).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Plant {
    #[default]
    None,
    /// Drop one recv readiness edge: the connection's pending bytes stop
    /// being delivered until *new* bytes arrive (the classic lost wakeup).
    /// Sequential clients never send again before the reply ⇒ stall.
    LostWakeup,
}

#[derive(Debug, Default)]
struct SimConn {
    to_server: VecDeque<u8>,
    to_client: Vec<u8>,
    client_closed: bool,
    server_closed: bool,
    recv_armed: bool,
    recv_token: Option<CompletionToken>,
    /// Lost-wakeup plant fired here: delivery suppressed until new bytes.
    suppressed: bool,
}

/// One cell's network endpoint: the listener plus every connection accepted
/// by this cell. The harness holds the same handle to play the client side.
#[derive(Debug)]
pub struct CellNet {
    cell: u16,
    accept_armed: bool,
    accept_token: Option<CompletionToken>,
    backlog: VecDeque<RawFd>,
    conns: BTreeMap<RawFd, SimConn>,
    next_fd: RawFd,
    rng: SplitMix64,
    plant: Plant,
    plant_fired: bool,
}

/// Synthetic listener fd for a cell (never a real fd).
pub fn listener_fd(cell: u16) -> RawFd {
    1_000_000 + i32::from(cell)
}

impl CellNet {
    pub fn new(cell: u16, seed: u64, plant: Plant) -> Rc<RefCell<CellNet>> {
        Rc::new(RefCell::new(CellNet {
            cell,
            accept_armed: false,
            accept_token: None,
            backlog: VecDeque::new(),
            conns: BTreeMap::new(),
            next_fd: 0,
            rng: SplitMix64::new(seed ^ 0xD15C_0000 ^ u64::from(cell)),
            plant,
            plant_fired: false,
        }))
    }

    /// Client side: open a connection to this cell; returns the fd handle.
    pub fn connect(&mut self) -> RawFd {
        self.next_fd += 1;
        let fd = i32::from(self.cell) * 100_000 + self.next_fd;
        self.conns.insert(fd, SimConn::default());
        self.backlog.push_back(fd);
        fd
    }

    /// Client side: send bytes. New arrivals clear a suppressed-delivery
    /// plant (edge-triggered semantics: the lost wakeup heals only on new
    /// data — which a reply-waiting client never produces).
    pub fn client_send(&mut self, fd: RawFd, bytes: &[u8]) {
        if let Some(conn) = self.conns.get_mut(&fd) {
            conn.to_server.extend(bytes);
            conn.suppressed = false;
        }
    }

    /// Client side: drain reply bytes.
    pub fn client_recv(&mut self, fd: RawFd) -> Vec<u8> {
        match self.conns.get_mut(&fd) {
            Some(conn) => core::mem::take(&mut conn.to_client),
            None => Vec::new(),
        }
    }

    /// Client side: half-close (FIN). The server reaps EOF and closes.
    pub fn client_close(&mut self, fd: RawFd) {
        if let Some(conn) = self.conns.get_mut(&fd) {
            conn.client_closed = true;
            conn.suppressed = false;
        }
    }

    /// True once the server closed its side too (teardown complete).
    pub fn closed(&self, fd: RawFd) -> bool {
        self.conns.get(&fd).is_none_or(|c| c.server_closed)
    }

    /// Total undelivered client→server bytes (progress accounting).
    pub fn pending_bytes(&self) -> usize {
        self.conns.values().map(|c| c.to_server.len()).sum()
    }
}

/// `BackendDriver` over a [`CellNet`]. One per cell.
#[derive(Debug)]
pub struct SimDriver {
    net: Rc<RefCell<CellNet>>,
    ops: Vec<IoOp>,
    stats: SubmitStats,
}

impl SimDriver {
    pub fn new(net: Rc<RefCell<CellNet>>) -> SimDriver {
        SimDriver { net, ops: Vec::new(), stats: SubmitStats::default() }
    }
}

impl BackendDriver for SimDriver {
    fn push(&mut self, op: IoOp) {
        self.ops.push(op);
    }

    fn submit_and_reap(
        &mut self,
        pool: &mut BufferPool,
        _wait: Wait,
        out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        let before = out.len();
        let mut net = self.net.borrow_mut();
        let submitted = self.ops.len() as u64;

        for op in self.ops.drain(..) {
            match op {
                IoOp::AcceptArm { token, .. } => {
                    net.accept_armed = true;
                    net.accept_token = Some(token);
                }
                IoOp::RecvArm { fd, token } => {
                    if let Some(conn) = net.conns.get_mut(&fd) {
                        conn.recv_armed = true;
                        conn.recv_token = Some(token);
                    }
                }
                IoOp::RecvDisarm { fd } => {
                    if let Some(conn) = net.conns.get_mut(&fd) {
                        conn.recv_armed = false;
                    }
                }
                IoOp::Send { fd, buf, len, token } => {
                    let result = match net.conns.get_mut(&fd) {
                        Some(conn) if !conn.server_closed => {
                            conn.to_client.extend_from_slice(&pool.bytes(buf)[..len as usize]);
                            CompletionResult::Sent { buf }
                        }
                        _ => CompletionResult::Error { errno: libc::EPIPE, buf: Some(buf) },
                    };
                    out.push(Completion { token, result });
                }
                IoOp::Close { fd, token } => {
                    if let Some(conn) = net.conns.get_mut(&fd) {
                        conn.server_closed = true;
                        conn.recv_armed = false;
                    }
                    out.push(Completion { token, result: CompletionResult::Closed });
                }
            }
        }

        // Accept everything queued (multishot semantics).
        if net.accept_armed {
            let token = net.accept_token.expect("armed implies token");
            while let Some(fd) = net.backlog.pop_front() {
                out.push(Completion { token, result: CompletionResult::Accepted { fd } });
            }
        }

        // Deliver one seeded chunk per armed connection per reap (BTreeMap
        // order = deterministic). Chunk boundaries are random so spanning
        // frames exercise the parser's accumulator on every run.
        let fds: Vec<RawFd> = net.conns.keys().copied().collect();
        for fd in fds {
            let CellNet { conns, rng, plant, plant_fired, .. } = &mut *net;
            let Some(conn) = conns.get_mut(&fd) else { continue };
            if !conn.recv_armed || conn.server_closed || conn.suppressed {
                continue;
            }
            let token = conn.recv_token.expect("armed implies token");
            if conn.to_server.is_empty() {
                if conn.client_closed {
                    // EOF: zero-length recv with a leased buffer (contract).
                    if let Some(buf) = pool.try_lease(LeaseKind::Recv) {
                        conn.recv_armed = false;
                        out.push(Completion {
                            token,
                            result: CompletionResult::Recv { buf, len: 0 },
                        });
                    }
                }
                continue;
            }
            // The lost-wakeup plant: one seeded readiness edge vanishes.
            if *plant == Plant::LostWakeup && !*plant_fired && rng.next_u64() % 256 == 0 {
                conn.suppressed = true;
                *plant_fired = true;
                continue;
            }
            let Some(buf) = pool.try_lease(LeaseKind::Recv) else { continue };
            let max = conn.to_server.len().min(pool.buf_size());
            let chunk = 1 + (rng.next_u64() as usize) % max;
            let bytes = pool.bytes_mut(buf);
            for (i, b) in conn.to_server.drain(..chunk).enumerate() {
                bytes[i] = b;
            }
            out.push(Completion {
                token,
                result: CompletionResult::Recv { buf, len: chunk as u32 },
            });
        }

        let produced = out.len() - before;
        self.stats = SubmitStats { syscalls: 1, sqes: submitted, cqes: produced as u64 };
        Ok(produced)
    }

    fn register_pool(&mut self, _pool: &mut BufferPool) -> io::Result<()> {
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "sim",
            multishot_accept: true,
            multishot_recv: true,
            provided_buffers: false,
            fixed_buffers: false,
            single_issuer: true,
            defer_taskrun: false,
            performance_tier: false, // gate tooling must reject sim numbers
        }
    }

    fn submit_stats(&self) -> SubmitStats {
        self.stats
    }
}
