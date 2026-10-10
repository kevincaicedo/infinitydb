//! `inf-fabric` — the cross-cell plane (master plan §6, milestone M0-E3):
//! SPSC rings, the N×(N−1) mesh with doorbells and credit flow control, and
//! the fabric op codec v0.
//!
//! Layering: this crate sits directly on `inf-foundation` (the dependency
//! DAG forbids anything else). Reply *routing to futures* lives above — the
//! cell drains `Op::Reply` frames here and completes its
//! `inf_runtime::FabricGate`; this crate's job is moving frames with bounded
//! memory and returning credits.
//!
//! `unsafe` is confined to the [`ring`] module (milestone §3.3) and
//! inventoried in `SAFETY.md`; the rest of the crate is `#![deny(unsafe_code)]`.

#![deny(unsafe_code)]
// ADR-0144 D1: a production `match` names every variant of its enum.
#![cfg_attr(
    not(test),
    deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)
)]

mod codec;
#[cfg(not(loom))]
mod mesh;
mod msg;
#[allow(unsafe_code)]
mod ring;

pub use codec::{
    ApplyArgs, CODEC_VERSION, CodecError, ErrCode, MAX_APPLY_ARGS, MAX_BATCH_OPS,
    MAX_INLINE_APPLY_ARGS, Op, Outcome, WriteFlags, decode, encode,
};
#[cfg(not(loom))]
pub use mesh::{CellFabric, FabricStats, Mesh, MeshConfig, SendError};
pub use msg::{FabricMsg, FabricToken, INLINE_MSG_CAP};
pub use ring::{Consumer, Producer, ring};
