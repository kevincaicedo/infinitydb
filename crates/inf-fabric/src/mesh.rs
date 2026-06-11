//! Mesh, doorbells, and credit flow control (M0-S09).
//!
//! Topology: one SPSC ring per **directed** cell pair — N×(N−1) rings for N
//! cells — plus one single-writer doorbell flag per directed pair. Each cell
//! owns a [`CellFabric`] handle holding the producers toward every peer and
//! the consumers from every peer; the handle moves to the cell's thread and
//! is never shared (L1).
//!
//! ## Credit protocol (deadlock-free by construction)
//!
//! - A **data op** (`Read`/`Write`/`Apply`; each op nested in a `Batch`
//!   counts individually) consumes one credit toward its destination at
//!   [`CellFabric::send`] time. No credits ⇒ `Err(SendError::NoCredit)` and
//!   the caller backpressures the *originating connection* (the server stops
//!   re-arming its recv — pressure goes to TCP, never into unbounded queues).
//! - Every data op is answered by exactly one `Reply`; draining the reply at
//!   the origin returns its credit.
//! - **Replies are always sendable**: each directed ring is sized
//!   `≥ 2 × data_credits`, so even with both directions saturated (D data
//!   frames from us + D replies owed to the peer's D outstanding ops) the
//!   ring cannot be full of unsendable traffic, and [`CellFabric::drain`]
//!   never has to block to make room — there is no wait-for cycle.
//!
//! Producer-side memory is exactly bounded: per destination, in-flight data
//! frames ≤ `data_credits` (each frame consumes ≥ 1 credit) and staged
//! replies ≤ the peer's outstanding ops ≤ `data_credits` — both 64-byte slot
//! class (spills tripwired, see [`FabricStats::spilled_frames`]).
//!
//! ## Doorbells
//!
//! One `AtomicBool` per directed pair. The producer rings it (`Release`)
//! after publishing frames at FABRIC-OUT; the consumer takes it (`Acquire`
//! swap) at FABRIC-IN. Single writer per side, no contended RMW storms. The
//! parked-reactor wakeup integration (kqueue `EVFILT_USER` / uring
//! `MSG_RING`) is server-layer work (E5) — at M0 cells poll doorbells each
//! loop iteration, which the spin-then-park idle strategy already amortizes.
//!
//! ## Validation notes (Linux box)
//!
//! The 10⁷-op DST deadlock battery (all-to-all saturation, ring-full storms,
//! "no progress stall > 10 ms simulated") belongs to the simulator (M0-S20);
//! the threaded smoke test here is the dev-tier stand-in. `perf c2c`
//! false-sharing attribution for ring/doorbell lines is Linux-only.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use inf_foundation::CellId;

use crate::codec::{Op, Outcome, encode};
use crate::msg::{FabricMsg, FabricToken};
use crate::ring::{Consumer, Producer, ring};

/// Mesh tuning. `ring_capacity` is per directed pair and must be a power of
/// two ≥ `2 × data_credits` (the reply-headroom invariant above).
#[derive(Copy, Clone, Debug)]
pub struct MeshConfig {
    pub ring_capacity: usize,
    pub data_credits: u32,
}

impl Default for MeshConfig {
    fn default() -> MeshConfig {
        MeshConfig { ring_capacity: 1024, data_credits: 512 }
    }
}

/// Why a send was refused.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SendError {
    /// Destination credit budget exhausted: `needed > available`. Caller
    /// must backpressure the originating connection and retry after replies
    /// drain. Never queue unboundedly around this.
    NoCredit { needed: u32, available: u32 },
}

impl core::fmt::Display for SendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SendError::NoCredit { needed, available } => {
                write!(f, "fabric credits exhausted (need {needed}, have {available})")
            }
        }
    }
}

impl std::error::Error for SendError {}

/// Single-writer doorbell flag for one directed pair.
#[derive(Debug, Default)]
struct Doorbell(AtomicBool);

