//! Scheduler groups v0: deficit-weighted budgets for the reactor loop's
//! EXECUTE and MAINTAIN steps (master plan §5.2, Seastar's discipline).
//!
//! Two classes exist at M0 — `Foreground` (parse/execute) and `Maintenance`
//! (stats flush; expiry/eviction/checkpoint slices join in M1/M2). Each
//! iteration refills every group's deficit by `quantum × weight`; work
//! charges against the deficit; the cap keeps an idle group from hoarding an
//! unbounded burst. Budgets are abstract work units — the loop charges one
//! unit per command executed / maintenance item processed.

/// Scheduling class. Indexes into the group table.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum GroupClass {
    Foreground = 0,
    Maintenance = 1,
}

const GROUPS: usize = 2;

#[derive(Copy, Clone, Debug)]
struct Group {
    weight: u32,
    deficit: u32,
}

/// Deficit-weighted group scheduler. See module docs.
#[derive(Debug)]
pub struct GroupScheduler {
    groups: [Group; GROUPS],
    quantum: u32,
    max_deficit: u32,
}

impl GroupScheduler {
    /// `quantum` is the per-iteration refill per weight point; `max_deficit`
    /// caps accumulated burst (idle groups don't hoard).
    ///
    /// # Panics
    /// Panics if any weight or the quantum is zero — a zero-weight group
    /// would silently starve, which is exactly what this scheduler exists to
    /// prevent.
    pub fn new(
        quantum: u32,
        max_deficit: u32,
        fg_weight: u32,
        maint_weight: u32,
    ) -> GroupScheduler {
        assert!(quantum > 0 && fg_weight > 0 && maint_weight > 0, "zero quantum/weight");
        GroupScheduler {
            groups: [
                Group { weight: fg_weight, deficit: quantum * fg_weight },
                Group { weight: maint_weight, deficit: quantum * maint_weight },
            ],
            quantum,
            max_deficit,
        }
    }

    /// M0 defaults: foreground-heavy (8:1), one-command quantum granularity
    /// scaled for pipelined batches.
    pub fn m0_default() -> GroupScheduler {
        GroupScheduler::new(64, 4096, 8, 1)
    }

    /// Per-iteration refill (loop step boundary).
    pub fn refill(&mut self) {
        for g in &mut self.groups {
            g.deficit = (g.deficit + self.quantum * g.weight).min(self.max_deficit);
        }
    }

    /// Work units `class` may spend this iteration.
    pub fn budget(&self, class: GroupClass) -> u32 {
        self.groups[class as usize].deficit
    }

    /// Charge spent work. Saturating: overspending a slice (one oversized
    /// batch) zeroes the deficit and the group sits out following slices —
    /// bounded, not punished forever.
    pub fn charge(&mut self, class: GroupClass, used: u32) {
        let g = &mut self.groups[class as usize];
        g.deficit = g.deficit.saturating_sub(used);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_refill_and_cap() {
        let mut s = GroupScheduler::new(10, 100, 8, 1);
        assert_eq!(s.budget(GroupClass::Foreground), 80);
        assert_eq!(s.budget(GroupClass::Maintenance), 10);
        for _ in 0..20 {
            s.refill();
        }
        assert_eq!(s.budget(GroupClass::Foreground), 100, "cap bounds the burst");
        assert_eq!(s.budget(GroupClass::Maintenance), 100);
    }

    #[test]
    fn charge_is_saturating() {
        let mut s = GroupScheduler::new(10, 100, 1, 1);
        s.charge(GroupClass::Foreground, 9999);
        assert_eq!(s.budget(GroupClass::Foreground), 0);
        s.refill();
        assert_eq!(s.budget(GroupClass::Foreground), 10, "recovers by quantum, not debt");
    }

    #[test]
    fn maintenance_keeps_progressing_under_foreground_load() {
        // The HoL guarantee in miniature: however much foreground charges,
        // maintenance's refill is untouched.
        let mut s = GroupScheduler::new(10, 1000, 8, 1);
        for _ in 0..100 {
            s.refill();
            let fg = s.budget(GroupClass::Foreground);
            s.charge(GroupClass::Foreground, fg); // saturate foreground
            s.charge(GroupClass::Maintenance, 5);
        }
        assert!(s.budget(GroupClass::Maintenance) >= 5, "maintenance never starved");
    }
}
