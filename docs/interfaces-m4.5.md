# M4.5 interface freezes — indexes & query (draft until M4.5 exit)

Companion to `interfaces-m0.md`/`interfaces-m2.md`, same contract: these
interfaces freeze at **M4.5 exit** (plan §3.2); changing a frozen one
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
| At-mutation maintenance hook (the ADR-0072 bracket + removal sites) | `inf-store`/`inf-server` | implemented (M4.5-S04, ADR-0076 — attach-block custody, the keyed-hash pk ref (`KeyHasher`, ADR-0094 — `hash64(key)` before 2026-08-28), the numbered-db funnel bracket, death hook + truncate + replay arm) |
| Backfill state machine (MAINTAIN slices, resumable watermark) | `inf-store` | implemented (M4.5-S05, ADR-0077 — store-resident walk, volatile resume-only watermark (crash ⇒ restart), per-index jobs, slot = id-rank, MAINTAIN-edge catalog flip) |
| Index checkpoint sidecar v1 (`.ick` v2 tag 0x06) | `inf-log` | implemented (M4.5-S06, ADR-0078 under the ADR-0073 constraints — 36-byte self-describing body meta `{ns, index id, generation, key-encoding version, key scheme, flags, entries_before, total_entries}` + strictly-ascending `(typed key bytes, entry_ref)` pairs, FINAL-closed streams; the only *soft* body class: damage rebuilds one projection, never refuses a boot) |
| Access-program form v1 | `inf-query` | implemented (M4.5-S09, ADR-0080 — `access::AccessProgram`: one access step + residual + page spec, serialized/versioned, `from_bytes` trust boundary; EXPLAIN rendering golden-pinned) |
| PartiQL subset v1 (grammar + total compiler + statement cache) | `inf-query` | implemented (M4.5-S09, ADR-0080 — `partiql::compile`/`StatementCache`/`CatalogView`; contract: `docs/partiql-subset.md` + the 303-case golden suite) |
| Predicate VM bytecode v1 | `inf-query` | implemented (M4.5-S07/S08, ADR-0079 — `predicate::PredicateProgram` + `PredicateVm`; this row lagged those stories and is corrected at S09) |
| `QueryOp` codec (fabric v1.2) | `inf-fabric` | pending (M4.5-S11) |
| Cursor wire format (opaque, CRC + version + shape + {index id, generation} binding) | `inf-server` | pending (M4.5-S11 — the binding half exists as `validate_binding`, S03) |

## Registration surface (M4.5-S03, ADR-0075 — the ADR-0072 D2 contract as-built)

- **Per-cell registry:** `Keyspace::idx_create / idx_drop_finish /
  idx_registry[_mut] / ns_has_indexes` in `inf-store`. DDL-rate only; the
  mutation path consults a cached per-namespace flag (S04 wires it) —
  never the registry.
- **Lifecycle:** `IndexRegistry::set_catalog_state / set_cell_state /
  rebuild` enforce the ADR-0075 D3 edge set; `Keyspace::idx_rebuild`
  bumps the generation and resets the owning store's tree in one
  transition. Catalog state is the planning authority; per-cell state
  is backfill progress.
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

## Maintenance surface (M4.5-S04, ADR-0076 — the ADR-0072 D3–D7 contract as-built)

- **Tree custody (ADR-0076 D1, amending ADR-0075 D1's wording):** each
  `CellStore` owns its namespace's trees in an attach block
  (`index_maint::CellIndexes`) — the maintenance-facing cache of the
  registry, resynced at DDL transitions, seed, and lazy materialization;
  every S03 accounting shape keeps its value with the fold source moved.
- **The primary-key ref is the key hash** (ADR-0076 D2; since ADR-0094
  the keyed SipHash-1-3 under the data directory's secret — it was
  `hash64(key)` before 2026-08-28) — durable-
  adjacent: S06 sidecars serialize `(typed key bytes, ref)` pairs, so
  changing the ref definition later is an encoding-class break
  (ADR-0073 D5.2 discipline). Collision odds and consequences are
  disclosed in the ADR.
- **The bracket:** `Keyspace::idx_bracket_begin / idx_bracket_commit /
  idx_bracket_abort` — pre-image eval + reservation before the mutation,
  post-image eval + dedup diff + idempotent tree ops after staging.
  Attachment rows (ADR-0076 D3): the two ADR-0072 named-ns plane sites
  plus the numbered-db funnel inside `execute_owned_into`; COPY runs
  store-level mini-brackets; `FLUSH*` runs the whole-namespace truncate;
  fabric `DEL`/`UNLINK` (`apply_counted`) is death-hook-covered.
- **Record deaths** outside a bracket (lazy expiry, the wheel reap,
  eviction) remove the dying document's entries at the death site; the
  responsibility split keys on the bracket's write-set hashes.
- **Failure contract:** typed pre-half refusals
  (`IdxMaintRefusal::{Reserve, EntryFlood}`, fault point
  `idx_reserve_refuse`); post-half failures set the cell-local
  `degraded` serving veto (`Keyspace::idx_degraded` — S09/S11 must
  consult it beside `validate_binding`; fault point `idx_apply_trip`);
  rebuild clears the veto.
- **Replay arm:** `Keyspace::idx_set_replay_maintenance(ns,
  Option<MaintMode>)` — `None` at boot (the no-sidecar path rebuilds via
  S05); S06's sidecar load arms `CatchUp`. Same code path as live,
  assertion strictness only (`Strict` scoped to converged indexes via
  `idx_set_converged`).
- **Counters (ADR-0076 D8):** per-index `IdxCounters` (sparse/inexact/
  nan/toolong skips, inserts/removes/prunes, degraded trips) via
  `Keyspace::idx_counters[_total]`; `INFO stats` renders the cell-scope
  fold; `INF.IDX LIST` (S10) renders per-index detail.

## Backfill surface (M4.5-S05, ADR-0077 — the plan's backfill machine as-built)

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

## Sidecar surface (M4.5-S06, ADR-0078 — the ADR-0073 constraints as-built)

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
- **Loader (D6):** `inf-store::SidecarLoader` — per-`(ns, id)` state
  machine (`Accepting → Loaded | Discarded{reason}`); binding checks
  {generation, `INDEX_KEY_ENCODING_VERSION`, key scheme}, ordinal
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

## Compiler surface (M4.5-S09, ADR-0080 — the ADR-0024 D2 fence as-built)

- **Total compilation:** `inf_query::partiql::compile[_with_max_bytes]`
  — statement text → `CompiledStatement { program, access, vm }`, or a
  `QlError` whose `Display` string is the documented rejection
  (`infinitydb/docs/partiql-subset.md` §7 — the compat contract; the
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
  — `begins_with` on the ADR-0074 D2 prefix property
  (`index_key_escape_prefix`, new in `inf-store::index_key`, owns the
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
