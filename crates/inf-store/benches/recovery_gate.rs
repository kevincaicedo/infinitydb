//! M4-S12 recovery-gate bench (§4.1: ≥ 1 GB/s/cell replay, 10 GB node
//! < 15 s, tiering on — the M2-gate regression re-proof, ADR-0057 D6).
//!
//! Measures the two hybrid-checkpoint apply paths recovery is built
//! from, isolated on the CPU side (MemFs — the device half of the boot
//! path is the M2/M2.5 machinery unchanged; the reference-box wall-clock
//! row joins the S22/S24 campaign per the dev-tier evidence rule):
//!
//! - **image row** — a v2 `.ick` of string post-images streamed through
//!   `read_ick_hybrid` → `TieredTable::apply_image` (decode + CRC +
//!   re-append + index insert): GB/s over file bytes, the M2 gate's
//!   currency.
//! - **ref row** — addr-ref sections streamed through the same loader →
//!   `apply_ref` (idempotency probe + insert, zero record bytes):
//!   entries/s. The L4 hypothesis from the ledger: ≥ 20 M entries/s —
//!   at 14 B/entry the cold *index* of a 10× RAM namespace recovers in
//!   seconds without touching the cold tier.
//!
//! Run: `taskset -c 4 cargo bench -p inf-store --bench recovery_gate`
//! Artifact: 3 replicates under `.artifacts/m4/s12/`.

use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

use inf_log::ckpt::{CkptConfig, IckReaderConfig, ick_file_name, read_ick_hybrid};
use inf_log::fs::SegmentFs;
use inf_log::fs::mem::MemFs;
use inf_log::{Lsn, NsId, RecordView, SegmentId, SyncIckWriter};
use inf_store::{AddressSpaceConfig, DemotionConfig, LogicalAddr, TieredTable};

const NS: NsId = NsId(31);
const PAGE: u64 = 1 << 20;

/// Image row: `n` string records of `value_len` bytes each.
fn bench_images(n: u64, value_len: usize) {
    let fs = MemFs::new();
    fs.create_dir_all(Path::new("shard-0")).expect("dir");
    let cfg = CkptConfig::default();
    let mut w = SyncIckWriter::create_v2(
        fs.clone(),
        Path::new("shard-0"),
        &cfg,
        0,
        1,
        Lsn::new(SegmentId(1), 64),
        &[NS.0],
    )
    .expect("create");
    let mut value = vec![0u8; value_len];
    for i in 0..n {
        let key = format!("img:{i:08}").into_bytes();
        value[..8].copy_from_slice(&i.to_le_bytes());
        w.append(&RecordView::StringPostImage { ns: NS, key: &key, value: &value })
            .expect("append");
    }
    let summary = w.finish().expect("finish");
    let path = Path::new("shard-0").join(ick_file_name(1));

    let budget = (n * (value_len as u64 + 64)).next_power_of_two().max(64 << 20);
    let demote =
        DemotionConfig { mem_budget_bytes: budget, mutable_permille: 1000, slice_bytes: PAGE };
    let mut best_gbps = 0.0f64;
    for _ in 0..3 {
        let mut table = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("ring"),
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            usize::try_from(n).expect("fits"),
        )
        .expect("ring");
        let t = Instant::now();
        read_ick_hybrid(
            &fs,
            &path,
            IckReaderConfig::default(),
            |record| {
                if let RecordView::StringPostImage { key, value, .. } = record {
                    table.apply_image(key, value, TieredTable::hash_key(key)).expect("fits");
                }
                Ok::<(), std::convert::Infallible>(())
            },
            |_| Ok(()),
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("load");
        let secs = t.elapsed().as_secs_f64();
        black_box(table.len());
        let gbps = summary.bytes as f64 / 1e9 / secs;
        if gbps > best_gbps {
            best_gbps = gbps;
        }
        println!(
            "  image rep: {:.3} GB/s ({:.0} Mrec/s, {:.3}s for {:.2} GB)",
            gbps,
            n as f64 / 1e6 / secs,
            secs,
            summary.bytes as f64 / 1e9
        );
    }
    println!(
        "image row: {n} records × {value_len} B → best {best_gbps:.3} GB/s (gate ≥ 1 GB/s/cell)"
    );
}

/// Ref row: `n` addr-refs (14 B each), applied index-only.
fn bench_refs(n: u64) {
    let fs = MemFs::new();
    fs.create_dir_all(Path::new("shard-0")).expect("dir");
    let cfg = CkptConfig::default();
    let mut w = SyncIckWriter::create_v2(
        fs.clone(),
        Path::new("shard-0"),
        &cfg,
        0,
        2,
        Lsn::new(SegmentId(1), 64),
        &[NS.0],
    )
    .expect("create");
    let watermark = 1 << 40;
    let mut state = 0x5EEDu64;
    for i in 0..n {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        w.append_ref(NS.0, watermark, state, i * 32).expect("ref");
    }
    let summary = w.finish().expect("finish");
    let path = Path::new("shard-0").join(ick_file_name(2));

    let demote =
        DemotionConfig { mem_budget_bytes: 64 << 20, mutable_permille: 1000, slice_bytes: PAGE };
    let mut best_meps = 0.0f64;
    for _ in 0..3 {
        let mut table = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("ring"),
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::from_raw(watermark).expect("48-bit"),
            },
            demote,
            usize::try_from(n).expect("fits"),
        )
        .expect("ring");
        // One manifested file covering every ref (M4-S14): `apply_ref`
        // counts each slot into its containing file, so the bench seeds
        // the catalog the way recovery does — the measured row includes
        // the live-set count maintenance, honestly.
        table.seed_recovered_files(
            &[inf_log::flush::TierFileMeta {
                id: 0,
                base: LogicalAddr::ZERO,
                data_len: n * 32,
                reason: inf_log::tier::SealReason::Capacity,
                path: Path::new("shard-0/cold/tier-000000.itier").to_path_buf(),
            }],
            1,
        );
        let t = Instant::now();
        read_ick_hybrid(
            &fs,
            &path,
            IckReaderConfig::default(),
            |_| Ok::<(), std::convert::Infallible>(()),
            |section| {
                for (hash, addr) in section.iter() {
                    table.apply_ref(hash, LogicalAddr::from_raw(addr).expect("48-bit"));
                }
                Ok(())
            },
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("load");
        let secs = t.elapsed().as_secs_f64();
        assert_eq!(table.len(), usize::try_from(n).expect("fits"), "every ref slotted");
        black_box(table.len());
        let meps = n as f64 / 1e6 / secs;
        if meps > best_meps {
            best_meps = meps;
        }
        println!(
            "  ref rep: {:.1} M entries/s ({:.3} GB/s of section bytes, {:.3}s for {} refs)",
            meps,
            summary.bytes as f64 / 1e9 / secs,
            secs,
            n
        );
    }
    println!("ref row: {n} refs → best {best_meps:.1} M entries/s (hypothesis ≥ 20 M/s)");
}

fn main() {
    println!("M4-S12 recovery-gate bench (hybrid checkpoint apply, MemFs CPU path)");
    // The M2 gate's shape: ~1 GB of images per cell.
    bench_images(2_000_000, 440);
    // The cold majority of a beyond-RAM namespace: 20 M refs = 280 MB
    // of section bytes standing in for a multi-hundred-GB cold tier.
    bench_refs(20_000_000);
}