impl Doorbell {
    /// Producer side: announce published frames.
    #[inline]
    fn ring(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Consumer side: consume the signal.
    #[inline]
    fn take(&self) -> bool {
        self.0.swap(false, Ordering::Acquire)
    }

    /// Consumer side: peek without consuming.
    #[inline]
    fn pending(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Per-destination outbound state.
struct Outbound {
    producer: Producer<FabricMsg>,
    doorbell: Arc<Doorbell>,
    /// Frames staged by `send`/`reply`, published at `flush` (FABRIC-OUT).
    staged: Vec<FabricMsg>,
    /// Remaining data credits toward this destination.
    credits: u32,
    /// Reused encode buffer — no per-send allocation.
    scratch: Vec<u8>,
}

/// Per-source inbound state.
struct Inbound {
    consumer: Consumer<FabricMsg>,
    doorbell: Arc<Doorbell>,
}

/// Always-on fabric counters (feeds `fabric_msgs_per_batch` and the spill
/// tripwire). Snapshot via [`CellFabric::stats`].
#[derive(Copy, Clone, Debug, Default)]
pub struct FabricStats {
    /// Frames published across all flushes.
    pub msgs_published: u64,
    /// Flush calls that published at least one frame.
    pub publish_batches: u64,
    /// Frames that exceeded [`crate::INLINE_MSG_CAP`] and heap-spilled.
    pub spilled_frames: u64,
    /// Frames drained whose bytes failed to decode (intra-process corruption
    /// — debug-asserted; counted instead of crashing in release).
    pub decode_errors: u64,
    /// Stale replies drained for tokens nobody waits on (counted, not fatal).
    pub orphan_replies: u64,
}

/// One cell's handle on the mesh: producers toward every peer, consumers
/// from every peer, credits, doorbells. Moves to its cell thread; all
/// methods take `&mut self` (single-owner, L1).
pub struct CellFabric {
    cell: CellId,
    cells: u16,
    next_seq: u64,
    config: MeshConfig,
    /// Indexed by destination cell id; `None` at `self.cell`.
    out: Vec<Option<Outbound>>,
    /// Indexed by source cell id; `None` at `self.cell`.
    inn: Vec<Option<Inbound>>,
    stats: FabricStats,
}

/// Mesh constructor: builds the N×(N−1) ring/doorbell graph and hands each
/// cell its [`CellFabric`].
#[derive(Debug)]
pub struct Mesh;

impl Mesh {
    /// Builds the full mesh for `cells` cells. Element `i` of the returned
    /// vec belongs to cell `i` (move it to that cell's thread).
    ///
    /// # Panics
    /// Panics if `cells == 0`, if `ring_capacity` is not a power of two, or
    /// if the reply-headroom invariant `ring_capacity ≥ 2 × data_credits`
    /// does not hold (the deadlock-freedom proof depends on it).
    // The mesh has no retained whole — construction hands each cell its
    // handle and disappears (frozen shape, interfaces-m0.md §4).
    #[allow(clippy::new_ret_no_self)]
    pub fn new(cells: u16, config: MeshConfig) -> Vec<CellFabric> {
        assert!(cells > 0, "mesh needs at least one cell");
        assert!(
            config.data_credits > 0 && config.ring_capacity >= 2 * config.data_credits as usize,
            "ring_capacity {} < 2 × data_credits {} — replies must always have headroom",
            config.ring_capacity,
            config.data_credits,
        );
        let n = usize::from(cells);
        let mut fabrics: Vec<CellFabric> = (0..cells)
            .map(|id| CellFabric {
                cell: CellId(id),
                cells,
                next_seq: 0,
                config,
                out: (0..n).map(|_| None).collect(),
                inn: (0..n).map(|_| None).collect(),
                stats: FabricStats::default(),
            })
            .collect();
        for src in 0..n {
            for dst in 0..n {
                if src == dst {
                    continue;
                }
                let (producer, consumer) = ring::<FabricMsg>(config.ring_capacity);
                let doorbell = Arc::new(Doorbell::default());
                fabrics[src].out[dst] = Some(Outbound {
                    producer,
                    doorbell: Arc::clone(&doorbell),
                    staged: Vec::new(),
                    credits: config.data_credits,
                    scratch: Vec::with_capacity(64),
                });
                fabrics[dst].inn[src] = Some(Inbound { consumer, doorbell });
            }
        }
        fabrics
    }
}

impl CellFabric {
    /// The cell this handle belongs to.
    #[inline]
    pub fn cell(&self) -> CellId {
        self.cell
    }

    /// Number of cells in the mesh.
    #[inline]
    pub fn cells(&self) -> u16 {
        self.cells
    }

    /// Mints the next reply-routing token (origin = this cell).
    #[inline]
    pub fn next_token(&mut self) -> FabricToken {
        let token = FabricToken::new(self.cell, self.next_seq);
        self.next_seq += 1;
        token
    }

    /// Credits a data op costs: one per point op, one per op nested in a
    /// batch (`Batch` headers are free — they collapse into one ring slot).
    fn credit_cost(op: &Op<'_>) -> u32 {
        match op {
            Op::Batch { ops } => ops.len() as u32,
            Op::Reply { .. } => 0,
            _ => 1,
        }
    }

    /// Remaining data credits toward `to` — the server's backpressure probe
    /// (out of credits ⇒ don't re-arm the originating connection's recv).
    pub fn credits(&self, to: CellId) -> u32 {
        self.outbound(to).credits
    }

    /// Stages a **data op** toward `to`, consuming its credits. The frame
    /// rides the next [`Self::flush`] (FABRIC-OUT).
    ///
    /// # Errors
    /// [`SendError::NoCredit`] when the destination budget cannot cover the
    /// op. Nothing is staged; the caller backpressures and retries later.
    ///
    /// # Panics
    /// Panics on `Op::Reply` (use [`Self::reply`] — replies are credit-free)
    /// and on `to == self.cell()` (local ops never ride the fabric).
    pub fn send(&mut self, to: CellId, op: &Op<'_>) -> Result<(), SendError> {
        assert!(
            !matches!(op, Op::Reply { .. }),
            "Op::Reply goes through CellFabric::reply (reserved headroom)"
        );
        let cost = Self::credit_cost(op);
        let spilled = {
            let outbound = self.outbound_mut(to);
            if outbound.credits < cost {
                return Err(SendError::NoCredit { needed: cost, available: outbound.credits });
            }
            outbound.credits -= cost;
            outbound.scratch.clear();
            encode(op, &mut outbound.scratch);
            let msg = FabricMsg::from_frame(&outbound.scratch);
            let spilled = msg.is_spilled();
            outbound.staged.push(msg);
            spilled
        };
        if spilled {
            self.stats.spilled_frames += 1;
        }
        Ok(())
    }

    /// Stages a reply toward `to`. Infallible by design: replies consume the
    /// reserved ring headroom (`ring_capacity − data_credits`), never data
    /// credits — this is what breaks the wait-for cycle.
    pub fn reply(&mut self, to: CellId, token: FabricToken, outcome: &Outcome<'_>) {
        let spilled = {
            let outbound = self.outbound_mut(to);
            outbound.scratch.clear();
            encode(&Op::Reply { token, outcome: *outcome }, &mut outbound.scratch);
            let msg = FabricMsg::from_frame(&outbound.scratch);
            let spilled = msg.is_spilled();
            outbound.staged.push(msg);
            spilled
        };
        if spilled {
            self.stats.spilled_frames += 1;
        }
    }

    /// FABRIC-OUT: publishes every staged frame that fits its ring (one
    /// `Release` store per non-empty destination) and rings doorbells.
    /// Returns frames published. Frames that didn't fit stay staged — by the
    /// sizing invariant that only happens transiently while the peer drains.
    pub fn flush(&mut self) -> usize {
        let mut published_total = 0;
        for outbound in self.out.iter_mut().flatten() {
            if outbound.staged.is_empty() {
                continue;
            }
            let mut pending = core::mem::take(&mut outbound.staged);
            let mut it = pending.drain(..);
            let published = outbound.producer.publish_batch(it.by_ref());
            outbound.staged = it.collect(); // unpublished remainder, in order
            if published > 0 {
                outbound.doorbell.ring();
                published_total += published;
                self.stats.msgs_published += published as u64;
            }
        }
        if published_total > 0 {
            self.stats.publish_batches += 1;
        }
        published_total
    }

    /// FABRIC-IN: drains up to `max` inbound frames (round-robin across
    /// peers, bounded), decoding each and handing it to `f(from, op)`.
    /// `Op::Reply` frames return their credit to the `from` destination
    /// *before* `f` sees them. Returns frames drained.
    ///
    /// Never blocks and never sends — this unconditional progress is half of
    /// the deadlock-freedom argument (see module docs).
    pub fn drain(&mut self, max: usize, mut f: impl FnMut(CellId, Op<'_>)) -> usize {
        let mut drained_total = 0;
        let peers = self.inn.len();
        for source in 0..peers {
            if drained_total >= max {
                break;
            }
            let Some(inbound) = self.inn[source].as_mut() else { continue };
            inbound.doorbell.take();
            let quota = max - drained_total;
            // Split borrows: credits live in `out[source]`, frames in
            // `inn[source]` — disjoint fields.
            let outbound_credits = self.out[source].as_mut().map(|o| &mut o.credits);
            let stats = &mut self.stats;
            let mut credits = outbound_credits;
            drained_total +=
                inbound.consumer.consume_batch(quota, |msg| {
                    match crate::codec::decode(msg.frame()) {
                        Ok(op) => {
                            if matches!(op, Op::Reply { .. })
                                && let Some(credits) = credits.as_deref_mut()
                            {
                                *credits += 1;
                            }
                            f(CellId(source as u16), op);
                        }
                        Err(_) => {
                            debug_assert!(false, "fabric frame failed to decode in-process");
                            stats.decode_errors += 1;
                        }
                    }
                });
        }
        drained_total
    }

    /// True if any peer has rung our doorbell since the last drain — the
    /// loop's cheap "is FABRIC-IN worth running" probe.
    pub fn doorbell_pending(&self) -> bool {
        self.inn.iter().flatten().any(|inbound| inbound.doorbell.pending())
    }

    /// Frames staged but not yet published (tests + FABRIC-OUT accounting).
    pub fn staged_frames(&self) -> usize {
        self.out.iter().flatten().map(|o| o.staged.len()).sum()
    }

    /// Outstanding (un-replied) data ops toward `to` — the exact
    /// producer-side memory bound: `outstanding ≤ data_credits` always.
    pub fn outstanding(&self, to: CellId) -> u32 {
        self.config.data_credits - self.outbound(to).credits
    }

    /// Always-on counters snapshot.
    pub fn stats(&self) -> FabricStats {
        self.stats
    }

    /// Count an orphan reply (token no longer waited on — e.g. the waiter
    /// was cancelled). Called by the drain consumer; kept here so the
    /// tripwire set lives in one place.
    pub fn note_orphan_reply(&mut self) {
        self.stats.orphan_replies += 1;
    }

    fn outbound(&self, to: CellId) -> &Outbound {
        self.out[to.as_usize()]
            .as_ref()
            .unwrap_or_else(|| panic!("cell {} has no fabric route to {to}", self.cell))
    }

    fn outbound_mut(&mut self, to: CellId) -> &mut Outbound {
        let cell = self.cell;
        self.out[to.as_usize()]
            .as_mut()
            .unwrap_or_else(|| panic!("cell {cell} has no fabric route to {to}"))
    }
}

impl core::fmt::Debug for CellFabric {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "CellFabric {{ cell: {}, cells: {}, staged: {}, stats: {:?} }}",
            self.cell,
            self.cells,
            self.staged_frames(),
            self.stats
        )
    }
}
