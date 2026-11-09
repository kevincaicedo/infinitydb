//! The plane's checked mesh and its two credit paths (ADR-0192).
//!
//! A reply the plane defers past the drain pass that read its op, the owner
//! pump's answer to a remote `PUBLISH`, waits on the fan legs it sends to
//! the peers. Were those legs to draw from the credits every other send
//! draws from, two cells publishing to each other's channels could hold
//! every credit of both directions in unanswered `INF.PUB`s, each pump
//! waiting for a credit only the other pump's reply returns. So the plane
//! spends a cell's credits toward a peer through two paths only: a general
//! send stages only while at least one credit toward its peer stays free
//! after it, and the pump's fan legs may take that last one, at most one in
//! flight toward each peer. A leg never waits, and the pump's reply waits
//! only on acks each peer sends at its drain. [`PlaneFabric`] is the one
//! holder of the cell's `CellFabric` and has no other path to its `send`.
//!
//! Every mesh the plane runs on is checked once, by [`checked_mesh_config`]:
//! the shipped binary's mesh and every simulator mesh are `const`s through
//! it, so a refused pair fails the build, and a mesh sized by a flag is
//! refused at the flag before any cell exists. The check keeps no copy of
//! the fabric's ring relation: it reads
//! [`MeshConfig::has_reply_headroom`], the relation's one statement.

use core::cell::Cell;
use core::fmt;

use inf_fabric::{
    ApplyArgs, CellFabric, FabricStats, FabricToken, MeshConfig, Op, Outcome, SendError,
};
use inf_foundation::{CellId, KeySlot};
use inf_runtime::GateWait;
use inf_store::SlotRouter;

use super::OwnedOutcome;
use crate::limits::FAN_RESERVE_PER_PEER;

/// Why a mesh the plane would run on was refused; each variant names the
/// numbers it refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeshConfigRefused {
    /// No general window: every credit toward a peer would be the publish
    /// fan's reserve or less.
    BelowFanReserve { data_credits: u32, floor: u32 },
    /// The fabric's rings are power-of-two sized.
    RingNotPowerOfTwo { ring_capacity: usize },
    /// The ring cannot hold the peer's replies beside this side's frames.
    NoReplyHeadroom { ring_capacity: usize, data_credits: u32 },
}

impl fmt::Display for MeshConfigRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            MeshConfigRefused::BelowFanReserve { data_credits, floor } => write!(
                f,
                "data_credits {data_credits} leaves no general window beside the publish fan's \
                 reserved credit: at least {floor}"
            ),
            MeshConfigRefused::RingNotPowerOfTwo { ring_capacity } => {
                write!(f, "ring_capacity {ring_capacity} is not a power of two")
            }
            MeshConfigRefused::NoReplyHeadroom { ring_capacity, data_credits } => write!(
                f,
                "ring_capacity {ring_capacity} has no reply headroom for data_credits \
                 {data_credits}: at most {}",
                ring_capacity / 2
            ),
        }
    }
}

impl std::error::Error for MeshConfigRefused {}

/// The one check of a mesh the plane runs on (ADR-0192 D5): more data
/// credits per directed pair than the publish fan's reserve, a power-of-two
/// ring, and the fabric's reply headroom. `const`, so a mesh written as a
/// constant is checked when the program is built.
///
/// # Errors
/// [`MeshConfigRefused`], in that order of checks.
pub const fn checked_mesh_config(
    ring_capacity: usize,
    data_credits: u32,
) -> Result<MeshConfig, MeshConfigRefused> {
    if data_credits <= FAN_RESERVE_PER_PEER {
        return Err(MeshConfigRefused::BelowFanReserve {
            data_credits,
            floor: FAN_RESERVE_PER_PEER + 1,
        });
    }
    if !ring_capacity.is_power_of_two() {
        return Err(MeshConfigRefused::RingNotPowerOfTwo { ring_capacity });
    }
    let config = MeshConfig { ring_capacity, data_credits };
    if !config.has_reply_headroom() {
        return Err(MeshConfigRefused::NoReplyHeadroom { ring_capacity, data_credits });
    }
    Ok(config)
}

// ---- the two staging paths (ADR-0192 D2, D3) -------------------------------------

/// Credits a general send needs free toward its peer: one for its point op,
/// and the publish fan's reserve left behind it.
const GENERAL_CREDITS_NEEDED: u32 = FAN_RESERVE_PER_PEER + 1;

