//! The server's timer roles (ADR-0154 D4 and its first amendment): one
//! owner per role on the cell's fixed timer set, every owner reserved by
//! [`ServerTimers::build`], the set's and the owners' one constructor, at
//! the cell's assembly, before its loop exists. So the set's `timer-owners`
//! census row is full at the serve mark, and a cell's timers are its roles'
//! owners, at most one arm each, however many hold episodes or accept
//! failures it sees.

use core::fmt;
use std::rc::Rc;

use inf_foundation::bounded::{AssemblyFull, CapCensus, CapError};
#[cfg(inf_canary_hold_arms_nothing)]
use inf_runtime::TimerArm;
use inf_runtime::{Reserved, TimerOwner, TimerSet};

use crate::limits::SERVER_TIMER_OWNERS;

/// A server cell's timer roles. A role's route is its index, which its
/// owner's notices carry back to the plane.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimerRole {
    /// Re-arms the listener's parked accept after an exhaustion failure,
    /// at most once a retry window while the failure lasts.
    AcceptRetry,
    /// The everysec tick: armed once the cell's durable plane exists, then
    /// its own successor a second later, from the delivery's time.
    Everysec,
    /// The fill hold's window (ADR-0089): armed at an episode's first hold,
    /// cancelled at its issue.
    FillHold,
    /// The FLUSH-class group hold's window (ADR-0092): the same shape.
    GroupHold,
    /// The tier-file retirement's retry (ADR-0154 A1, ADR-0156 D3): armed at
    /// a retried file's due when a consumed `Retry` finds it idle; its
    /// callback compares the latest due with the turn, no I/O, and that
    /// iteration's walk offers what is due.
    TierRetire,
}

impl TimerRole {
    /// Every role in route order, the order their owners are reserved.
    pub const ALL: [TimerRole; 5] = [
        TimerRole::AcceptRetry,
        TimerRole::Everysec,
        TimerRole::FillHold,
        TimerRole::GroupHold,
        TimerRole::TierRetire,
    ];

    /// The roles, and so the server's timer positions per cell: one listener's
    /// accept retry, the three durable roles and the tier-file retirement's.
    pub const COUNT: u32 = TimerRole::ALL.len() as u32;

    /// The route the role's owner is reserved with.
    #[must_use]
    pub const fn route(self) -> u64 {
        self as u64
    }

    /// The role a notice's route names, `None` for a route no role holds.
    #[must_use]
    pub fn from_route(route: u64) -> Option<TimerRole> {
        TimerRole::ALL.into_iter().find(|role| role.route() == route)
    }

    /// The role's name in a refusal's text.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            TimerRole::AcceptRetry => "accept-retry",
            TimerRole::Everysec => "everysec",
            TimerRole::FillHold => "fill-hold",
            TimerRole::GroupHold => "group-hold",
            TimerRole::TierRetire => "tier-retire",
        }
    }
}

impl fmt::Display for TimerRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a cell's timer assembly was refused: the set's build (`Build`, the
/// census's or the allocator's refusal), or a role's reserve (`Full`, a cap
/// short of the roles; `Exhausted`, positions retired). The last two are
/// derived never on a correct build: the cap is the role count and a fresh
/// set has retired nothing. Each prints its own token, never a derived
/// `Debug`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimerAssemblyError {
    Build(CapError),
    Full(AssemblyFull, TimerRole),
    Exhausted(TimerRole),
}

impl fmt::Display for TimerAssemblyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimerAssemblyError::Build(refused) => write!(f, "{refused}"),
            TimerAssemblyError::Full(full, role) => write!(f, "{full}: {role}"),
            TimerAssemblyError::Exhausted(role) => {
                write!(f, "{}-exhausted-at-assembly: {role}", SERVER_TIMER_OWNERS.name())
            }
        }
    }
}

impl std::error::Error for TimerAssemblyError {}

/// The owners of the three durable roles. The plane holds them until its
/// durable plane is built, which then holds them for the cell's life.
#[derive(Debug)]
pub(crate) struct DurableTimers {
    pub(crate) everysec: TimerOwner,
    pub(crate) fill_hold: TimerOwner,
    pub(crate) group_hold: TimerOwner,
    /// Canary: each hold owner's arm, cancelled at the build, which its
    /// episodes hold in place of an arm of their own.
    #[cfg(inf_canary_hold_arms_nothing)]
    pub(crate) dead_fill: Option<TimerArm>,
    #[cfg(inf_canary_hold_arms_nothing)]
    pub(crate) dead_group: Option<TimerArm>,
}

/// A server cell's timer owners, one per role, as the assembly reserves
/// them. The plane keeps the accept retry's, the durable roles' with the
/// seat of its ack gate until its durable plane takes them, and the tier
/// retirement's until its tier cell takes it. No role closes its owner, so
/// no position frees after the assembly.
#[derive(Debug)]
pub(crate) struct ServerTimers {
    pub(crate) accept_retry: TimerOwner,
    pub(crate) durable: DurableTimers,
    pub(crate) tier_retire: TimerOwner,
}

