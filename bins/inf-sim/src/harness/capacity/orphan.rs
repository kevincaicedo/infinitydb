//! The orphan leg (ADR-0177 D4): a fabric wait dropped with its request out leaves its holder
//! Orphaned until that request's one completion, which frees it. A namespace `KEYS` reaches the
//! state from the wire: a fan of reads answers at its first failed leg and drops the legs still
//! out. Cell X holds the namespace, its first peer Y does not and answers an error, and the
//! program returns that error at once, dropping the wait of its second peer Z, which the harness
//! holds with the request out. The namespaces are seeded at boot, X and Z holding one and Y
//! none: the state a `DROP` leaves between its origin's apply and its fan's legs.
//!
//! The wire is the independent observation: Y's error read whole while Z is held, and the
//! connection's next command answered. The holder at the error's read and the late reply's
//! count (`orphan_replies`) are the product's own counts, wiring checks; the gate's stale
//! completions are the census's, judged 0 at the report. Engagement: the harness ran no
//! iteration of Z between the `KEYS` and the error's read, else `VACUOUS orphan-unengaged`,
//! and an unengaged leg judges neither the holder nor the late reply.

use inf_store::NsSpec;

use super::{CELLS, Rig, X, Y, Z, encode};
use crate::harness::{FROZEN_MESH, namespace_specs};

/// What a peer answers an op for a namespace it does not hold, which a `KEYS` passes through
/// whole as its reply.
const PEER_ERROR: &[u8] = b"-ERR the selected namespace was dropped (INF.NS USE again)\r\n";

/// The leg: Z held from the `KEYS` until its error is read.
pub(super) fn orphan_leg(seed: u64) -> Rig {
    orphan_with(seed, Some(Z))
}

/// The leg with `held` the cell the harness holds over the `KEYS`.
fn orphan_with(seed: u64, held: Option<usize>) -> Rig {
    let namespace = namespace_specs(1);
    let name = namespace[0].name.clone();
    let mut each: [&[NsSpec]; CELLS as usize] = [&[]; CELLS as usize];
    each[X] = &namespace;
    each[Z] = &namespace;
    let mut rig = Rig::with_setup(seed, 0, 15_000, each, FROZEN_MESH, None);
    let client = rig.admitted(X);
    if !preconditions(&mut rig, client, &name) {
        rig.settle();
        return rig;
    }
    let orphans_before = rig.plane(X).fabric_stats().orphan_replies;
    let z_before = rig.iterations[Z];
    rig.held = held;
    let before = rig.clients[client].replies.len();
    rig.send(client, &encode(&[b"KEYS", b"*"]));
    let answered =
        rig.until("orphan-keys-stalled", |rig| rig.clients[client].replies.len() > before);
    let z_ran = rig.iterations[Z] - z_before;
    rig.read("z_iterations", z_ran);
    let engaged = answered && z_ran == 0;
    if answered {
        let reply = rig.clients[client].replies[before].clone();
        judge_error_read(&mut rig, &reply, engaged);
    }
    rig.held = None;
    if engaged {
        rig.until("orphan-reply-stalled", |rig| {
            rig.plane(X).fabric_stats().orphan_replies > orphans_before
        });
    }
    rig.settle();
    let orphans = rig.plane(X).fabric_stats().orphan_replies - orphans_before;
    rig.read("orphan_replies", orphans);
    judge_after_release(&mut rig, client);
    if !engaged {
        rig.violate(format!(
            "VACUOUS orphan-unengaged: the harness ran {z_ran} iteration(s) of cell {Z} between \
             the KEYS and its reply's read"
        ));
    }
    rig
}

/// The leg's state, read before the `KEYS`: X's `client` binds the namespace
/// (`orphan-use-refused`), and a client of Y is refused it (`orphan-peer-holds`).
fn preconditions(rig: &mut Rig, client: usize, name: &[u8]) -> bool {
    let bound = rig.call(client, &[b"INF.NS", b"USE", name]);
    let use_ok = bound == b"+OK\r\n";
    if !use_ok {
        let text = String::from_utf8_lossy(&bound).into_owned();
        rig.violate(format!("orphan-use-refused: cell {X}'s client read {text:?} binding it"));
    }
    let peer = rig.admitted(Y);
    let refused = rig.call(peer, &[b"INF.NS", b"USE", name]);
    let peer_lacks = refused.starts_with(b"-");
    if !peer_lacks {
        let text = String::from_utf8_lossy(&refused).into_owned();
        rig.violate(format!("orphan-peer-holds: cell {Y}'s client read {text:?} binding it"));
    }
    use_ok && peer_lacks
}

/// Read while Z is held: the reply is Y's error whole (`keys-reply-wrong`), and, engaged, X
/// holds the dropped leg's holder (`orphan-holder-freed`).
fn judge_error_read(rig: &mut Rig, reply: &[u8], engaged: bool) {
    if reply != PEER_ERROR {
        let text = String::from_utf8_lossy(reply).into_owned();
        rig.violate(format!(
            "keys-reply-wrong: cell {X}'s client read {text:?}, not cell {Y}'s error whole"
        ));
    }
    let holders = rig.plane(X).suspended();
    rig.read("holders_at_the_error", holders as u64);
    if engaged && holders != 1 {
        rig.violate(format!(
            "orphan-holder-freed: cell {X} holds {holders} holder(s) while cell {Z} holds the \
             dropped leg's request, not its one"
        ));
    }
}

/// After the late reply and the settle: X holds no holder (`orphan-holder-kept`), and the
/// connection's next command is answered (`orphan-conn-unanswered`).
fn judge_after_release(rig: &mut Rig, client: usize) {
    let left = rig.plane(X).suspended();
    if left != 0 {
        rig.violate(format!("orphan-holder-kept: cell {X} holds {left} holder(s) once settled"));
    }
    let pong = rig.call(client, &[b"PING"]);
    if pong != b"+PONG\r\n" {
        let text = String::from_utf8_lossy(&pong).into_owned();
        rig.violate(format!("orphan-conn-unanswered: cell {X}'s client read {text:?} to a PING"));
    }
}

#[cfg(test)]
mod tests {
    use super::super::CloseLeg;
    use super::orphan_with;

    /// The engagement's planted red: with no cell held, Z runs between the `KEYS` and its
    /// reply's read, so the leg reads `VACUOUS` and judges neither the holder nor the late
    /// reply. Whether Z's reply beats Y's error is the rig's stepping order, so the test asserts
    /// no count: the `VACUOUS` line is the report's one violation.
    #[test]
    fn the_leg_without_the_hold_reads_unengaged_alone() {
        let report = orphan_with(0xC0FFEE, None).report(CloseLeg::Orphan);
        let violations: Vec<&str> = report.violations.iter().collect();
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].starts_with("VACUOUS orphan-unengaged:"), "{violations:?}");
    }
}