/// The plane's one holder of its cell's [`CellFabric`]: every method the
/// plane calls but `send`, delegated under the same names, and the two paths
/// that stage a data op, [`PlaneFabric::stage_general`] and
/// [`PlaneFabric::stage_fan_leg`] (ADR-0192 D2). No other path reaches the
/// fabric's `send`, so a new sender picks one of the two.
pub(super) struct PlaneFabric {
    fabric: CellFabric,
}

impl PlaneFabric {
    /// Wraps the cell's handle and makes the cell's one [`FanReserve`].
    /// The shipped binary and the simulator build every mesh through
    /// [`checked_mesh_config`]; a test's literal mesh is held here, in
    /// debug builds, to the same floor.
    pub(super) fn new(fabric: CellFabric) -> (PlaneFabric, FanReserve) {
        debug_assert!(
            (0..fabric.cells())
                .map(CellId)
                .filter(|&peer| peer != fabric.cell())
                .all(|peer| fabric.credits(peer) >= GENERAL_CREDITS_NEEDED),
            "a plane's mesh keeps a general window beside the publish fan's reserve"
        );
        (PlaneFabric { fabric }, FanReserve { _sealed: () })
    }

    /// Stages a general op toward `to` only if the publish fan's reserve
    /// stays free after it: `credits(to) > FAN_RESERVE_PER_PEER`, one
    /// comparison, since a [`PointOp`] costs one credit. A refusal is the
    /// fabric's own `NoCredit`, which each funnel answers as it did before
    /// the reserve: `send_apply` and `send_apply_ns` park on the cell's
    /// credit waiters, `try_send_apply` falls back to the async path, and the
    /// accept hand-off keeps the socket local.
    ///
    /// # Errors
    /// [`SendError::NoCredit`] with `needed` the op and the reserve.
    pub(super) fn stage_general(&mut self, to: CellId, op: &PointOp<'_>) -> Result<(), SendError> {
        self.keep_reserve(to)?;
        self.fabric.send(to, &op.0)?;
        // A point op cost one credit, so the reserve is still free. This
        // reads it on a stage made at the floor; the unit test reads that
        // each stage costs exactly one at any count.
        debug_assert!(
            self.fabric.credits(to) >= FAN_RESERVE_PER_PEER,
            "a general op took the publish fan's reserved credit"
        );
        Ok(())
    }

    /// The reserve's guard: one comparison on the fabric's credit probe.
    fn keep_reserve(&self, to: CellId) -> Result<(), SendError> {
        let available = self.fabric.credits(to);
        if available >= GENERAL_CREDITS_NEEDED {
            return Ok(());
        }
        Err(SendError::NoCredit { needed: GENERAL_CREDITS_NEEDED, available })
    }

    /// Stages one publish fan leg toward `to`, which may take the credit
    /// general sends leave free. Legs of one fan go in strictly ascending
    /// peer order, so a fan has at most one leg toward each peer, and the
    /// cell's one [`FanReserve`], which the open fan borrows, makes a fan
    /// settle before the next opens: when a fan opens no leg is in flight
    /// and each peer has a free credit, so a leg never waits (ADR-0192 D3).
    ///
    /// # Errors
    /// [`FanLegRefused`], derived never: a peer at or below the fan's last,
    /// or no credit free. Nothing is staged, and the refusal is counted
    /// once, in the cell's `fabric_reserve_refused_cell`.
    pub(super) fn stage_fan_leg(
        &mut self,
        legs: &mut FanLegs<'_>,
        to: CellId,
        op: &FanLegOp<'_>,
    ) -> Result<(), FanLegRefused> {
        let refused = if legs.last.is_some_and(|last| to.0 <= last.0) {
            FanLegRefused::NotAscending
        } else {
            match self.fabric.send(to, &op.0) {
                Ok(()) => {
                    legs.last = Some(to);
                    return Ok(());
                }
                Err(SendError::NoCredit { .. }) => FanLegRefused::NoCredit,
            }
        };
        legs.refused.set(legs.refused.get() + 1);
        Err(refused)
    }

    pub(super) fn next_token(&mut self) -> FabricToken {
        self.fabric.next_token()
    }

    pub(super) fn reply(&mut self, to: CellId, token: FabricToken, outcome: &Outcome<'_>) {
        self.fabric.reply(to, token, outcome);
    }

    pub(super) fn flush(&mut self) -> usize {
        self.fabric.flush()
    }

