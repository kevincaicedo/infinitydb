//! `inf-simd` — vectorized primitives for the data plane (master plan §20):
//! SIMD CRLF scanning and SWAR ASCII-integer parsing, both salvaged from
//! `vortex-proto` (§24) with their proptest equivalence suites.
//!
//! `unsafe` is platform intrinsics only, inventoried in `SAFETY.md`; every
//! SIMD path is property-tested against a scalar oracle.

#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod crlf;
mod swar;

pub use crlf::{CrlfPositions, find_crlf, scalar_scan_crlf, scan_crlf};
pub use swar::swar_parse_int;
