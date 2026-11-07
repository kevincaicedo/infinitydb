#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and filesystem fixtures, not cell code"
)]
//! The catalog writer's answers to two senders on real threads.
//!
//! A namespace DDL asks the control thread's catalog writer for a `META`
//! swap and acts on the answer: it applies, fans and replies only once the
//! swap that carries it is durable. A second sender (cell 0's index
//! readiness persist, sent from its MAINTAIN outside the DDL ticket) can
//! enqueue between the DDL numbering its request and sending it. The
//! writer must answer each request only after that request's own `META`
//! write returned, and its published word must never decrease (ADR-0189
//! D2). Here the DDL's sender is held at that instant (the
//! `ddl_request_held` point), the second request is sent, written and
//! published, and the DDL is released. The oracle decodes `META` from the
//! disk at the moment the DDL sees its answer and samples the word after
//! every step; it reads no counter of the writer's.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use inf_foundation::fault::{self, FaultSpec, HoldGate};
use inf_server::{CkptSpace, ControlHandle, StdSegmentFs, load_catalog};
use inf_store::{FsyncClass, IndexCatalog, NsCatalog, NsId, NsMode, NsSpec};

/// The namespace the DDL creates or drops.
const DDL_NS: u32 = 16;

fn spec(id: u32, name: &[u8]) -> NsSpec {
    NsSpec {
        id: NsId(id),
        name: name.to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Everysec),
        policy: None,
        maxmemory: None,
        tier: None,
    }
}

/// A catalog with `entries`, its allocator past every id.
fn catalog(entries: Vec<NsSpec>) -> NsCatalog {
    NsCatalog { next_id: DDL_NS + 2, entries, index: IndexCatalog::default(), dropped: Vec::new() }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("inf-catalog-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

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

/// What the held DDL observed when it took its wait as satisfied.
struct Receipt {
    /// `META` decoded from the disk at that moment.
    meta: NsCatalog,
}

/// Sends the DDL's request on its own thread, held between numbering and
/// sending it, and returns its thread with the gate that holds it.
fn held_ddl(
    control: &Arc<ControlHandle>,
    dir: &Path,
    ddl: Ddl,
) -> (std::thread::JoinHandle<Receipt>, HoldGate) {
    let gate = HoldGate::new();
    let control = Arc::clone(control);
    let dir = dir.to_path_buf();
    let armed = gate.clone();
    let sender = std::thread::spawn(move || {
        fault::arm(inf_server::fault::DDL_REQUEST_HELD, FaultSpec::Nth(1));
        fault::arm_hold(inf_server::fault::DDL_REQUEST_HELD, &armed);
        drop(armed);
        let epoch = match ddl {
            Ddl::Create => {
                control.request_persist_create(catalog(Vec::new()), spec(DDL_NS, b"fresh")).0
            }
            Ddl::Drop => control.request_persist_drop(catalog(Vec::new()), DDL_NS, true),
        };
        // The program's wait, as the DDL program runs it: its answer is
        // taken once the published word covers its epoch.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !control.persisted(epoch) {
            assert!(Instant::now() < deadline, "the DDL's wait was never satisfied");
            std::thread::yield_now();
        }
        let meta = load_catalog(&dir).expect("META decodes").expect("META exists");
        Receipt { meta }
    });
    (sender, gate)
}

fn two_senders(ddl: Ddl, tag: &str) {
    let dir = temp_dir(tag);
    // A `DROP` drops a namespace `META` already names.
    let seed = match ddl {
        Ddl::Create => None,
        Ddl::Drop => {
            let seeded = catalog(vec![spec(DDL_NS, b"ledger")]);
            inf_log::meta::write_meta(&StdSegmentFs, &dir, &seeded.encode()).expect("seed META");
            Some(seeded)
        }
    };
    let cells = inf_foundation::CellCount::new(2).expect("two cells");
    let (control, mut inbox, _issuers) =
        ControlHandle::detached_in(seed.as_ref(), CkptSpace::full(cells), 0);
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
    // The DDL sends, and then waits; the writer has not yet run it.
    let receipt = sender.join().expect("the DDL's sender");
    words.push(control.persisted_epoch());
    inbox.drain(&StdSegmentFs, &dir).expect("the DDL's META swap");
    words.push(control.persisted_epoch());
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        ddl.carried_by(&receipt.meta),
        "{ddl:?}: the DDL took its answer while the durable META did not carry it (entries {:?}, \
         dropped {:?}); the word's samples {words:?}",
        receipt.meta.entries.iter().map(|e| e.id.0).collect::<Vec<_>>(),
        receipt.meta.dropped,
    );
    assert!(
        words.windows(2).all(|w| w[0] <= w[1]),
        "{ddl:?}: the published word decreased: {words:?}"
    );
}

/// A `CREATE` held between numbering and sending its request while a
/// readiness persist is sent, written and published: its answer must
/// follow its own `META` write, and the word must never decrease.
#[test]
#[ignore = "red until the writer numbers requests at dequeue and answers each through its own \
            slot (ADR-0189 D2)"]
fn a_create_held_before_its_send_is_answered_after_its_own_meta() {
    two_senders(Ddl::Create, "create");
}

/// The same for a `DROP`: its answer must follow a `META` without the
/// namespace.
#[test]
#[ignore = "red until the writer numbers requests at dequeue and answers each through its own \
            slot (ADR-0189 D2)"]
fn a_drop_held_before_its_send_is_answered_after_its_own_meta() {
    two_senders(Ddl::Drop, "drop");
}