    pub(super) fn drain(&mut self, max: usize, f: impl FnMut(CellId, Op<'_>)) -> usize {
        self.fabric.drain(max, f)
    }

    pub(super) fn doorbell_pending(&self) -> bool {
        self.fabric.doorbell_pending()
    }

    pub(super) fn staged_frames(&self) -> usize {
        self.fabric.staged_frames()
    }

    pub(super) fn note_orphan_reply(&mut self) {
        self.fabric.note_orphan_reply();
    }

    pub(super) fn stats(&self) -> FabricStats {
        self.fabric.stats()
    }
}

/// One data op that costs one credit: built only as an `Apply`, an
/// `ApplyNs` or an `AdoptConn`, the shapes the plane's general funnels send,
/// never a `Batch` (one credit per member) or a `Reply` (credit-free, sent
/// through `reply`), so the reserve's guard is one comparison and no cost
/// is computed or copied here.
pub(super) struct PointOp<'a>(Op<'a>);

impl<'a> PointOp<'a> {
    pub(super) fn apply(
        token: FabricToken,
        slot: KeySlot,
        cmd: u8,
        args: ApplyArgs<'a>,
        program: bool,
    ) -> PointOp<'a> {
        PointOp(Op::Apply { token, slot, cmd, args, program })
    }

    pub(super) fn apply_ns(
        token: FabricToken,
        slot: KeySlot,
        cmd: u8,
        ns: u32,
        args: ApplyArgs<'a>,
        program: bool,
    ) -> PointOp<'a> {
        PointOp(Op::ApplyNs { token, slot, cmd, ns, args, program })
    }

    pub(super) fn adopt_conn(token: FabricToken, fd: u32) -> PointOp<'static> {
        PointOp(Op::AdoptConn { token, fd })
    }
}

/// The argv of one publish fan leg, `INF.PUBFAN channel payload`, with the
/// publisher's `(conn, seq)` tag on the leg to the publisher's own cell
/// (ADR-0101 D2): the one spelling, read by the owner pump's legs and the
/// owner-local fan alike.
pub(super) struct PubFanArgv<'a> {
    argv: [&'a [u8]; 5],
    len: usize,
}

impl<'a> PubFanArgv<'a> {
    pub(super) fn new(
        channel: &'a [u8],
        payload: &'a [u8],
        tag: Option<(&'a [u8], &'a [u8])>,
    ) -> PubFanArgv<'a> {
        match tag {
            Some((conn, seq)) => {
                PubFanArgv { argv: [PUBFAN_NAME, channel, payload, conn, seq], len: 5 }
            }
            None => PubFanArgv { argv: [PUBFAN_NAME, channel, payload, b"", b""], len: 3 },
        }
    }

    pub(super) fn argv(&self) -> &[&'a [u8]] {
        &self.argv[..self.len]
    }

    /// The leg's op, which [`PlaneFabric::stage_fan_leg`] alone stages, as
    /// the general funnels build theirs: `cmd` packs the protocol and the
    /// database, the slot is the channel's, and the op is a program's.
    /// `None` only past the codec's argument cap, which five slices never
    /// reach.
    pub(super) fn op(&self, token: FabricToken, cmd: u8) -> Option<FanLegOp<'a>> {
        let args = ApplyArgs::new(self.argv())?;
        let slot = SlotRouter::slot_of(self.argv[1]);
        Some(FanLegOp(Op::Apply { token, slot, cmd, args, program: true }))
    }
}

/// The fan leg's verb.
const PUBFAN_NAME: &[u8] = b"INF.PUBFAN";

/// One fan leg's op: an `INF.PUBFAN` `Apply`, built only by
/// [`PubFanArgv::op`], so a leg carries no other op.
pub(super) struct FanLegOp<'a>(Op<'a>);

impl<'a> FanLegOp<'a> {
    /// A refused leg's fallback (derived never; ADR-0192 D3): the same
    /// `Apply`, a point op, staged through the general path and its wait.
    pub(super) fn into_general(self) -> PointOp<'a> {
        PointOp(self.0)
    }
}

/// The cell's one publish-fan reserve: made only by [`PlaneFabric::new`],
/// neither `Clone` nor `Copy`. The owner pump holds it while it runs and an
/// open [`FanLegs`] borrows it, so two fans of one cell are never open at
/// once.
#[derive(Debug)]
pub(super) struct FanReserve {
    _sealed: (),
}

