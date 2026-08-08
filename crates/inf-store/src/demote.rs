//! Demotion policy vocabulary (M4-S07, ADR-0053): the second answer to
//! memory pressure. Cache namespaces **evict** (the M1 machinery,
//! untouched); durable-tiered namespaces **demote** — seal the mutable
//! tail down to its fraction target, flush the sealed bytes (S11's
//! pipeline; the steel-thread writer until then — ADR-0053 D6), and
//! release RAM pages below the flushed watermark. All three legs run as
//! budgeted MAINTAIN slices; nothing here blocks foreground.
//!
//! This module is configuration and vocabulary only. The mechanism lives
//! where its state lives: seal marks and slice steps on
//! [`TieredTable`](crate::TieredTable), watermark arithmetic on
//! [`AddressSpace`](crate::AddressSpace), and the pressure driver on
//! [`Keyspace::demote_tick`](crate::Keyspace::demote_tick) — the M1
//! mechanism/policy split, kept.

/// EvictionPressure **v2** (M4 plan §3.2, extending the M1 freeze): how a
/// namespace answers memory pressure. The response is namespace-shaped,
/// decided once at table granularity — never a branch on the per-op path
/// (the S03 degenerate case stays instruction-identical).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EvictionPressure {
    /// Cache namespaces evict — the M1 path, unchanged.
    Evict,
    /// Durable-tiered namespaces demote: seal → flush → release
    /// (ADR-0053), driven by [`Keyspace::demote_tick`](crate::Keyspace::demote_tick).
    Demote,
}

/// Per-namespace demotion configuration (ADR-0053 D1–D3). S19 surfaces
/// these as `INF.NS` keys; until then they are construction parameters.
#[derive(Copy, Clone, Debug)]
pub struct DemotionConfig {
    /// RAM-residency budget in bytes: the committed ring window this
    /// namespace may hold (ADR-0053 D1 — resident bytes, not live bytes).
    pub mem_budget_bytes: u64,
    /// Mutable-region target as a fraction of the budget, in permille
    /// (ADR-0053 D2; default 250 = 25%, S22-tuned).
    pub mutable_permille: u32,
    /// Seal/release advancement budget per MAINTAIN slice (ADR-0053 D3;
    /// default one commit page). The fill-test slack bound equals this.
    pub slice_bytes: u64,
}

/// ADR-0053 D2 default: 25% of the memory budget stays mutable.
pub const MUTABLE_PERMILLE_DEFAULT: u32 = 250;

impl DemotionConfig {
    /// The defaults for a given budget: 25% mutable fraction, one commit
    /// page (`page_bytes`) per slice.
    #[must_use]
    pub fn for_budget(mem_budget_bytes: u64, page_bytes: u64) -> DemotionConfig {
        DemotionConfig {
            mem_budget_bytes,
            mutable_permille: MUTABLE_PERMILLE_DEFAULT,
            slice_bytes: page_bytes,
        }
    }

    /// The ring reservation this budget needs — ADR-0052 D1 instantiated
    /// with ADR-0053 D3's slack: `next_pow2(budget + slice)`. `None` on
    /// overflow or a zero budget — namespace creation surfaces that as a
    /// typed configuration error, never a wrap or panic.
    #[must_use]
    pub fn ring_reserve_bytes(&self) -> Option<usize> {
        if self.mem_budget_bytes == 0 {
            return None;
        }
        let want = self.mem_budget_bytes.checked_add(self.slice_bytes)?;
        let ring = want.checked_next_power_of_two()?;
        usize::try_from(ring).ok()
    }

    /// The mutable-region byte target (`budget × permille / 1000`).
    #[must_use]
    pub fn mutable_target_bytes(&self) -> u64 {
        // Permille ≤ 1000 by construction; the product fits u128 → u64.
        ((u128::from(self.mem_budget_bytes) * u128::from(self.mutable_permille)) / 1000) as u64
    }
}

/// One demotion MAINTAIN round's work (the debt-drain observable the
/// fill AC reads).
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct DemoteStats {
    /// Bytes sealed read-only (ro-boundary advancement).
    pub sealed_bytes: u64,
    /// Bytes released below the flushed watermark (head advancement).
    pub released_bytes: u64,
    /// Tables that had demotion work this round.
    pub tables_active: u32,
}

impl DemoteStats {
    pub fn absorb(&mut self, other: DemoteStats) {
        self.sealed_bytes += other.sealed_bytes;
        self.released_bytes += other.released_bytes;
        self.tables_active += other.tables_active;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_derivation_is_adr_0052_d1_with_slice_slack() {
        let config = DemotionConfig::for_budget(64 << 20, 1 << 20);
        assert_eq!(config.ring_reserve_bytes(), Some(128 << 20), "next_pow2(64 MiB + 1 MiB)");
        // Exactly a power of two + slice still rounds up (the slack is
        // part of the reservation, not carved from the budget).
        let exact = DemotionConfig::for_budget((64 << 20) - (1 << 20), 1 << 20);
        assert_eq!(exact.ring_reserve_bytes(), Some(64 << 20));
    }

    #[test]
    fn invalid_budgets_refuse_typed_not_panicking() {
        let zero = DemotionConfig::for_budget(0, 1 << 20);
        assert_eq!(zero.ring_reserve_bytes(), None, "zero budget is a config error");
        let overflow = DemotionConfig::for_budget(u64::MAX - 1, 1 << 20);
        assert_eq!(overflow.ring_reserve_bytes(), None, "overflow refuses, never wraps");
    }

    #[test]
    fn mutable_target_is_the_permille_fraction() {
        let config = DemotionConfig::for_budget(100 << 20, 1 << 20);
        assert_eq!(config.mutable_permille, MUTABLE_PERMILLE_DEFAULT);
        assert_eq!(config.mutable_target_bytes(), 25 << 20, "25% of 100 MiB");
    }
}
