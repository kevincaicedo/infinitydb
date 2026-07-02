//! The control thread (M2-S08, ADR-0015 D3) — the slow plane the
//! architecture always reserved (§4). Its first job: **single writer of
//! the node catalog `META` file**. Cells must never block on file I/O, so
//! DDL persists asynchronously: the origin cell's pump sends a request
//! over a bounded channel and parks on a cell-local waitlist; this thread
//! performs the write-new + fsync + rename + dir-fsync swap (`inf-log`'s
//! `meta` protocol) and publishes a monotone **persist epoch**; every
//! cell's MAINTAIN observes the epoch and wakes its parked pumps.
//!
//! The thread also owns namespace-id allocation (ids are node-unique,
//! allocated once, never reused — ADR-0015 D2): a shared `AtomicU32`
//! seeded from the catalog's `next_id` at boot. One `fetch_add` per DDL is
//! control-plane traffic; L1's no-shared-atomics rule binds the data plane.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;

use inf_log::fs::StdSegmentFs;
use inf_log::meta::{read_meta, write_meta};
use inf_store::{FIRST_NAMED_NS_ID, NsCatalog};

/// One catalog snapshot to persist (the origin cell's post-apply export).
struct PersistReq {
    catalog: NsCatalog,
    /// The epoch value published once this snapshot is durable.
    epoch: u64,
}

/// Shared handle the assembly wires into every cell's plane.
pub struct ControlHandle {
    tx: mpsc::SyncSender<PersistReq>,
    next_ns_id: AtomicU32,
    next_epoch: AtomicU64,
    persisted_epoch: Arc<AtomicU64>,
    /// Manual-checkpoint request epoch (M2-S10, ADR-0016 D7): bumping it
    /// asks every durable cell to checkpoint; cells edge-detect it in
    /// MAINTAIN (one relaxed load — the persisted-epoch pattern).
    /// `INF.CKPT`/`BGSAVE` ride this at S20 (per-cell targeting refines
    /// there).
    ckpt_epoch: AtomicU64,
}

impl ControlHandle {
    /// Allocates one namespace id (never reused — ADR-0015 D2).
    pub fn alloc_ns_id(&self) -> u32 {
        self.next_ns_id.fetch_add(1, Ordering::Relaxed)
    }

    /// The id the allocator would hand out next (catalog `next_id`).
    pub fn next_ns_id(&self) -> u32 {
        self.next_ns_id.load(Ordering::Relaxed)
    }

    /// Queues `catalog` for a durable swap; returns the epoch to await.
    /// The bounded channel makes DDL admission explicit: a full queue
    /// blocks the *sender* briefly (DDL-rate traffic, never the data path
    /// of other connections — the pump yields between commands).
    pub fn request_persist(&self, catalog: NsCatalog) -> u64 {
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        self.tx.send(PersistReq { catalog, epoch }).expect("control thread alive (fail-stop)");
        epoch
    }

    /// True once every persist up to `epoch` is durable. Cells poll this
    /// from MAINTAIN (one relaxed load) and wake their parked DDL pumps.
    pub fn persisted(&self, epoch: u64) -> bool {
        self.persisted_epoch.load(Ordering::Acquire) >= epoch
    }

    /// The highest durable epoch (the MAINTAIN edge-detection input).
    pub fn persisted_epoch(&self) -> u64 {
        self.persisted_epoch.load(Ordering::Acquire)
    }

    /// Requests a checkpoint on every durable cell (M2-S10; the S20
    /// `INF.CKPT` surface calls this). Returns the request epoch.
    pub fn request_ckpt_all(&self) -> u64 {
        self.ckpt_epoch.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The manual-checkpoint request epoch (MAINTAIN edge-detection input).
    pub fn ckpt_epoch(&self) -> u64 {
        self.ckpt_epoch.load(Ordering::Relaxed)
    }
}

/// Reads the catalog at boot (`None` = fresh node). Corruption is a typed
/// error — the node must refuse to start, never guess (§8.4).
pub fn load_catalog(data_dir: &Path) -> std::io::Result<Option<NsCatalog>> {
    let Some(payload) = read_meta(&StdSegmentFs, data_dir)? else {
        return Ok(None);
    };
    NsCatalog::decode(&payload)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// Spawns the control thread. `seed` is the boot-loaded catalog (its
/// `next_id` seeds the allocator; a fresh node starts at the named floor).
///
/// The thread fail-stops the process on a failed swap: the catalog is
/// durability metadata and a lost DDL after its `+OK` would be a §8.2
/// violation — same rule class as fsync failure.
pub fn spawn(data_dir: PathBuf, seed: Option<&NsCatalog>) -> Arc<ControlHandle> {
    let next_id = seed.map_or(FIRST_NAMED_NS_ID, |c| c.next_id.max(FIRST_NAMED_NS_ID));
    let persisted = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::sync_channel::<PersistReq>(64);
    let handle = Arc::new(ControlHandle {
        tx,
        next_ns_id: AtomicU32::new(next_id),
        next_epoch: AtomicU64::new(0),
        persisted_epoch: Arc::clone(&persisted),
        ckpt_epoch: AtomicU64::new(0),
    });
    let allocator = Arc::clone(&handle);
    std::thread::Builder::new()
        .name("inf-control".into())
        .spawn(move || {
            while let Ok(req) = rx.recv() {
                // The persisted next_id must always cover the allocator so
                // ids never regress across restart, even for namespaces
                // whose DDL raced this snapshot.
                let mut catalog = req.catalog;
                catalog.next_id = catalog.next_id.max(allocator.next_ns_id());
                let payload = catalog.encode();
                if let Err(err) = write_meta(&StdSegmentFs, &data_dir, &payload) {
                    // §8.4 fail-stop: a DDL was acked against this swap.
                    panic!("catalog META swap failed (fail-stop): {err}");
                }
                persisted.store(req.epoch, Ordering::Release);
            }
            // Channel closed = node shutdown; nothing to flush (every
            // acked DDL already persisted before its reply).
        })
        .expect("spawn control thread");
    handle
}