/// One open fan: the reserve it borrows, the last peer a leg went to,
/// every leg's reply waiter in leg order, and the cell's count of refused
/// legs.
#[must_use = "a fan settles before the next one opens"]
pub(super) struct FanLegs<'r> {
    _reserve: &'r mut FanReserve,
    last: Option<CellId>,
    waiters: Vec<GateWait<u64, OwnedOutcome>>,
    refused: &'r Cell<u64>,
}

impl<'r> FanLegs<'r> {
    /// Opens a fan on the cell's reserve; `refused` is the cell's
    /// `fabric_reserve_refused_cell`.
    pub(super) fn open(reserve: &'r mut FanReserve, refused: &'r Cell<u64>) -> FanLegs<'r> {
        FanLegs { _reserve: reserve, last: None, waiters: Vec::new(), refused }
    }

    /// A staged leg's reply waiter, or a refused leg's general one: either
    /// way the fan's settle awaits it.
    pub(super) fn push(&mut self, waiter: GateWait<u64, OwnedOutcome>) {
        self.waiters.push(waiter);
    }

    /// Awaits every leg's reply and hands each outcome to `each`, in leg
    /// order; then the reserve's borrow ends. A leg's credit returned
    /// before its waiter completes, so after this no leg of the fan holds a
    /// credit.
    pub(super) async fn settle(self, mut each: impl FnMut(OwnedOutcome)) {
        for waiter in self.waiters {
            each(waiter.await);
        }
    }
}

