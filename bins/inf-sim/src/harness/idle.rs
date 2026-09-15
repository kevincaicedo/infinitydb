//! The idle-phase driver (F-L11-04/N17 regimes): idle clients, park
//! deadlines, and the idle timeout oracle.

use super::*;

// ---- admission + idle phase (ADR-0123, batch 50) ------------------------------

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
    started: bool,
    done: bool,
}

impl IdlePhase {
    pub(super) fn new(wanted: usize) -> IdlePhase {
        IdlePhase { wanted, clients: Vec::new(), started: false, done: wanted == 0 }
    }

    /// A monotone step counter for the stall detector: state transitions
    /// plus one per client waiting inside its deadline window.
    pub(super) fn progress(&self) -> u64 {
        self.clients
            .iter()
            .map(|c| match c.state {
                IdleState::Fresh => 0,
                IdleState::Active { .. } => 1,
                IdleState::Refused | IdleState::Closed { .. } => 2,
            })
            .sum()
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
    violations: &mut Vec<String>,
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
    // The plane stamps activity at the buffer's receipt (a step before the
    // reply is observed) and compares whole milliseconds.
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
                    _ => {}
                }
            }
            IdleState::Refused | IdleState::Closed { .. } => {}
        }
    }
    idle.done = idle
        .clients
        .iter()
        .all(|c| matches!(c.state, IdleState::Refused | IdleState::Closed { .. }));
    idle.done
}
