//! The Linux body of the `parallel_boot` bench — see the target root
//! (`../parallel_boot.rs`) for why it lives in its own module: it depends
//! on `UringDriver`, which exists only under
//! `cfg(all(target_os = "linux", feature = "uring"))`.
//!
//! M2-S15 parallel cold-boot rehearsal (dev tier): time a real N-cell
//! node — SO_REUSEPORT listeners, uring drivers, loop-resident recovery,
//! the `-LOADING` gate — from cell-thread spawn to `all_ready`, over
//! synthetic per-cell durable images (checkpoint at ~50% + tail, the
//! steady-state shape). Every cell replays its own log in parallel (L1,
//! §8.1 — no cross-cell merge exists to time).
//!
//! The M2 gate (10 GB / 8 cells cold-boots to serving < 15 s) binds on
//! the reference box at S22 with a cold page cache; this rehearsal proves
//! the orchestration parallelism and gives the dev-tier number. Disclose
//! cache state (warm unless you dropped caches between build and boot).
//!
//! Run:  cargo bench -p inf-server --bench parallel_boot
//!       (pin externally if desired: `taskset -c 4-19 …`)
//! Env:  INF_BOOT_DIR (default `target/parallel-boot`),
//!       INF_BOOT_CELLS (default 8), INF_BOOT_MIB (default 1280 per cell
//!       ⇒ 10 GiB total), INF_BOOT_REPS (default 3, rebuild-free — the
//!       node is torn down and re-booted over the same images).

use std::os::fd::IntoRawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use inf_alloc::BufferPool;
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::CellId;
use inf_foundation::KeyHasher;
use inf_foundation::time::{Clock, StdClock};
use inf_log::ckpt::{SyncIckWriter, ick_file_name};
use inf_log::fs::StdSegmentFs;
use inf_log::{
    CkptConfig, Lsn, Manifest, MutationEffect, NsId, RecordView, SegmentConfig, SegmentId,
    SegmentRotor, StagingConfig, StagingRing, create_cell_dirs, scan_log_dir, segment_file_name,
    write_manifest,
};
use inf_runtime::net::listen_reuseport;
use inf_runtime::{BackendDriver, CellLoop, LoopConfig, UringDriver};
use inf_server::{DurableConfig, NodeInfo, NoopObserver, ServerPlane};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StoreConfig};

const NS: NsId = NsId(16);
const VALUE_LEN: usize = 512;
const RECORDS_PER_FRAME: u64 = 64;
/// Encoded record ≈ key(12) + value(512) + framing overhead.
const APPROX_RECORD_BYTES: u64 = 530;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// M2-S22 cold-cache rows: evict every file under `root` from the page
/// cache — sync (dirty pages don't drop) then fadvise(DONTNEED). No
/// root privileges needed; the artifact discloses the method. This module
/// is Linux-only, so `posix_fadvise` needs no further guard here.
fn cool_tree(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            cool_tree(&path);
        } else if let Ok(file) = std::fs::File::open(&path) {
            use std::os::fd::AsRawFd;
            let _ = file.sync_all();
            // SAFETY: fadvise on a live fd; offset 0 + len 0 = whole file.
            unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        }
    }
}

fn key_of(i: u64) -> [u8; 12] {
    let mut key = *b"k:0000000000";
    let digits = format!("{i:010}");
    key[2..].copy_from_slice(digits.as_bytes());
    key
}

