//! `inf-replica` — InfinityDB workspace crate (see master plan §20).
//!
//! Stub at M0: this crate's milestone has not started; it exists so the
//! dependency DAG and boundaries are enforced before code arrives.
#![forbid(unsafe_code)]
// ADR-0144 D1: a production `match` names every variant of its enum.
#![cfg_attr(
    not(test),
    deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)
)]
