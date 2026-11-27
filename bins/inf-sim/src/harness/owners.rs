//! O1, one task per owner (ADR-0177 D2), judged here and never in the
//! product: each cell reports its raw counts ([`ConnOwners`]) and this
//! module compares them, beside the runner's own reading of its clients.
//!
//! Per step: a `conn` task alive (held and not Retired) for each occupied
//! connection slot, since a slot vacates only when its pump returns and a
//! task retires at that return; both within the cell's share `C`; and,
//! where the runner holds its clients and their frames, the clients that
//! hold a slot by the runner's own reading at most both. A client holds a
//! slot once it has read a frame other than the `maxclients` refusal (its
//! slot and task were made at admission, before any frame but the refusal)
//! until the net posts its server-side close. That close comes first: the
//! net posts it at the server's `Close` op, and the slot vacates only when
//! the pump returns after reading `Closed`, so a client leaves the count no
//! later than its slot does. At a settled node no `conn` header stays
//! Retired: a header kept Retired for good (a waker kept past its pump's
//! return) holds its class slot, and after `C` of them every accept is
//! refused, while every per-step clause stays green.

use inf_server::ConnOwners;

use crate::driver::Violations;

/// The per-step clauses on one cell's owners, with `clients` the runner's
/// count of its clients on the cell that hold a slot (`None` where the
/// runner holds no clients): the first one broken, as its token and counts.
pub(crate) fn step_breach(owners: ConnOwners, clients: Option<usize>) -> Option<String> {
    let ConnOwners { held, retired, slots, share } = owners;
    let alive = held.checked_sub(retired);
    if alive.and_then(|alive| usize::try_from(alive).ok()) != Some(slots) {
        return Some(format!(
            "conn-tasks-unequal: {held} conn tasks held, {retired} of them Retired, {slots} slots"
        ));
    }
    let share_slots = usize::try_from(share).unwrap_or(usize::MAX);
    if held > share || slots > share_slots {
        return Some(format!("conn-over-share: {held} conn tasks, {slots} slots, share {share}"));
    }
    match clients {
        Some(clients) if clients > slots => Some(format!(
            "conn-under-clients: {clients} clients hold a slot by their own reading, \
             {slots} slots and {held} tasks held ({retired} Retired)"
        )),
        Some(_) | None => None,
    }
}

/// The leak's clause at a settled node: no `conn` header Retired.
pub(crate) fn settled_breach(owners: ConnOwners) -> Option<String> {
    let ConnOwners { held, retired, .. } = owners;
    (retired != 0).then(|| {
        format!("conn-retired-kept: {retired} of {held} conn tasks Retired at a settled node")
    })
}

/// O1's judge over one runner's cells: each cell's first breach, recorded
/// once with where it was read, kept until the runner takes them.
pub(crate) struct OwnersJudge {
    breached: Vec<bool>,
    violations: Violations,
}

impl OwnersJudge {
    pub(crate) fn new(cells: usize) -> OwnersJudge {
        OwnersJudge { breached: vec![false; cells], violations: Violations::new() }
    }

    /// One step's clauses on `cell`'s owners ([`step_breach`]); the cell's
    /// first breach reads `O1 <token>: … on cell <cell> at <at>`.
    pub(crate) fn step(
        &mut self,
        cell: usize,
        owners: ConnOwners,
        clients: Option<usize>,
        at: &dyn std::fmt::Display,
    ) {
        if let Some(breach) = step_breach(owners, clients) {
            self.record(cell, &breach, at);
        }
    }

    /// The settled node's clause on `cell`'s owners ([`settled_breach`]),
    /// recorded as [`step`](Self::step)'s.
    pub(crate) fn settled(&mut self, cell: usize, owners: ConnOwners, at: &dyn std::fmt::Display) {
        if let Some(breach) = settled_breach(owners) {
            self.record(cell, &breach, at);
        }
    }

    /// The breaches recorded so far, for the runner's verdict.
    pub(crate) fn take_violations(&mut self) -> Violations {
        core::mem::take(&mut self.violations)
    }

    /// Records a cell's first breach under its caller's check, so a
    /// per-step and a settled breach each print.
    #[track_caller]
    fn record(&mut self, cell: usize, breach: &str, at: &dyn std::fmt::Display) {
        if !self.breached[cell] {
            self.breached[cell] = true;
            self.violations.push(format!("O1 {breach} on cell {cell} at {at}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owners(held: u32, retired: u32, slots: usize, share: u32) -> ConnOwners {
        ConnOwners { held, retired, slots, share }
    }

    fn token(breach: Option<String>) -> Option<String> {
        breach.map(|text| text.split(':').next().unwrap_or_default().to_string())
    }

    /// The judge reads Retired apart: three held of which one Retired is two
    /// alive, equal to two slots, with two clients holding them; each clause
    /// broken alone reads its own token.
    #[test]
    fn owners_judge_reads_retired_apart() {
        assert_eq!(step_breach(owners(3, 1, 2, 4), Some(2)), None, "green: alive = slots ≥ H");
        assert_eq!(
            token(step_breach(owners(3, 1, 3, 4), Some(2))).as_deref(),
            Some("conn-tasks-unequal")
        );
        assert_eq!(
            token(step_breach(owners(3, 1, 2, 4), Some(3))).as_deref(),
            Some("conn-under-clients")
        );
        assert_eq!(
            token(step_breach(owners(4, 0, 4, 3), None)).as_deref(),
            Some("conn-over-share")
        );
        assert_eq!(token(settled_breach(owners(1, 1, 0, 4))).as_deref(), Some("conn-retired-kept"));
        assert_eq!(settled_breach(owners(2, 0, 2, 4)), None, "control: nothing Retired");
        let mut judge = OwnersJudge::new(2);
        judge.step(1, owners(3, 1, 3, 4), None, &"step 7");
        judge.step(1, owners(3, 1, 3, 4), None, &"step 8");
        let violations = judge.take_violations();
        assert_eq!(violations.len(), 1, "a cell's first breach only: {violations:?}");
    }
}
