//! Predicate VM bytecode v1 (M4.5-S07, ADR-0079): one predicate engine,
//! three surfaces — PartiQL `WHERE` residuals (S09), JSONPath filter
//! expressions (S13), and post-1.0 live-query predicates.
//!
//! A predicate is a flat, non-Turing-complete boolean expression over
//! path-resolved document values: a prefix-encoded op stream over two
//! pools (embedded ADR-0040 path programs; typed constants). There is
//! no jump opcode — control flow is the expression tree's shape, so
//! termination is structural, not an evaluator promise (ADR-0079 D2/D8).
//! `program` owns the serialized form (S07); `vm` owns evaluation
//! against the ADR-0079 D4–D6 semantics (S08).

mod program;
mod vm;

pub(crate) use program::explain as explain_predicate;
pub use program::{
    CmpOp, Constant, Predicate, PredicateBuildError, PredicateError, PredicateErrorKind,
    PredicateProgram, encode,
};
pub use vm::{EvalFlags, EvalOutcome, PredicateEvalError, PredicateVm};

/// Format ceiling for one serialized predicate program (ADR-0079 D7).
/// Same class as `PATH_BYTES_CEILING`: programs ride `QueryOp` fabric
/// frames and sit behind cursors. The operational default is the S09
/// statement-size config; this is the format's hard bound.
pub const PROGRAM_BYTES_CEILING: usize = 0xFFFF;

/// Distinct paths per predicate. DynamoDB expressions cap at 4 KB text /
/// ~300 operands — 64 distinct attributes is beyond any sane statement.
pub const PATHS_MAX: usize = 64;

/// Constant-pool entries: five full IN lists (100 members each) plus
/// scalar operands, with slack.
pub const CONSTANTS_MAX: usize = 512;

/// Ops (leaves + connectives) per program. An IN list is one op here,
/// so 256 exceeds the DynamoDB-shaped ceiling comfortably.
pub const OPS_MAX: usize = 256;

/// IN-list members — DynamoDB's IN operand cap, deliberate parity
/// (documented in the S09 subset spec).
pub const IN_MEMBERS_MAX: usize = 100;

/// n-ary AND/OR operand bound; longer chains nest (the depth budget
/// absorbs them at log arity).
pub const BOOL_ARITY_MAX: usize = 64;

/// Expression nesting bound, counting every level including the leaf.
/// The S08 evaluator's explicit stack is a fixed array of this size —
/// allocation-free by construction; real predicates nest ≤ ~5.
pub const NESTING_DEPTH_MAX: usize = 32;
