//! The idle-phase driver (F-L11-04/N17 regimes): idle clients, park
//! deadlines, and the idle timeout oracle.

use super::*;
use crate::driver::Violations;

// ---- admission + idle phase (ADR-0123, ADR-0193) ------------------------------

/// Redis's reply past `maxclients` (networking.c; the plane's constant).
pub(super) const REFUSAL_FRAME: &[u8] = b"-ERR max number of clients reached\r\n";

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum IdleKind {
    /// `PING`, then silence: must be closed at the deadline.
    Plain,
    /// `SUBSCRIBE`, then silence: exempt, must survive the deadline.
    Subscriber,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum IdleState {
    /// Sent its one command, awaiting the reply.
    Fresh,
    /// Replied at `since`: idle from here.
    Active { since: Nanos },
    /// Refused at accept (the cell was full) — outside the timeout oracle.
    Refused,
    /// Server-closed at `at` (plain) or client-closed at the end (subscriber).
    Closed { at: Nanos },
}

struct IdleClient {
    cell: usize,
    fd: RawFd,
    kind: IdleKind,
    state: IdleState,
    rx: Vec<u8>,
}

pub(super) struct IdlePhase {
    wanted: usize,
    clients: Vec<IdleClient>,
    /// The hot raise past the boot capacity, on a scenario that boots one.
    raise: Option<HotRaise>,
    started: bool,
    done: bool,
}

/// `CONFIG SET maxclients <B + 1> maxmemory 100mb` from one client of cell
/// 0 (ADR-0123 A1): its slots are fixed at boot, so the raise is refused
/// whole with A1's bytes, and `CONFIG GET` then reads both keys unchanged.
/// The bytes are written here from the decision's text, not read from the
/// server, so the probe shares the wire format alone with the plane.
struct HotRaise {
    fd: RawFd,
    rx: Vec<u8>,
    /// The exchanges still to read, each the bytes its request must answer,
    /// in order; the first is the raise's.
    expected: Vec<(Vec<u8>, Vec<u8>)>,
    answered: usize,
}

/// A raise past the boot capacity's answer, ADR-0123 A1's bytes.
const BOOT_CAPACITY_REFUSAL: &[u8] =
    b"-ERR maxclients exceeds boot capacity; restart with --maxclients\r\n";

impl HotRaise {
    /// The probe of a node booted at `maxclients`, its requests sent.
    fn start(nets: &[Rc<RefCell<CellNet>>], maxclients: u32) -> HotRaise {
        let boot = maxclients.to_string();
        let above = (u64::from(maxclients) + 1).to_string();
        let get_maxclients = format!("*2\r\n$10\r\nmaxclients\r\n${}\r\n{boot}\r\n", boot.len());
        let expected = vec![
            (
                encode(&[
                    b"CONFIG".to_vec(),
                    b"SET".to_vec(),
                    b"maxclients".to_vec(),
                    above.into_bytes(),
                    b"maxmemory".to_vec(),
                    b"100mb".to_vec(),
                ]),
                BOOT_CAPACITY_REFUSAL.to_vec(),
            ),
            (
                encode(&[b"CONFIG".to_vec(), b"GET".to_vec(), b"maxclients".to_vec()]),
                get_maxclients.into_bytes(),
            ),
            (
                encode(&[b"CONFIG".to_vec(), b"GET".to_vec(), b"maxmemory".to_vec()]),
                b"*2\r\n$9\r\nmaxmemory\r\n$1\r\n0\r\n".to_vec(),
            ),
        ];
        let mut net = nets[0].borrow_mut();
        let fd = net.connect();
        for (request, _) in &expected {
            net.client_send(fd, request);
        }
        HotRaise { fd, rx: Vec::new(), expected, answered: 0 }
    }

    fn done(&self) -> bool {
        self.answered == self.expected.len()
    }

    /// Reads what reached the probe; each framed reply is held to its
    /// expected bytes, and the client closes after the last.
    fn step(&mut self, nets: &[Rc<RefCell<CellNet>>], violations: &mut Violations) {
        if self.done() {
            return;
        }
        let mut net = nets[0].borrow_mut();
        self.rx.extend_from_slice(&net.client_recv(self.fd));
        while !self.done()
            && let Some(n) = reply_len(&self.rx)
        {
            let reply: Vec<u8> = self.rx.drain(..n).collect();
            let want = &self.expected[self.answered].1;
            if &reply != want {
                violations.push(format!(
                    "hot-raise: reply {} to a raise past the boot capacity read {:?}, want {:?}",
                    self.answered,
                    String::from_utf8_lossy(&reply),
                    String::from_utf8_lossy(want)
                ));
            }
            self.answered += 1;
        }
        if self.done() {
            net.client_close(self.fd);
        }
    }
}

impl IdlePhase {
    /// Pushes the idle clients, and the hot-raise probe, that have read a
    /// reply (a frame other than the refusal), as their cells and fds.
    pub(super) fn admitted(&self, out: &mut Vec<(usize, RawFd)>) {
        let replied = |c: &&IdleClient| {
            matches!(c.state, IdleState::Active { .. } | IdleState::Closed { .. })
        };
        out.extend(self.clients.iter().filter(replied).map(|c| (c.cell, c.fd)));
        if let Some(raise) = &self.raise
            && raise.answered > 0
        {
            out.push((0, raise.fd));
        }
    }

    pub(super) fn new(wanted: usize) -> IdlePhase {
        IdlePhase { wanted, clients: Vec::new(), raise: None, started: false, done: wanted == 0 }
    }

    /// A monotone step counter for the stall detector: state transitions
    /// plus one per client waiting inside its deadline window, and the hot
    /// raise's replies.
    pub(super) fn progress(&self) -> u64 {
        let raise = self.raise.as_ref().map_or(0, |raise| raise.answered as u64);
        raise
            + self
                .clients
                .iter()
                .map(|c| match c.state {
                    IdleState::Fresh => 0,
                    IdleState::Active { .. } => 1,
                    IdleState::Refused | IdleState::Closed { .. } => 2,
                })
                .sum::<u64>()
    }

    pub(super) fn refused_per_cell(&self) -> Vec<(usize, u64)> {
        self.clients.iter().filter(|c| c.state == IdleState::Refused).map(|c| (c.cell, 1)).collect()
    }
}

/// Drives the idle phase one step; true once it is over (or never wanted).
#[allow(clippy::too_many_arguments)] // the scheduler-step context, not an API surface
pub(super) fn drive_idle_phase(
    scenario: &Scenario,
    idle: &mut IdlePhase,
    nets: &[Rc<RefCell<CellNet>>],
    clock: &Rc<VirtualClock>,
    rng: &mut SplitMix64,
    regular_unwound: bool,
    report: &mut SimReport,
    violations: &mut Violations,
) -> bool {
    if idle.done {
        return true;
    }
    if !idle.started {
        // Wait for every regular connection to unwind server-side so the
        // idle clients are admitted (the shares are free again).
        if !regular_unwound {
            return false;
        }
        idle.started = true;
        if scenario.maxclients != 0 {
            idle.raise = Some(HotRaise::start(nets, scenario.maxclients));
        }
        for i in 0..idle.wanted * 2 {
            let kind = if i % 2 == 0 { IdleKind::Plain } else { IdleKind::Subscriber };
            let cell = (rng.next_u64() % u64::from(scenario.cells)) as usize;
            let mut net = nets[cell].borrow_mut();
            let fd = net.connect();
            let wire = match kind {
                IdleKind::Plain => encode(&[b"PING".to_vec()]),
                IdleKind::Subscriber => {
                    encode(&[b"SUBSCRIBE".to_vec(), format!("idle:{i}").into_bytes()])
                }
            };
            net.client_send(fd, &wire);
            idle.clients.push(IdleClient {
                cell,
                fd,
                kind,
                state: IdleState::Fresh,
                rx: Vec::new(),
            });
        }
        return false;
    }
    let timeout = Nanos(u64::from(scenario.timeout_secs) * 1_000_000_000);
    // The plane restarts a client's clock at each buffer it takes, at its
    // reply's send and at the send's completion (ADR-0193 D2), each within a
    // step of the reply's observation, and compares whole milliseconds.
    let lower = Nanos(timeout.0.saturating_sub(scenario.step_ns_max + 1_000_000));
    let upper = Nanos(timeout.0 + 4 * scenario.step_ns_max + 1_000_000);
    let now = clock.now();
    for (i, c) in idle.clients.iter_mut().enumerate() {
        let mut net = nets[c.cell].borrow_mut();
        let rx = net.client_recv(c.fd);
        c.rx.extend_from_slice(&rx);
        match c.state {
            IdleState::Fresh => {
                if c.rx.starts_with(REFUSAL_FRAME) {
                    c.state = IdleState::Refused;
                    report.refused_clients += 1;
                    net.client_abandon(c.fd);
                    continue;
                }
                let replied = match c.kind {
                    IdleKind::Plain => c.rx == b"+PONG\r\n",
                    IdleKind::Subscriber => reply_len(&c.rx).is_some(),
                };
                if replied {
                    c.rx.clear();
                    c.state = IdleState::Active { since: now };
                }
            }
            IdleState::Active { since } => {
                let idle_for = Nanos(now.0.saturating_sub(since.0));
                let closed = net.closed(c.fd);
                match c.kind {
                    IdleKind::Plain if closed => {
                        if idle_for < lower {
                            violations.push(format!(
                                "idle client {i}: reaped after {} ns, before its {} ns deadline",
                                idle_for.0, timeout.0
                            ));
                        }
                        c.state = IdleState::Closed { at: now };
                        report.idle_closed += 1;
                    }
                    IdleKind::Plain if idle_for > upper => {
                        violations.push(format!(
                            "idle client {i}: still open {} ns past its {} ns deadline",
                            idle_for.0 - timeout.0,
                            timeout.0
                        ));
                        net.client_close(c.fd);
                        c.state = IdleState::Closed { at: now };
                    }
                    IdleKind::Subscriber if closed => {
                        violations.push(format!(
                            "idle subscriber {i}: closed by the server after {} ns — subscribers \
                             are exempt from `timeout`",
                            idle_for.0
                        ));
                        c.state = IdleState::Closed { at: now };
                    }
                    IdleKind::Subscriber if idle_for > upper => {
                        // Outlived the deadline: leave, so the leak oracle
                        // sees the server unwind it.
                        net.client_close(c.fd);
                        c.state = IdleState::Closed { at: now };
                        report.idle_survived += 1;
                    }
                    IdleKind::Plain | IdleKind::Subscriber => {}
                }
            }
            IdleState::Refused | IdleState::Closed { .. } => {}
        }
    }
    if let Some(raise) = idle.raise.as_mut() {
        raise.step(nets, violations);
    }
    idle.done = idle.raise.as_ref().is_none_or(HotRaise::done)
        && idle
            .clients
            .iter()
            .all(|c| matches!(c.state, IdleState::Refused | IdleState::Closed { .. }));
    idle.done
}
