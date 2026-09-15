//! Shared fixture for the `recover_*` and `fault_recovery` integration
//! tests (review 2026-08-30 L18 R13, batch 65): the one durable
//! namespace, the fixed clock and anchor, the cell's `DurableConfig`,
//! the keyspace, the recovery call, the frame stamp and the by-hand log
//! builder each binary used to carry as its own copy. A binary that
//! needs a different segment shape wraps [`cfg_with`] / [`recover_with`]
//! under the same names.
#![allow(dead_code, reason = "six test binaries each use a subset of this fixture")]

use std::path::PathBuf;

use inf_foundation::time::Nanos;
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    CkptConfig, FRAME_HEADER_LEN, FrameStamp, Lsn, MutationEffect, NsId, SegmentConfig, SegmentId,
    SegmentRotor, StagingConfig, StagingRing, create_cell_dirs, segment_file_name,
};
use inf_server::{DurableConfig, RecoverStats, open_cell_log};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StoreConfig, WallAnchor};

pub const NS: NsId = NsId(16);
pub const CELL: u16 = 0;
pub const UNIX_BASE: u64 = 1_750_000_000_000;

pub fn now() -> Nanos {
    Nanos::from_millis(1)
}

pub fn anchor() -> WallAnchor {
    WallAnchor { internal_ms: 0, unix_ms: UNIX_BASE }
}

/// The default cell config: 64 KiB buffered segments, one frame in flight.
pub fn cfg() -> DurableConfig {
    cfg_with(SegmentConfig { segment_bytes: 1 << 16, ..Default::default() })
}

pub fn cfg_with(segment: SegmentConfig) -> DurableConfig {
    DurableConfig {
        data_dir: PathBuf::from("data"),
        staging: StagingConfig::default(),
        segment,
        ckpt: CkptConfig::default(),
        recover: Default::default(),
        flush_bound: 1,
        fua_p50_us_probed: 0,
        device: Default::default(),
        fill: Default::default(),
        group: Default::default(),
    }
}

/// A keyspace with the one `always` durable namespace the fixtures write.
pub fn fresh_keyspace() -> Keyspace {
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

pub fn get(ks: &mut Keyspace, key: &[u8]) -> Option<Vec<u8>> {
    ks.ns_store_mut(NS).expect("ns store").get(key, now()).map(<[u8]>::to_vec)
}

pub fn recover(
    fs: &MemFs,
    ks: &mut Keyspace,
) -> std::io::Result<(SegmentRotor<MemFs>, RecoverStats)> {
    recover_with(fs, ks, &cfg())
}

pub fn recover_with(
    fs: &MemFs,
    ks: &mut Keyspace,
    cfg: &DurableConfig,
) -> std::io::Result<(SegmentRotor<MemFs>, RecoverStats)> {
    open_cell_log(fs.clone(), ks, CELL, cfg, anchor(), now())
        .map(|(rotor, stats, _seed)| (rotor, stats))
}

pub fn stamp(epoch: u32, seq: u64, covered_lsn: u64) -> FrameStamp {
    FrameStamp { epoch, seq, covered_lsn }
}

/// A fresh cell log written through the real rotor + staging ring, one
/// frame per call, with a poke for the tests that then damage it.
pub struct LogBuilder {
    pub fs: MemFs,
    pub rotor: SegmentRotor<MemFs>,
    pub ring: StagingRing,
    pub log_dir: PathBuf,
}

impl LogBuilder {
    pub fn new(fs: &MemFs, cfg: &DurableConfig) -> LogBuilder {
        let dirs = create_cell_dirs(fs, &cfg.data_dir.join(format!("shard-{CELL}"))).expect("dirs");
        let rotor =
            SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), cfg.segment).expect("rotor");
        LogBuilder { fs: fs.clone(), rotor, ring: StagingRing::new(cfg.staging), log_dir: dirs.log }
    }

    /// Stage `records` as one frame, flush it, and return the frame's
    /// (base offset, per-record LSNs). Frames attest like a live `always`
    /// plane (ADR-0031 D1): each stamps `covered_lsn` = its own base — the
    /// watermark of a group commit that fsynced every prior frame.
    pub fn frame(&mut self, records: &[MutationEffect<'_>]) -> (Lsn, Vec<Lsn>) {
        let staged: Vec<_> =
            records.iter().map(|effect| self.ring.stage(effect).expect("stage")).collect();
        self.rotor.maintain(0).expect("maintain");
        let slot = self.rotor.begin_frame(self.ring.pending_frame_len(), 0).expect("reserve");
        let covered = slot.base().to_u64();
        let lease = self.ring.seal(slot.first_record_lsn(), covered, slot.layout());
        let frame = self.ring.leased_frame(&lease).to_vec();
        self.rotor.commit_frame(slot, &frame).expect("commit");
        let lsns: Vec<Lsn> = staged.iter().map(|&at| lease.lsn_of(at)).collect();
        self.ring.release(lease);
        let first = lsns[0];
        (Lsn::new(first.segment, first.offset - FRAME_HEADER_LEN as u32), lsns)
    }

    pub fn set_frame(&mut self, key: &[u8], value: &[u8]) -> (Lsn, Vec<Lsn>) {
        self.frame(&[MutationEffect::StringSet { ns: NS, key, value }])
    }

    pub fn seg_path(&self, id: SegmentId) -> PathBuf {
        self.log_dir.join(segment_file_name(id))
    }

    pub fn poke(&self, id: SegmentId, offset: u32, bytes: &[u8]) {
        let mut file = self.fs.open_write(&self.seg_path(id)).expect("open");
        file.write_at(u64::from(offset), bytes).expect("poke");
    }
}
