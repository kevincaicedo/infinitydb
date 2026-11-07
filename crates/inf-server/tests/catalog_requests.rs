#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and filesystem fixtures, not cell code"
)]
#![allow(clippy::disallowed_types, reason = "test target: the disk's log is shared with a thread")]
//! The catalog writer's answers on real threads (ADR-0189 D2, D3).
//!
//! A namespace DDL asks the control thread's catalog writer for a `META`
//! swap and acts on the answer: it applies, fans and replies only once the
//! swap that carries it is durable. The writer numbers each request as it
//! dequeues it and answers it through its own slot after that request's
//! `META` write returned; a refusal writes nothing; its published word
//! never decreases; and every payload it writes is shaped from its own
//! state, so no lagging export drops or revives a DDL in flight.
//!
//! The oracles read the disk, never the writer's counters: `META`
//! decoded from the disk at the moment a requester sees its answer, the
//! word's samples after every step, and the operations a recording file
//! system saw (`META.new` is created only to be renamed onto `META`).

#[path = "../../../tests/crash-matrix/receipt.rs"]
mod receipt;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use inf_foundation::fault::{self, FaultSpec, HoldGate};
use inf_log::fs::{SegmentFs, StdSegmentFile, StdSegmentFs, TierIoMode};
use inf_server::{CkptSpace, ControlHandle, ControlInbox, load_catalog};
use inf_store::ddl::{Answer, CreatePersisted, DdlRefusal, Receipt, Requested};
use inf_store::{FsyncClass, IndexCatalog, NsCatalog, NsId, NsMode, NsSpec};

/// The namespace the DDL creates or drops.
const DDL_NS: u32 = 16;

fn spec(id: u32, name: &[u8], mode: NsMode) -> NsSpec {
    let fsync = (mode == NsMode::Durable).then_some(FsyncClass::Everysec);
    NsSpec {
        id: NsId(id),
        name: name.to_vec(),
        mode,
        fsync,
        policy: None,
        maxmemory: None,
        tier: None,
    }
}

/// A catalog with `entries`, its allocator past every id.
fn catalog(entries: Vec<NsSpec>) -> NsCatalog {
    NsCatalog { next_id: DDL_NS + 8, entries, index: IndexCatalog::default(), dropped: Vec::new() }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("inf-catalog-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A detached writer on two cells, seeded with `seed` written to `dir`.
fn writer(dir: &Path, seed: Option<&NsCatalog>) -> (Arc<ControlHandle>, ControlInbox) {
    if let Some(seed) = seed {
        inf_log::meta::write_meta(&StdSegmentFs, dir, &seed.encode()).expect("seed META");
    }
    let cells = inf_foundation::CellCount::new(2).expect("two cells");
    let (control, inbox, _issuers) = ControlHandle::detached_in(seed, CkptSpace::full(cells), 0);
    (control, inbox)
}

/// A `CREATE` of `name`, its id reserved under the DDL ticket as a program
/// reserves it (the ticket released once the request is sent): the spec
/// and the request's token.
fn reserved_create(
    control: &Arc<ControlHandle>,
    export: NsCatalog,
    name: &[u8],
    mode: NsMode,
) -> (NsSpec, Requested<CreatePersisted>) {
    let ticket = control.ddl_try_acquire(0).expect("the DDL ticket is free");
    let reservation = ticket.reserve_ns_id().expect("an id is left");
    let spec = spec(reservation.id().0, name, mode);
    (spec.clone(), control.request_create(export, spec, reservation))
}

/// `META` as the disk holds it.
fn meta(dir: &Path) -> NsCatalog {
    load_catalog(dir).expect("META decodes").expect("META exists")
}

/// The ids `META`'s entries name.
fn ids(catalog: &NsCatalog) -> Vec<u32> {
    catalog.entries.iter().map(|e| e.id.0).collect()
}

/// The answer once the writer gave it, spinning on the word (the
/// program parks on its edge instead).
fn answered<R: Receipt>(
    control: &ControlHandle,
    mut requested: Requested<R>,
) -> Result<R, DdlRefusal> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match requested.answer(control.catalog_word()) {
            Answer::Pending(again) => requested = again,
            Answer::Refused(refusal) => return Err(refusal),
            Answer::Accepted(receipt) => return Ok(receipt),
        }
        assert!(Instant::now() < deadline, "the writer never answered");
        std::thread::yield_now();
    }
}

/// The point a [`RecordingFs`] holds its `META` write at.
const META_WRITE_HELD: &str = "test_meta_write_held";

