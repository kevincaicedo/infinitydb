//! `inf-fabric` limits: one `const` per bound, with its unit, its owner and
//! what a caller observes when it is crossed (INFINITY_STYLE "Put a limit
//! on everything").

/// Requested bytes one retained-ring allocation may take (ADR-0157 D1):
/// 65,536, for the slot block and for the shared header block alike. Owner:
/// the retained ring's admission, which reads both layouts before it
/// allocates anything. Crossing: a payload whose slot block or header is
/// larger is refused with `RingLayoutRefused`, before any allocation or
/// endpoint exists; the bound never truncates a path or a reply.
pub const RETAINED_RING_BACKING_BYTES_MAX: usize = 65_536;
