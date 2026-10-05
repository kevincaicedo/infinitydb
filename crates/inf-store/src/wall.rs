//! Wall-clock bridge (M2-S08): converts between the injected internal
//! clock ([`Nanos`], L7) and Unix-epoch milliseconds. Log records carry
//! *absolute Unix-ms* deadlines (`ExpireAt`, ADR-0011) so replay is
//! deterministic regardless of when it runs; the store keeps *internal*
//! deadlines. The anchor pins one instant on both clocks and the bridge is
//! plain millisecond arithmetic around it — the same saturating/checked
//! semantics as the server's `wall_ms`/`internal_from_unix_ms` pair
//! (M1-S03), replicated here so replay never reaches upward for it.

use inf_foundation::time::Nanos;

use crate::record::InternalDeadline;

/// One instant expressed on both clocks: `internal_ms` on the injected
/// monotonic clock and `unix_ms` on the wall clock. Injected at boot (L7 —
/// the simulator owns both values).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct WallAnchor {
    pub internal_ms: u64,
    pub unix_ms: u64,
}

impl WallAnchor {
    /// Unix-epoch milliseconds for an internal deadline (the value an
    /// `ExpireAt` record carries). Deadlines that would land before the
    /// Unix epoch clamp to 0 — only reachable through absurd anchors, and
    /// clamping beats wrapping.
    #[must_use]
    pub fn unix_from_internal(&self, at: Nanos) -> u64 {
        let unix = at.as_millis() as i64 - self.internal_ms as i64 + self.unix_ms as i64;
        unix.max(0) as u64
    }

    /// The internal-clock deadline for a Unix-epoch deadline (ADR-0111
    /// D2): an instant before the internal clock's origin is
    /// [`InternalDeadline::BeforeOrigin`], expired at every reading of
    /// the clock, and a far one saturates into the store bound. `None`
    /// means the i64 arithmetic overflowed — the caller decides the
    /// policy (replay clamps forward to the store bound, "never", so an
    /// unrepresentable deadline can never resurrect as an
    /// already-expired key; see `Keyspace::apply_record`).
    #[must_use]
    pub fn internal_from_unix(&self, unix_ms: u64) -> Option<InternalDeadline> {
        let delta = i64::try_from(unix_ms).ok()?.checked_sub(i64::try_from(self.unix_ms).ok()?)?;
        let internal = i64::try_from(self.internal_ms).ok()?.checked_add(delta)?;
        Some(InternalDeadline::from_internal_ms(internal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANCHOR: WallAnchor = WallAnchor { internal_ms: 5_000, unix_ms: 1_750_000_000_000 };

    #[test]
    fn round_trips_after_the_anchor() {
        for internal_ms in [5_000u64, 5_001, 60_000, 86_400_000] {
            let at = Nanos::from_millis(internal_ms);
            let unix = ANCHOR.unix_from_internal(at);
            assert_eq!(unix, 1_750_000_000_000 + (internal_ms - 5_000));
            assert_eq!(ANCHOR.internal_from_unix(unix), Some(InternalDeadline::At(at)));
        }
    }

    #[test]
    fn pre_anchor_internal_maps_below_the_unix_anchor() {
        // Internal instants before the anchor still convert exactly (the
        // i64 arithmetic goes below the anchor, not saturating at it).
        let unix = ANCHOR.unix_from_internal(Nanos::from_millis(4_000));
        assert_eq!(unix, 1_750_000_000_000 - 1_000);
    }

    #[test]
    fn pre_origin_unix_is_before_the_origin() {
        // A Unix deadline before the internal clock's origin is expired
        // at every reading of the clock: never clamped onto the origin,
        // where it would read as live for a millisecond, never wrapped.
        let ancient = 1_750_000_000_000 - 1_000_000;
        assert_eq!(ANCHOR.internal_from_unix(ancient), Some(InternalDeadline::BeforeOrigin));
        assert_eq!(ANCHOR.internal_from_unix(0), Some(InternalDeadline::BeforeOrigin));
        // One millisecond before the origin, and the origin itself.
        let origin = 1_750_000_000_000 - 5_000;
        assert_eq!(ANCHOR.internal_from_unix(origin - 1), Some(InternalDeadline::BeforeOrigin));
        assert_eq!(ANCHOR.internal_from_unix(origin), Some(InternalDeadline::At(Nanos::ZERO)));
    }

    #[test]
    fn unix_below_epoch_clamps_to_zero() {
        // An anchor with internal far ahead of unix can push conversions
        // below the epoch; they clamp at 0.
        let skewed = WallAnchor { internal_ms: 1_000_000, unix_ms: 10 };
        assert_eq!(skewed.unix_from_internal(Nanos::ZERO), 0);
    }

    #[test]
    fn overflow_is_none_not_a_panic() {
        assert_eq!(ANCHOR.internal_from_unix(u64::MAX), None, "u64 > i64::MAX");
        // i64-representable but past the store bound: saturates into it
        // (ADR-0111 D2), never a wrap.
        let far = InternalDeadline::At(crate::record::saturating_deadline(u64::MAX));
        assert_eq!(ANCHOR.internal_from_unix(u64::MAX / 4), Some(far));
    }
}
