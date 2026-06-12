//! Byte-level compat-diff harness for InfinityDB M0 (story M0-S15).
//!
//! Diffs raw RESP reply bytes between real Redis (the oracle, spawned per
//! run) and the in-process InfinityDB command executor (the candidate)
//! across a command × edge-case matrix. Once `infinityd` serves TCP, the
//! same matrix plugs it in as the candidate via `INFINITYD_BIN`.
#![forbid(unsafe_code)]

pub mod candidate;
pub mod matrix;
pub mod resp;
