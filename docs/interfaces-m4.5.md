# M4.5 interface freezes — indexes & query (draft until M4.5 exit)

Companion to `interfaces-m0.md`/`interfaces-m2.md`, same contract: these
interfaces freeze at **M4.5 exit**; changing a frozen one
afterwards requires an ADR. Until the milestone exits they are *drafts* —
changes before exit still record their reasoning in the owning ADR.
Status column tracks arrival.

| Interface | Crate | Status |
|-----------|-------|--------|
| Ordered-map API (insert/remove/range-cursor over (typed key bytes, ref); re-seek cursors, never pinned) | `inf-store` | implemented (M4.5-S01 — `OrderedMap` over the `Fixed8`/`VarKey` schemes, `OrderedCursor`; arena/node layout internal) |
| Typed index-key encoding v1 (order-preserving; version bound out-of-band) | `inf-store` | implemented (M4.5-S02, ADR-0074 — `index_key`; `INDEX_KEY_ENCODING_VERSION = 1` binds in the registry + sidecar header, never in key bytes) |
| Index registry entry `{id, generation, ns, path program bytes, key type, state}` | `inf-store` | implemented (M4.5-S03, ADR-0075 D1 — `IndexSpec`/`IndexRegistry`, per cell; `INDEXES_PER_NODE_MAX = 64`) |
| Index catalog persistence (namespace-catalog payload **v3**; v2 byte-identical while pristine) | `inf-store` (encoding) / `inf-server` (swap) | implemented (M4.5-S03, ADR-0075 D2 — index records + never-regressing id/generation counters ride the `META` swap; `fuzz_catalog` in the same PR) |
| Declaration lifecycle {declared → backfilling → ready → dropping} + fleet-readiness aggregation | `inf-store` / `inf-server::control` | implemented (M4.5-S03, ADR-0075 D3–D5 — explicit invalid-transition rejection; `IndexBoard` per-cell × per-slot ready generations; catalog `ready` ⟺ every cell reports the exact generation) |
| Cursor/compile binding gate `{ns, index id, generation}` | `inf-store` | implemented (M4.5-S03, ADR-0075 D7 — `IndexRegistry::validate_binding`, typed `{UnknownIndex, StaleGeneration, NotReady}`; S09/S11 consult it) |
| At-mutation maintenance hook (a bracket around each indexed write — pre-image evaluation and node reservation before the mutation applies, post-image diff and tree ops after its effect stages, ADR-0072 D3 — plus the record-death removal sites) | `inf-store`/`inf-server` | implemented (M4.5-S04; mechanics ADR-0139 — alias-group identity, coverage by entry point and full key, bounded enumeration; attach-block custody, the keyed-hash pk ref (`KeyHasher`, ADR-0094 — `hash64(key)` before 2026-08-28), the numbered-db funnel bracket, death hook + truncate + replay arm) |
| Backfill state machine (MAINTAIN slices, resumable watermark) | `inf-store` | implemented (M4.5-S05, ADR-0077 — store-resident walk, volatile resume-only watermark (crash ⇒ restart), per-index jobs, slot = id-rank, MAINTAIN-edge catalog flip) |
| Index checkpoint sidecar v1 (`.ick` v2 tag 0x06) | `inf-log` | implemented (M4.5-S06, ADR-0078; tag 0x06 on `.ick` v2, framed `{tag, body_len, entry_count}` before the footer like every section, CRCs in the footer's digest chain, outside the per-namespace entry counts, one namespace per section, each body bound to `{index id, generation}` and the key-encoding version, ADR-0073 — 36-byte self-describing body meta `{ns, index id, generation, key-encoding version, key scheme, flags, entries_before, total_entries}` + strictly-ascending `(typed key bytes, entry_ref)` pairs, FINAL-closed streams; the only *soft* body class: damage rebuilds one projection, never refuses a boot) |
| Access-program form v1 | `inf-query` | implemented (M4.5-S09, ADR-0080 — `access::AccessProgram`: one access step + residual + page spec, serialized/versioned, `from_bytes` trust boundary; EXPLAIN rendering golden-pinned) |
| PartiQL subset v1 (grammar + total compiler + statement cache) | `inf-query` | implemented (M4.5-S09, ADR-0080 — `partiql::compile`/`StatementCache`/`CatalogView`; contract: `docs/partiql-subset.md` + the 303-case golden suite) |
| Predicate VM bytecode v1 | `inf-query` | implemented (M4.5-S07/S08, ADR-0079 — `predicate::PredicateProgram` + `PredicateVm`; this row lagged those stories and is corrected at S09) |
| `QueryOp` codec (fabric v1.2) | `inf-fabric` | pending (M4.5-S11) |
| Cursor wire format (opaque, CRC + version + shape + {index id, generation} binding) | `inf-server` | pending (M4.5-S11 — the binding half exists as `validate_binding`, S03) |

## Registration surface (M4.5-S03; ADR-0075, ADR-0072 D2)

- **Per-cell registry:** `Keyspace::idx_create / idx_drop_finish /
  idx_registry[_mut] / ns_has_indexes` in `inf-store`. DDL-rate only; the
  mutation path consults a cached per-namespace flag (S04 wires it) —
  never the registry.
- **Lifecycle:** `IndexRegistry::set_catalog_state / set_cell_state /
  rebuild` admit declared → backfilling → ready, ready → backfilling
  only as a rebuild with a fresh generation, and declared, backfilling or
  ready → dropping; every other edge is a typed `InvalidTransition`
  (ADR-0075 D3). `Keyspace::idx_rebuild` bumps the generation and resets
  the owning store's tree in one transition. Catalog state is the
  planning authority; per-cell state is backfill progress. An accepted
  change replaces the `dropping` edge with retirement, a durable catalog
  write that removes the declaration, followed by per-cell reclamation of
  the retired `{id, generation}`'s tree in maintenance slices, and admits
  a rebuild from `backfilling` as well as from `ready` (ADR-0075 A1); it
  is not built.
- **Persistence:** declarations ride the namespace catalog (payload v3)
  through the existing control-thread `META` swap — persist-then-ack
  unchanged; `ControlHandle` allocates index ids and generations
  (never reused, counters covered at every persist).
- **Readiness:** cells publish `(slot, generation)` to
  `inf-server::IndexBoard` from MAINTAIN (S05); the catalog flips
  `backfilling → ready` only on `fleet_ready` — generation-exact, so
  stale reports after a rebuild read as not-ready.
- **Restart (ADR-0075 D4):** declarations survive as catalog records;
  runtime state regresses to `backfilling` (the pre-crash-`ready` hint
  retained for S06's sidecar load); `dropping` resumes its drop;
  generations never bump at boot.
- **Accounting (ADR-0075 D6):** `idx_tree_bytes`/`idx_slack_bytes` are
  L5 domains folded into `MemoryReport`, `INFO memory`, and the
  namespace budget comparison (`MAXMEMORY` counts index bytes).

## Maintenance surface (M4.5-S04; mechanics ADR-0139, which supersedes ADR-0076)

- **Construction extension, not built:** ADR-0160 specifies borrowed
  program views, fallible attachment ownership and preparation before
  local registry/attachment publication.
- **Tree custody (ADR-0139 D1):** each `CellStore` owns its namespace's
  trees in an attach block (`index_maint::CellIndexes`) — the
  maintenance-facing cache of the registry, resynced at DDL transitions,
  seed, and lazy materialization. `IndexTree`'s key scheme is private.
- **The ref is hash evidence; identity is the full key (ADR-0139 D2).**
  An entry is `(typed key bytes, PkRef)`, `PkRef` = the keyed hash of the
  document's key (`KeyHasher`, ADR-0094). Two keys can share a ref, and
  two such documents with an equal indexed value share **one** entry, so
  an entry is a fact about the ref's *alias group* `G(h)` — every
  physically present record whose full key hashes to `h`:
  - *Removal* happens only after a complete enumeration of the group
    found no other member that holds the key; otherwise the entry stays
    (`idx_alias_kept`). `OrderedMap::remove` / `IndexTree::remove` take
    the enumeration's `&AliasView` as a witness — its only constructor is
    private to `index_alias` — so a removal decided on the hash alone
    does not compile.
  - *Insertion* finding the pair present is legal iff an alias holds it.
  - **Resolution (rule 5 — the contract S11, S12, S14 consume; not built
    here):** a ref is resolved by enumerating `G(h)` through
    `index_alias`, never by first match. `|G| = 1`: that document.
    `|G| > 1`: a member is served iff it holds the key, re-evaluated;
    `COUNT(*)` counts members that hold it. `|G| = 0` is a violated
    invariant: typed error, the index degrades on that cell.
  - **Position (rule 6):** inside a group, order and resume position are
    decided by the full primary key. No client-visible token carries a
    ref (a keyed-hash output). `PageResume` still embeds one — M4.5-S11's
    cursor format (ADR-0142) replaces it.
  - `PkRef::to_raw` / `from_raw` exist for the sidecar's serialization
    boundary only.
- **The enumeration and its bounds (ADR-0139 D9):**
  `Index::probe_exact_bounded(hash, groups_max, visit) → ProbeEnd` is the
  one probe-chain walk over fragment matches (`each_exact` is a call of
  it); a group is charged when its control bytes are loaded, match or
  not. `index_alias::alias_view` builds on it under three limits
  (`inf_store::limits`): `IDX_ALIAS_WALK_GROUPS_MAX` = 32 groups,
  `IDX_ALIAS_REHASH_MAX` = 16 fragment matches **fetched** — every
  record the walk reads is charged before it is read, a write-set key
  excluded by full key included; only the death sites' by-address skip
  of the dying record is free — and `IDX_ALIAS_GROUP_MAX` = 8 members
  **of the view**, i.e. after exclusion. A bracket enumerates once per
  distinct removing ref (≤ one walk per write-set key), a death hook
  once. Crossing any is
  `AliasWalk::Over(AliasLimit)`, which carries no view: nothing is
  decided, `idx_alias_walk_over` + 1, and the indexes the question
  touches degrade on that cell — the participating set from a bracket
  commit, **every** non-degraded index from a death hook. `REBUILD`
  clears the veto.
- **The bracket (ADR-0139 step table):** `Keyspace::idx_bracket_begin /
  idx_bracket_commit`. The pre-half notes the write set (hash **and** key
  bytes), sets the participating set whole before evaluating anything
  (D12), evaluates each key's physical pre-image, reserves the `new`
  side of scratch at `|old|` and checks tree headroom. The commit-half
  evaluates the post-image, diffs and applies. **A bracket ends only in
  its commit-half — there is no abort**; the `COPY` mini-brackets commit
  on every outcome. Attachment rows (D3): the two ADR-0072 named-ns plane
  sites plus the numbered-db funnel; `FLUSH*` truncates whole trees;
  fabric `DEL`/`UNLINK` (`apply_counted`) is death-hook-covered.
- **Coverage of a record death (ADR-0139 D4)** is decided once, from the
  entry point and the full key:
  - the eviction entry points (`evict_record`, `reap_expired_at` →
    `free_record_uncovered`) are **never covered** — the hook runs; if
    the victim's full key is in the open bracket's write set, the bracket
    forgets that key's `old` ranges (compacted out before the diff sorts)
    and the prune is void (`idx_gate_forget`);
  - any other `free_record` is covered iff its full key is in the write
    set and no index is pruned; with a prune engaged the hook runs for
    the pruned indexes and the mask clears (`idx_prune_void`);
  - a hash in the write set whose key is not is an alias: hooked, and
    counted (`idx_cover_alias`).
- **Failure contract:** typed pre-half refusals
  (`IdxMaintRefusal::{Reserve, EntryFlood}`; fault points
  `idx_reserve_refuse`, `idx_scratch_refuse`). `EntryFlood` covers the
  entry cap, the match cap and `BRACKET_KEY_BYTES_MAX` (32 MiB of encoded
  keys per phase). Every bracket and death-hook scratch growth is
  `try_reserve`. Post-half failures — a flooding or growing post-image,
  `AliasWalk::Over`, the planted `idx_apply_trip` — set the cell-local
  `degraded` serving veto on **every** participating index
  (`Keyspace::idx_degraded` — S09/S11 must consult it beside
  `validate_binding`); a death hook that cannot evaluate or grow degrades
  the index. Rebuild clears the veto. The pre-apply refusal of a
  post-image is ADR-0139 D5, not built.
- **Replay arm:** `Keyspace::idx_set_replay_maintenance(ns,
  Option<MaintMode>)` — `None` at boot (the no-sidecar path rebuilds via
  S05); S06's sidecar load arms `CatchUp`. Same code path as live,
  assertion strictness only (`Strict` scoped to converged indexes via
  `idx_set_converged`).
- **Counters (ADR-0139 D8; population: cell, fold: node):** per-index
  `IdxCounters` (sparse/inexact/nan/toolong skips, inserts/removes/
  prunes, degraded trips, `alias_kept`, `alias_held_marks` — the held
  marking's work, ≤ the entries the removals evaluated) plus the
  store-scoped
  `alias_groups`, `cover_alias`, `alias_walk_over`,
  `alias_walk_groups_max` (a maximum), `gate_forget`, `prune_void`, via
  `Keyspace::idx_counters[_total]`; `INFO stats` renders the fold as
  `idx_*`; `INF.IDX LIST` (S10) renders per-index detail.

## Backfill surface (M4.5-S05, ADR-0077 — the backfill machine as built)

- **The tick:** `Keyspace::idx_backfill_tick(now, BackfillBudget)` —
  registry sync (job create / rebuild-reset / drop / park) then budgeted
  walk slices, tick-granularity round-robin across jobs. **Serving cells
  only**: the plane gates on recovery completion (replay maintains
  nothing by default, ADR-0076 D7). The walk is `CellStore`-resident on
  the reverse-binary home-group enumeration (the SCAN guarantee), reaps
  expired records on encounter, and inserts via the attach block's
  idempotent `backfill_insert_doc`.
- **Watermark (ADR-0077 D2):** the cursor is volatile and resume-only —
  never consulted for membership, never persisted; **crash ⇒ restart the
  walk**. Boot clears jobs (`seed_catalog`); rebuild (generation bump)
  resets them.
- **Completion (D4):** store materialized → `idx_set_converged` →
  cell machine `Ready`; the plane republishes `(slot, generation)` to
  `IndexBoard` **every** MAINTAIN tick. `slot = idx_slot_of(id)` — the
  id's rank among live declarations (D5; derived, never stored; false
  `fleet_ready` impossible — generations are globally unique).
- **The catalog flip (D6):** each cell flips its local entry
  `backfilling → ready` on observing `fleet_ready(slot, generation)` in
  MAINTAIN (`idx_fleet_candidates` → `set_catalog_state`); cell 0 alone
  persists on its flip edge (the ADR-0075 D4 `was_ready` hint for S06).
- **Failure (D7):** eval overflow or tree-capacity exhaustion mid-walk
  degrades the index and **parks** the build (`BackfillPhase::Parked`) —
  no convergence, no publication; rebuild resets. Fault point
  `idx_backfill_trip`.
- **Progress (D8):** `idx_backfill_progress()` (per-job rows) and
  `idx_backfill_info()` (phase counts + cumulative totals); `INFO stats`
  renders `idx_backfill_*`; per-index rendering rides S10's
  `INF.IDX LIST`. DST: `inf-sim --scenario m45-backfill`.

## Sidecar surface (M4.5-S06; ADR-0078, ADR-0073)

- **Writer:** the checkpoint's sidecar phase runs after `walk_done`
  (derived data last) — `Keyspace::idx_sidecar_candidates` captures the
  emission plan (converged + non-degraded only, D1),
  `idx_sidecar_emit` streams each tree through its re-seek cursor, and
  `IckStream::stage_idx_entry / stage_idx_final` (sync tier:
  `SyncIckWriter::append_idx_entry / append_idx_final`) frame the
  sections. Eligibility re-checks between slices; any change abandons
  the stream (no FINAL ⇒ the loader discards it). `.ick` **v2 selects**
  iff `tiered_present || idx_declared_on_durable()` (registration, not
  convergence — D7); cells with neither stay v1 byte-identical.
- **Footer accounting (D2):** sidecar entries join **neither**
  `records_total` nor the per-ns presize counts — the soft class must
  not be audit-load-bearing. `section_count` and the digest (stored
  CRC, ADR-0073 D3.3) cover 0x06 like every class.
- **Reader:** `next_step_hybrid`/`read_ick_hybrid` gained the fifth
  handler (`IckIdxSidecarStep::{Section, Damaged}`); body CRC/canon
  failures deliver `Damaged` and the read continues (D4); records-only
  loaders refuse typed (`IdxSidecarSectionUnsupported` — the ADR-0073
  D7 downgrade boundary). Fuzz: `fuzz_index_sidecar` + the `ick_decode`
  sidecar oracles.
- **Maintenance-rules bits (ADR-0078 A2):** the meta's `flags` byte is
  bit 0 `FINAL`, bits 1–3 `maint_rules` (`IdxSidecarRules`, three bits;
  `IDXSIDECAR_RULES_SHIFT` / `_MASK`, one definition for writer and
  reader), bits 4–7 zero. The store passes `IDX_MAINT_RULES` (= version
  `IDX_MAINT_RULES_VERSION` = 1) to the writer; the reader decodes and
  surfaces the value and never judges it. A writer before A2 left the
  bits zero, so its sidecars read as rules 0; a reader before A2 refuses
  the new bits as a body-class failure — both directions discard and
  rebuild, neither can fail a boot.
- **Loader (D6):** `inf-store::SidecarLoader` — per-`(ns, id)` state
  machine (`Accepting → Loaded | Discarded{reason}`); binding checks
  {generation, `INDEX_KEY_ENCODING_VERSION`, key scheme, **maintenance
  rules** (`SidecarRebuildReason::MaintenanceRules`)}, ordinal
  contiguity, and the ascending canon via `IndexTree::append`'s own
  refusal (`OrderedMap::append` — the rightmost-spine bulk path, the
  < 15 s gate's mechanism). `finish_load` at checkpoint end discards
  open streams and arms `idx_set_replay_maintenance(CatchUp)` per
  loaded namespace; `commit_ready` at end of replay flips loaded
  indexes converged + cell-`Ready` (readiness still aggregates through
  the S05 board — a sidecar never flips catalog state directly) and
  records every decision.
- **The decision record (L10):** per index per boot on the registry
  (`sidecar_boot()` — `Loaded{entries}` / `Rebuilt{reason}`, reasons
  `SidecarRebuildReason`); `INFO stats` renders the fold
  (`idx_sidecar_{loaded,rebuilt,entries_loaded,damaged}`); a
  `was_ready` index that ends rebuilt logs its serving downgrade
  loudly. Crash ⇒ restart (ADR-0077 D2) is untouched — no sidecar means
  the S05 machine rebuilds. DST: `inf-sim --scenario m45-sidecar`;
  crash rows: `tests/crash-matrix/tests/sidecar.rs`.

## Compiler surface (M4.5-S09; ADR-0080, ADR-0024 D2)

- **Total compilation:** `inf_query::partiql::compile[_with_max_bytes]`
  — statement text → `CompiledStatement { program, access, vm }`, or a
  `QlError` whose `Display` string is the documented rejection
  ([`partiql-subset.md`](partiql-subset.md) §7 — the compat contract; the
  300-case golden suite pins it verbatim). The output type has exactly
  one access-step field; no code path compares two candidate plans —
  ambiguity is a typed refusal naming the explicit `FROM ns."index"`
  form.
- **Catalog input:** the `partiql::CatalogView` trait (`resolve_ns`,
  `index_by_name`, `indexes`, `catalog_epoch`) — planning reads catalog
  state only (ADR-0075 D3); `inf-server` implements it over the real
  catalog at S10/S11. `IndexRegistry::epoch()` (new, additive) backs
  the epoch; server views fold namespace DDL in.
- **Access-program form v1:** `inf_query::access` —
  `Access`/`AccessStep::{PkGet, IndexRange, Scan}`/`RangeEdge`/
  `Projection`; `encode` is the only writer, `AccessProgram::from_bytes`
  the trust boundary (nested residual revalidation included);
  `AccessProgram::explain()` is the deterministic rendering S12 reuses.
  Bounds are **encoded key bytes** (the truth-table mapping runs once,
  at compile); `{index id, generation, key type}` ride the program and
  re-assert at the executing cell via `validate_binding`.
- **Range bounds (ADR-0080 D3):** constructed against the S02 encoding
  — `begins_with` on the string encoding's prefix property: `s` starts
  with `p` exactly when `enc(s)` starts with `escape(p)` (`enc(p)` without
  its terminator), so the bounds are `escape(p)` and `escape(p)` with its
  last non-0xFF byte incremented (ADR-0074 D2;
  `index_key_escape_prefix`, new in `inf-store::index_key`, owns the
  escape image), cross-numeric bounds via integral tightening (i64
  index) and encoded-word neighbor stepping (f64 index), reversed/
  contradictory ranges compile empty (never an error). Proven by the
  `partiql_bounds` oracle: encoded-key membership ≡ the production VM
  verdict for every admitted value (boundary corpus + property lane).
- **Statement cache:** `partiql::StatementCache` — the M3-S10
  `ProgramCache` shape keyed by raw statement text, epoch-guarded
  (stale entries recompile, counted as `invalidations`); the value
  holds the residual's `PredicateVm` pools pre-decoded (the S08 cold
  path lives in the cache, not per execution). Rejections are never
  cached.
- **Page step (ADR-0080 D4):** `inf_query::page::RangePager` — seek
  (resume pair or lower edge; `OrderedCursor::resume_after`, new,
  additive — mid-key exact), upper-edge check, scan-budget bound
  (entries **scanned**, not matched), statement-`LIMIT` countdown,
  resume production. S11 drives it per page and owns doc resolution,
  TTL filtering, wire assembly, and yields; `COUNT(*)` pages return
  {matched, scanned} — the DynamoDB `Count`/`ScannedCount` register.
- **Scan consent:** the `FROM ns.SCAN` grammar and
  `AccessStep::Scan` compile here (grammar is one contract); S14 owns
  execution, rate limits, and the storm proof. `SCAN` is a reserved
  index name (S10 refuses it at `CREATE`).
- **Fuzz:** `fuzz_partiql_parse` (statement bytes: no panic,
  deterministic accept/reject, round-trip, EXPLAIN total) and
  `fuzz_access_program` (decoder bytes: no panic, decode→encode byte
  identity) — same-PR L9.
