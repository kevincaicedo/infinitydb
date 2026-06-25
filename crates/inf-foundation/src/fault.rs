//! Deterministic fault-point vocabulary and trigger primitives (L7, L9).
//!
//! Fault points are named, stable strings. Injection state is explicit and
//! seedable so crash tests can replay one exact occurrence sequence.

#[cfg(any(test, feature = "fault-injection"))]
use crate::hash64;

/// A named point where a test harness may inject a durability fault.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FaultPoint {
    name: &'static str,
}

impl FaultPoint {
    #[inline]
    pub const fn new(name: &'static str) -> FaultPoint {
        FaultPoint { name }
    }

    #[inline]
    pub const fn name(self) -> &'static str {
        self.name
    }
}

/// Static fault inventory validation error.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FaultInventoryError {
    EmptyName { index: usize },
    DuplicateName { first: usize, second: usize, name: &'static str },
}

/// Validate that a static fault inventory has non-empty, unique names.
pub fn validate_fault_inventory(points: &[FaultPoint]) -> Result<(), FaultInventoryError> {
    let mut i = 0;
    while i < points.len() {
        let name = points[i].name();
        if name.is_empty() {
            return Err(FaultInventoryError::EmptyName { index: i });
        }

        let mut j = i + 1;
        while j < points.len() {
            if name == points[j].name() {
                return Err(FaultInventoryError::DuplicateName { first: i, second: j, name });
            }
            j += 1;
        }
        i += 1;
    }

    Ok(())
}

/// Deterministic trigger policy for one fault point.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FaultTrigger {
    Never,
    Always,
    /// Fire exactly when the 1-indexed occurrence counter reaches `occurrence`.
    Nth {
        occurrence: u64,
    },
    /// Fire when a seeded deterministic draw falls below `numerator/denominator`.
    Probability {
        numerator: u32,
        denominator: u32,
        seed: u64,
    },
}

impl FaultTrigger {
    pub const fn nth(occurrence: u64) -> Result<FaultTrigger, FaultTriggerError> {
        if occurrence == 0 {
            Err(FaultTriggerError::ZeroOccurrence)
        } else {
            Ok(FaultTrigger::Nth { occurrence })
        }
    }

    pub const fn probability(
        numerator: u32,
        denominator: u32,
        seed: u64,
    ) -> Result<FaultTrigger, FaultTriggerError> {
        if denominator == 0 || numerator > denominator {
            Err(FaultTriggerError::InvalidProbability { numerator, denominator })
        } else {
            Ok(FaultTrigger::Probability { numerator, denominator, seed })
        }
    }

    #[cfg(any(test, feature = "fault-injection"))]
    fn fires(self, point: FaultPoint, occurrence: u64) -> bool {
        match self {
            FaultTrigger::Never => false,
            FaultTrigger::Always => true,
            FaultTrigger::Nth { occurrence: target } => occurrence == target,
            FaultTrigger::Probability { numerator, denominator, seed } => {
                probability_fires(point, occurrence, numerator, denominator, seed)
            }
        }
    }
}

/// Trigger construction error.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FaultTriggerError {
    ZeroOccurrence,
    InvalidProbability { numerator: u32, denominator: u32 },
}

/// Explicit occurrence state for one trigger.
#[derive(Clone, Debug)]
pub struct FaultTriggerState {
    trigger: FaultTrigger,
    seen: u64,
}

impl FaultTriggerState {
    #[inline]
    pub const fn new(trigger: FaultTrigger) -> FaultTriggerState {
        FaultTriggerState { trigger, seen: 0 }
    }

    #[inline]
    pub const fn trigger(&self) -> FaultTrigger {
        self.trigger
    }

    #[inline]
    pub const fn occurrences(&self) -> u64 {
        self.seen
    }

    /// Record one occurrence and report whether the fault fires.
    #[inline]
    pub fn should_fire(&mut self, point: FaultPoint) -> bool {
        #[cfg(any(test, feature = "fault-injection"))]
        {
            self.seen = self.seen.saturating_add(1);
            self.trigger.fires(point, self.seen)
        }

        #[cfg(not(any(test, feature = "fault-injection")))]
        {
            let _ = point;
            false
        }
    }
}

#[cfg(any(test, feature = "fault-injection"))]
fn probability_fires(
    point: FaultPoint,
    occurrence: u64,
    numerator: u32,
    denominator: u32,
    seed: u64,
) -> bool {
    if numerator == 0 {
        return false;
    }
    if numerator == denominator {
        return true;
    }

    let point_seed = hash64(point.name().as_bytes(), seed);
    let draw = hash64(&occurrence.to_le_bytes(), point_seed);
    let scaled = ((u128::from(draw) * u128::from(denominator)) >> 64) as u32;
    scaled < numerator
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: FaultPoint = FaultPoint::new("fault_a");
    const B: FaultPoint = FaultPoint::new("fault_b");

    #[test]
    fn fault_inventory_validation_accepts_unique_names() {
        assert_eq!(validate_fault_inventory(&[A, B]), Ok(()));
    }

    #[test]
    fn fault_inventory_validation_rejects_empty_name() {
        assert_eq!(
            validate_fault_inventory(&[A, FaultPoint::new("")]),
            Err(FaultInventoryError::EmptyName { index: 1 })
        );
    }

    #[test]
    fn fault_inventory_validation_rejects_duplicate_name() {
        assert_eq!(
            validate_fault_inventory(&[A, B, A]),
            Err(FaultInventoryError::DuplicateName { first: 0, second: 2, name: "fault_a" })
        );
    }

    #[test]
    fn fault_nth_trigger_fires_once_on_target_occurrence() {
        let trigger = FaultTrigger::nth(3).expect("non-zero occurrence");
        let mut state = FaultTriggerState::new(trigger);

        assert!(!state.should_fire(A));
        assert!(!state.should_fire(A));
        assert!(state.should_fire(A));
        assert!(!state.should_fire(A));
        assert_eq!(state.occurrences(), 4);
    }

    #[test]
    fn fault_always_and_never_triggers_are_deterministic() {
        let mut always = FaultTriggerState::new(FaultTrigger::Always);
        let mut never = FaultTriggerState::new(FaultTrigger::Never);

        for _ in 0..16 {
            assert!(always.should_fire(A));
            assert!(!never.should_fire(A));
        }
    }

    #[test]
    fn fault_probability_trigger_replays_by_seed() {
        let trigger = FaultTrigger::probability(1, 3, 0xC0FF_EE00).expect("valid probability");
        let mut left = FaultTriggerState::new(trigger);
        let mut right = FaultTriggerState::new(trigger);

        for _ in 0..64 {
            assert_eq!(left.should_fire(A), right.should_fire(A));
        }
    }

    #[test]
    fn fault_probability_validates_bounds() {
        assert_eq!(
            FaultTrigger::probability(2, 1, 0),
            Err(FaultTriggerError::InvalidProbability { numerator: 2, denominator: 1 })
        );
        assert_eq!(
            FaultTrigger::probability(0, 0, 0),
            Err(FaultTriggerError::InvalidProbability { numerator: 0, denominator: 0 })
        );
        assert_eq!(FaultTrigger::nth(0), Err(FaultTriggerError::ZeroOccurrence));
    }
}
