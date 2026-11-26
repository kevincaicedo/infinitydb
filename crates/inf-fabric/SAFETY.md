# inf-fabric SAFETY

`inf-fabric` is one of the four audited unsafe-leaf crates, and scopes it mechanically: the crate root is `#![deny(unsafe_code)]` with a
single `#[allow(unsafe_code)]` on the `ring` module, which covers its one
child, `ring/retained.rs`. Everything outside `ring.rs` and
`ring/retained.rs` — codec, mesh, credits, doorbells, limits — is safe
Rust.

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
  loom reports the over-read as a causality violation.
- `unsafe impl Sync for Shared<T> where T: Send`: only values of `T` cross
  threads (by move); no `&T` is ever shared, so `T: Sync` is not required.
- `Drop for Shared` runs when both handles are gone — the final `Arc` drop
  synchronizes with all prior handle activity, giving exclusive access; the
  unconsumed range `head..tail` is dropped in place exactly once.

One algorithm serves both ownership variants (ADR-0157 D1): the producing
and consuming halves (`Sender`, `Receiver`) are generic over a `Backing`
that names the indices and the slots, selected at compile time, so every
`write_slot`, `read_slot` and index store above is the same code for
HEAD's `Arc`-backed ring and the retained ring.

## `ring/retained.rs` — the retained backing and its join (ADR-0157 D2–D4)

The retained ring's slot block and shared header (`Header`: the indices,
the endpoint count, the slot `Vec`) are two fallibly reserved `Vec`s the
backing owner, `RetainedRing`, holds. The header block is one element,
never grown, moved or freed while the ring is Live. Every unsafe block
rests on these rules:

- **`HeaderRef::header`** dereferences the header's address, taken once at
  the split by `Vec::as_ptr` (no reference to the element is formed). It
  is valid from the split until the owner's join reads an `Acquire` zero
  of the endpoint count, which needs this endpoint's release; in between
  the owner forms no `&mut` to the header, and every mutation through the
  shared borrow is an atomic or a slot's `UnsafeCell`, under the
  ownership protocol of `ring.rs`.
- **The release and the join.** An endpoint's last access is its one
  `fetch_sub(1, Release)` in its `Drop`; it frees nothing. The owner's
  `join` loads the count with `Acquire`; zero joins both decrements
  through their read-modify-write release sequence, so every endpoint's
  slot and index access happens before the owner's next read. Only then
  does the owner read the indices (`Relaxed`, ordered by the join) and
  move a queued value out (`take_queued`: `head..tail` initialized, no
  endpoint left, `head` advanced before the value leaves, so it moves out
  once). A nonzero count changes nothing: the owner waits for the host's
  terminal event.
- **`Send for RetainedProducer` / `RetainedConsumer`** for `T: Send`: an
  endpoint reaches the header only through `HeaderRef`; the two touch
  disjoint state; only values of `T` cross threads, by move. Neither is
  `Clone` (`compile_fail` doctests, with `Send` refused for `Rc`).
- **`Permit::write`**: a permit exists only while a reservation has
  granted slots left, each vacant and producer-owned when it was granted
  (`free_slots`), none published before the reservation's drop, which
  publishes every slot written with one `Release` store.
- **The owner's `Drop`** before Terminal fails stop (class I) over a
  `ManuallyDrop` phase, so nothing is freed while an endpoint may read it;
  in an unwind it returns, the storage leaked: forgetting never authorizes
  a free. A Terminal owner holds no block.

## Verification

- **Loom** (`loom_*` tests, `RUSTFLAGS="--cfg loom"`): publish/consume,
  wrap-around, full→recover, empty-vs-first-publish races, batch
  visibility, and the stale-cache consume path (the branch FABRIC-IN takes
  on every loaded drain; a `cfg(loom)` refresh counter on `Consumer`
  asserts it ran), with loom's permuted orderings. CI runs this on every
  merge touching the crate.
- **Loom, the retained join** (`loom_retained_join_*`, ADR-0157 D5's three
  programs, explored with no preemption, permutation or time bound): both
  endpoints released concurrently, where a join before the fixture's
  thread join must see the producer's tail and the consumer's head; the
  producer held, and the consumer held, where no execution joins. Their
  plants, `inf_canary_ring_release_relaxed`, `inf_canary_ring_join_relaxed`,
  `inf_canary_ring_release_before_write` (`JOIN_VISIBILITY_GAP`) and
  `inf_canary_ring_join_after_one` (`JOIN_WITH_HELD_ENDPOINT`), are rows of
  `scripts/sim-canaries.sh`; `inf_canary_ring_drop_live_silent` is the
  class-I `Drop`'s, on the test that catches its panic and reads the
  message.
- **Loom, the retirement** (`loom_retained_retirement_before_the_thread_join`):
  the concurrent release again, the owner retiring to Terminal as soon as
  its join reads zero and before the fixture joins its threads, so only the
  join orders the endpoints' accesses before the retirement's. Under Loom
  the header carries a tomb, a cell every `HeaderRef` access reads and
  every retirement step on the header writes (zero-sized otherwise), so an
  endpoint access after its release, to a slot or to a plain header field,
  is a causality violation. Its plants, `inf_canary_ring_slot_after_release`
  and `inf_canary_ring_header_after_release` (a slot write, a read of
  `mask`, in the producer's drop after its decrement), are rows of
  `scripts/sim-canaries.sh`.
- **Miri**: the non-loom unit tests (including a two-thread stress test at
  reduced count) run under Miri in CI with strict provenance, the retained
  ring's among them: both drop orders, the last release on another thread
  with the owner retiring before it joins the endpoint threads (a
  post-release access is a data race or a use after free there; both
  plants above are red under it), a failed startup transfer, full and
  queued rings, zero-sized and over-aligned payloads, a cancel at every
  construction boundary, the
  budgeted retirement; `tests/retained_ring.rs` refuses each block through
  `inf-alloc`'s counting allocator and reconciles requested and freed
  bytes. The two tests that leak a dropped backing by design are skipped
  under Miri.
- **`perf c2c`** false-sharing attribution of the `CachePadded` index lines
  (reference box, pinned P-cores 4/6): the only HITM lines are the two
  index lines — offset 0x0 only, 128 B apart — and the slot payload lines;
  remote HITM 0. Measured twice, 2026-06-11 (95,778 samples) and
  2026-09-13 (92,609 samples, the same shape on the later tree).