/// Why [`PlaneFabric::stage_fan_leg`] refused a leg: derived never, so
/// each one is counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FanLegRefused {
    /// The peer is at or below the fan's last leg's peer.
    NotAscending,
    /// No credit toward the peer is free.
    NoCredit,
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use inf_fabric::{ApplyArgs, FabricToken, Mesh, MeshConfig, Outcome, SendError};
    use inf_foundation::{CellId, KeySlot};

    use super::{
        FanLegRefused, FanLegs, MeshConfigRefused, PlaneFabric, PointOp, PubFanArgv,
        checked_mesh_config,
    };

    const PEER: CellId = CellId(1);

    /// A two-cell mesh of 16 ring slots and four credits, which both the
    /// fabric's relation and its successor with a progress term admit: the
    /// plane on cell 0, and cell 1's raw handle to answer it.
    fn two_cells() -> (PlaneFabric, super::FanReserve, inf_fabric::CellFabric) {
        let mut cells = Mesh::new(2, MeshConfig { ring_capacity: 16, data_credits: 4 });
        let peer = cells.pop().expect("two cells");
        let (plane, reserve) = PlaneFabric::new(cells.pop().expect("two cells"));
        (plane, reserve, peer)
    }

    fn apply(plane: &mut PlaneFabric) -> PointOp<'static> {
        PointOp::apply(
            plane.next_token(),
            KeySlot::new(0).expect("slot 0"),
            2,
            ApplyArgs::EMPTY,
            true,
        )
    }

    fn leg(plane: &mut PlaneFabric) -> super::FanLegOp<'static> {
        let token = plane.next_token();
        PubFanArgv::new(b"chan", b"payload", None).op(token, 2).expect("five slices fit")
    }

    /// A fan with no leg left to await settles on its first poll.
    fn settle(legs: FanLegs<'_>) {
        let mut settled = pin!(legs.settle(|_| {}));
        let poll = settled.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(poll, Poll::Ready(())), "a fan with no waiter settles at once");
    }

    /// Each general stage, of each point shape, costs exactly one
    /// credit; the stage that would take the reserve is refused with the
    /// fabric's own `NoCredit`, and a fan leg then takes the last credit.
    #[test]
    fn a_general_stage_keeps_the_reserve_and_a_leg_takes_it() {
        let (mut plane, mut reserve, _peer) = two_cells();
        let refused = Cell::new(0);
        let token = plane.next_token();
        let ops = [
            apply(&mut plane),
            PointOp::apply_ns(
                token,
                KeySlot::new(0).expect("slot 0"),
                2,
                16,
                ApplyArgs::EMPTY,
                true,
            ),
            PointOp::adopt_conn(plane.next_token(), 7),
        ];
        for (stage, op) in ops.iter().enumerate() {
            let before = plane.fabric.credits(PEER);
            assert_eq!(plane.stage_general(PEER, op), Ok(()), "stage {stage}");
            assert_eq!(plane.fabric.credits(PEER), before - 1, "stage {stage} costs one credit");
        }
        let fourth = apply(&mut plane);
        assert_eq!(
            plane.stage_general(PEER, &fourth),
            Err(SendError::NoCredit { needed: 2, available: 1 })
        );
        let mut legs = FanLegs::open(&mut reserve, &refused);
        let op = leg(&mut plane);
        assert_eq!(plane.stage_fan_leg(&mut legs, PEER, &op), Ok(()));
        assert_eq!(plane.fabric.credits(PEER), 0, "the leg took the reserved credit");
        assert_eq!(refused.get(), 0);
        settle(legs);
    }

    /// A second leg to one peer in one fan is refused, nothing staged,
    /// and the refused leg's op then stages on the general path.
    #[test]
    fn a_second_leg_to_one_peer_is_refused_then_staged_general() {
        let (mut plane, mut reserve, _peer) = two_cells();
        let refused = Cell::new(0);
        let mut legs = FanLegs::open(&mut reserve, &refused);
        let first = leg(&mut plane);
        assert_eq!(plane.stage_fan_leg(&mut legs, PEER, &first), Ok(()));
        let second = leg(&mut plane);
        let before = plane.fabric.credits(PEER);
        assert_eq!(plane.stage_fan_leg(&mut legs, PEER, &second), Err(FanLegRefused::NotAscending));
        assert_eq!(plane.fabric.credits(PEER), before, "a refused leg stages nothing");
        assert_eq!(refused.get(), 1, "the refusal is counted once");
        assert_eq!(plane.stage_general(PEER, &second.into_general()), Ok(()));
        assert_eq!(plane.fabric.credits(PEER), before - 1);
        settle(legs);
    }

    /// Once a fan settles and its leg's reply is drained, the leg's
    /// credit is back and the next fan's leg stages on it.
    #[test]
    fn a_settled_fans_credit_returns_for_the_next_fan() {
        let (mut plane, mut reserve, mut peer) = two_cells();
        let refused = Cell::new(0);
        for _ in 0..3 {
            let op = apply(&mut plane);
            assert_eq!(plane.stage_general(PEER, &op), Ok(()));
        }
        let mut legs = FanLegs::open(&mut reserve, &refused);
        let op = leg(&mut plane);
        assert_eq!(plane.stage_fan_leg(&mut legs, PEER, &op), Ok(()));
        settle(legs);
        assert_eq!(plane.fabric.credits(PEER), 0);
        plane.flush();
        let mut leg_token: Option<FabricToken> = None;
        peer.drain(16, |_, op| {
            if let inf_fabric::Op::Apply { token, args, .. } = op
                && args.as_slice().first() == Some(&&b"INF.PUBFAN"[..])
            {
                leg_token = Some(token);
            }
        });
        let token = leg_token.expect("the leg reached the peer");
        peer.reply(CellId(0), token, &Outcome::Int(1));
        peer.flush();
        plane.drain(16, |_, _| {});
        assert_eq!(plane.fabric.credits(PEER), 1, "the leg's credit is back");
        let mut legs = FanLegs::open(&mut reserve, &refused);
        let op = leg(&mut plane);
        assert_eq!(plane.stage_fan_leg(&mut legs, PEER, &op), Ok(()));
        settle(legs);
        assert_eq!(refused.get(), 0, "no leg of either fan was refused");
    }

    /// Each case is one on which the fabric's relation and its successor
    /// with a progress term agree, so no verdict here rests on a copy of
    /// the relation; the boundary case reads the fabric's function.
    #[test]
    fn checked_mesh_config_refuses_each_shape_and_reads_the_fabric_relation() {
        assert!(checked_mesh_config(8, 2).is_ok());
        assert!(checked_mesh_config(4096, 1024).is_ok());
        assert_eq!(
            checked_mesh_config(8, 1).err(),
            Some(MeshConfigRefused::BelowFanReserve { data_credits: 1, floor: 2 })
        );
        assert_eq!(
            checked_mesh_config(8, 5).err(),
            Some(MeshConfigRefused::NoReplyHeadroom { ring_capacity: 8, data_credits: 5 })
        );
        assert_eq!(
            checked_mesh_config(12, 2).err(),
            Some(MeshConfigRefused::RingNotPowerOfTwo { ring_capacity: 12 })
        );
        let boundary = MeshConfig { ring_capacity: 8, data_credits: 4 };
        assert_eq!(checked_mesh_config(8, 4).is_ok(), boundary.has_reply_headroom());
    }
}
