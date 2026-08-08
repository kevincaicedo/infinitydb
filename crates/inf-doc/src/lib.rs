//! `inf-doc` — the InfinityDB document engine's format layer (M3-S01/S02;
//! master plan §10.1; format authority **ADR-0036**, frozen at M3 exit).
//!
//! What lives here:
//! - **Tape form** ([`TapeDoc`], [`TapeBuilder`]): the durable, canonical
//!   `idoc` byte encoding — compact byte tags + u24 skip lengths, one
//!   value one encoding (L7). This is what records, `DocFull` payloads,
//!   checkpoints, tier files, and replicas carry.
//! - **Arena form** ([`ArenaDoc`]): the RAM node projection for
//!   large/edit-hot documents; `freeze()` re-derives canonical tape bytes
//!   (`freeze(morph(t)) == t`, the M4 demotion contract).
//! - **Unified cursors** ([`DocValue`], [`ObjCursor`], [`ArrCursor`]):
//!   form-agnostic reads — the S09 evaluator's and M4.5 predicate VM's
//!   substrate.
//! - **JSON text boundary** ([`JsonParser`] in, [`ser`] out — M3-S05/S06):
//!   SIMD-assisted parse to canonical tape; cursor-driven serialization to
//!   wire buffers with RedisJSON formatting options and the canonical
//!   (comparator) mode.
//! - **Path mutation** ([`apply`] — M3-S11/S12): two-phase plan/apply
//!   over plain canonical tape bytes (§3.4 R4/R5); the shape S16's fast
//!   path optimizes under and S17's `DocDelta` replay reuses.
//! - **Program cache** ([`path::ProgramCache`] — M3-S10): one bounded,
//!   counted, deterministic LRU per cell.
//! - **Reference model** ([`model`]): an owned tree for tests,
//!   differential oracles, and goldens — never on the data plane.
//!
//! Boundary rules (milestone §3.3): depends only on `inf-foundation`,
//! `inf-simd` (from S05), `inf-alloc`; never sees RESP, sockets, records,
//! or log files; `deny(unsafe_code)` with exactly one audited exception —
//! the [`emit`] region (ADR-0049, the ADR-0047 D3 escalation), inventoried
//! in `SAFETY.md` and Miri-covered; every decoder is iterative,
//! depth/size-bounded, and fuzzed (`fuzz/fuzz_targets/idoc_decode.rs`).

#![deny(unsafe_code)]

pub mod apply;
pub mod arena;
mod build;
pub mod cursor;
pub mod delta;
// The ADR-0049 audited unsafe region: reserve-once/write-unchecked emit
// primitives. SAFETY.md carries the block inventory; everything else in
// this crate stays deny(unsafe_code).
#[allow(unsafe_code)]
mod emit;
mod error;
mod header;
#[cfg(feature = "doc-intern-keys")]
pub mod intern;
pub mod json;
pub mod limits;
mod merge;
pub mod model;
pub mod path;
pub mod ser;
pub mod tape;

pub use apply::{
    ApplyError, ApplyOp, ApplyOutcome, MatchResult, Number, ScalarPatch, array_operand,
    merge_absent_document, patch_scalar_in_place,
};
pub use arena::{ArenaDoc, DocMemReport, DocRef, FreezeScratch};
pub use build::TapeBuilder;
pub use cursor::{ArrCursor, DocValue, ObjCursor};
pub use delta::{DeltaDecodeError, DeltaOpcode, decode_apply_op, encode_apply_op};
pub use error::DocError;
pub use header::{FLAG_INTERNED, HEADER_LEN, MAGIC, VERSION};
pub use json::{JsonErrorKind, JsonParseError, JsonParser, ParseLimits, parse_number_token};
pub use path::{Matches, PathError, PathErrorKind, PathProgram, ProgramCache};
pub use ser::{
    Reply, SerializeOpts, serialize_canonical_into, serialize_into, serialize_number_text,
    serialize_reply_into,
};
pub use tape::{DocStr, TapeDoc};