fn value_of(i: u64, buf: &mut [u8; VALUE_LEN]) {
    let mut x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for chunk in buf.chunks_mut(8) {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let bytes = x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

fn durable_config(root: &Path) -> DurableConfig {
    DurableConfig {
        data_dir: root.to_path_buf(),
        staging: StagingConfig::with_capacity(128 << 10),
        segment: SegmentConfig { segment_bytes: 256 << 20, ..Default::default() },
        ckpt: CkptConfig::default(),
        recover: Default::default(),
        flush_bound: 1,
        fua_p50_us_probed: 0,
        device: Default::default(),
        fill: Default::default(),
        group: Default::default(),
    }
}

/// Build cell `cell`'s image: unique-key sets, ckpt-begin marker at ~50%,
/// `.ick` of the state at the marker, manifest, covered prefix truncated —
/// the recovery_replay `ick-tail` shape, per cell.
fn build_cell_image(root: &Path, cfg: &DurableConfig, cell: u16, records: u64) -> u64 {
    let fs = StdSegmentFs;
    let shard = root.join(format!("shard-{cell}"));
    let dirs = create_cell_dirs(&fs, &shard).expect("dirs");
    let mut rotor = SegmentRotor::create_fresh(fs, dirs.log.clone(), cfg.segment).expect("rotor");
    let mut ring = StagingRing::new(cfg.staging);
    let mut value = [0u8; VALUE_LEN];
    let mut staged = 0u64;
    let mut begin: Option<Lsn> = None;
    let ckpt_at = records / 2;
    let mut extents: std::collections::BTreeMap<u32, u64> = std::collections::BTreeMap::new();
    let flush = |ring: &mut StagingRing,
                 rotor: &mut SegmentRotor<StdSegmentFs>,
                 extents: &mut std::collections::BTreeMap<u32, u64>|
     -> inf_log::FrameLease {
        rotor.maintain(0).expect("maintain");
        let lease = ring.flush_into(rotor, 0).expect("flush").expect("frame");
        extents.insert(rotor.active_segment().0, u64::from(rotor.active_written()));
        lease
    };

    for i in 0..records {
        if ckpt_at == i {
            if staged > 0 {
                let lease = flush(&mut ring, &mut rotor, &mut extents);
                ring.release(lease);
                staged = 0;
            }
            let at = ring.stage(&MutationEffect::CkptBegin { ckpt_id: 1 }).expect("stage");
            let lease = flush(&mut ring, &mut rotor, &mut extents);
            begin = Some(lease.lsn_of(at));
            ring.release(lease);
        }
        let key = key_of(i);
        value_of(i, &mut value);
        ring.stage(&MutationEffect::StringSet { ns: NS, key: &key, value: &value }).expect("stage");
        staged += 1;
        if staged == RECORDS_PER_FRAME || i + 1 == records {
            let lease = flush(&mut ring, &mut rotor, &mut extents);
            ring.release(lease);
            staged = 0;
        }
    }
    drop(rotor);

    let begin = begin.expect("marker staged");
    let floor = begin.segment;
    let mut w = SyncIckWriter::create(fs, &dirs.ckpt, &cfg.ckpt, cell, 1, begin, &[NS.0])
        .expect("ick create");
    for i in 0..ckpt_at {
        let key = key_of(i);
        value_of(i, &mut value);
        w.append(&RecordView::StringPostImage { ns: NS, key: &key, value: &value })
            .expect("ick record");
    }
    w.finish().expect("ick publish");
    let ick_bytes = std::fs::metadata(dirs.ckpt.join(ick_file_name(1))).expect("ick meta").len();
    let scan = scan_log_dir(&fs, &dirs.log).expect("scan");
    let segments: Vec<SegmentId> =
        scan.segments().iter().copied().filter(|id| *id >= floor).collect();
    write_manifest(
        &fs,
        &shard,
        &Manifest {
            ckpt_id: 1,
            begin_lsn: begin,
            segments,
            tiers: Vec::new(),
            key_hash_id: KeyHasher::default().identity(),
        },
    )
    .expect("manifest");
    for &id in scan.segments().iter().filter(|id| **id < floor) {
        std::fs::remove_file(dirs.log.join(segment_file_name(id))).expect("truncate");
    }
    extents.iter().filter(|(id, _)| **id >= floor.0).map(|(_, end)| *end).sum::<u64>() + ick_bytes
}

/// One cell's boot thread body, generic over the recovery fs tier
/// (M2.5-S08 A/B: `ReadAheadFs` vs bare `StdSegmentFs`).
#[allow(clippy::too_many_arguments)]
fn drive_cell<F: inf_log::fs::SegmentFs + Clone + 'static>(
    fs: F,
    i: usize,
    cells: u16,
    listener: std::net::TcpListener,
    fabric: inf_fabric::CellFabric,
    control: Arc<inf_server::ControlHandle>,
    root: &Path,
    stop: &AtomicBool,
) {
    let mut pool = BufferPool::new(256, 4096);
    let mut driver = UringDriver::new(256).expect("uring");
    driver.register_pool(&mut pool).expect("register");
    let node = Rc::new(NodeInfo::default());
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    node.wall_anchor.set((0, unix_ms));
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(NsSpec {
        id: NS,
        name: b"bench".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
        tier: None,
    })
    .expect("ns");
    let mut plane = ServerPlane::new(
        CellId(i as u16),
        cells,
        listener.into_raw_fd(),
        ks,
        fabric,
        node,
        NoopObserver,
        false,
    );
    plane.set_control(control);
    plane.begin_recovery(fs, &durable_config(root), i as u16, StdClock::new().now());
    let config = LoopConfig {
        park_default: Some(std::time::Duration::from_micros(500)),
        ..Default::default()
    };
    let mut cell_loop = CellLoop::new(driver, StdClock::new(), pool, config);
    while !stop.load(Ordering::Relaxed) {
        cell_loop.run_iteration(&mut plane).expect("iteration");
        if let Some(err) = plane.take_boot_error() {
            panic!("cell {i} recovery failed: {err}");
        }
    }
}

/// Boot the node (the infinityd assembly shape) and time spawn→all_ready.
fn boot_once(root: &Path, cells: u16) -> f64 {
    let stop = Arc::new(AtomicBool::new(false));
    let first = listen_reuseport(0).expect("listen");
    let port = inf_runtime::net::bound_port(&first).expect("port");
    let mut listeners = vec![first];
    for _ in 1..cells {
        listeners.push(listen_reuseport(port).expect("listen same port"));
    }
    let fabrics = Mesh::new(cells, MeshConfig { ring_capacity: 1024, data_credits: 256 });
    let boot_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let catalog = inf_server::load_catalog(root).expect("catalog");
    let control =
        inf_server::spawn_control(root.to_path_buf(), catalog.as_ref(), cells, boot_unix_ms);
    let board = Arc::clone(control.recovery_board());

    let started = Instant::now();
    let mut handles = Vec::new();
    for (i, (fabric, listener)) in fabrics.into_iter().zip(listeners).enumerate() {
        let stop = Arc::clone(&stop);
        let control = Arc::clone(&control);
        let root = root.to_path_buf();
        handles.push(std::thread::spawn(move || {
            // M2.5-S08 A/B: INF_BOOT_READAHEAD=1 forces the prefetch arm
            // (the multi-cell regime infinityd's cells==1 policy declines
            // — the A/B that measured why); default matches the shipped
            // multi-cell posture (no prefetch).
            if env_u64("INF_BOOT_READAHEAD", 0) != 0 {
                drive_cell(
                    inf_server::ReadAheadFs::new(StdSegmentFs, true),
                    i,
                    cells,
                    listener,
                    fabric,
                    control,
                    &root,
                    &stop,
                );
            } else {
                drive_cell(StdSegmentFs, i, cells, listener, fabric, control, &root, &stop);
            }
        }));
    }
    while !board.all_ready() {
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(std::time::Duration::from_micros(500));
    }
    let wall = started.elapsed().as_secs_f64() * 1e3;
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        handle.join().expect("cell thread");
    }
    wall
}

