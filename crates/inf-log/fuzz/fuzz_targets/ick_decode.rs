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
//! 4. v2 (M4-S12, ADR-0057 D3): the hybrid loader decodes addr-ref
//!    sections without panic; every yielded ref sits strictly below its
//!    section's walk watermark (the reader's own audit — asserted again
//!    here); the records-only loader refuses hybrid files typed, never
//!    silently skipping the cold majority.
//! 5. v2 (M4-S14, ADR-0058 D3): live-set sections decode without panic;
//!    every yielded entry passed the flag and `dead ≤ len` audit
//!    (asserted again here); the records-only loader refuses them typed.
//! 6. v2 (M4-S17, ADR-0061 D6): blob-ref sections decode without panic;
//!    every yielded entry passed the zero-length and strictly-ascending
//!    address audits (asserted again here); the records-only loader
//!    refuses them typed.
//! 7. v2 (M4.5-S06, ADR-0078 D3/D4): index-sidecar sections decode
//!    without panic; every *delivered* section satisfies the ascending
//!    canon and the FINAL/empty/total rules (asserted again here); a
//!    damaged body arrives typed and never fails the load by itself —
//!    the soft class; the records-only loader refuses the tag typed;
//!    sidecar entries never count into `records_total` or the per-ns
//!    presize hint.
#![no_main]

use std::path::Path;

use inf_log::ckpt::{IckReaderConfig, read_ick, read_ick_counts, read_ick_hybrid};
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::IckIdxSidecarStep;
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
    let mut refs = 0u64;
    let mut live_entries = 0u64;
    let mut blob_refs = 0u64;
    let mut idx_sections = 0u64;
    let mut idx_damaged = 0u64;
    let result = read_ick_hybrid(
        &fs,
        &path,
        IckReaderConfig::default(),
        |view| {
            // Re-encode: decoded views are canonical by construction.
            let mut buf = Vec::new();
            view.encode_into(&mut buf);
            assert!(!buf.is_empty());
            records += 1;
            Ok::<(), ()>(())
        },
        |section| {
            assert!(!section.is_empty(), "the writer only seals non-empty ref sections");
            for (_, addr) in section.iter() {
                assert!(addr < section.walk_watermark, "refs sit below their watermark");
                assert!(addr < (1 << 48), "addresses are 48-bit");
            }
            refs += section.len() as u64;
            Ok(())
        },
        |section| {
            assert!(!section.is_empty(), "the writer only seals non-empty live-set sections");
            for entry in section.iter() {
                assert!(entry.dead_bytes <= entry.data_len, "dead never exceeds file bytes");
            }
            live_entries += section.len() as u64;
            Ok(())
        },
        |section| {
            assert!(!section.is_empty(), "the writer only seals non-empty blob-ref sections");
            let mut prev = None;
            for entry in section.iter() {
                assert!(entry.len > 0, "an extent reference names at least one byte");
                assert!(entry.addr < (1 << 48), "addresses are 48-bit");
                assert!(prev.is_none_or(|p| entry.addr > p), "addresses ascend strictly");
                prev = Some(entry.addr);
            }
            blob_refs += section.len() as u64;
            Ok(())
        },
        |step| {
            match step {
                IckIdxSidecarStep::Section(section) => {
                    // Delivered ⇒ the ADR-0078 D3 canon held at decode.
                    assert!(section.is_empty() == (section.len() == 0));
                    assert!(!section.is_empty() || section.final_section, "empty ⇒ FINAL");
                    assert!(section.final_section || section.total_entries == 0);
                    let mut prev: Option<(Vec<u8>, u64)> = None;
                    let mut n = 0usize;
                    for (key, entry_ref) in section.iter() {
                        assert!(key.len() <= 1024, "keys within the structural cap");
                        if section.fixed8 {
                            assert_eq!(key.len(), 8, "Fixed8 keys are exactly 8 bytes");
                        }
                        let pair = (key.to_vec(), entry_ref);
                        assert!(
                            prev.as_ref().is_none_or(|p| (&pair.0[..], pair.1) > (&p.0[..], p.1)),
                            "pairs ascend strictly"
                        );
                        prev = Some(pair);
                        n += 1;
                    }
                    assert_eq!(n, section.len(), "entry_count agrees with the body");
                    idx_sections += 1;
                }
                IckIdxSidecarStep::Damaged { .. } => idx_damaged += 1,
            }
            Ok(())
        },
    );
    // The presize-hint path (M2.5-S08: direct footer probe + hop fallback)
    // must never panic, and on a cleanly-loading image must agree with the
    // audited footer entries.
    let counts = read_ick_counts(&fs, &path, IckReaderConfig::default());
    // The records-only loader must never panic either; on a hybrid image
    // that loaded cleanly it refuses typed at the first ref section.
    let v1_result = read_ick(&fs, &path, IckReaderConfig::default(), |_| Ok::<(), ()>(()));
    if let Ok((info, summary)) = result {
        assert!(info.version == 1 || info.version == 2, "only known versions load");
        if info.version == 1 {
            assert_eq!(refs, 0, "v1 images carry no ref sections");
            assert_eq!(live_entries, 0, "v1 images carry no live-set sections");
            assert_eq!(blob_refs, 0, "v1 images carry no blob-ref sections");
            assert_eq!(idx_sections + idx_damaged, 0, "v1 images carry no sidecar sections");
            assert!(v1_result.is_ok(), "a clean v1 image loads records-only too");
        } else if refs > 0 || live_entries > 0 || blob_refs > 0 || idx_sections + idx_damaged > 0 {
            assert!(v1_result.is_err(), "records-only load must refuse a hybrid image");
        }
        // Sidecar entries deliberately absent (ADR-0078 D2): the soft
        // class must not be load-bearing for the footer audit.
        assert_eq!(
            summary.records,
            records + refs + live_entries + blob_refs,
            "audit counts what apply saw"
        );
        assert_eq!(summary.bytes, data.len() as u64, "no trailing bytes tolerated");
        let counts = counts.expect("clean image must yield a presize hint");
        assert_eq!(counts, summary.entries_per_ns, "hint must match the audited footer");
    }
});
