# inf-fabric SAFETY

`inf-fabric` is one of the four crates allowed `unsafe` (milestone M0 §3.3),
and scopes it mechanically: the crate root is `#![deny(unsafe_code)]` with a
single `#[allow(unsafe_code)]` on the `ring` module. Everything outside
`ring.rs` — codec, mesh, credits, doorbells — is safe Rust.

## `ring.rs` — SPSC ring ownership protocol

All unsafe blocks rest on one protocol, documented on `Shared<T>`:

- `tail` has exactly one writer (the producer); `head` exactly one (the
  consumer). Each side keeps a local free-running copy and only *loads* the
  other's index — `Acquire` on load, `Release` on store, no `SeqCst`.
- Slot ownership: `head..tail` is initialized and consumer-owned;
  `tail..head + capacity` is vacant and producer-owned. The `Release` store
  of `tail` transfers slots producer→consumer; the `Release` store of `head`
  transfers them back.
- `write_slot` requires the slot to be vacant and unpublished (checked by
  `free_slots` arithmetic before every write); `read_slot` requires the slot
  to be initialized and consumed exactly once (the consumer's `head` is
  advanced past it before the user callback runs, and `AdvanceGuard`
  publishes progress even on unwind, so a panicking callback cannot cause a
  double-read).
- **Cached indices are lower bounds, not fresh views.** Neither side
  reloads the other's index on its fast path: `free_slots` returns early
  while the cached `head` already leaves room, and `consume_batch` reads
  slots `head..tail_cache` without a new load while the cached `tail`
  already covers the request. This is sound because a cached index is a
  *lower* bound established by an earlier `Acquire`: every slot below the
  cached `tail` was ordered by that load and `head` only advances inside
  that range, so a read within it needs no new acquire; symmetrically a
  stale `head_cache` only *understates* free space, so the producer never
  writes a slot the consumer may still read. A read or write *past* the
  cached bound is the race — `loom_stale_tail_cache_serves_without_a_refresh`
  reads on a stale cache while the producer recycles a freed slot, and
  loom reports the over-read as a causality violation (batch 48, F-L12-05).
- `unsafe impl Sync for Shared<T> where T: Send`: only values of `T` cross
  threads (by move); no `&T` is ever shared, so `T: Sync` is not required.
- `Drop for Shared` runs when both handles are gone — the final `Arc` drop
  synchronizes with all prior handle activity, giving exclusive access; the
  unconsumed range `head..tail` is dropped in place exactly once.

## Verification

- **Loom** (`loom_*` tests, `RUSTFLAGS="--cfg loom"`): publish/consume,
  wrap-around, full→recover, empty-vs-first-publish races, batch
  visibility, and the stale-cache consume path (the branch FABRIC-IN takes
  on every loaded drain; a `cfg(loom)` refresh counter on `Consumer`
  asserts it ran), with loom's permuted orderings. CI runs this on every
  merge touching the crate.
- **Miri**: the non-loom unit tests (including a two-thread stress test at
  reduced count) run under Miri in CI with strict provenance.
- **`perf c2c`** false-sharing attribution of the `CachePadded` index lines
  is **Linux-only and deferred to the reference box** (M0-S08 third AC).
