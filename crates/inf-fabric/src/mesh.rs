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

/// Bytes after which an open pack seals into a ring slot (also the rough
/// per-slot spill size — one heap allocation amortized over the whole pack
/// instead of one per >62 B frame, M0-R1).
const PACK_SEAL_BYTES: usize = 2048;
/// Frames after which an open pack seals (bounds frames-per-slot so a
/// bounded drain overshoots its frame budget by at most one chunk's worth).
const PACK_SEAL_FRAMES: u32 = 64;
/// Slots consumed per `consume_batch` call inside `drain` — small, so the
/// frame budget is re-checked between chunks.
const DRAIN_SLOT_CHUNK: usize = 8;

/// Per-destination outbound state.
struct Outbound {
    producer: Producer<FabricMsg>,
    doorbell: Arc<Doorbell>,
    /// Sealed slots awaiting `flush` (FABRIC-OUT publication).
    staged: Vec<FabricMsg>,
    /// Remaining data credits toward this destination.
    credits: u32,
    /// Open pack: concatenated encoded frames (each self-delimiting via the
    /// codec header) that will seal into ONE ring slot. This is the M0-R1
    /// frame-packing remediation: per-op slots and per-op spill allocations
    /// were a top cross-cell cost (`.artifacts/m0/2026-06-12-linux-devbox/`).
    pack: Vec<u8>,
    /// Frames currently in the open pack.
    pack_frames: u32,
}

impl Outbound {
    /// Seals the open pack into one staged ring slot. Returns whether the
    /// sealed slot heap-spilled (one allocation for the whole pack).
    fn seal(&mut self) -> bool {
        if self.pack.is_empty() {
            return false;
        }
        let msg = FabricMsg::from_frame(&self.pack);
        let spilled = msg.is_spilled();
        self.staged.push(msg);
        self.pack.clear();
        self.pack_frames = 0;
        spilled
    }

