//! Representation limits owned by the RESP decoder.

/// Bytes addressed by one argv frame's u32 offsets. Larger declarations
/// return `WireError::FrameTooLong` before any payload is buffered.
pub const ARGV_FRAME_BYTES_MAX: usize = u32::MAX as usize;

/// Entries represented by one argv's u32 count. Larger declarations
/// return `WireError::TooManyArgs` before any argv storage grows.
pub const ARGV_ENTRIES_MAX: usize = u32::MAX as usize;
