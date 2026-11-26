//! A failed create's owner (ADR-0156 D1, D4): each pipeline holds one
//! residue slot, the admitted owner of the file a create names, filled
//! before the create's first file effect. A create that fails at or after
//! `create_tier` may have left its file made and named, so the slot keeps
//! the file's meta until the control thread's unlink of it answers; the
//! next create runs only once the slot is empty again, and then makes the
//! same name, so a failed create spends no file id and no create meets the
//! residue's file or its unlink in flight.
//!
//! | Slot | Event | Next |
//! |---|---|---|
//! | Empty | a create reaches `create_tier` | Filling (inside the one create step) |
//! | Filling | the create succeeds | Empty; `next_id` moves |
//! | Filling | any error from `create_tier` on | Held, due at once; `next_id` unmoved |
//! | Held or Out | a create | the same; the create is deferred (`CreateDeferred`) |
//! | Held | the walk offers it | Out |
//! | Out | its `Unlinked` or `AlreadyAbsent` consumed | Empty |
//! | Out | its `Retry` consumed | Held, due a retry interval later |
//!
//! Whether a file exists is never inferred from the errno: the worker's
//! unlink answers it (`NotFound` is terminal, since no create of the name
//! runs while the slot is not empty).

use inf_foundation::LogicalAddr;
use inf_foundation::time::Nanos;

use super::TierFileMeta;
use crate::tier::SealReason;

/// One pipeline's failed-create owner. `Filling` lives only inside one
/// synchronous create step, so the walk sees the other three.
#[derive(Debug, Default)]
pub(super) enum ResidueSlot {
    #[default]
    Empty,
    /// A create step holds the name it is making.
    Filling { meta: TierFileMeta, bytes: u64 },
    /// A failed create's file, offered once `due` has passed. `bytes` is
    /// what the attempt may have written, charged to the namespace's disk
    /// bytes until a terminal result.
    Held { meta: TierFileMeta, due: Nanos, bytes: u64 },
    /// Its job is out: the meta is the job's, the bytes still charged.
    Out { bytes: u64 },
}

/// The slot as the walk and the gauges read it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ResidueState {
    /// No failed create's file is owned.
    Empty,
    /// A failed create's file waits for its offer, due at the injected
    /// time given.
    Held(Nanos),
    /// Its job is out.
    Out,
}

impl ResidueSlot {
    /// The meta of the file a create of `id` at `path` makes: the slot's
    /// record of the name, built before the call.
    pub(super) fn meta_of(id: u32, path: std::path::PathBuf) -> TierFileMeta {
        TierFileMeta {
            id,
            base: LogicalAddr::ZERO,
            data_len: 0,
            reason: SealReason::Shutdown,
            path,
        }
    }

    /// Whether a create may run: only over an empty slot. A planted canary
    /// (`inf_canary_create_while_residue_out`) lets a create run while the
    /// residue's job is out.
    pub(super) fn admits_create(&self) -> bool {
        match self {
            ResidueSlot::Empty => true,
            ResidueSlot::Out { .. } => cfg!(inf_canary_create_while_residue_out),
            ResidueSlot::Filling { .. } | ResidueSlot::Held { .. } => false,
        }
    }

    pub(super) fn state(&self) -> ResidueState {
        match self {
            ResidueSlot::Empty => ResidueState::Empty,
            ResidueSlot::Held { due, .. } => ResidueState::Held(*due),
            ResidueSlot::Out { .. } => ResidueState::Out,
            ResidueSlot::Filling { .. } => {
                debug_assert!(false, "a slot is Filling only inside a create step");
                ResidueState::Empty
            }
        }
    }

    /// The bytes the slot charges to the namespace's disk usage.
    pub(super) fn bytes(&self) -> u64 {
        match self {
            ResidueSlot::Empty => 0,
            ResidueSlot::Filling { bytes, .. }
            | ResidueSlot::Held { bytes, .. }
            | ResidueSlot::Out { bytes } => *bytes,
        }
    }
}
