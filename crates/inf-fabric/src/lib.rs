//! `inf-fabric` — the cross-shard plane (master plan §6, milestone M0-E3):
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

mod codec;
#[cfg(not(loom))]
mod mesh;
mod msg;
#[allow(unsafe_code)]
mod ring;

pub use codec::{
    ApplyArgs, CODEC_VERSION, CodecError, ErrCode, MAX_APPLY_ARGS, MAX_BATCH_OPS, Op, Outcome,
    WriteFlags, decode, encode,
};
#[cfg(not(loom))]
pub use mesh::{CellFabric, FabricStats, Mesh, MeshConfig, SendError};
pub use msg::{FabricMsg, FabricToken, INLINE_MSG_CAP};
pub use ring::{Consumer, Producer, ring};
