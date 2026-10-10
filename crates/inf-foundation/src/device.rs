//! Device identity (M4.5-S42, ADR-0091 D2): what an `io-properties.toml`
//! model *describes*, so a boot can tell a model measured on this device
//! from one that travelled with the data directory (a restore onto
//! another volume, a container whose mount changed, a probe file copied
//! from another host). Plain data — the probe fills it from
//! `/proc/self/mountinfo`, `/sys` and `/dev/disk/by-uuid` (safe text
//! reads, never `statfs` FFI); the parser reads it back; the boot
//! compares the two with [`DeviceIdentity::mismatch`].

/// The identity of the filesystem + block device holding a directory.
/// Every field may be unknown (empty / 0): the comparison compares only
/// what both sides carry (ADR-0091 D2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// Filesystem type as `mountinfo` names it (`ext4`, `xfs`, `tmpfs`).
    pub fs_type: String,
    /// The filesystem's own UUID (`/dev/disk/by-uuid` → mount source);
    /// empty when unavailable (containers without `/dev/disk`).
    pub fs_uuid: String,
    /// The mount source as the kernel names it (`/dev/nvme0n1p3`);
    /// empty for pseudo filesystems and unresolvable mounts.
    pub device_path: String,
    /// `st_dev` of the directory as `major:minor` — provenance only
    /// (block devices renumber across boots), never compared.
    pub device_major_minor: String,
    /// The block device's logical / physical block sizes (0 = unknown).
    /// Logical is the torn-write unit the frame reader's rule assumes
    /// ≤ `FRAME_ALIGN`; physical is the read-modify-write boundary a
    /// sub-block write pays (S39c's question).
    pub block_logical_bytes: u32,
    pub block_physical_bytes: u32,
    /// `uname -r` at probe time — provenance only, never compared.
    pub kernel_release: String,
}

/// The boot's reading of a stored identity against the live one.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum IdentityVerdict {
    /// The file carries no identity (schema ≤ 2) or neither side carries
    /// a comparable field: accepted, disclosed.
    #[default]
    Unverifiable,
    /// A comparable field agrees.
    Verified,
    /// A comparable field disagrees: the model describes another device.
    Mismatch,
}

impl DeviceIdentity {
    /// True when no field is known — a probe that could not identify
    /// the device (the schema-2 shape).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fs_type.is_empty()
            && self.fs_uuid.is_empty()
            && self.device_path.is_empty()
            && self.device_major_minor.is_empty()
            && self.block_logical_bytes == 0
            && self.block_physical_bytes == 0
            && self.kernel_release.is_empty()
    }

    /// Compare a stored identity (`self`) with the live one (`current`),
    /// most specific field first, only where both sides know the field
    /// (ADR-0091 D2): the UUID decides when both have one; else the
    /// device path *and* the filesystem type; else unverifiable. Returns
    /// the verdict and, on a mismatch, the human-readable reason.
    #[must_use]
    pub fn mismatch(&self, current: &DeviceIdentity) -> (IdentityVerdict, Option<String>) {
        if !self.fs_uuid.is_empty() && !current.fs_uuid.is_empty() {
            return if self.fs_uuid == current.fs_uuid {
                (IdentityVerdict::Verified, None)
            } else {
                (
                    IdentityVerdict::Mismatch,
                    Some(format!(
                        "filesystem uuid {} (probed) vs {} (now)",
                        self.fs_uuid, current.fs_uuid
                    )),
                )
            };
        }
        if !self.device_path.is_empty() && !current.device_path.is_empty() {
            let same_path = self.device_path == current.device_path;
            let same_type = self.fs_type.is_empty()
                || current.fs_type.is_empty()
                || self.fs_type == current.fs_type;
            return if same_path && same_type {
                (IdentityVerdict::Verified, None)
            } else {
                (
                    IdentityVerdict::Mismatch,
                    Some(format!(
                        "device {} {} (probed) vs {} {} (now)",
                        self.fs_type, self.device_path, current.fs_type, current.device_path
                    )),
                )
            };
        }
        (IdentityVerdict::Unverifiable, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(fs_type: &str, uuid: &str, path: &str) -> DeviceIdentity {
        DeviceIdentity {
            fs_type: fs_type.into(),
            fs_uuid: uuid.into(),
            device_path: path.into(),
            ..Default::default()
        }
    }

    /// ADR-0091 D2: the UUID decides when both sides have one, whatever
    /// the path says (a device renamed across boots is the same
    /// filesystem).
    #[test]
    fn uuid_decides_when_both_sides_carry_one() {
        let stored = ident("ext4", "aaaa", "/dev/nvme0n1p3");
        let same = ident("ext4", "aaaa", "/dev/nvme1n1p3");
        assert_eq!(stored.mismatch(&same).0, IdentityVerdict::Verified);
        let other = ident("ext4", "bbbb", "/dev/nvme0n1p3");
        let (verdict, reason) = stored.mismatch(&other);
        assert_eq!(verdict, IdentityVerdict::Mismatch);
        assert!(reason.expect("reason").contains("uuid aaaa"));
    }

    /// Without UUIDs the device path and filesystem type decide together.
    #[test]
    fn path_and_type_decide_without_uuids() {
        let stored = ident("ext4", "", "/dev/nvme0n1p3");
        assert_eq!(
            stored.mismatch(&ident("ext4", "", "/dev/nvme0n1p3")).0,
            IdentityVerdict::Verified
        );
        assert_eq!(
            stored.mismatch(&ident("xfs", "", "/dev/nvme0n1p3")).0,
            IdentityVerdict::Mismatch
        );
        assert_eq!(stored.mismatch(&ident("ext4", "", "/dev/sda1")).0, IdentityVerdict::Mismatch);
        // A side that does not know the type does not veto.
        assert_eq!(stored.mismatch(&ident("", "", "/dev/nvme0n1p3")).0, IdentityVerdict::Verified);
    }

    /// Nothing comparable on either side is never a mismatch — the
    /// schema-2 shape and the container-without-/dev shape.
    #[test]
    fn nothing_comparable_is_unverifiable() {
        let empty = DeviceIdentity::default();
        assert!(empty.is_empty());
        assert_eq!(
            empty.mismatch(&ident("ext4", "aaaa", "/dev/x")).0,
            IdentityVerdict::Unverifiable
        );
        let stored = ident("ext4", "aaaa", "");
        assert_eq!(stored.mismatch(&ident("ext4", "", "/dev/x")).0, IdentityVerdict::Unverifiable);
    }
}
