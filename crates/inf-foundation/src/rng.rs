//! Injected randomness (L7): cells and the simulator draw entropy through
//! this seam only, so every run is seed-replayable.

pub trait Entropy {
    fn next_u64(&mut self) -> u64;

    /// Uniform value in `0..bound` (bound > 0) via 128-bit multiply rejection-free mapping.
    #[inline]
    fn next_below(&mut self, bound: u64) -> u64 {
        debug_assert!(bound > 0);
        ((u128::from(self.next_u64()) * u128::from(bound)) >> 64) as u64
    }
}

/// SplitMix64 — tiny, well-distributed, and exactly reproducible.
#[derive(Clone, Debug)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> SplitMix64 {
        SplitMix64 { state: seed }
    }
}

impl Entropy for SplitMix64 {
    #[inline]
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_sequence() {
        // First three outputs for seed 0 (SplitMix64 reference implementation).
        let mut rng = SplitMix64::new(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(rng.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn next_below_stays_in_bounds() {
        let mut rng = SplitMix64::new(42);
        for _ in 0..10_000 {
            assert!(rng.next_below(7) < 7);
        }
    }
}
