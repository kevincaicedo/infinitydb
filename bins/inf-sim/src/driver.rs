//! What the simulator prints of a run's violations, and how.
//!
//! Every printed violation text goes through one line writer
//! ([`write_text`]), which escapes control characters and cuts the escaped
//! text at its bound, so a seed's line stays one line of bounded length.

mod line;

pub use line::{one_line, write_text};
