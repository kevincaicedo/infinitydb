//! Typed completion tokens — the u64 that rides through the backend
//! (io_uring `user_data`, kqueue `udata`) and comes back on completions.
//!
//! Layout (frozen at M0 exit, milestone §3.2): `{class:8, slot:24, gen:32}`.
//! `slot` identifies the owning object (connection slot, listener index);
//! `gen` disambiguates slot reuse so a stale completion can never be routed
//! to a successor object (the Vortex completion-token lesson).

/// What kind of operation this token was attached to.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TokenClass {
    Accept = 0,
    Recv = 1,
    Send = 2,
    Close = 3,
    /// Reserved for cross-thread wakeups (doorbell integration, fabric M0-E3).
    Wake = 4,
}

impl TokenClass {
    const fn from_u8(v: u8) -> Option<TokenClass> {
        match v {
            0 => Some(TokenClass::Accept),
            1 => Some(TokenClass::Recv),
            2 => Some(TokenClass::Send),
            3 => Some(TokenClass::Close),
            4 => Some(TokenClass::Wake),
            _ => None,
        }
    }
}

const SLOT_BITS: u32 = 24;
const GEN_BITS: u32 = 32;
pub const MAX_SLOT: u32 = (1 << SLOT_BITS) - 1;

/// Completion token: `{class:8, slot:24, gen:32}` packed into a u64.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct CompletionToken(u64);

impl CompletionToken {
    /// # Panics
    /// Panics if `slot >= 2^24` — slot ids are allocated by bounded slabs,
    /// so an out-of-range slot is a programming error, not load.
    #[inline]
    pub fn new(class: TokenClass, slot: u32, generation: u32) -> CompletionToken {
        assert!(slot <= MAX_SLOT, "token slot {slot} exceeds 24 bits");
        CompletionToken(
            ((class as u64) << (SLOT_BITS + GEN_BITS))
                | (u64::from(slot) << GEN_BITS)
                | u64::from(generation),
        )
    }

    #[inline]
    pub fn class(self) -> TokenClass {
        // Constructors guarantee a valid class byte; decode cannot fail here.
        TokenClass::from_u8((self.0 >> (SLOT_BITS + GEN_BITS)) as u8)
            .expect("CompletionToken with invalid class byte")
    }

    #[inline]
    pub fn slot(self) -> u32 {
        ((self.0 >> GEN_BITS) as u32) & MAX_SLOT
    }

    #[inline]
    pub fn generation(self) -> u32 {
        self.0 as u32
    }

    /// Raw value for the backend (`user_data`/`udata`).
    #[inline]
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// Decode a raw value coming back from the backend. `None` if the class
    /// byte is not a known [`TokenClass`] — backends treat that as a foreign
    /// completion and surface it as an error rather than misrouting it.
    #[inline]
    pub fn from_u64(raw: u64) -> Option<CompletionToken> {
        TokenClass::from_u8((raw >> (SLOT_BITS + GEN_BITS)) as u8).map(|_| CompletionToken(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_all_fields() {
        for class in [
            TokenClass::Accept,
            TokenClass::Recv,
            TokenClass::Send,
            TokenClass::Close,
            TokenClass::Wake,
        ] {
            for (slot, generation) in
                [(0, 0), (1, u32::MAX), (MAX_SLOT, 7u32), (0xAB_CDEF, 0xDEAD_BEEF)]
            {
                let t = CompletionToken::new(class, slot, generation);
                assert_eq!(t.class(), class);
                assert_eq!(t.slot(), slot);
                assert_eq!(t.generation(), generation);
                assert_eq!(CompletionToken::from_u64(t.as_u64()), Some(t));
            }
        }
    }

    #[test]
    #[should_panic(expected = "exceeds 24 bits")]
    fn oversized_slot_panics() {
        let _ = CompletionToken::new(TokenClass::Recv, MAX_SLOT + 1, 0);
    }

    #[test]
    fn unknown_class_byte_is_rejected() {
        assert_eq!(CompletionToken::from_u64(u64::MAX), None);
    }
}
