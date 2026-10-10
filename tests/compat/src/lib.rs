#![allow(
    clippy::disallowed_types,
    reason = "test-only, not cell code: file fixtures (ADR-0144 D5), oracle lock (ADR-0106 D2)"
)]
//! Byte-level compat-diff harness for InfinityDB M0 (story M0-S15).
//!
//! Diffs raw RESP reply bytes between real Redis (the oracle, spawned per
//! run) and the in-process InfinityDB command executor (the candidate)
//! across a command × edge-case matrix. Once `infinityd` serves TCP, the
//! same matrix plugs it in as the candidate via `INFINITYD_BIN`.
#![forbid(unsafe_code)]

pub mod candidate;
pub mod harness;
pub mod json_oracle;
pub mod matrix;
pub mod matrixgen;
pub mod replyshapes;
pub mod resp;
