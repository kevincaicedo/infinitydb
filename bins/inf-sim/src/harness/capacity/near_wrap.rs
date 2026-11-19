//! The near-wrap leg (ADR-0194 A2, ADR-0177 D4): every cell's fabric holders are built free at
//! the generation two uses short of the key's 23-bit field, on a mesh of 8 data credits a peer,
//! so rounds of fans carry each slot's word past the field. A reader's reply is its own keys'
//! values, in argv order, which the harness wrote and the gate never sees (`fan-reply-lost`).
//! Engagement: every cell's gate took a holder past the field, read from the gate's count of
//! such takes, which reads the generation its publish stamped (`key-wrap-unengaged`).

use super::{CELLS, Rig, encode, names_on};

/// A mesh of 8 data credits a peer, on rings with the reply headroom the plane's mesh check
/// reads.
const NEAR_WRAP_MESH: inf_fabric::MeshConfig = crate::harness::scenario_mesh(1024, 8);
/// The holders' first generation: two uses short of the key's field.
const FIRST_GENERATION: u32 = (1 << inf_foundation::limits::GATE_KEY_GENERATION_BITS) - 2;
/// Keys one fan reads, one leg each: a connection's window of units.
const LEGS: usize = 32;
/// Fans each reader pipelines.
const ROUNDS: usize = 8;

/// Each cell's reader pipelines `ROUNDS` `MGET`s of 32 keys the next cell owns, each key set
/// to its own name first; every reply is that array, exact and in order.
pub(super) fn holder_near_wrap_leg(seed: u64) -> Rig {
    near_wrap_at(seed, Some(FIRST_GENERATION))
}

/// The leg with every cell's holders free at `first_generation`, the plane's default at `None`.
fn near_wrap_at(seed: u64, first_generation: Option<u32>) -> Rig {
    let none: [&[_]; CELLS as usize] = [&[]; CELLS as usize];
    let mut rig = Rig::with_setup(seed, 0, 15_000, none, NEAR_WRAP_MESH, first_generation);
    let cells = usize::from(CELLS);
    let keys: Vec<Vec<Vec<u8>>> = (0..cells)
        .map(|cell| names_on((cell + 1) % cells, &format!("wrap:{cell}"), LEGS))
        .collect();
    for (cell, names) in keys.iter().enumerate() {
        let writer = rig.admitted((cell + 1) % cells);
        let sets: Vec<u8> =
            names.iter().flat_map(|name| encode(&[b"SET", name.as_slice(), name])).collect();
        let owed = rig.clients[writer].replies.len() + names.len();
        rig.send(writer, &sets);
        rig.until("the near-wrap keys' SETs", |rig| rig.clients[writer].replies.len() >= owed);
    }
    let readers: Vec<usize> = (0..cells).map(|cell| rig.admitted(cell)).collect();
    for (&reader, names) in readers.iter().zip(&keys) {
        let mut argv: Vec<&[u8]> = vec![b"MGET"];
        argv.extend(names.iter().map(Vec::as_slice));
        let mget = encode(&argv);
        let fans: Vec<u8> = (0..ROUNDS).flat_map(|_| mget.iter().copied()).collect();
        rig.send(reader, &fans);
    }
    rig.until("the near-wrap fans' replies", |rig| {
        readers.iter().all(|&id| rig.clients[id].replies.len() > ROUNDS)
    });
    for (cell, (&reader, names)) in readers.iter().zip(&keys).enumerate() {
        let owed = values(names);
        let replies = &rig.clients[reader].replies;
        let read = replies.len().saturating_sub(1);
        let exact = replies.iter().skip(1).take(ROUNDS).filter(|reply| **reply == owed).count();
        rig.read("near_wrap_exact", exact as u64);
        if exact < ROUNDS {
            rig.violate(format!(
                "fan-reply-lost: cell {cell}'s reader read {exact} exact of its {ROUNDS} fans' \
                 replies ({read} read)"
            ));
        }
    }
    rig.settle();
    for cell in 0..cells {
        let wraps = wrapped_takes(&rig, cell);
        rig.read("holders_past_the_field", wraps);
        if wraps == 0 {
            rig.violate(format!(
                "VACUOUS key-wrap-unengaged: cell {cell}'s gate took no holder past the key's \
                 generation field"
            ));
        }
    }
    rig
}

/// The `MGET` reply of `names`, each set to itself.
fn values(names: &[Vec<u8>]) -> Vec<u8> {
    let mut reply = format!("*{}\r\n", names.len()).into_bytes();
    for name in names {
        reply.extend_from_slice(format!("${}\r\n", name.len()).as_bytes());
        reply.extend_from_slice(name);
        reply.extend_from_slice(b"\r\n");
    }
    reply
}

/// The cell's gate's takes past the key's field: `inf-server`'s `fault-points` hook, which
/// `dst` forwards (the simulator runs only with `dst`).
#[cfg(feature = "dst")]
fn wrapped_takes(rig: &Rig, cell: usize) -> u64 {
    rig.plane(cell).gate_wrapped_takes()
}

#[cfg(not(feature = "dst"))]
fn wrapped_takes(_: &Rig, _: usize) -> u64 {
    0
}

#[cfg(all(test, feature = "dst"))]
mod tests {
    use super::super::CloseLeg;
    use super::near_wrap_at;

    /// The leg's control: at the default first generation no slot reaches the key's field, so
    /// every cell's tap reads no take past it and the leg is `VACUOUS`, its replies still exact.
    /// A tap that counts every take reads green here.
    #[test]
    fn the_leg_at_the_default_generation_is_unengaged() {
        let report = near_wrap_at(0xC0FFEE, None).report(CloseLeg::HolderNearWrap);
        let violations: Vec<&str> = report.violations.iter().collect();
        let unengaged = violations.iter().filter(|v| v.contains("VACUOUS key-wrap-unengaged"));
        assert_eq!(unengaged.count(), usize::from(super::super::CELLS), "{violations:?}");
        assert!(!violations.iter().any(|v| v.contains("fan-reply-lost")), "{violations:?}");
    }
}
