//! M2-S03 ACs: the staging ring is fixed-capacity with exact
//! `log_staging_bytes` accounting at every append/drain site, refuses with
//! typed backpressure when full (bounded memory, never an unbounded
//! queue), and loses or reorders nothing — every accepted effect comes
//! back from the log byte-identical, in order, at the LSN its lease
//! resolved.

use std::path::PathBuf;

use inf_log::FRAME_ALIGN;
use inf_log::fs::mem::MemFs;
use inf_log::{
    FRAME_HEADER_LEN, FRAME_TRAILER_LEN, Lsn, MutationEffect, NsId, ReaderConfig, SegmentConfig,
    SegmentId, SegmentReader, SegmentRotor, StagingConfig, StagingRing, create_cell_dirs,
};

const FRAME_OVERHEAD: u32 = (FRAME_HEADER_LEN + FRAME_TRAILER_LEN) as u32;

fn mem_rotor(fs: &MemFs, segment_bytes: u32) -> (SegmentRotor<MemFs>, PathBuf) {
    let dirs = create_cell_dirs(fs, &PathBuf::from("data/shard-0")).expect("dirs");
    let cfg = SegmentConfig { segment_bytes, ..Default::default() };
    let rotor = SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), cfg).expect("rotor");
    (rotor, dirs.log)
}

fn encoded(effect: &MutationEffect<'_>) -> Vec<u8> {
    let mut buf = Vec::new();
    effect.record().encode_into(&mut buf);
    buf
}

/// Deterministic xorshift64* — no ambient randomness in tests (L7).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

#[test]
fn accounting_is_exact_at_every_append_and_drain_site() {
    let fs = MemFs::new();
    let (mut rotor, _) = mem_rotor(&fs, 1 << 20);
    let mut ring = StagingRing::new(StagingConfig { capacity_bytes: 4096 });

    assert_eq!(ring.staged_bytes(), 0);
    assert_eq!(ring.pending_frame_len(), FRAME_OVERHEAD);
    // Two buffers plus the O_DIRECT alignment slack (ADR-0086 D6).
    assert_eq!(ring.resident_bytes(), 2 * (4096 + 2 * FRAME_ALIGN as usize));
    // The never-fits bound admission must reject against (not retry).
    assert_eq!(ring.max_record_len(), 4096 - FRAME_OVERHEAD);
    assert!(ring.would_fit(ring.max_record_len() as usize));
    assert!(!ring.would_fit(ring.max_record_len() as usize + 1));

    let mut manual_sum = 0u32;
    let effects = [
        MutationEffect::StringSet { ns: NsId(1), key: b"alpha", value: b"12345678" },
        MutationEffect::Delete { ns: NsId(2), key: b"beta" },
        MutationEffect::ExpireAt { ns: NsId(3), at_unix_ms: 1_780_000_000_000, key: b"gamma" },
        MutationEffect::NsOp { ns: NsId(4), payload: b"create topic" },
    ];
    for effect in &effects {
        let len = u32::try_from(effect.encoded_len()).expect("fits");
        ring.stage(effect).expect("fits");
        manual_sum += len;
        assert_eq!(ring.staged_bytes(), manual_sum, "gauge exact after every append");
        assert_eq!(ring.pending_frame_len(), manual_sum + FRAME_OVERHEAD);
    }
    assert_eq!(ring.pending_records(), 4);
    let stats = ring.stats();
    assert_eq!(stats.appends, 4);
    assert_eq!(stats.append_bytes, u64::from(manual_sum));

    // Drain: staged moves to in-flight exactly, staging returns to zero.
    let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame emitted");
    assert_eq!(ring.staged_bytes(), 0, "gauge exact after drain");
    assert_eq!(ring.in_flight_bytes(), manual_sum + FRAME_OVERHEAD);
    assert_eq!(lease.frame_len(), manual_sum + FRAME_OVERHEAD);
    assert_eq!(lease.record_count(), 4);

    ring.release(lease);
    assert_eq!(ring.in_flight_bytes(), 0);
    assert_eq!(
        ring.resident_bytes(),
        2 * (4096 + 2 * FRAME_ALIGN as usize),
        "domain memory is constant"
    );

    // Empty iterations emit no frame.
    assert!(ring.flush_into(&mut rotor, 0).expect("flush").is_none());
}

