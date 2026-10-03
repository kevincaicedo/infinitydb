//! `inf-store` limits: one `const` per bound, with its unit, its owner
//! and what a client observes when it is crossed (INFINITY_STYLE "Put a
//! limit on everything"). Bounds older than this module still live
//! beside their code; a new one lands here.

/// Control groups (16 slots each) one alias enumeration may **load**
/// (ADR-0139 D9). Charged by the traversal when it reads a group's
/// control bytes, fragment match or not: a chain that ends in its 32nd
/// group is complete, one that needs a 33rd is over. Crossing ⇒
/// `AliasWalk::Over`: the question is unanswered, the indexes it touches
/// degrade on the cell (`idx_alias_walk_over`), and a client sees the
/// typed degraded veto on statements — never a refused write.
pub const IDX_ALIAS_WALK_GROUPS_MAX: usize = 32;

/// Fragment matches one alias enumeration may **fetch** — one record
/// read and at most one keyed hash of ≤ 255 B each (ADR-0139 D9). Every
/// record the walk reads is charged before it is read, a write-set key
/// the bracket excludes by full key included; the walk stops **before**
/// the 17th. Crossing ⇒ `AliasWalk::Over`.
pub const IDX_ALIAS_REHASH_MAX: usize = 16;

/// Confirmed members of one alias group `G(h)` a **view** may hold —
/// the group minus whoever the caller excluded (ADR-0139 D9); the view
/// is a fixed array of this many addresses, so the common `|G| = 1` path
/// allocates nothing. Crossing (a 9th view member) ⇒ `AliasWalk::Over`.
pub const IDX_ALIAS_GROUP_MAX: usize = 8;

/// Encoded index-key bytes one bracket phase may hold (ADR-0139 D10;
/// **Proposed** value). Without it a ≈ 1 MiB document under 64 indexes
/// reaches 65,536 × 1,024 B = 64 MiB per phase. Crossing: a pre-image ⇒
/// the typed `EntryFlood` refusal, nothing changed; a post-image ⇒ the
/// participating indexes degrade (until the pre-apply admit refuses
/// it); a death hook ⇒ the index degrades (a death cannot refuse).
pub const BRACKET_KEY_BYTES_MAX: usize = 32 << 20;

/// The recovery step budget's price of one boot settle read (a cold
/// record's key window, ADR-0174 D3): 128 KiB, about 100 µs of a device
/// that moves 1 GiB/s, so a read is charged as the bytes the step could
/// have moved meanwhile. Owner: the end-of-replay settle walk, which adds
/// it per read to the bytes it walked. Crossing: the walk yields after
/// the record whose combined charge reaches the step's budget, and the
/// next step resumes at its cursor.
pub const SETTLE_READ_CHARGE_BYTES: u64 = 128 << 10;

/// Wheel nodes one store may hold (ADR-0008 A1 rule 7): the node's
/// `next` link is 24 bits and `NIL` (2²⁴ − 1) is reserved, so node indices
/// are `0..NIL` — a count of 2²⁴ − 1. This is the width bound, not a byte
/// budget: each node's 16 B is charged with its record to the namespace
/// budget that admitted the record. Crossing ⇒ the placement is
/// `Refused`: the record is swept instead of scheduled (`wheel_fallback`
/// + 1), never left to lazy expiry alone and never a client error.
pub const WHEEL_NODES_MAX: usize = (1 << 24) - 1;

/// Shards of the wheel's membership table (key hash → node), selected
/// by the hash's top byte (ADR-0008 A1 rule 7). Each shard doubles at
/// 7/8 load, so one growth rehashes at most ⌈`WHEEL_NODES_MAX` / 256⌉ ×
/// 8/7 ≈ 75 k entries. Crossing: none — the count is structural.
pub const WHEEL_MEMBER_SHARDS: usize = 256;

/// Index slots one keyspace expiry slice may sweep, shared by every store
/// the slice serves (ADR-0008 A1 rule 6) — the default of
/// `ExpiryBudget::max_sweep_slots`. Crossing ⇒ continuation: the pass
/// resumes at its next slot on a later slice, a spent budget withholds
/// the sweep from the stores after it (their wheels still tick), and the
/// rotation's hand parks at the first store left unserved.
pub const EXPIRY_SWEEP_SLOTS_PER_SLICE: u32 = 256;

/// Unbounded expiry slices one frozen-time drain may take (ADR-0008 A1
/// O3; the accounting oracles' drain, never a serving path). Owner: the
/// callers of `expiry_settled` — the plane's `drain_expiry`, the
/// simulator's model drain and the store tests. An unbounded slice catches
/// every wheel up and ends at most one sweep pass per store; settling
/// takes the pass under way and one begun at the frozen instant, plus one
/// more for each `Over` that owes the sweep meanwhile. Crossing: the drain
/// returns unsettled and the oracle reading the records reports what it
/// retained.
pub const EXPIRY_DRAIN_SLICES_MAX: usize = 64;

/// Index slots one sweep chunk walks before the slice's fire budget is
/// checked again (ADR-0008 A1 rule 6). Crossing ⇒ a slice overshoots its
/// fire budget by at most this many reaps.
pub const EXPIRY_SWEEP_CHUNK_SLOTS: usize = 16;

/// Tombstone nodes one store's wheel may hold (ADR-0008 A1 rule 4),
/// derived: a removal with no successor leaves one tombstone at its
/// list's tail, at most one per slot list per list epoch — 4 tiers × 2 ×
/// 512 slots, plus one per overflow horizon walk over the u40 deadline
/// range (2⁴⁰ / 2²⁷). ≤ 197 KiB of nodes. Crossing: unreachable — a debug
/// assertion at creation and the `wheel_tombstones` gauge.
pub const WHEEL_TOMBSTONES_MAX: u64 = 4 * 2 * 512 + (1 << 40) / (1 << 27);

/// Idoc bytes, header and body, one stored document may hold (ADR-0169
/// D2): the record value cap less the document value prefix, so every
/// document a sink writes fits one `DocFull` and one inline record. Owner:
/// inf-store, which alone knows the record layout. Crossing: a live
/// mutation refuses in the parser or at the head of the apply plan, both
/// clamped through `record_doc_limits` (`ERR document too large`); at the
/// sink (COPY, a replayed `DocFull`) it is `OpError::TooLarge`. Nothing
/// changes.
#[cfg(feature = "doc")]
pub const DOC_IDOC_BYTES_MAX: usize = crate::record::MAX_VAL_LEN - crate::doc::VALUE_PREFIX_LEN;

/// Body bytes one stored document may hold: [`DOC_IDOC_BYTES_MAX`] less
/// the idoc header, named by the crate that owns it (ADR-0169 D2). The
/// body axis `DocLimits` carries; same owner and crossing.
#[cfg(feature = "doc")]
pub const DOC_BODY_BYTES_MAX: usize = DOC_IDOC_BYTES_MAX - inf_doc::HEADER_LEN;

// A `DocDelta` carries the stored idoc length in its 3-byte `post_len`
// field. That field has no owner const of its own: the encoder's width
// check reuses the version mask, so the relation rides it (ADR-0169 I4).
#[cfg(feature = "doc")]
const _: () = assert!(DOC_IDOC_BYTES_MAX <= inf_log::DOC_VERSION_MASK as usize);
// The record clamp only ever lowers the format ceiling.
#[cfg(feature = "doc")]
const _: () = assert!(DOC_BODY_BYTES_MAX <= inf_doc::limits::DOC_BYTES_MAX);