pub fn run() {
    let cells = env_u64("INF_BOOT_CELLS", 8) as u16;
    let mib_per_cell = env_u64("INF_BOOT_MIB", 1280);
    let reps = env_u64("INF_BOOT_REPS", 3);
    let root: PathBuf = std::env::var("INF_BOOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_TARGET_TMPDIR")).join("parallel-boot"));

    let records_per_cell = mib_per_cell * (1 << 20) / APPROX_RECORD_BYTES;
    println!(
        "# parallel_boot dev rehearsal: {cells} cells × {mib_per_cell} MiB \
         (≈ {records_per_cell} records/cell), reps {reps}"
    );
    if root.exists() {
        std::fs::remove_dir_all(&root).expect("clear old images");
    }
    std::fs::create_dir_all(&root).expect("bench dir");

    let build_start = Instant::now();
    let mut replay_bytes_total = 0u64;
    for cell in 0..cells {
        replay_bytes_total +=
            build_cell_image(&root, &durable_config(&root), cell, records_per_cell);
    }
    println!(
        "# built {cells} images in {:.1}s — replay set {:.2} GiB (data extents + .ick)",
        build_start.elapsed().as_secs_f64(),
        replay_bytes_total as f64 / (1u64 << 30) as f64,
    );

    let cold = env_u64("INF_BOOT_COOL", 0) != 0;
    if cold {
        println!("# cold-cache mode: fadvise(DONTNEED) over every image file before each rep");
    }
    for rep in 0..reps {
        if cold {
            cool_tree(&root);
        }
        let wall_ms = boot_once(&root, cells);
        let gib_s = replay_bytes_total as f64 / (1u64 << 30) as f64 / (wall_ms / 1e3);
        println!(
            "boot rep {rep}: {wall_ms:.0} ms to all-ready — aggregate {gib_s:.2} GiB/s \
             ({:.2} GiB/s/cell effective)",
            gib_s / f64::from(cells),
        );
    }
    println!("# gate context: 10 GB/8 cells < 15 s binds on the reference box (S22, cold cache)");
    std::fs::remove_dir_all(&root).ok();
}
