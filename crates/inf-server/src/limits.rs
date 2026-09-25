//! Runtime bounds owned by the server. Every crossing names its behavior here.

/// Bytes read by the process supervisor for one CPU sample. An oversized
/// procfs record is refused and the read board retains its preceding sample.
pub const PROCESS_STAT_BYTES_MAX: u64 = 16 * 1024;
