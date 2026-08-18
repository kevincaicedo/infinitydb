//! `inf-query` — the feature-gated query engine (master plan §20;
//! ADR-0024).
//!
//! Landed surface: **predicate VM bytecode v1** (M4.5-S07, ADR-0079) —
//! the one compiled predicate form PartiQL `WHERE` residuals (S09),
//! JSONPath filter expressions (S13), and post-1.0 live queries share —
//! and its **evaluator** (M4.5-S08): iterative, allocation-free,
//! fuel-bounded, proven against a naive reference oracle. The PartiQL
//! compiler (S09) and the cursor machinery (S11) build on both.
//!
//! Allowed edges: `inf-foundation`, `inf-doc`, `inf-store` — never RESP,
//! sockets, raw record memory, or log files (L11).
#![forbid(unsafe_code)]

pub mod predicate;