/// The real file system, with every operation that changes a directory
/// logged by path; creating `META.new` holds at [`META_WRITE_HELD`] when
/// the thread armed a gate for it.
#[derive(Clone, Default)]
struct RecordingFs {
    log: Arc<Mutex<Vec<String>>>,
}

impl RecordingFs {
    fn note(&self, op: &str, path: &Path) {
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        self.log.lock().expect("the log").push(format!("{op} {name}"));
    }

    fn ops(&self) -> Vec<String> {
        self.log.lock().expect("the log").clone()
    }
}

impl SegmentFs for RecordingFs {
    type File = StdSegmentFile;

    fn create_dir_all(&self, dir: &Path) -> std::io::Result<()> {
        StdSegmentFs.create_dir_all(dir)
    }
    fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        self.note("sync_dir", dir);
        StdSegmentFs.sync_dir(dir)
    }
    fn list_dir(&self, dir: &Path) -> std::io::Result<Vec<String>> {
        StdSegmentFs.list_dir(dir)
    }
    fn create_segment(&self, path: &Path, prealloc_bytes: u64) -> std::io::Result<StdSegmentFile> {
        self.note("create", path);
        StdSegmentFs.create_segment(path, prealloc_bytes)
    }
    fn create_tier(&self, path: &Path, mode: TierIoMode) -> std::io::Result<StdSegmentFile> {
        self.note("create", path);
        StdSegmentFs.create_tier(path, mode)
    }
    fn create_meta(&self, path: &Path) -> std::io::Result<StdSegmentFile> {
        self.note("create", path);
        fault::hold(META_WRITE_HELD);
        StdSegmentFs.create_meta(path)
    }
    fn create_meta_direct(&self, path: &Path) -> std::io::Result<StdSegmentFile> {
        self.note("create", path);
        StdSegmentFs.create_meta_direct(path)
    }
    fn open_dir(&self, dir: &Path) -> std::io::Result<StdSegmentFile> {
        StdSegmentFs.open_dir(dir)
    }
    fn open_write(&self, path: &Path) -> std::io::Result<StdSegmentFile> {
        self.note("open_write", path);
        StdSegmentFs.open_write(path)
    }
    fn open_read(&self, path: &Path) -> std::io::Result<StdSegmentFile> {
        StdSegmentFs.open_read(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.note("rename", from);
        StdSegmentFs.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.note("remove", path);
        StdSegmentFs.remove_file(path)
    }
}

// ---- two senders: a DDL held between building and sending its request ----

/// The DDL whose sender is held, and what its answer must carry.
#[derive(Clone, Copy, Debug)]
enum Ddl {
    /// `CREATE` of a namespace `META` does not hold: its answer must
    /// follow a `META` that names it.
    Create,
    /// `DROP` of a namespace `META` holds: its answer must follow a
    /// `META` without it, its tombstone in its place.
    Drop,
}

impl Ddl {
    /// Whether `catalog` carries this DDL's change.
    fn carried_by(self, catalog: &NsCatalog) -> bool {
        let named = catalog.entries.iter().any(|e| e.id.0 == DDL_NS);
        match self {
            Ddl::Create => named,
            Ddl::Drop => !named && catalog.dropped.contains(&DDL_NS),
        }
    }
}

/// Sends the DDL's request on its own thread, held before it is sent, and
/// returns the thread (its answer: `META` decoded from the disk the moment
/// the DDL saw its acceptance) with the gate that holds it.
fn held_ddl(
    control: &Arc<ControlHandle>,
    dir: &Path,
    ddl: Ddl,
) -> (std::thread::JoinHandle<NsCatalog>, HoldGate) {
    let gate = HoldGate::new();
    let control = Arc::clone(control);
    let dir = dir.to_path_buf();
    let armed = gate.clone();
    let sender = std::thread::spawn(move || {
        fault::arm(inf_server::fault::DDL_REQUEST_HELD, FaultSpec::Nth(1));
        fault::arm_hold(inf_server::fault::DDL_REQUEST_HELD, &armed);
        drop(armed);
        let durable = NsMode::Durable;
        let accepted = match ddl {
            Ddl::Create => {
                let (_, requested) =
                    reserved_create(&control, catalog(Vec::new()), b"fresh", durable);
                answered(&control, requested).map(|_| ())
            }
            Ddl::Drop => {
                let requested = control.request_drop(catalog(Vec::new()), DDL_NS, true);
                answered(&control, requested).map(|_| ())
            }
        };
        assert_eq!(accepted, Ok(()), "the writer accepts the DDL");
        meta(&dir)
    });
    (sender, gate)
}