    /// Seals when the open pack hits its byte or frame cap.
    fn seal_if_full(&mut self) -> bool {
        if self.pack.len() >= PACK_SEAL_BYTES || self.pack_frames >= PACK_SEAL_FRAMES {
            self.seal()
        } else {
            false
        }
    }
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
    /// Per-cell parked flags (single-writer, set by each cell's park
    /// handshake) — read at flush to decide a doorbell wakeup (M0-R1).
    park_flags: Option<Arc<Vec<AtomicBool>>>,
    /// Wakes a parked peer's reactor. `dyn` is fine: invoked only on the
    /// ring-toward-parked-peer cold path.
    peer_wake: Option<Box<dyn Fn(CellId) + Send>>,
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
                park_flags: None,
                peer_wake: None,
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
                    pack: Vec::with_capacity(PACK_SEAL_BYTES),
                    pack_frames: 0,
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
        let sealed_spill = {
            let outbound = self.outbound_mut(to);
            if outbound.credits < cost {
                return Err(SendError::NoCredit { needed: cost, available: outbound.credits });
            }
            outbound.credits -= cost;
            encode(op, &mut outbound.pack);
            outbound.pack_frames += 1;
            outbound.seal_if_full()
        };
        if sealed_spill {
            self.stats.spilled_frames += 1;
        }
        Ok(())
    }

    /// Stages a reply toward `to`. Infallible by design: replies consume the
    /// reserved ring headroom (`ring_capacity − data_credits`), never data
    /// credits — this is what breaks the wait-for cycle. Replies ride the
    /// same per-destination pack as data ops, in send order (the origin's
    /// delivery-time RTT bookkeeping depends on reply FIFO per pair).
    pub fn reply(&mut self, to: CellId, token: FabricToken, outcome: &Outcome<'_>) {
        let sealed_spill = {
            let outbound = self.outbound_mut(to);
            encode(&Op::Reply { token, outcome: *outcome }, &mut outbound.pack);
            outbound.pack_frames += 1;
            outbound.seal_if_full()
        };
        if sealed_spill {
            self.stats.spilled_frames += 1;
        }
    }

    /// FABRIC-OUT: publishes every staged frame that fits its ring (one
    /// `Release` store per non-empty destination) and rings doorbells.
    /// Returns frames published. Frames that didn't fit stay staged — by the
    /// sizing invariant that only happens transiently while the peer drains.
    pub fn flush(&mut self) -> usize {
        let mut published_total = 0;
        let mut seal_spills = 0;
        for (dst, slot) in self.out.iter_mut().enumerate() {
            let Some(outbound) = slot.as_mut() else { continue };
            if outbound.seal() {
                seal_spills += 1;
            }
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
                // Doorbell wakeups (M0-R1): if the peer is parked (or about
                // to park), kick its reactor. The SeqCst fence pairs with
                // the peer's park handshake (flag store → fence → doorbell
                // check) so either we see its parked flag or it sees our
                // doorbell — a missed wake degrades to the park timeout,
                // never a hang.
                if let (Some(flags), Some(wake)) = (&self.park_flags, &self.peer_wake) {
                    std::sync::atomic::fence(Ordering::SeqCst);
                    if flags[dst].load(Ordering::Relaxed) {
                        wake(CellId(dst as u16));
                    }
                }
            }
        }
        self.stats.spilled_frames += seal_spills;
        if published_total > 0 {
            self.stats.publish_batches += 1;
        }
        published_total
    }

    /// Wires doorbell wakeups (M0-R1): `park_flags[i]` is set by cell `i`'s
    /// park handshake; `wake(i)` must end cell `i`'s kernel park (e.g. a
    /// `LoopWaker` eventfd write). Cold path — invoked only when a doorbell
    /// rings toward a parked peer.
    pub fn set_wakeups(
        &mut self,
        park_flags: Arc<Vec<AtomicBool>>,
        wake: impl Fn(CellId) + Send + 'static,
    ) {
        self.park_flags = Some(park_flags);
        self.peer_wake = Some(Box::new(wake));
    }

    /// FABRIC-IN: drains inbound frames up to a budget of `max` (round-robin
    /// across peers, bounded), decoding each and handing it to `f(from, op)`.
    /// `Op::Reply` frames return their credit to the `from` destination
    /// *before* `f` sees them. Returns frames drained.
    ///
    /// Slots are packed (M0-R1): one slot carries up to [`PACK_SEAL_FRAMES`]
    /// concatenated frames, decoded in send order. Slots are consumed in
    /// chunks of [`DRAIN_SLOT_CHUNK`] so the frame budget overshoots by at
    /// most one chunk's packing — bounded, like every step budget.
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
            // Split borrows: credits live in `out[source]`, frames in
            // `inn[source]` — disjoint fields.
            let mut credits = self.out[source].as_mut().map(|o| &mut o.credits);
            let stats = &mut self.stats;
            while drained_total < max {
                let chunk = DRAIN_SLOT_CHUNK.min(max - drained_total);
                let mut frames = 0usize;
                let consumed = inbound.consumer.consume_batch(chunk, |msg| {
                    let mut bytes = msg.frame();
                    while !bytes.is_empty() {
                        match crate::codec::decode_prefix(bytes) {
                            Ok((op, used)) => {
                                if matches!(op, Op::Reply { .. })
                                    && let Some(credits) = credits.as_deref_mut()
                                {
                                    *credits += 1;
                                }
                                f(CellId(source as u16), op);
                                frames += 1;
                                bytes = &bytes[used..];
                            }
                            Err(_) => {
                                debug_assert!(false, "fabric frame failed to decode in-process");
                                stats.decode_errors += 1;
                                break;
                            }
                        }
                    }
                });
                drained_total += frames;
                if consumed < chunk {
                    break;
                }
            }
        }
        drained_total
    }

    /// True if any peer has rung our doorbell since the last drain — the
    /// loop's cheap "is FABRIC-IN worth running" probe.
    pub fn doorbell_pending(&self) -> bool {
        self.inn.iter().flatten().any(|inbound| inbound.doorbell.pending())
    }

    /// Transport slots pending publication: sealed slots plus one for each
    /// destination with an open (non-empty) pack. Nonzero ⟺ unflushed bytes
    /// exist (tests + FABRIC-OUT accounting).
    pub fn staged_frames(&self) -> usize {
        self.out.iter().flatten().map(|o| o.staged.len() + usize::from(!o.pack.is_empty())).sum()
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
