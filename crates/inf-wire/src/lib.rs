//! `inf-wire` — RESP2/RESP3 protocol engine (master plan §20, milestone
//! M0-E4): the command parser (SWAR/SIMD primitives from `inf-simd`,
//! zero-copy over wire buffers, bounded resumable state), the reply
//! serializer, and perfect-hash command dispatch with key specs.
//!
//! Boundary law (§3.3): this crate never sees a socket or a record — it
//! transforms byte slices. Fully safe; the SIMD lives in `inf-simd`.
#![forbid(unsafe_code)]

mod command;
mod parser;
mod writer;

pub use command::{
    COMMANDS, CmdFlags, CommandId, CommandMeta, KeyIter, KeySpec, arity_ok, extract_keys, lookup,
};
pub use parser::{ArgvRef, ConnParser, FrameIter, INLINE_ARGS, Parsed, ParserLimits, WireError};
pub use writer::{Protocol, RespWriter};
