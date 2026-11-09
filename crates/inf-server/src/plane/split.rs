//! The one grouping pass of a command's keys by owning cell (ADR-0173 D7):
//! every key routed once, through the command's key spec, into the one
//! owner of them all or a plan of one leg per owning cell. A multi-key
//! command that splits per owner calls this pass; a second grouping pass
//! would be a second copy of the routing truth.
//!
//! The split's work is bounded by the command's keys K (at most the
//! parser's 1,022): K router lookups, one sort of K `(owner, position)`
//! pairs, one walk that cuts the legs, and one sort of at most K legs. No
//! map and no scan per key, so nothing grows with owners × keys.

use core::ops::Range;

use super::scatter::key_positions;
use super::*;

/// How a command's keys are owned.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum OwnerSplit {
    /// Every key has this one owner (a command with no key: this cell).
    One(CellId),
    /// Two or more owners: one leg each.
    Many(GatherPlan),
}

/// The legs of a command whose keys have two or more owners. `order` holds
/// the argv positions of the keys, grouped by leg, each leg's span of it
/// ascending; `legs` are ordered local first, then by their first key's
/// position; `leg_of_position[i]` is the leg of the command's `i`-th key,
/// in the key spec's order.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct GatherPlan {
    order: Vec<usize>,
    legs: Vec<GatherLeg>,
    leg_of_position: Vec<usize>,
}

/// One owner's keys: a span of the split's `order`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GatherLeg {
    pub(super) owner: CellId,
    pub(super) span: Range<usize>,
}

impl GatherPlan {
    pub(super) fn legs(&self) -> &[GatherLeg] {
        &self.legs
    }

    /// The argv positions of `leg`'s keys, ascending.
    pub(super) fn positions(&self, leg: &GatherLeg) -> &[usize] {
        &self.order[leg.span.clone()]
    }

    /// The command's keys, in the key spec's order: the leg each belongs to.
    pub(super) fn leg_of_position(&self) -> &[usize] {
        &self.leg_of_position
    }

    /// Keys of the command, every leg's together.
    pub(super) fn key_count(&self) -> usize {
        self.order.len()
    }
}

/// Routes every key of `argv` once, by `router`, from `cell`'s point of
/// view: the one owner of them all, or the plan of one leg per owner.
pub(super) fn split_by_owner(
    router: &SlotRouter,
    cell: CellId,
    meta: &inf_wire::CommandMeta,
    argv: &[&[u8]],
) -> OwnerSplit {
    // (owner, argv position, key ordinal), in the key spec's order.
    let mut keys: Vec<(u16, usize, usize)> = Vec::new();
    for (ordinal, position) in key_positions(meta, argv).enumerate() {
        let owner = router.cell_of(SlotRouter::slot_of(argv[position]));
        keys.push((owner.0, position, ordinal));
    }
    let Some(&(first, ..)) = keys.first() else {
        return OwnerSplit::One(cell);
    };
    if keys.iter().all(|&(owner, ..)| owner == first) {
        return OwnerSplit::One(CellId(first));
    }
    keys.sort_unstable_by_key(|&(owner, position, _)| (owner, position));
    let order: Vec<usize> = keys.iter().map(|&(_, position, _)| position).collect();
    let mut legs: Vec<GatherLeg> = Vec::new();
    let mut begin = 0;
    for end in 1..=keys.len() {
        if end == keys.len() || keys[end].0 != keys[begin].0 {
            legs.push(GatherLeg { owner: CellId(keys[begin].0), span: begin..end });
            begin = end;
        }
    }
    legs.sort_unstable_by_key(|leg| (leg.owner != cell, order[leg.span.start]));
    let mut leg_of_position = vec![0; keys.len()];
    for (leg_index, leg) in legs.iter().enumerate() {
        for &(_, _, ordinal) in &keys[leg.span.clone()] {
            leg_of_position[ordinal] = leg_index;
        }
    }
    let plan = GatherPlan { order, legs, leg_of_position };
    debug_assert!(plan.tiles(cell), "the plan tiles the keys once, legs local first");
    OwnerSplit::Many(plan)
}

