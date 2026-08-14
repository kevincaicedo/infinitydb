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
| At-mutation maintenance hook (the ADR-0072 bracket + removal sites) | `inf-store`/`inf-server` | implemented (M4.5-S04, ADR-0076 — attach-block custody, `hash64(key)` pk ref, the numbered-db funnel bracket, death hook + truncate + replay arm) |
| Backfill state machine (MAINTAIN slices, resumable watermark) | `inf-store` | pending (M4.5-S05) |
| Index checkpoint sidecar v1 (`.ick` v2 tag 0x06) | `inf-log` | pending (M4.5-S06 — constraints frozen in ADR-0073) |
| Access-program form v1 | `inf-query` | pending (M4.5-S09) |
| Predicate VM bytecode v1 | `inf-query` | pending (M4.5-S07/S08) |
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
- **The primary-key ref is `hash64(key)`** (ADR-0076 D2) — durable-
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
