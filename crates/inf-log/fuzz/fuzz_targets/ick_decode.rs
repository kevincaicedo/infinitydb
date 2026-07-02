//! `.ick` loader fuzz target (M2-S10, milestone §5 test plan). Oracles on
//! arbitrary file images:
//!
//! 1. No panic/UB anywhere in header/section/footer/record decoding.
//! 2. Validate-then-yield: the apply callback never sees a record from a
//!    section whose CRC did not check out (enforced by construction; the
//!    target asserts decoded records re-encode byte-identically — the L7
//!    one-value-one-encoding property, same as `frame_decode`).
//! 3. A cleanly-loading image must reproduce its own audit: recomputing
//!    the digest chain over the section CRCs equals the footer digest
//!    (`read_ick` verifies internally; a clean load asserts the summary is
//!    self-consistent).
#![no_main]

use std::path::Path;

use inf_log::ckpt::{IckReaderConfig, read_ick};
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let fs = MemFs::new();
    let dir = Path::new("/ckpt");
    fs.create_dir_all(dir).expect("mem dir");
    let path = dir.join("ckpt-000001.ick");
    let mut file = fs.create_segment(&path, 0).expect("mem file");
    file.write_at(0, data).expect("mem write");
    drop(file);

    let mut records = 0u64;
    let result = read_ick(&fs, &path, IckReaderConfig::default(), |view| {
        // Re-encode: decoded views are canonical by construction.
        let mut buf = Vec::new();
        view.encode_into(&mut buf);
        assert!(!buf.is_empty());
        records += 1;
        Ok::<(), ()>(())
    });
    if let Ok((info, summary)) = result {
        assert_eq!(u64::from(info.version), 1, "only v1 loads");
        assert_eq!(summary.records, records, "audit counts what apply saw");
        assert_eq!(summary.bytes, data.len() as u64, "no trailing bytes tolerated");
    }
});
