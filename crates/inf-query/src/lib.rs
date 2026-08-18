//! `inf-query` — the feature-gated query engine (master plan §20;
//! ADR-0024).
//!
//! Landed surface: **predicate VM bytecode v1** (M4.5-S07, ADR-0079) —
//! the one compiled predicate form PartiQL `WHERE` residuals (S09),
//! JSONPath filter expressions (S13), and post-1.0 live queries share —
//! its **evaluator** (M4.5-S08): iterative, allocation-free,
//! fuel-bounded, proven against a naive reference oracle — and the
//! **PartiQL subset compiler** (M4.5-S09, ADR-0080): statement text →
//! access-program form v1 (`access`), with the per-cell statement cache
//! (`partiql::StatementCache`) and the index-range page step (`page`)
//! S11's execution futures drive.
//!
//! Allowed edges: `inf-foundation`, `inf-doc`, `inf-store` — never RESP,
//! sockets, raw record memory, or log files (L11).
#![forbid(unsafe_code)]

pub mod access;
pub mod page;
pub mod partiql;
pub mod predicate;