fn two_senders(ddl: Ddl, tag: &str) {
    let dir = temp_dir(tag);
    // A `DROP` drops a namespace `META` already names.
    let seed = match ddl {
        Ddl::Create => None,
        Ddl::Drop => Some(catalog(vec![spec(DDL_NS, b"ledger", NsMode::Durable)])),
    };
    let (control, mut inbox) = writer(&dir, seed.as_ref());
    let (sender, gate) = held_ddl(&control, &dir, ddl);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !gate.is_held() {
        assert!(Instant::now() < deadline, "the DDL's sender never reached its send");
        std::thread::yield_now();
    }
    // The second sender: cell 0's readiness export, from a cell whose
    // export lags the DDL (it still holds what `META` held).
    let lagging = catalog(seed.as_ref().map_or_else(Vec::new, |c| c.entries.clone()));
    control.request_persist(lagging);
    inbox.drain(&StdSegmentFs, &dir).expect("the second request's META swap");
    let mut words = vec![control.persisted_epoch()];
    gate.release();
    while control.persist_requests_sent() < 2 {
        assert!(Instant::now() < deadline, "the DDL never sent its request");
        std::thread::yield_now();
    }
    // The DDL waits; the writer has not yet run it.
    words.push(control.persisted_epoch());
    inbox.drain(&StdSegmentFs, &dir).expect("the DDL's META swap");
    words.push(control.persisted_epoch());
    let at_answer = sender.join().expect("the DDL's sender");
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        ddl.carried_by(&at_answer),
        "{ddl:?}: the DDL took its answer while the durable META did not carry it (entries {:?}, \
         dropped {:?}); the word's samples {words:?}",
        ids(&at_answer),
        at_answer.dropped,
    );
    assert!(
        words.windows(2).all(|w| w[0] <= w[1]),
        "{ddl:?}: the published word decreased: {words:?}"
    );
}

/// A `CREATE` held before its send while a readiness persist is sent,
/// written and published: its answer must follow its own `META` write,
/// and the word must never decrease.
#[test]
fn a_create_held_before_its_send_is_answered_after_its_own_meta() {
    two_senders(Ddl::Create, "create");
    receipt::verified("ddl_request_held", "answered-after-own-meta");
}

/// The same for a `DROP`: its answer must follow a `META` without the
/// namespace.
#[test]
fn a_drop_held_before_its_send_is_answered_after_its_own_meta() {
    two_senders(Ddl::Drop, "drop");
}

// ---- an answer and its own META write ----

