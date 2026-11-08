//! What the simulator prints of a run's violations, and how.
//!
//! A run's violations are one [`Violations`] list, whose every entry carries
//! the check that raised it; a single run prints them in their bounded form
//! ([`Violations::printed_lines`]). Every printed violation text goes through
//! one line writer ([`write_text`]), which escapes control characters and
//! cuts the escaped text at its bound, so a seed's line stays one line of
//! bounded length.

mod line;
mod violations;

pub use line::{one_line, write_text};
pub use violations::Violations;
