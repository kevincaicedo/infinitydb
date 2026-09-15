//! The acquisition model's oracles: publication, program, committed and
//! aborted checks, and the whole-program serial run they compare against.

use super::*;

impl Model {
    /// An aborted transaction published nothing; a committed one
    /// published every non-failed leg.
    pub(super) fn check_publication(&mut self) {
        for i in 0..self.txns.len() {
            let t = &self.txns[i];
            let Phase::Terminal(outcome) = &t.phase else { continue };
            let legs: Vec<(usize, Cell)> = t
                .owners
                .iter()
                .enumerate()
                .map(|(oi, o)| (oi, *o))
                .filter(|(_, o)| t.writes_all.iter().any(|k| self.owner_of(*k) == *o))
                .collect();
            let published: Vec<Cell> =
                legs.iter().filter(|(oi, _)| t.published[*oi]).map(|(_, o)| *o).collect();
            let unpublished: Vec<Cell> = legs
                .iter()
                .filter(|(oi, _)| !t.published[*oi] && !t.failed[*oi])
                .map(|(_, o)| *o)
                .collect();
            let msg = match outcome {
                Outcome::Committed if !unpublished.is_empty() => Some(format!(
                    "PARTIAL PUBLICATION: T{} decided Commit but cell(s) {unpublished:?} never \
                         published its leg",
                    i + 1
                )),
                Outcome::Aborted(reason) if t.decided_commit && !published.is_empty() => {
                    if unpublished.is_empty() {
                        Some(format!(
                            "FALSE ABORT: T{} replied aborted ({reason}) but every owner published \
                                 its leg",
                            i + 1
                        ))
                    } else {
                        Some(format!(
                            "PARTIAL PUBLICATION: T{} ({reason}) published on cell(s) \
                                 {published:?} and discarded on cell(s) {unpublished:?}",
                            i + 1
                        ))
                    }
                }
                Outcome::Aborted(reason) if !published.is_empty() => {
                    let key = t
                        .writes_all
                        .iter()
                        .find(|k| self.owner_of(**k) == published[0])
                        .copied()
                        .expect("a written key on the published leg");
                    Some(format!(
                        "STAGING VIOLATION: T{} aborted ({reason}) but its write to key \
                             {key}@cell{} is published",
                        i + 1,
                        published[0]
                    ))
                }
                _ => None,
            };
            if let Some(msg) = msg {
                self.report.violations.push(msg);
            }
        }
    }

    /// The serial oracle (M6-S07's AC): every terminal transaction's
    /// reply array and published state equal the program run alone
    /// against the pre-state, in queue order.
    pub(super) fn check_programs(&mut self) {
        for i in 0..self.txns.len() {
            let t = &self.txns[i];
            let Phase::Terminal(outcome) = t.phase.clone() else { continue };
            if t.bypass || t.program.is_empty() {
                continue;
            }
            let serial = self.serial_run(i);
            match outcome {
                Outcome::Committed => self.check_committed(i, &serial),
                Outcome::Aborted(reason) => self.check_aborted(i, reason, &serial),
                Outcome::Stuck => {}
            }
        }
    }

    fn check_committed(&mut self, i: usize, serial: &Serial) {
        let t = &self.txns[i];
        if let Some(reason) = serial.abort {
            self.report.violations.push(format!(
                "PROGRAM ORACLE: T{} committed where the serial history aborts ({reason})",
                i + 1
            ));
            return;
        }
        let got = t.reply.clone().expect("a committed transaction replied");
        let mut out = Vec::new();
        for (idx, (g, w)) in got.iter().zip(&serial.replies).enumerate() {
            if g != w {
                out.push(format!(
                    "REPLY MISMATCH: T{}'s {} replied {} where the serial history replies {}",
                    i + 1,
                    t.program[idx].label(),
                    g.text(),
                    w.text()
                ));
            }
        }
        let seq = t.commit_seq.expect("committed");
        for key in t.writes_all.clone() {
            let later =
                self.committed_writes.get(&key).is_some_and(|v| v.iter().any(|(s, _)| *s > seq));
            if later {
                continue;
            }
            let want = serial.state.get(&key).copied().flatten();
            let got = self.published(key);
            if got == want {
                continue;
            }
            let writer = t
                .program
                .iter()
                .rposition(|c| c.writes() && c.keys().contains(&key))
                .expect("a written key has a writing command");
            let kind = match t.program[writer] {
                Cmd::Dep(_) => "DEPENDENT COMMAND",
                _ => "PLAIN COMMAND",
            };
            out.push(format!(
                "{kind}: T{}'s {} left key {key}@cell{} = {} where the serial history gives {}",
                i + 1,
                t.program[writer].label(),
                self.owner_of(key),
                name(got),
                name(want)
            ));
        }
        self.report.violations.extend(out);
    }

    fn check_aborted(&mut self, i: usize, reason: &'static str, serial: &Serial) {
        let t = &self.txns[i];
        // Only a command's own failure is the oracle's business —
        // cancellations, dirty watches and refusals are not.
        let Some(idx) = t.failed_idx else { return };
        let label = t.program[idx].label();
        let msg = match serial.abort {
            None => format!(
                "DEPENDENT LEG: T{} aborted ({reason}) but the serial history commits its {label}",
                i + 1
            ),
            Some(want) if want != reason => format!(
                "PROGRAM ORACLE: T{} aborted ({reason}) where the serial history aborts ({want})",
                i + 1
            ),
            Some(_) => return,
        };
        self.report.violations.push(msg);
    }

    /// The program run alone against the pre-state with the same
    /// planted refusal and condition failure: replies in queue order,
    /// the state afterwards, and where `INF.TX` aborts.
    fn serial_run(&self, txn: usize) -> Serial {
        let t = &self.txns[txn];
        let spec = &t.spec;
        let mut state = t.pre.clone();
        let mut replies = Vec::new();
        let mut cond_used = false;
        let mut abort = None;
        for cmd in &t.program {
            let reply = match cmd {
                Cmd::Get(k) => Reply::Value(state.get(k).copied().flatten()),
                Cmd::Set(k) => {
                    if spec.fails_at == Some(self.owner_of(*k)) && !cond_used {
                        cond_used = true;
                        Reply::Err("condition failed")
                    } else {
                        state.insert(*k, Some(txn));
                        Reply::Ok
                    }
                }
                Cmd::Dep(Dependent::Move { src, dst }) => {
                    let value = state.get(src).copied().flatten();
                    if value.is_none() {
                        Reply::Err("no such key")
                    } else if spec.refuses_at == Some(self.owner_of(*dst)) {
                        Reply::Err("destination refused")
                    } else {
                        state.insert(*dst, value);
                        state.insert(*src, None);
                        Reply::Ok
                    }
                }
                Cmd::Dep(Dependent::SetIfNoneExist { keys }) => {
                    if keys.iter().any(|k| spec.refuses_at == Some(self.owner_of(*k))) {
                        Reply::Err("refused")
                    } else if keys.iter().any(|k| state.get(k).copied().flatten().is_some()) {
                        Reply::Int(0)
                    } else {
                        for k in keys {
                            state.insert(*k, Some(txn));
                        }
                        Reply::Int(1)
                    }
                }
            };
            let failure = reply.failure();
            replies.push(reply);
            if spec.native
                && let Some(reason) = failure
            {
                abort = Some(reason);
                break;
            }
        }
        Serial { replies, state, abort }
    }
}