impl ServerTimers {
    /// The cell's timer set on `census` and its owners, one a role, every one
    /// reserved here, before the cell's loop exists. A refusal returns no
    /// set: what it built drops with it, while the census is still
    /// assembling, an abandoned assembly the census does not count.
    pub(crate) fn build(
        census: &Rc<CapCensus>,
    ) -> Result<(TimerSet, ServerTimers), TimerAssemblyError> {
        let mut set =
            TimerSet::build(SERVER_TIMER_OWNERS, census).map_err(TimerAssemblyError::Build)?;
        let [accept_retry, everysec, fill_hold, group_hold, tier_retire] =
            TimerRole::ALL.map(|role| reserve(&mut set, role));
        let timers = ServerTimers {
            accept_retry: accept_retry?,
            durable: DurableTimers {
                everysec: everysec?,
                fill_hold: fill_hold?,
                group_hold: group_hold?,
                #[cfg(inf_canary_hold_arms_nothing)]
                dead_fill: None,
                #[cfg(inf_canary_hold_arms_nothing)]
                dead_group: None,
            },
            tier_retire: tier_retire?,
        };
        // Canary: each hold owner is armed and at once cancelled, one after
        // the other, before the loop exists; its episodes hold that dead arm.
        #[cfg(inf_canary_hold_arms_nothing)]
        let timers = {
            let mut timers = timers;
            let durable = &mut timers.durable;
            durable.dead_fill = dead_arm(&mut set, &durable.fill_hold);
            durable.dead_group = dead_arm(&mut set, &durable.group_hold);
            timers
        };
        Ok((set, timers))
    }
}

/// Canary: `owner` armed a millisecond out and cancelled at once.
#[cfg(inf_canary_hold_arms_nothing)]
fn dead_arm(set: &mut TimerSet, owner: &TimerOwner) -> Option<TimerArm> {
    use inf_foundation::time::Nanos;
    let arm = set.arm(owner, Nanos::from_millis(1), Nanos(0)).ok()?;
    let _ = set.cancel(&arm);
    Some(arm)
}

/// `role`'s owner, reserved with its route.
fn reserve(set: &mut TimerSet, role: TimerRole) -> Result<TimerOwner, TimerAssemblyError> {
    match set.reserve(role.route()) {
        Reserved::Owner(owner) => Ok(owner),
        Reserved::Full => {
            Err(TimerAssemblyError::Full(AssemblyFull::new(SERVER_TIMER_OWNERS), role))
        }
        Reserved::Exhausted => Err(TimerAssemblyError::Exhausted(role)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inf_foundation::CellId;
    use inf_foundation::bounded::{CapFill, CensusPhase};

    /// Every role has its owner, routed by its index, and the census row is
    /// full: every role's owner live before the mark, none after it violated.
    #[test]
    fn the_assembly_reserves_one_owner_per_role_and_fills_its_row() {
        let (census, mark) = CapCensus::new(CellId(3));
        let (set, timers) = ServerTimers::build(&census).expect("assembled");
        let durable = &timers.durable;
        let positions = [
            timers.accept_retry.position(),
            durable.everysec.position(),
            durable.fill_hold.position(),
            durable.group_hold.position(),
            timers.tier_retire.position(),
        ];
        assert_eq!(positions, [0, 1, 2, 3, 4]);
        assert_eq!(set.capacity_now(), TimerRole::COUNT);
        let row = census.row(SERVER_TIMER_OWNERS.name()).expect("the timer-owners row");
        assert_eq!(row.fill, CapFill::Assembly);
        assert_eq!(
            (row.entries_max, row.high_water, row.crossings),
            (TimerRole::COUNT, TimerRole::COUNT, 0)
        );
        mark.take();
        assert_eq!(census.phase(), CensusPhase::Serving);
        assert_eq!(census.cap_assembly_violations(), 0);
        // The cell stops before its set drops, as a loop's `Drop` marks it.
        census.mark_stopping();
        drop(set);
    }

    #[test]
    fn a_route_names_its_role() {
        for role in TimerRole::ALL {
            assert_eq!(TimerRole::from_route(role.route()), Some(role));
        }
        assert_eq!(TimerRole::from_route(u64::from(TimerRole::COUNT)), None);
    }

    /// Each refusal prints its token: the census row's name in E's form for
    /// `Full`, and M's own for `Exhausted`.
    #[test]
    fn a_refused_assembly_prints_its_token() {
        let full =
            TimerAssemblyError::Full(AssemblyFull::new(SERVER_TIMER_OWNERS), TimerRole::GroupHold);
        assert_eq!(full.to_string(), "timer-owners-full-at-assembly: group-hold");
        let exhausted = TimerAssemblyError::Exhausted(TimerRole::Everysec);
        assert_eq!(exhausted.to_string(), "timer-owners-exhausted-at-assembly: everysec");
        let build = TimerAssemblyError::Build(CapError::Census);
        assert_eq!(build.to_string(), CapError::Census.to_string());
    }

    /// A second assembly on one census is the census's refusal (an
    /// `Assembly` name registers once), and it builds nothing.
    #[test]
    fn a_second_assembly_on_one_census_is_refused() {
        let (census, _mark) = CapCensus::new(CellId(0));
        let _first = ServerTimers::build(&census).expect("assembled");
        let second = ServerTimers::build(&census).map(|_| ());
        assert_eq!(second, Err(TimerAssemblyError::Build(CapError::Census)));
    }
}
