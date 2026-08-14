//! Index-sidecar decode fuzz target (M4.5-S06 — the L9 obligation
//! ADR-0073 D5.4 names, landing with the reader). Structure-aware where
//! `ick_decode` is byte-blind: the input drives a *writer* op stream
//! (valid multi-index sidecar images across both key schemes, sealed by
//! the real `SyncIckWriter`), then optionally corrupts bytes — so the
//! reader's 0x06 arm and body canon see deep, well-formed-then-damaged
//! shapes from the first execution instead of waiting on corpus luck.
//!
//! Oracles:
//! 1. No panic/UB anywhere, corrupted or not.
//! 2. Uncorrupted images load cleanly with zero damaged deliveries and
//!    reproduce the writer's summary exactly (records_total and the
//!    per-ns counts both exclude sidecar entries — ADR-0078 D2).
//! 3. Every delivered section satisfies the ADR-0078 D3 canon
//!    (re-asserted here); a Damaged delivery never fails the load by
//!    itself — file-level errors come only from framing/digest damage.
#![no_main]

use std::path::Path;

use inf_log::ckpt::{IckReaderConfig, ick_file_name, read_ick_counts, read_ick_hybrid};
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{CkptConfig, IckIdxSidecarStep, IdxSidecarMeta, Lsn, SegmentId, SyncIckWriter};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let fs = MemFs::new();
    let dir = Path::new("/ckpt");
    fs.create_dir_all(dir).expect("mem dir");
    // Small sections force multi-section streams early.
    let cfg = CkptConfig { section_bytes: 64 + u32::from(data[0]), ..Default::default() };
    let mut w =
        SyncIckWriter::create_v2(fs.clone(), dir, &cfg, 0, 1, Lsn::new(SegmentId(1), 64), &[16])
            .expect("create v2");

    // Interpret the input as an op stream over four index streams; the
    // per-index counters keep the writer's own canon (ascending pairs,
    // contiguous ordinals) satisfied — writer asserts must never fire
    // on generator output.
    let mut ordinals = [0u64; 4];
    let mut next_key = [0u64; 4];
    let mut finaled = [false; 4];
    let mut input = &data[1..];
    let mut ops = 0;
    while input.len() >= 3 && ops < 512 {
        let (op, a, b) = (input[0], input[1], input[2]);
        input = &input[3..];
        ops += 1;
        let slot = (a % 4) as usize;
        let fixed8 = slot % 2 == 0;
        let meta = IdxSidecarMeta {
            ns: 16,
            index_id: slot as u32 + 1,
            generation: u64::from(slot as u32) + 1,
            key_encoding_version: 1,
            fixed8,
        };
        if finaled[slot] {
            continue;
        }
        match op % 3 {
            0 | 1 => {
                // Strictly-ascending key derivation: a monotone counter
                // stepped by the fuzz byte (+1 so equality never occurs).
                next_key[slot] += u64::from(b) + 1;
                let fixed_key = next_key[slot].to_be_bytes();
                let var_key = format!("k{:016x}-{}", next_key[slot], b % 8);
                let key: &[u8] = if fixed8 { &fixed_key } else { var_key.as_bytes() };
                w.append_idx_entry(&meta, ordinals[slot], key, u64::from(b))
                    .expect("mem write");
                ordinals[slot] += 1;
            }
            _ => {
                w.append_idx_final(&meta, ordinals[slot]).expect("mem write");
                finaled[slot] = true;
            }
        }
    }
    // Even slots close their streams; odd ones may stay FINAL-less —
    // the loader's Incomplete shape is reader-legal and must decode.
    for slot in (0..4).step_by(2) {
        if !finaled[slot] && ordinals[slot] > 0 {
            let meta = IdxSidecarMeta {
                ns: 16,
                index_id: slot as u32 + 1,
                generation: u64::from(slot as u32) + 1,
                key_encoding_version: 1,
                fixed8: true,
            };
            w.append_idx_final(&meta, ordinals[slot]).expect("mem write");
        }
    }
    let summary = w.finish().expect("finish");
    let path = dir.join(ick_file_name(1));

    // Optional corruption from the input tail: up to four byte-xors.
    let mut image = fs.contents(&path).expect("image");
    let mut corrupted = false;
    let tail = &data[data.len().saturating_sub(3 * ((data[0] % 5) as usize))..];
    for chunk in tail.chunks_exact(3) {
        let at = (usize::from(chunk[0]) << 8 | usize::from(chunk[1])) % image.len();
        if chunk[2] != 0 {
            image[at] ^= chunk[2];
            corrupted = true;
        }
    }
    let fs2 = MemFs::new();
    fs2.create_dir_all(dir).expect("mem dir");
    let mut f = fs2.create_segment(&path, 0).expect("mem file");
    f.write_at(0, &image).expect("mem write");
    drop(f);

    let mut sections = 0u64;
    let mut damaged = 0u64;
    let result = read_ick_hybrid(
        &fs2,
        &path,
        IckReaderConfig::default(),
        |_| Ok::<(), ()>(()),
        |_| Ok(()),
        |_| Ok(()),
        |_| Ok(()),
        |step| {
            match step {
                IckIdxSidecarStep::Section(section) => {
                    assert!(!section.is_empty() || section.final_section, "empty ⇒ FINAL");
                    assert!(section.final_section || section.total_entries == 0);
                    let mut prev: Option<(Vec<u8>, u64)> = None;
                    let mut n = 0usize;
                    for (key, entry_ref) in section.iter() {
                        assert!(key.len() <= 1024);
                        if section.fixed8 {
                            assert_eq!(key.len(), 8);
                        }
                        let pair = (key.to_vec(), entry_ref);
                        assert!(
                            prev.as_ref().is_none_or(|p| (&pair.0[..], pair.1) > (&p.0[..], p.1)),
                            "delivered pairs ascend strictly"
                        );
                        prev = Some(pair);
                        n += 1;
                    }
                    assert_eq!(n, section.len());
                    sections += 1;
                }
                IckIdxSidecarStep::Damaged { .. } => damaged += 1,
            }
            Ok(())
        },
    );
    // The counts probe never panics either way.
    let _ = read_ick_counts(&fs2, &path, IckReaderConfig::default());
    if !corrupted {
        let (_, audit) = result.expect("uncorrupted generator output loads cleanly");
        assert_eq!(audit, summary, "loader audit reproduces the writer summary");
        assert_eq!(damaged, 0, "no damage without corruption");
        assert_eq!(sections, u64::from(summary.sections), "every section delivers");
    }
});
