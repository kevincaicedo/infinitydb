//! M2-S16 recovery-policy legs over the named fault points: the failure
//! states the points inject are exactly the inputs the M2-S14 taxonomy
//! classifies at the next boot — `torn_frame` recovers minus the torn
//! frame, `power_cut_after_seal` resumes at the seal boundary. Point
//! mechanics (fire + typed error per point) live in
//! `inf-log/tests/fault_points.rs`; the crash matrix drives these paths
//! across policies × workloads at M2-S17.

use std::path::PathBuf;

use inf_foundation::fault::{self, FaultSpec};
use inf_foundation::time::Nanos;
use inf_log::fs::mem::MemFs;
use inf_log::{
    CkptConfig, FRAME_HEADER_LEN, Lsn, MutationEffect, NsId, SegmentConfig, SegmentRotor,
    StagingConfig, StagingRing, create_cell_dirs,
};
use inf_server::{DurableConfig, open_cell_log};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StoreConfig, WallAnchor};

const NS: NsId = NsId(16);
const CELL: u16 = 0;

fn now() -> Nanos {
    Nanos::from_millis(1)
}

fn anchor() -> WallAnchor {
    WallAnchor { internal_ms: 0, unix_ms: 1_750_000_000_000 }
}

fn cfg() -> DurableConfig {
    DurableConfig {
        data_dir: PathBuf::from("data"),
        staging: StagingConfig::default(),
        segment: SegmentConfig { segment_bytes: 1 << 16, ..Default::default() },
        ckpt: CkptConfig::default(),
        recover: Default::default(),
        flush_bound: 1,
        fua_p50_us_probed: 0,
        device: Default::default(),
        fill: Default::default(),
        group: Default::default(),
    }
}

fn fresh_keyspace() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(NsSpec {
        id: NS,
        name: b"ledger".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
        tier: None,
    })
    .expect("ns");
    ks
}

struct LogBuilder {
    rotor: SegmentRotor<MemFs>,
    ring: StagingRing,
}

impl LogBuilder {
    fn new(fs: &MemFs, cfg: &DurableConfig) -> LogBuilder {
        let dirs = create_cell_dirs(fs, &cfg.data_dir.join(format!("shard-{CELL}"))).expect("dirs");
        let rotor = SegmentRotor::create_fresh(fs.clone(), dirs.log, cfg.segment).expect("rotor");
        LogBuilder { rotor, ring: StagingRing::new(cfg.staging) }
    }

    /// One `SET key value` frame; returns its base LSN.
    fn set_frame(&mut self, key: &[u8], value: &[u8]) -> Lsn {
        let at = self.ring.stage(&MutationEffect::StringSet { ns: NS, key, value }).expect("stage");
        self.rotor.maintain(0).expect("maintain");
        let lease = self.ring.flush_into(&mut self.rotor, 0).expect("flush").expect("frame");
        let lsn = lease.lsn_of(at);
        self.ring.release(lease);
        Lsn::new(lsn.segment, lsn.offset - FRAME_HEADER_LEN as u32)
    }
}

fn get(ks: &mut Keyspace, key: &[u8]) -> Option<Vec<u8>> {
    ks.ns_store_mut(NS).expect("ns store").get(key, now()).map(<[u8]>::to_vec)
}

/// `torn_frame` fires on the final write before a crash → the next boot
/// truncates the torn tail (M2-S14) and serves everything before it.
#[test]
fn torn_frame_then_crash_recovers_minus_the_torn_frame() {
    fault::disarm_all();
    let fs = MemFs::new();
    let config = cfg();
    let mut log = LogBuilder::new(&fs, &config);
    log.set_frame(b"a", b"1");
    log.set_frame(b"b", b"2");
    fault::arm(inf_log::fault::TORN_FRAME, FaultSpec::Nth(1));
    let torn_base = log.set_frame(b"c", b"3"); // succeeds — the disk lies
    assert_eq!(fault::fired(inf_log::fault::TORN_FRAME), 1);
    fault::disarm_all();
    drop(log); // power cut

    let mut ks = fresh_keyspace();
    let (mut rotor, stats, _seed) =
        open_cell_log(fs.clone(), &mut ks, CELL, &config, anchor(), now()).expect("recovers");
    assert_eq!(stats.torn_truncated_at, Some(torn_base), "tail truncated at the torn frame");
    assert_eq!(get(&mut ks, b"a").as_deref(), Some(b"1".as_slice()));
    assert_eq!(get(&mut ks, b"b").as_deref(), Some(b"2".as_slice()));
    assert_eq!(get(&mut ks, b"c"), None, "the torn frame was never durable");
    // The cell keeps serving: the log resumes at the truncation point.
    rotor.maintain(0).expect("resume");
}

/// `power_cut_after_seal` → the sealed segment is durable; boot resumes
/// at the seal boundary with zero loss and no torn truncation.
#[test]
fn power_cut_after_seal_recovers_at_the_seal_boundary() {
    fault::disarm_all();
    let fs = MemFs::new();
    let config = DurableConfig {
        segment: SegmentConfig { segment_bytes: 4096, ..Default::default() },
        ..cfg()
    };
    let mut log = LogBuilder::new(&fs, &config);
    fault::arm(inf_log::fault::POWER_CUT_AFTER_SEAL, FaultSpec::Always);
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let died = loop {
        let key = format!("k:{}", keys.len()).into_bytes();
        let staged = log
            .ring
            .stage(&MutationEffect::StringSet { ns: NS, key: &key, value: &[0xCD; 600] })
            .expect("stage");
        log.rotor.maintain(0).ok();
        match log.ring.flush_into(&mut log.rotor, 0) {
            Ok(lease) => {
                let lease = lease.expect("frame");
                let _ = staged;
                log.ring.release(lease);
                keys.push(key);
            }
            Err(err) => break err,
        }
    };
    fault::disarm_all();
    assert!(died.to_string().contains("power_cut_after_seal"), "{died}");
    assert!(!keys.is_empty(), "several frames landed before the seal");
    drop(log); // the process dies at the seal

    let mut ks = fresh_keyspace();
    let (mut rotor, stats, _seed) =
        open_cell_log(fs.clone(), &mut ks, CELL, &config, anchor(), now()).expect("recovers");
    assert_eq!(stats.torn_truncated_at, None, "a sealed end is clean, never torn");
    for key in &keys {
        assert!(get(&mut ks, key).is_some(), "sealed frame for {key:?} survives");
    }
    rotor.maintain(0).expect("resume at the seal boundary");
}
