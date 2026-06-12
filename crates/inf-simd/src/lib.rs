//! `inf-simd` — vectorized primitives for the data plane (master plan §20):
//! SIMD CRLF scanning, SWAR ASCII-integer parsing (both salvaged from
//! `vortex-proto`, §24), and 16-way Swiss-table group probes (M0-S14).
//!
//! `unsafe` is platform intrinsics only, inventoried in `SAFETY.md`; every
//! SIMD path is property-tested against a scalar oracle.

#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod crlf;
mod group16;
mod swar;

pub use crlf::{CrlfPositions, find_crlf, scalar_scan_crlf, scan_crlf};
pub use group16::{
    eq_mask16, high_bit_mask16, prefetch_read, scalar_eq_mask16, scalar_high_bit_mask16,
};
pub use swar::swar_parse_int;
