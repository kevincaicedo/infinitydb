//! The cold leg of `m4-extent-window` (ADR-0163 D2): the window's handout
//! against the cell's cold FIFO. One tiered namespace with `COLD-READ-QD` 4,
//! so the cell's foreground cold FIFO has 16 places. 56 writers overwrite 56
//! cold keys with blob values, so each of their resolves is a cold read;
//! they arrive one per read transfer, four per cold-read service time, so at
//! most four intents wait where 16 fit. The extent fdatasyncs are held until
//! every writer has registered or parked (the window closes at the first
//! hold, so one to four register and the rest park), then released at a
//! sync latency of eight cold-read service times: a handout's 16 turns
//! resolve in four, and no entry folds meanwhile. After the release the only
//! foreground cold intents are the turns' second resolves, at most the
//! window's 16 — the FIFO's 16 places — so no reply is `BUSY`. A 57th
//! writer, on a key in RAM, arrives while the first handout's turns
//! resolve; it parks behind the queue. Every reply is `+OK`, at most 16
//! extent files are open, no turn is refused and no writer is left parked.
//! Engagement: the release found every writer registered or parked, one to
//! four of them registered, and the 57th writer was sent while turns were
//! out (token `extent-cold-unengaged`).

use super::*;

/// Writers: 56 on cold keys and the 57th on a key in RAM.
pub(super) const WRITERS: usize = COLD_WRITERS + 1;
/// Writers that overwrite a cold key.
const COLD_WRITERS: usize = 56;
/// The namespace's `COLD-READ-QD`: four reads in flight, a cold FIFO of 16.
pub(super) const READ_QD: &[u8] = b"4";
/// Max virtual nanoseconds per scheduler step: a quarter of a read transfer
/// on average.
pub(super) const STEP_NS_MAX: u64 = 500_000;
/// The byte timeline's read rate: a 16 KiB read window transfers in 1 ms.
const READ_BYTES_PER_S: u64 = 16_384_000;
/// A cold-read service time at four reads in flight: four transfers.
const SERVICE_NS: u64 = 4_000_000;
/// The fdatasync service time: eight cold-read service times.
const SYNC_NS: u64 = 8 * SERVICE_NS;
/// The gap between two cold writers' sends: four per service time.
const ARRIVAL_GAP_NS: u64 = SERVICE_NS / 4;
/// The writers whose cold reads complete before the first LOG step closes
/// the window: at most the reads in flight.
const REGISTERED_AT_RELEASE_MAX: usize = 4;
/// The cold keys' inline values.
const COLD_VALUE_LEN: usize = 1024;
/// A filler's inline value, under the 4 KiB blob threshold.
const FILLER_VALUE_LEN: usize = 4000;
/// Connections that write the cold keys and the filler.
const PREP_WRITERS: usize = 32;
/// Fillers between two reads of the cold keys' residence.
const FILLER_BATCH: usize = 256;
/// Filler batches before the run gives up on the keys going cold: 32 MiB.
const FILLER_BATCHES_MAX: usize = 32;

/// The leg's device: the fdatasync on the flush unit's timeline at eight
/// cold-read service times, tier reads on the byte timeline, plain writes
/// off it at a page copy's 8 µs.
pub(super) fn stall() -> StallConfig {
    StallConfig {
        base_ns: SYNC_NS,
        write_base_ns: 8_000,
        read_bytes_per_s: READ_BYTES_PER_S,
        ..StallConfig::instant()
    }
}

/// Writer `id`'s key: a cold key, or the 57th writer's key in RAM.
fn key_of(id: usize) -> Vec<u8> {
    if id < COLD_WRITERS { format!("c:{id}").into_bytes() } else { b"r:0".to_vec() }
}

/// The leg: the cold keys made cold, the paced overwrites under the hold,
/// the release, and the 57th writer sent once the first turns are out.
pub(super) fn run(
    run: &mut Run<'_>,
    writers: &mut [Writer],
    creations: &mut Creations,
    report: &mut ExtentWindowReport,
) {
    for writer in writers.iter_mut() {
        writer.key = Some(key_of(writer.id));
        writer.not_before = Nanos(u64::MAX);
    }
    if let Err(what) = prepare(run, report) {
        report.violations.push(what);
        return;
    }
    run.disk.hold_syncs(Some(is_extent));
    let start = run.clock.now().0.saturating_add(SERVICE_NS);
    for writer in writers.iter_mut().take(COLD_WRITERS) {
        writer.not_before = Nanos(start + writer.id as u64 * ARRIVAL_GAP_NS);
    }
    let mut phase = Phase::Held;
    let mut idle = 0u64;
    while writers.iter().any(|w| !w.done()) {
        if !traffic_step(run, writers, creations, report, &mut idle) {
            return;
        }
        let Some(stats) = run.node.plane(0).durable_stats() else { continue };
        phase = match phase {
            Phase::Held
                if creations.seen.len() as u64 + stats.extent_parks >= COLD_WRITERS as u64 =>
            {
                run.disk.hold_syncs(None);
                Phase::Released { registered: creations.seen.len() }
            }
            Phase::Released { registered } if stats.extent_turns_out > 0 => {
                writers[COLD_WRITERS].not_before = run.clock.now();
                Phase::Sent { registered }
            }
            Phase::Held | Phase::Released { .. } | Phase::Sent { .. } => phase,
        };
    }
    engagement(run.scenario.seed, phase, report);
}

