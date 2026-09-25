//! `inf-wire` — RESP2/RESP3 protocol engine (master plan §20, milestone
//! M0-E4): the command parser (SWAR/SIMD primitives from `inf-simd`,
//! zero-copy over wire buffers, bounded resumable state), the reply
//! serializer, and perfect-hash command dispatch with key specs.
//!
//! Boundary law (§3.3): this crate never sees a socket or a record — it
//! transforms byte slices. Fully safe; the SIMD lives in `inf-simd`.
#![forbid(unsafe_code)]
// ADR-0144 D1: a production `match` names every variant of its enum.
#![cfg_attr(
    not(test),
    deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)
)]

mod command;
pub mod limits;
mod parser;
mod writer;

pub use command::{
    COMMANDS, CmdFlags, CommandId, CommandMeta, KeyIter, KeySpec, KeyspaceScope, arity_ok,
    extract_keys, key_spec, keyspace_scope, lookup,
};
pub use parser::{
    ArgvRef, ConnParser, DEFAULT_MAX_BULK_BYTES, FRAME_HEADROOM_BYTES, FrameIter, INLINE_ARGS,
    Parsed, ParserLimits, WireError,
};
pub use writer::{Protocol, RespWriter};
