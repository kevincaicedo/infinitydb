//! M2 durability fault-point inventory.
//!
//! These names are stable test inputs for the crash matrix. Actual injection
//! is owned by the harness or server path that reaches the dangerous step.

use inf_foundation::fault::FaultPoint;

pub const LOG_APPEND_SHORT_WRITE: FaultPoint = FaultPoint::new("log_append_short_write");
pub const TORN_FRAME: FaultPoint = FaultPoint::new("torn_frame");
pub const FSYNC_ERR: FaultPoint = FaultPoint::new("fsync_err");
pub const MANIFEST_RENAME_FAIL: FaultPoint = FaultPoint::new("manifest_rename_fail");
pub const POWER_CUT_AFTER_MANIFEST: FaultPoint = FaultPoint::new("power_cut_after_manifest");
pub const DIR_FSYNC_FAIL: FaultPoint = FaultPoint::new("dir_fsync_fail");
pub const CHECKPOINT_WRITE_ENOSPC: FaultPoint = FaultPoint::new("checkpoint_write_enospc");
pub const POWER_CUT_AFTER_SEAL: FaultPoint = FaultPoint::new("power_cut_after_seal");

/// Initial M2 durability fault points; open to M3+ extension by ADR/milestone.
pub const M2_DURABILITY_FAULT_POINTS: &[FaultPoint] = &[
    LOG_APPEND_SHORT_WRITE,
    TORN_FRAME,
    FSYNC_ERR,
    MANIFEST_RENAME_FAIL,
    POWER_CUT_AFTER_MANIFEST,
    DIR_FSYNC_FAIL,
    CHECKPOINT_WRITE_ENOSPC,
    POWER_CUT_AFTER_SEAL,
];

#[cfg(test)]
mod tests {
    use super::*;
    use inf_foundation::fault::validate_fault_inventory;

    #[test]
    fn fault_m2_inventory_names_are_stable_and_unique() {
        validate_fault_inventory(M2_DURABILITY_FAULT_POINTS).expect("unique fault names");

        let expected = [
            "log_append_short_write",
            "torn_frame",
            "fsync_err",
            "manifest_rename_fail",
            "power_cut_after_manifest",
            "dir_fsync_fail",
            "checkpoint_write_enospc",
            "power_cut_after_seal",
        ];

        assert_eq!(M2_DURABILITY_FAULT_POINTS.len(), expected.len());
        for (point, name) in M2_DURABILITY_FAULT_POINTS.iter().zip(expected) {
            assert_eq!(point.name(), name);
        }
    }
}
