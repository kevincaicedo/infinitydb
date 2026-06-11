//! Fabric message slot and reply-routing token.
//!
//! [`FabricMsg`] is the unit carried by the mesh rings: a 64-byte-class slot
//! holding one encoded codec frame. Frames up to [`INLINE_MSG_CAP`] bytes are
//! stored inline (the common case for Read/Reply traffic); larger frames
//! spill to a `Box<[u8]>`.
//!
//! **M1 optimization path:** spill allocations are flagged per cell
//! ([`crate::CellFabric::spilled_frames`]). The gate for replacing the spill
//! (e.g. with a per-pair side-buffer or borrowed wire-buffer slices) is the
//! fabric hop RTT measured on the Linux reference box — not assumed.

use core::fmt;

use inf_foundation::CellId;

/// Frames at most this long are stored inline in the 64-byte slot.
pub const INLINE_MSG_CAP: usize = 62;

const SEQ_BITS: u32 = 48;
const SEQ_MASK: u64 = (1 << SEQ_BITS) - 1;

/// Reply-routing key: `{origin_cell:16, seq:48}`. Minted by
/// [`crate::CellFabric::next_token`], monotonic per cell, unique node-wide.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FabricToken(pub u64);

impl FabricToken {
    /// Packs an origin cell and per-cell sequence number.
    ///
    /// # Panics
    ///
    /// Panics if `seq` does not fit in 48 bits.
    #[inline]
    pub fn new(origin: CellId, seq: u64) -> FabricToken {
        assert!(seq <= SEQ_MASK, "fabric token sequence overflows 48 bits: {seq}");
        FabricToken((u64::from(origin.0) << SEQ_BITS) | seq)
    }

    /// The cell that minted this token (where the reply routes back to).
    #[inline]
    pub fn origin_cell(self) -> CellId {
        CellId((self.0 >> SEQ_BITS) as u16)
    }

    /// Per-origin-cell monotonic sequence number.
    #[inline]
    pub fn seq(self) -> u64 {
        self.0 & SEQ_MASK
    }
}

impl fmt::Display for FabricToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.origin_cell(), self.seq())
    }
}

/// One encoded codec frame in a 64-byte-class ring slot.
#[derive(Clone, Debug)]
pub struct FabricMsg {
    payload: Payload,
}

#[derive(Clone, Debug)]
enum Payload {
    Inline { len: u8, buf: [u8; INLINE_MSG_CAP] },
    Spill(Box<[u8]>),
}

const _: () = assert!(
    core::mem::size_of::<FabricMsg>() == 64,
    "FabricMsg must stay in the 64-byte slot class (master plan §6.1)"
);

impl FabricMsg {
    /// Copies one encoded frame into a slot, spilling to the heap if it
    /// exceeds [`INLINE_MSG_CAP`].
    pub fn from_frame(frame: &[u8]) -> FabricMsg {
        if frame.len() <= INLINE_MSG_CAP {
            let mut buf = [0u8; INLINE_MSG_CAP];
            buf[..frame.len()].copy_from_slice(frame);
            // Length fits in u8 because INLINE_MSG_CAP < 256.
            FabricMsg { payload: Payload::Inline { len: frame.len() as u8, buf } }
        } else {
            FabricMsg { payload: Payload::Spill(frame.into()) }
        }
    }

    /// The encoded frame bytes.
    #[inline]
    pub fn frame(&self) -> &[u8] {
        match &self.payload {
            Payload::Inline { len, buf } => &buf[..usize::from(*len)],
            Payload::Spill(frame) => frame,
        }
    }

    /// True when the frame did not fit inline (heap spill — tripwire input).
    #[inline]
    pub fn is_spilled(&self) -> bool {
        matches!(self.payload, Payload::Spill(_))
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn token_packs_origin_and_seq() {
        let token = FabricToken::new(CellId(7), 123_456);
        assert_eq!(token.origin_cell(), CellId(7));
        assert_eq!(token.seq(), 123_456);
        let max = FabricToken::new(CellId(u16::MAX), SEQ_MASK);
        assert_eq!(max.origin_cell(), CellId(u16::MAX));
        assert_eq!(max.seq(), SEQ_MASK);
    }

    #[test]
    #[should_panic(expected = "overflows 48 bits")]
    fn token_seq_overflow_panics() {
        let _ = FabricToken::new(CellId(0), 1 << SEQ_BITS);
    }

    #[test]
    fn inline_and_spill_round_trip() {
        let small = vec![0xABu8; INLINE_MSG_CAP];
        let msg = FabricMsg::from_frame(&small);
        assert!(!msg.is_spilled());
        assert_eq!(msg.frame(), &small[..]);

        let big = vec![0xCDu8; INLINE_MSG_CAP + 1];
        let msg = FabricMsg::from_frame(&big);
        assert!(msg.is_spilled());
        assert_eq!(msg.frame(), &big[..]);

        let empty = FabricMsg::from_frame(&[]);
        assert!(!empty.is_spilled());
        assert_eq!(empty.frame(), &[] as &[u8]);
    }
}
