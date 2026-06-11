//! Byte-level compat-diff harness for InfinityDB M0 (story M0-S15).
//!
//! Diffs raw RESP reply bytes between two live servers across a command ×
//! edge-case matrix. Self-validating today: `redis-server` vs `redis-server`
//! must be 100% identical. Once `infinityd` exists it plugs in as the
//! candidate via the `INFINITYD_BIN` env var.
//!
//! Only the RESP client exists so far; the command/edge-case `matrix`,
//! `runner`, and `server` harness modules land with M0-S15.
#![forbid(unsafe_code)]

pub mod resp;
