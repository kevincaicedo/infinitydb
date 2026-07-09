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
//!
//! The sync-tier seal fsync has its own point (`inf_log::fault::FSYNC_ERR`);
//! this one exists because the reactor tier defers the seal fsync through
//! the driver (ADR-0013 D4) — on a live node, fsync failure is a CQE, and
//! this point is the deterministic stand-in for that CQE.

pub const DURABLE_FSYNC_EIO: &str = "durable_fsync_eio";

/// Inventory for the CI coverage check.
pub const ALL: &[&str] = &[DURABLE_FSYNC_EIO];
