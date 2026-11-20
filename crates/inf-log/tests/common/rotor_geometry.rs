//! The log geometry of a rotor-only integration test, stated once for this
//! crate's tests. A test includes this file by `#[path]` rather than through
//! `support`, so it does not compile the runtime harness it never drives.

use inf_log::{DEFAULT_STAGING_BYTES, LogGeometry, SegmentConfig, StagingConfig};

/// The rotor's checked geometry: a staging ring no larger than one segment,
/// `min(4 MiB, segment_bytes)`, so every frame it can seal fits (the harness
/// rule, ADR-0191).
pub fn geometry(segment: SegmentConfig) -> LogGeometry {
    let capacity = segment.segment_bytes.min(DEFAULT_STAGING_BYTES);
    LogGeometry::new(StagingConfig::with_capacity(capacity), segment).expect("the test's geometry")
}