/// The writer held inside its `META` write: the requester reads no
/// answer, and `META` does not yet name the namespace; released, the
/// answer is `Accepted` and `META` names it.
#[test]
fn an_answer_waits_for_its_own_meta_write() {
    let dir = temp_dir("held-write");
    let (control, inbox) = writer(&dir, None);
    let (_, requested) = reserved_create(&control, catalog(Vec::new()), b"fresh", NsMode::Durable);
    let gate = HoldGate::new();
    let armed = gate.clone();
    let drainer = {
        let dir = dir.clone();
        std::thread::spawn(move || {
            let mut inbox = inbox;
            fault::arm_hold(META_WRITE_HELD, &armed);
            drop(armed);
            inbox.drain(&RecordingFs::default(), &dir).expect("the META swap");
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !gate.is_held() {
        assert!(Instant::now() < deadline, "the writer never began its META write");
        std::thread::yield_now();
    }
    let requested = match requested.answer(control.catalog_word()) {
        Answer::Pending(requested) => requested,
        Answer::Refused(refusal) => panic!("the writer refused the CREATE: {refusal:?}"),
        Answer::Accepted(_) => panic!(
            "answered before its META write returned (META on disk names the namespace: {})",
            load_catalog(&dir).ok().flatten().is_some_and(|c| ids(&c).contains(&DDL_NS))
        ),
    };
    gate.release();
    drainer.join().expect("the writer's thread");
    let receipt = answered(&control, requested).expect("accepted");
    assert_eq!(receipt.id(), NsId(DDL_NS));
    assert!(ids(&meta(&dir)).contains(&DDL_NS), "META names the namespace once answered");
    std::fs::remove_dir_all(&dir).ok();
}

// ---- refusals write nothing ----

/// A refused request writes no `META` (no `META.new` is created), leaves
/// `META`'s bytes as they were and spends no namespace id (ADR-0148 D4): a
/// `CREATE` whose name the export holds, then a `CREATE` that takes the
/// same id, and a `CREATE` while another is pending.
#[test]
fn a_refused_request_writes_nothing() {
    let dir = temp_dir("refusal");
    let seed = catalog(vec![spec(DDL_NS, b"ledger", NsMode::Durable)]);
    let (control, mut inbox) = writer(&dir, Some(&seed));
    let fs = RecordingFs::default();
    let before = std::fs::read(dir.join("META")).expect("META");
    let next = control.next_ns_id();
    let export = catalog(seed.entries.clone());
    let (_, requested) = reserved_create(&control, export.clone(), b"ledger", NsMode::Memory);
    inbox.drain(&fs, &dir).expect("the drain");
    assert_eq!(answered(&control, requested).map(|r| r.id()), Err(DdlRefusal::NameExists));
    assert!(fs.ops().is_empty(), "the refusal touched the disk: {:?}", fs.ops());
    assert_eq!(std::fs::read(dir.join("META")).expect("META"), before, "META's bytes moved");
    assert_eq!(control.next_ns_id(), next, "the refusal spent a namespace id");
    // An accepted CREATE takes the id the refused one did not spend, and is
    // pending until its notice; a second is refused.
    let (_, first) = reserved_create(&control, export.clone(), b"first", NsMode::Memory);
    inbox.drain(&fs, &dir).expect("the drain");
    assert_eq!(answered(&control, first).map(|r| r.id()), Ok(NsId(next)), "the unspent id");
    assert_eq!(control.next_ns_id(), next + 1, "the acceptance commits its id");
    let written = fs.ops().len();
    let after_first = std::fs::read(dir.join("META")).expect("META");
    let (_, second) = reserved_create(&control, export, b"second", NsMode::Memory);
    inbox.drain(&fs, &dir).expect("the drain");
    assert_eq!(answered(&control, second).map(|r| r.id()), Err(DdlRefusal::PendingConflict));
    assert_eq!(control.next_ns_id(), next + 1, "the refusal spent a namespace id");
    assert_eq!(fs.ops().len(), written, "the refusal touched the disk: {:?}", &fs.ops()[written..]);
    assert_eq!(std::fs::read(dir.join("META")).expect("META"), after_first, "META's bytes moved");
    assert_eq!(control.persisted_epoch(), 3, "every request moved the word");
    std::fs::remove_dir_all(&dir).ok();
}

/// A `CREATE` sent on a reservation the counter has moved past (two
/// `CREATE`s on one ticket's one reservation) is refused before any other
/// verdict, writes nothing and commits nothing (ADR-0148 D4).
#[test]
fn a_create_on_a_spent_reservation_is_refused_and_writes_nothing() {
    let dir = temp_dir("spent-reservation");
    let (control, mut inbox) = writer(&dir, None);
    let fs = RecordingFs::default();
    let ticket = control.ddl_try_acquire(0).expect("the DDL ticket is free");
    let first = ticket.reserve_ns_id().expect("an id is left");
    let again = ticket.reserve_ns_id().expect("an id is left");
    assert_eq!(first.id(), again.id(), "a reservation moves nothing");
    let id = first.id().0;
    let accepted =
        control.request_create(catalog(Vec::new()), spec(id, b"first", NsMode::Memory), first);
    inbox.drain(&fs, &dir).expect("the drain");
    assert_eq!(answered(&control, accepted).map(|r| r.id()), Ok(NsId(id)));
    let written = fs.ops().len();
    let after = std::fs::read(dir.join("META")).expect("META");
    let spent =
        control.request_create(catalog(Vec::new()), spec(id, b"again", NsMode::Memory), again);
    inbox.drain(&fs, &dir).expect("the drain");
    assert_eq!(answered(&control, spent).map(|r| r.id()), Err(DdlRefusal::IdNotNext));
    assert_eq!(fs.ops().len(), written, "the refusal touched the disk: {:?}", &fs.ops()[written..]);
    assert_eq!(std::fs::read(dir.join("META")).expect("META"), after, "META's bytes moved");
    assert_eq!(control.next_ns_id(), id + 1, "the refusal committed nothing");
    drop(ticket);
    std::fs::remove_dir_all(&dir).ok();
}

// ---- every payload carries the DDL in flight ----

/// A DDL's own `META` carries its change though its export carries the
/// old state, and a lagging export sent in its window neither revives a
/// dropped namespace nor drops a created one: a memory `DROP`, a
/// `CREATE`, and a memory `CREATE` rolled back.
#[test]
fn every_payload_carries_the_ddl_in_flight() {
    let dir = temp_dir("own-payload");
    let ledger = spec(DDL_NS, b"ledger", NsMode::Memory);
    let seed = catalog(vec![ledger.clone()]);
    let (control, mut inbox) = writer(&dir, Some(&seed));
    let fs = StdSegmentFs;
    // A memory DROP whose export still names the namespace.
    let dropped = control.request_drop(catalog(vec![ledger.clone()]), DDL_NS, false);
    inbox.drain(&fs, &dir).expect("the drain");
    assert!(answered(&control, dropped).is_ok());
    assert!(!ids(&meta(&dir)).contains(&DDL_NS), "the DROP's own META names the namespace");
    control.request_persist(catalog(vec![ledger.clone()]));
    inbox.drain(&fs, &dir).expect("the drain");
    assert!(!ids(&meta(&dir)).contains(&DDL_NS), "a lagging export revived the dropped namespace");
    std::fs::remove_dir_all(&dir).ok();
    // A CREATE, then a readiness export that lacks it, then its rollback
    // whose export still names it.
    let dir = temp_dir("own-payload-create");
    let (control, mut inbox) = writer(&dir, None);
    let (fresh, created) = reserved_create(&control, catalog(Vec::new()), b"fresh", NsMode::Memory);
    inbox.drain(&fs, &dir).expect("the drain");
    assert!(answered(&control, created).is_ok());
    control.request_persist(catalog(Vec::new()));
    inbox.drain(&fs, &dir).expect("the drain");
    assert!(
        ids(&meta(&dir)).contains(&fresh.id.0),
        "a lagging export dropped the created namespace"
    );
    let rolled_back = control.request_drop(catalog(vec![fresh.clone()]), fresh.id.0, false);
    inbox.drain(&fs, &dir).expect("the drain");
    assert!(answered(&control, rolled_back).is_ok(), "the rollback's drop is accepted");
    control.request_persist(catalog(vec![fresh.clone()]));
    inbox.drain(&fs, &dir).expect("the drain");
    assert!(
        !ids(&meta(&dir)).contains(&fresh.id.0),
        "a lagging export revived the rolled-back namespace"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A dropped durable namespace's index records leave `META` with its
/// entry: a lagging export that still carries the namespace and an index
/// on it is written as a `META` the boot decodes.
#[cfg(feature = "doc")]
#[test]
fn an_index_on_the_dropped_namespace_leaves_meta_with_it() {
    let dir = temp_dir("dropped-index");
    let ledger = spec(DDL_NS, b"ledger", NsMode::Durable);
    let mut lagging = catalog(vec![ledger.clone()]);
    lagging.index.entries.push(inf_store::IndexSpec {
        id: inf_store::IndexId(inf_store::FIRST_INDEX_ID),
        generation: 1,
        ns: NsId(DDL_NS),
        name: b"by-price".to_vec(),
        program: inf_doc::path::compile(b"$.price").expect("valid path").as_bytes().to_vec(),
        key_type: inf_store::IndexKeyType::F64,
        state: inf_store::IndexState::Ready,
    });
    lagging.index.next_id = inf_store::FIRST_INDEX_ID + 1;
    lagging.index.next_generation = 2;
    let (control, mut inbox) = writer(&dir, Some(&lagging));
    let fs = StdSegmentFs;
    let dropped = control.request_drop(catalog(Vec::new()), DDL_NS, true);
    inbox.drain(&fs, &dir).expect("the drain");
    assert!(answered(&control, dropped).is_ok());
    // Cell 0's readiness export, from a cell whose fan leg has not landed.
    control.request_persist(lagging);
    inbox.drain(&fs, &dir).expect("the drain");
    let payload = inf_log::meta::read_meta(&StdSegmentFs, &dir).expect("META reads").expect("META");
    let decoded = NsCatalog::decode(&payload);
    std::fs::remove_dir_all(&dir).ok();
    let decoded = decoded.unwrap_or_else(|e| panic!("the written META does not decode: {e:?}"));
    assert!(!ids(&decoded).contains(&DDL_NS), "the dropped namespace's entry is in META");
    assert!(decoded.index.entries.is_empty(), "an index on the dropped namespace is in META");
    assert_eq!(decoded.dropped, vec![DDL_NS], "its tombstone is");
}