#[test]
fn full_ring_refuses_with_typed_backpressure_and_bounded_memory() {
    let fs = MemFs::new();
    let (mut rotor, _) = mem_rotor(&fs, 1 << 20);
    let capacity = 1024u32;
    let mut ring = StagingRing::new(StagingConfig { capacity_bytes: capacity });

    let value = [0xCD_u8; 100];
    let effect = MutationEffect::StringSet { ns: NsId(1), key: b"key", value: &value };
    let len = u32::try_from(effect.encoded_len()).expect("fits");

    let mut accepted = 0u32;
    let err = loop {
        match ring.stage(&effect) {
            Ok(_) => accepted += 1,
            Err(err) => break err,
        }
    };
    // The refusal is typed, exact, and arrived precisely when the next
    // record would no longer fit.
    assert_eq!(err.needed, len);
    assert_eq!(err.available, capacity - FRAME_OVERHEAD - accepted * len);
    assert!(err.available < len);
    assert!(!ring.would_fit(effect.encoded_len()));
    assert_eq!(ring.stats().refusals, 1);
    // Bounded: staged bytes never exceed capacity minus frame overhead.
    assert_eq!(ring.staged_bytes(), accepted * len);
    assert!(ring.pending_frame_len() <= capacity);

    // The refused record was not partially staged: drain and re-check.
    let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame");
    assert_eq!(lease.record_count(), accepted);
    ring.release(lease);
    assert!(ring.would_fit(effect.encoded_len()), "capacity returns after drain");
}

#[test]
fn backlogged_ring_keeps_staging_and_stays_bounded() {
    let fs = MemFs::new();
    let (mut rotor, _) = mem_rotor(&fs, 1 << 20);
    let mut ring = StagingRing::new(StagingConfig { capacity_bytes: 512 });

    ring.stage(&MutationEffect::Delete { ns: NsId(1), key: b"one" }).expect("fits");
    let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame");

    // The lease is outstanding (write CQE not yet reaped, in S05 terms):
    // staging continues into the other buffer, sealing must wait.
    assert!(ring.backlogged());
    assert!(!ring.can_seal(), "no records staged yet");
    ring.stage(&MutationEffect::Delete { ns: NsId(1), key: b"two" }).expect("fits");
    assert!(!ring.can_seal(), "previous frame still in flight");

    // Fill the staging buffer: refusals, not growth (memory bounded at
    // 2 × capacity while backlogged — the storm shape).
    let value = [0xEE_u8; 64];
    let mut refused = 0;
    for _ in 0..64 {
        if ring.stage(&MutationEffect::StringSet { ns: NsId(1), key: b"k", value: &value }).is_err()
        {
            refused += 1;
        }
    }
    assert!(refused > 0, "storm hit the bound");
    assert_eq!(ring.resident_bytes(), 2 * (512 + 2 * FRAME_ALIGN as usize));
    assert!(ring.pending_frame_len() <= 512);

    // Release unblocks the next seal; nothing staged was lost.
    let staged_before = ring.staged_bytes();
    ring.release(lease);
    assert!(ring.can_seal());
    assert_eq!(ring.staged_bytes(), staged_before);
    let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame");
    ring.release(lease);
}

/// An effect owning its bytes, so a pushed-back mutation can be retried in
/// a later iteration exactly as a paused connection would resend it.
enum OwnedEffect {
    Set { key: Vec<u8>, value: Vec<u8> },
    Del { key: Vec<u8> },
    Exp { key: Vec<u8>, at_unix_ms: u64 },
}

impl OwnedEffect {
    fn as_effect(&self) -> MutationEffect<'_> {
        match self {
            OwnedEffect::Set { key, value } => {
                MutationEffect::StringSet { ns: NsId(1), key, value }
            }
            OwnedEffect::Del { key } => MutationEffect::Delete { ns: NsId(2), key },
            OwnedEffect::Exp { key, at_unix_ms } => {
                MutationEffect::ExpireAt { ns: NsId(3), at_unix_ms: *at_unix_ms, key }
            }
        }
    }
}