/// Where the leg is: syncs held; released, with the writers registered at
/// the release; the 57th writer sent.
#[derive(Copy, Clone, Debug)]
enum Phase {
    Held,
    Released { registered: usize },
    Sent { registered: usize },
}

/// The leg's engagement: the release came, one to four writers had
/// registered by it, and the 57th writer was sent while turns were out.
fn engagement(seed: u64, phase: Phase, report: &mut ExtentWindowReport) {
    let what = match phase {
        Phase::Sent { registered } if (1..=REGISTERED_AT_RELEASE_MAX).contains(&registered) => {
            return;
        }
        Phase::Sent { registered } => format!(
            "{registered} writers registered before the release, not 1 to \
             {REGISTERED_AT_RELEASE_MAX}"
        ),
        Phase::Released { .. } => "the 57th writer was never sent: no turn was out".to_owned(),
        Phase::Held => "the extent syncs were never released".to_owned(),
    };
    report.violations.push(format!("VACUOUS seed {seed:#x}: extent-cold-unengaged — {what}"));
}

/// One prep connection: its fd, its reply bytes, whether a write is out.
struct Prep {
    fd: std::os::fd::RawFd,
    rx: Vec<u8>,
    inflight: bool,
}

/// Writes the cold keys, then filler until every cold key has left RAM, then
/// the 57th writer's key, which stays in RAM. Every reply must be `+OK`.
fn prepare(run: &mut Run<'_>, report: &mut ExtentWindowReport) -> Result<(), String> {
    let mut preps: Vec<Prep> = (0..PREP_WRITERS)
        .map(|_| {
            let fd = run.node.nets[0].borrow_mut().connect();
            let mut net = run.node.nets[0].borrow_mut();
            net.client_send(fd, &encode(&[b"INF.NS", b"USE", NS_NAME]));
            Prep { fd, rx: Vec::new(), inflight: true }
        })
        .collect();
    let mut work: Vec<(Vec<u8>, usize)> =
        (0..COLD_WRITERS).map(|id| (key_of(id), COLD_VALUE_LEN)).collect();
    let cold_keys: Vec<Vec<u8>> = (0..COLD_WRITERS).map(key_of).collect();
    let hasher = crate::harness::node_hasher(run.scenario.seed);
    for batch in 0..=FILLER_BATCHES_MAX {
        write_all(run, &mut preps, &mut work, report)?;
        if cold_count(run.node, &hasher, &cold_keys) == COLD_WRITERS {
            work.push((key_of(COLD_WRITERS), COLD_VALUE_LEN));
            return write_all(run, &mut preps, &mut work, report);
        }
        let base = batch * FILLER_BATCH;
        work.extend(
            (base..base + FILLER_BATCH).map(|j| (format!("f:{j}").into_bytes(), FILLER_VALUE_LEN)),
        );
    }
    Err(format!(
        "VACUOUS seed {:#x}: extent-cold-unengaged — the cold keys stayed in RAM under {} MiB \
         of filler",
        run.scenario.seed,
        (FILLER_BATCHES_MAX * FILLER_BATCH * FILLER_VALUE_LEN) >> 20
    ))
}

/// Writes every pending `(key, value length)` through the prep connections,
/// one write in flight each, until each is acknowledged.
fn write_all(
    run: &mut Run<'_>,
    preps: &mut [Prep],
    work: &mut Vec<(Vec<u8>, usize)>,
    report: &mut ExtentWindowReport,
) -> Result<(), String> {
    work.reverse();
    for _ in 0..STALL_STEPS {
        if work.is_empty() && preps.iter().all(|p| !p.inflight) {
            return Ok(());
        }
        report.scheduler_steps += 1;
        run.step().map_err(|err| format!("prep phase: {err}"))?;
        let mut net = run.node.nets[0].borrow_mut();
        for prep in preps.iter_mut() {
            let bytes = net.client_recv(prep.fd);
            prep.rx.extend_from_slice(&bytes);
            while let Some(n) = reply_len(&prep.rx) {
                let reply: Vec<u8> = prep.rx.drain(..n).collect();
                if reply != b"+OK\r\n" {
                    return Err(format!(
                        "prep write answered {:?}",
                        String::from_utf8_lossy(&reply)
                    ));
                }
                prep.inflight = false;
            }
            if !prep.inflight
                && let Some((key, len)) = work.pop()
            {
                net.client_send(prep.fd, &encode(&[b"SET", &key, &vec![b'f'; len]]));
                prep.inflight = true;
            }
        }
    }
    Err("prep phase: no progress".to_owned())
}

/// How many of `keys` the namespace resolves cold: below its RAM head.
fn cold_count(node: &Node, hasher: &inf_store::KeyHasher, keys: &[Vec<u8>]) -> usize {
    let ks = node.plane(0).keyspace();
    let Some((_, table)) = ks.tiered_namespaces().next() else { return 0 };
    keys.iter()
        .filter(|key| {
            matches!(table.lookup(key, hasher.hash(key), &[]), inf_store::TieredLookup::Cold(_))
        })
        .count()
}
