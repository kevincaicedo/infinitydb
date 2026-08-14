//! Named fault points this crate declares (M4.5-S04, ADR-0076 D5;
//! registry and cost contract in `inf_foundation::fault` — compiled out
//! of release builds, armed by tests through dev-dependency feature
//! unification). `scripts/check-fault-points.sh` discovers this
//! inventory and fails CI on any declared-but-unwired or
//! declared-but-untested point.
//!
//! | point | site | documented failure path |
//! |---|---|---|
//! | `idx_reserve_refuse` | `CellIndexes::reserve` (the bracket pre-half, before the mutation applies) | the plan-then-commit reservation refuses: the mutation fails with a typed error and document, index, and accounting are all unchanged (ADR-0072 D7.1) |
//! | `idx_apply_trip` | `CellIndexes::apply_diff` (the bracket commit-half, after the mutation staged) | a mid-diff invariant trip: the index marks `degraded` (counted), the document mutation stands, and serving refuses typed until rebuild (ADR-0072 D7.2 — wrong results are never served) |
//! | `idx_backfill_trip` | `CellIndexes::backfill_insert_doc` (the M4.5-S05 walk, ADR-0077 D7) | a walked document the tree has no headroom for: the index marks `degraded` (counted), the build parks — the cell never reports ready for an unservable tree; rebuild resets |

pub const IDX_RESERVE_REFUSE: &str = "idx_reserve_refuse";
pub const IDX_APPLY_TRIP: &str = "idx_apply_trip";
pub const IDX_BACKFILL_TRIP: &str = "idx_backfill_trip";

/// Inventory for the CI coverage check.
pub const ALL: &[&str] = &[IDX_RESERVE_REFUSE, IDX_APPLY_TRIP, IDX_BACKFILL_TRIP];