/// The storm + integrity AC: a fixed workload driven through a small ring
/// and small segments — pushback observed, memory bounded at the ring
/// capacity, and **every** generated record is eventually admitted and
/// comes back from the log byte-identical, in order, at its
/// lease-resolved LSN (zero loss, zero reorder).
#[test]
fn storm_loses_and_reorders_nothing() {
    let fs = MemFs::new();
    let segment_bytes = 8 << 10;
    let (mut rotor, log_dir) = mem_rotor(&fs, segment_bytes);
    let capacity = 512;
    let mut ring = StagingRing::new(StagingConfig { capacity_bytes: capacity });
    let mut rng = Rng(0xC0FF_EE00_0000_0001);

    let workload: Vec<OwnedEffect> = (0..2_000)
        .map(|_| {
            let key = vec![b'k'; 1 + rng.below(12) as usize];
            match rng.below(3) {
                0 => OwnedEffect::Set {
                    key,
                    value: vec![(rng.next() & 0xFF) as u8; rng.below(128) as usize],
                },
                1 => OwnedEffect::Del { key },
                _ => OwnedEffect::Exp { key, at_unix_ms: rng.next() },
            }
        })
        .collect();

    // Expected sequence: (lease-resolved LSN, encoded bytes) per record,
    // in admission order.
    let mut expected: Vec<(Lsn, Vec<u8>)> = Vec::new();
    let mut pending: Vec<(inf_log::StagedAt, Vec<u8>)> = Vec::new();
    let mut pushbacks = 0u64;
    let mut cursor = 0;

    while cursor < workload.len() || !ring.is_empty() {
        // EXECUTE: admit a burst; on StagingFull the connection pauses for
        // the rest of the iteration and resends after the drain.
        let burst = 1 + rng.below(24) as usize;
        for _ in 0..burst {
            let Some(item) = workload.get(cursor) else { break };
            let effect = item.as_effect();
            match ring.stage(&effect) {
                Ok(at) => {
                    pending.push((at, encoded(&effect)));
                    cursor += 1;
                }
                Err(_) => {
                    pushbacks += 1;
                    break;
                }
            }
            assert!(ring.pending_frame_len() <= capacity, "memory bounded by the ring");
        }

        // MAINTAIN + LOG: drain into the active frame, resolve LSNs,
        // release the lease (write "completed" — the sync tier).
        rotor.maintain(0).expect("maintain");
        if let Some(lease) = ring.flush_into(&mut rotor, 0).expect("flush") {
            for (at, bytes) in pending.drain(..) {
                expected.push((lease.lsn_of(at), bytes));
            }
            ring.release(lease);
        }
    }

    assert_eq!(expected.len(), workload.len(), "every pushed-back record was re-admitted");
    assert!(pushbacks > 0, "the storm must actually hit the bound");
    assert!(rotor.stats().rotations > 0, "the storm must cross segments");

    // Read back through the S04 reader: byte-identical, in order.
    let mut replayed: Vec<(Lsn, Vec<u8>)> = Vec::new();
    let last = rotor.active_segment().0;
    for id in 0..=last {
        let mut reader = SegmentReader::open(&fs, &log_dir, SegmentId(id), ReaderConfig::default())
            .expect("open segment");
        let end = reader
            .apply_frames(|frame| {
                for record in frame.records() {
                    let (lsn, view) = record.expect("valid record");
                    let mut bytes = Vec::new();
                    view.encode_into(&mut bytes);
                    replayed.push((lsn, bytes));
                }
                Ok::<(), std::convert::Infallible>(())
            })
            .expect("replay");
        if id == last {
            assert_eq!(end.at(), rotor.active_written(), "tail ends at the write cursor");
        }
    }
    assert_eq!(replayed.len(), expected.len(), "zero loss");
    for (i, (exp, got)) in expected.iter().zip(&replayed).enumerate() {
        assert_eq!(exp, got, "record {i} must match LSN and bytes exactly");
    }
}

#[test]
#[should_panic(expected = "stale StagedAt")]
fn stale_token_is_rejected_by_generation_check() {
    let fs = MemFs::new();
    let (mut rotor, _) = mem_rotor(&fs, 1 << 20);
    let mut ring = StagingRing::new(StagingConfig { capacity_bytes: 512 });

    let stale = ring.stage(&MutationEffect::Delete { ns: NsId(1), key: b"a" }).expect("fits");
    let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame");
    let _ = lease.lsn_of(stale); // fine: same generation
    ring.release(lease);

    ring.stage(&MutationEffect::Delete { ns: NsId(1), key: b"b" }).expect("fits");
    let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame");
    let _ = lease.lsn_of(stale); // panics: token from an earlier generation
}
