//! Bounds owned by the simulator. Every crossing names its behaviour here.

/// Escaped bytes of one printed violation text, counted after control
/// characters are escaped. Owner: the driver's one line writer
/// (`driver::write_text`), which every single-run violation line, every
/// sweep results and stderr line and the panic hook's line go through.
/// Crossing: the escaped text is cut between two characters and outside an
/// escape, and ends ` … (<b> bytes not printed)`, `<b>` in escaped bytes.
pub const VIOLATION_TEXT_BYTES_MAX: usize = 2_048;
