//! Named fault points this crate declares (M2-S17, §8.4; registry and
//! cost contract in `inf_foundation::fault`, pattern in ADR-0019 D6:
//! point names are owned by the crate that owns the mechanism —
//! completion routing is the plane's). `scripts/check-fault-points.sh`
//! discovers this inventory and fails CI on any declared-but-unwired or
//! declared-but-untested point.
//!
//! | point | site | documented failure path |
//! |---|---|---|
//! | `durable_fsync_eio` | `DurableCell::on_synced` | the fsync completion arrives as a device-reported EIO instead of `Synced` — the watermark freezes (no ack for the affected batch can ever fire) and the process fail-stops with [`EXIT_DURABLE_FAILSTOP`](crate::EXIT_DURABLE_FAILSTOP) (the fsyncgate rule; ADR-0020 D3) |
//! | `shadow_twin_read_fail` | `plane::tiered::read_cold_record` | a shadow twin's cold read fails (M4.5-S37, ADR-0093 D4.3/A3): the reconciler leaves the ticket for the next round, `DBSIZE`'s drain answers the typed `-ERR DBSIZE: shadow twin … unreadable` (relayed through a scattered leg), `DEL`'s forced resolution answers its error — never an inexact count, never a removal |
//! | `cold_enqueue_full` | `plane::tiered::probe` + `plane::tiered::fetch_key` | the `ColdReads` enqueue refuses `QueueFull` (the BUSY leg the review of 2026-08-30, C2′/F-L06-02/F-L06-04, found untestable deterministically): every read command answers the typed `BUSY cold-read queue saturated` — `GET`, `MGET`, `EXISTS`/`TOUCH`, and a `SCAN` page alike — never a nil, a `:0`, or a silently shorter page |
//!
//! The sync-tier seal fsync has its own point (`inf_log::fault::FSYNC_ERR`);
//! this one exists because the reactor tier defers the seal fsync through
//! the driver (ADR-0013 D4) — on a live node, fsync failure is a CQE, and
//! this point is the deterministic stand-in for that CQE.

pub const DURABLE_FSYNC_EIO: &str = "durable_fsync_eio";
pub const SHADOW_TWIN_READ_FAIL: &str = "shadow_twin_read_fail";
pub const COLD_ENQUEUE_FULL: &str = "cold_enqueue_full";

/// Inventory for the CI coverage check.
pub const ALL: &[&str] = &[DURABLE_FSYNC_EIO, SHADOW_TWIN_READ_FAIL, COLD_ENQUEUE_FULL];
