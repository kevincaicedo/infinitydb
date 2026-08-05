# M4-S19 — Per-namespace memory + disk budgets (dev-tier evidence)

ADR-0062 (accepted before the frozen-surface change). Box: Linux dev
box (dev-tier per the box profile; no claim-ledger row).

Evidence is test-carried (config/admission/teardown — correctness
surfaces, not timings):

- `crates/inf-store/tests/tiered_budgets.rs`
  (`cargo test -p inf-store --test tiered_budgets`):
  - `aggregate_va_bounds_creation_and_drop_returns_capacity` — the D4
    admission bound: third create refuses typed with exact
    `{requested, admitted, limit}` before any mmap, registry rollback,
    drop returns exactly one ring, freed capacity re-admits.
  - `memory_fill_respects_each_namespace_budget` — the S07 bound
    (`committed ≤ budget + slice`) per namespace at every observation
    point across a concurrent 2-namespace 2× fill; aggregate == Σ
    per-table.
  - `disk_pressure_engages_before_the_cap` — first firing in
    `[7/8·budget, budget)` at MAINTAIN polling cadence; extent device
    bytes inside `disk_used` to the byte.
  - `spec_materialization_and_hot_reload_apply_together` — every
    derived config applied; hot reload updates registry + table
    together or neither; ring-bounded growth refusal; the D2 clamps
    (incl. dead-ratio 50..=100) at registration.
  - `namespace_drop_returns_disk_va_and_accounting_to_zero` (real FS)
    — VA structural return, `statvfs` + empty-directory asserts,
    accounting zero, typed refusal for new access.
- `crates/inf-server/src/admin.rs::inf_ns_tiering_surface` — the D1
  discriminator errors, `USE` refusal (D8), `SET` hot/create-only
  semantics, `INF.NS INFO` tier block read-back.
- `crates/inf-server/tests/node_e2e.rs::
  tiered_namespace_lifecycle_survives_restart` — CREATE with budget
  keys over TCP on a 2-cell durable node (9-arg NSFAN, AllOk), `INFO
  tiering` scrape, `SET` reload, **catalog v2 restart persistence**
  (reloaded value included), DROP back to the §3.3 zero contract.
- Catalog v2: `crates/inf-store/src/catalog.rs` tests — full-field
  round trip, v1-payload acceptance (pinned against a byte-exact v1
  encoder, the v0.3.0-alpha upgrade path), truncation at every prefix
  over the tier block, invalid tier/io-mode/range bytes typed.
