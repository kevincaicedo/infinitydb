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
