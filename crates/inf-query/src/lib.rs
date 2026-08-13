//! `inf-query` — InfinityDB workspace crate (see master plan §20).
//!
//! Stub at M4.5-S00 (ADR-0072 D9): the PartiQL subset compiler, predicate
//! VM, and cursor machinery arrive with M4.5-S07+; the crate exists now so
//! the dependency DAG and boundaries are enforced before code arrives.
//! Allowed edges: `inf-foundation`, `inf-doc`, `inf-store` — never RESP,
//! sockets, raw record memory, or log files (L11).
#![forbid(unsafe_code)]
