//! ADR-0202 D7: a whole directory listing is a lint error unless its site
//! names its directory's bound. Each `inf_log::` entry of clippy.toml draws
//! its lint, with its exact path, at one plant here. Compile only.
use std::path::Path;

use inf_log::fs::{SegmentFs, StdSegmentFs};

pub fn list_dir(p: &Path) {
    let _ = StdSegmentFs.list_dir(p); // PLANT clippy::disallowed_methods inf_log::fs::SegmentFs::list_dir
}

pub fn list_extent_ids(p: &Path) {
    let _ = inf_log::blob::list_extent_ids(&StdSegmentFs, p); // PLANT clippy::disallowed_methods inf_log::blob::list_extent_ids
}

pub fn list_quarantined_extent_ids(p: &Path) {
    let _ = inf_log::blob::list_quarantined_extent_ids(&StdSegmentFs, p); // PLANT clippy::disallowed_methods inf_log::blob::list_quarantined_extent_ids
}
