//! The plane's checked mesh and its credit paths (ADR-0192).
//!
//! Every mesh the plane runs on is checked once, by [`checked_mesh_config`]:
//! the shipped binary's mesh and every simulator mesh are `const`s through
//! it, so a refused pair fails the build, and a mesh sized by a flag is
//! refused at the flag before any cell exists. The check keeps no copy of
//! the fabric's ring relation: it reads
//! [`MeshConfig::has_reply_headroom`], the relation's one statement.

use core::fmt;

use inf_fabric::MeshConfig;

use crate::limits::FAN_RESERVE_PER_PEER;

/// Why a mesh the plane would run on was refused; each variant names the
/// numbers it refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeshConfigRefused {
    /// No general window: every credit toward a peer would be the publish
    /// fan's reserve or less.
    BelowFanReserve { data_credits: u32, floor: u32 },
    /// The fabric's rings are power-of-two sized.
    RingNotPowerOfTwo { ring_capacity: usize },
    /// The ring cannot hold the peer's replies beside this side's frames.
    NoReplyHeadroom { ring_capacity: usize, data_credits: u32 },
}

impl fmt::Display for MeshConfigRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            MeshConfigRefused::BelowFanReserve { data_credits, floor } => write!(
                f,
                "data_credits {data_credits} leaves no general window beside the publish fan's \
                 reserved credit: at least {floor}"
            ),
            MeshConfigRefused::RingNotPowerOfTwo { ring_capacity } => {
                write!(f, "ring_capacity {ring_capacity} is not a power of two")
            }
            MeshConfigRefused::NoReplyHeadroom { ring_capacity, data_credits } => write!(
                f,
                "ring_capacity {ring_capacity} has no reply headroom for data_credits \
                 {data_credits}: at most {}",
                ring_capacity / 2
            ),
        }
    }
}

impl std::error::Error for MeshConfigRefused {}

/// The one check of a mesh the plane runs on (ADR-0192 D5): more data
/// credits per directed pair than the publish fan's reserve, a power-of-two
/// ring, and the fabric's reply headroom. `const`, so a mesh written as a
/// constant is checked when the program is built.
///
/// # Errors
/// [`MeshConfigRefused`], in that order of checks.
pub const fn checked_mesh_config(
    ring_capacity: usize,
    data_credits: u32,
) -> Result<MeshConfig, MeshConfigRefused> {
    if data_credits <= FAN_RESERVE_PER_PEER {
        return Err(MeshConfigRefused::BelowFanReserve {
            data_credits,
            floor: FAN_RESERVE_PER_PEER + 1,
        });
    }
    if !ring_capacity.is_power_of_two() {
        return Err(MeshConfigRefused::RingNotPowerOfTwo { ring_capacity });
    }
    let config = MeshConfig { ring_capacity, data_credits };
    if !config.has_reply_headroom() {
        return Err(MeshConfigRefused::NoReplyHeadroom { ring_capacity, data_credits });
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::{MeshConfigRefused, checked_mesh_config};
    use inf_fabric::MeshConfig;

    /// Each case is one on which the fabric's relation and its successor
    /// with a progress term agree, so no verdict here rests on a copy of
    /// the relation; the boundary case reads the fabric's function.
    #[test]
    fn checked_mesh_config_refuses_each_shape_and_reads_the_fabric_relation() {
        assert!(checked_mesh_config(8, 2).is_ok());
        assert!(checked_mesh_config(4096, 1024).is_ok());
        assert_eq!(
            checked_mesh_config(8, 1).err(),
            Some(MeshConfigRefused::BelowFanReserve { data_credits: 1, floor: 2 })
        );
        assert_eq!(
            checked_mesh_config(8, 5).err(),
            Some(MeshConfigRefused::NoReplyHeadroom { ring_capacity: 8, data_credits: 5 })
        );
        assert_eq!(
            checked_mesh_config(12, 2).err(),
            Some(MeshConfigRefused::RingNotPowerOfTwo { ring_capacity: 12 })
        );
        let boundary = MeshConfig { ring_capacity: 8, data_credits: 4 };
        assert_eq!(checked_mesh_config(8, 4).is_ok(), boundary.has_reply_headroom());
    }
}
