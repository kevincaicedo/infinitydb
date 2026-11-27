//! `m1-pub-cross`: publishes that cross between cells faster than their
//! owners answer. Each cell carries one pinned `PSUBSCRIBE chan:*`, so every
//! channel's owner fans a leg to every other cell; publishers are pinned
//! round-robin to the cells, and each `PUBLISH` goes to a channel its target
//! cell owns, with every fourth command a `SET` or `GET` on a key that cell
//! owns. With a narrow credit window both directions' `INF.PUB`s can hold
//! every credit while each owner's pump fans back toward the other: the run
//! stalls unless the fan's legs draw a credit no deferred op can hold.
//!
//! The plan shapes the load only. The channel and key owners are found with
//! the slot router the node uses, so a wrong owner weakens the load and
//! cannot pass a stall; the oracles (the stall detector, the delivery
//! ledger, the apply replay) share no code with the plan.

use inf_foundation::rng::{Entropy, SplitMix64};
use inf_store::SlotRouter;

use super::{Check, Scenario, SimClient, SubPlan, encode, subscription_plan};

/// Which cell's channels a publisher of `m1-pub-cross` publishes to: the
/// scenario's leg (`--pub-target`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PubTarget {
    /// The next cell, wrapping: two cells publish to each other's channels,
    /// more cells form a ring.
    Next,
    /// A cell other than the publisher's own, drawn per command.
    Others,
    /// The publisher's own cell: no `INF.PUB` crosses (the control).
    Own,
}

impl PubTarget {
    /// Every target, in the order the help text lists them.
    pub const ALL: [PubTarget; 3] = [PubTarget::Next, PubTarget::Others, PubTarget::Own];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            PubTarget::Next => "next",
            PubTarget::Others => "others",
            PubTarget::Own => "own",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<PubTarget> {
        PubTarget::ALL.into_iter().find(|target| target.name() == name)
    }
}

/// The cell client or subscriber `index` connects to: pinned round-robin
/// on `m1-pub-cross`, one seeded draw elsewhere, as every scenario drew it.
pub(super) fn placement(scenario: &Scenario, index: usize, rng: &mut SplitMix64) -> usize {
    match scenario.pub_target {
        Some(_) => index % usize::from(scenario.cells),
        None => (rng.next_u64() % u64::from(scenario.cells)) as usize,
    }
}

/// Subscribers: one per cell on `m1-pub-cross`, whatever `--cells` says.
pub(super) fn subscribers(scenario: &Scenario) -> usize {
    match scenario.pub_target {
        Some(_) => usize::from(scenario.cells),
        None => scenario.subscribers,
    }
}

/// Subscriber `index`'s plan: `PSUBSCRIBE chan:*` on `m1-pub-cross`.
pub(super) fn subscription(scenario: &Scenario, index: usize) -> SubPlan {
    match scenario.pub_target {
        Some(_) => SubPlan::Pattern,
        None => subscription_plan(index, scenario.channels.max(1)),
    }
}

/// The channels and keys each cell owns, and the publishers' target rule.
pub(super) struct CrossPlan {
    target: PubTarget,
    channels: Vec<Vec<u64>>,
    keys: Vec<Vec<u64>>,
}

impl CrossPlan {
    /// The plan of an `m1-pub-cross` run, `None` on every other scenario.
    ///
    /// # Errors
    /// A cell that owns none of the scenario's channels or keys: the load
    /// could not reach it, so the run says so instead of passing.
    pub(super) fn new(scenario: &Scenario) -> Option<Result<CrossPlan, String>> {
        let target = scenario.pub_target?;
        let cells = usize::from(scenario.cells);
        let router = SlotRouter::new_contiguous(scenario.cells);
        let owned = |count: u64, prefix: &str| {
            let mut by_cell = vec![Vec::new(); cells];
            for n in 0..count {
                let name = format!("{prefix}{n}");
                let owner = router.cell_of(SlotRouter::slot_of(name.as_bytes()));
                by_cell[usize::from(owner.0)].push(n);
            }
            by_cell
        };
        let channels = owned(scenario.channels, "chan:");
        let keys = owned(scenario.key_space, "key:");
        for (cell, (chans, owned_keys)) in channels.iter().zip(&keys).enumerate() {
            if chans.is_empty() || owned_keys.is_empty() {
                return Some(Err(format!(
                    "m1-pub-cross: cell {cell} owns {} of chan:0..{} and {} of key:0..{}: \
                     the load cannot reach it",
                    chans.len(),
                    scenario.channels,
                    owned_keys.len(),
                    scenario.key_space
                )));
            }
        }
        Some(Ok(CrossPlan { target, channels, keys }))
    }

    /// The client's next command: a `SET` or `GET` on a key its target cell
    /// owns every fourth command, else a `PUBLISH` to a channel that cell
    /// owns (its channel returned for the delivery ledger).
    pub(super) fn next_command(&self, client: &mut SimClient) -> (Vec<u8>, Check, Option<u64>) {
        let cells = self.channels.len();
        let target = match self.target {
            PubTarget::Next => (client.cell + 1) % cells,
            PubTarget::Own => client.cell,
            PubTarget::Others if cells == 1 => client.cell,
            PubTarget::Others => {
                let step = 1 + (client.rng.next_u64() % (cells as u64 - 1)) as usize;
                (client.cell + step) % cells
            }
        };
        if client.sent % 4 == 3 {
            let owned = &self.keys[target];
            let key =
                format!("key:{}", owned[(client.rng.next_u64() % owned.len() as u64) as usize]);
            let argv = if client.rng.next_u64().is_multiple_of(2) {
                vec![b"GET".to_vec(), key.into_bytes()]
            } else {
                let value = format!("v{}", client.sent);
                vec![b"SET".to_vec(), key.into_bytes(), value.into_bytes()]
            };
            return (encode(&argv), Check::None, None);
        }
        let owned = &self.channels[target];
        let chan = owned[(client.rng.next_u64() % owned.len() as u64) as usize];
        client.pub_seq[chan as usize] += 1;
        let payload = format!("m:{}:{}", client.id, client.pub_seq[chan as usize]);
        let argv =
            vec![b"PUBLISH".to_vec(), format!("chan:{chan}").into_bytes(), payload.into_bytes()];
        (encode(&argv), Check::None, Some(chan))
    }
}