impl GatherPlan {
    /// Every key in exactly one leg, each leg's positions ascending and
    /// one owner's, the legs local first and then by first position.
    fn tiles(&self, cell: CellId) -> bool {
        let mut covered = 0;
        let mut previous: Option<(bool, usize)> = None;
        for (leg_index, leg) in self.legs.iter().enumerate() {
            let positions = self.positions(leg);
            covered += positions.len();
            let ascending = positions.windows(2).all(|pair| pair[0] < pair[1]);
            let rank = (leg.owner != cell, positions[0]);
            let ordered = previous.is_none_or(|previous| previous < rank);
            let owned = self.leg_of_position.iter().filter(|&&l| l == leg_index).count();
            if !ascending || !ordered || owned != positions.len() {
                return false;
            }
            previous = Some(rank);
        }
        covered == self.leg_of_position.len()
    }
}

#[cfg(test)]
mod tests {
    use inf_wire::{CommandId, lookup};

    use super::*;

    /// `count` keys `name:n` and the owner of each under a contiguous router
    /// of `cells` cells.
    fn keys(name: &str, count: usize, cells: u16) -> (SlotRouter, Vec<Vec<u8>>, Vec<u16>) {
        let router = SlotRouter::new_contiguous(cells);
        let keys: Vec<Vec<u8>> = (0..count).map(|n| format!("{name}:{n}").into_bytes()).collect();
        let owners = keys.iter().map(|k| router.cell_of(SlotRouter::slot_of(k)).0).collect();
        (router, keys, owners)
    }

    /// The tiling over the three key-spec shapes a split command has: every key
    /// once, in its owner's leg, ascending, the local leg first and the
    /// rest by first position; one owner is no plan.
    #[test]
    fn the_plan_tiles_every_key_spec_shape() {
        let (router, names, owners) = keys("k", 24, 4);
        for (command, tail, step) in [
            (CommandId::JsonMget, &[&b"$"[..]][..], 1),
            (CommandId::Del, &[][..], 1),
            (CommandId::Mset, &[][..], 2),
        ] {
            let meta = lookup(command_name(command)).expect("a registered command");
            let mut argv: Vec<&[u8]> = vec![meta.name.as_bytes()];
            for name in &names {
                argv.push(name);
                if step == 2 {
                    argv.push(b"v");
                }
            }
            argv.extend_from_slice(tail);
            for cell in 0..4 {
                let OwnerSplit::Many(plan) = split_by_owner(&router, CellId(cell), meta, &argv)
                else {
                    panic!("{command:?}: 24 keys over 4 cells have one owner");
                };
                assert!(plan.tiles(CellId(cell)), "{command:?} from cell {cell}: {plan:?}");
                assert_eq!(plan.key_count(), 24);
                assert_eq!(plan.legs()[0].owner, CellId(cell), "the local leg first");
                for (ordinal, &leg) in plan.leg_of_position().iter().enumerate() {
                    let leg = &plan.legs()[leg];
                    assert_eq!(leg.owner.0, owners[ordinal], "{command:?} key {ordinal}");
                    let position = 1 + ordinal * step;
                    assert!(plan.positions(leg).contains(&position), "{command:?} {ordinal}");
                }
            }
        }
        let meta = lookup(b"JSON.MGET").expect("registered");
        let argv: Vec<&[u8]> = vec![b"JSON.MGET", &names[0], &names[0], b"$"];
        let owner = CellId(owners[0]);
        assert_eq!(split_by_owner(&router, CellId(0), meta, &argv), OwnerSplit::One(owner));
    }

    fn command_name(id: CommandId) -> &'static [u8] {
        inf_wire::COMMANDS.iter().find(|meta| meta.id == id).expect("a row").name.as_bytes()
    }
}
