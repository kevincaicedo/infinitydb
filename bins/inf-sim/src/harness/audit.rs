//! The M0 scenario's model audit: node vs model entry folds, expiry
//! equalization, the round-trip audit, and the planted canary.

use super::*;

// ---- quiescent audits + content reconciliation (review of 2026-08-30) ---------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(super) enum AuditState {
    Idle,
    /// Clients hold their windows until every in-flight command answered.
    Draining,
}

/// `(scope, key) → (value, expiry deadline in internal ms)` — the content
/// both sides are compared on.
type Entries = BTreeMap<(ExecScope, Vec<u8>), (Vec<u8>, Option<u64>)>;

type Plane = ServerPlane<SharedOracle, inf_server::StdSegmentFs>;

type Cells = Vec<(CellLoop<SimDriver, Rc<VirtualClock>>, Plane)>;

/// Every cell's live entries at `now`. A key present on two cells breaks
/// slot ownership and is reported as such rather than folded away.
pub(super) fn fold_node_entries(
    cells: &Cells,
    now: Nanos,
    violations: &mut Vec<String>,
) -> Entries {
    let mut node: Entries = BTreeMap::new();
    for (i, (_, plane)) in cells.iter().enumerate() {
        plane.fold_live_entries(now, |scope, key, value, deadline| {
            if node.insert((scope, key.to_vec()), (value.to_vec(), deadline)).is_some() {
                violations.push(format!(
                    "key {:?} on {scope:?} is live on cell {i} and on another cell",
                    String::from_utf8_lossy(key)
                ));
            }
        });
    }
    node
}

/// The model's live entries at `now`, through the node's own walker.
pub(super) fn fold_model_entries(model: &mut Keyspace, now: Nanos) -> Entries {
    let mut entries: Entries = BTreeMap::new();
    fold_live_entries(model, now, |scope, key, value, deadline| {
        entries.insert((scope, key.to_vec()), (value.to_vec(), deadline));
    });
    entries
}

fn entry_name(scope: ExecScope, key: &[u8]) -> String {
    format!("{scope:?}:{}", String::from_utf8_lossy(key))
}

/// Exact comparison of two entry maps; `None` when they agree, otherwise
/// one violation naming counts and up to five examples per difference
/// class (missing, extra, value, deadline).
pub(super) fn reconcile_entries(node: &Entries, model: &Entries, label: &str) -> Option<String> {
    if node == model {
        return None;
    }
    let sample =
        |iter: &mut dyn Iterator<Item = String>| iter.take(5).collect::<Vec<_>>().join(", ");
    let node_only = sample(
        &mut node.keys().filter(|k| !model.contains_key(*k)).map(|(s, k)| entry_name(*s, k)),
    );
    let model_only = sample(
        &mut model.keys().filter(|k| !node.contains_key(*k)).map(|(s, k)| entry_name(*s, k)),
    );
    let values = sample(&mut node.iter().filter_map(|(k, (nv, _))| {
        let (mv, _) = model.get(k)?;
        (nv != mv).then(|| {
            format!(
                "{} node {:?} vs model {:?}",
                entry_name(k.0, &k.1),
                String::from_utf8_lossy(&nv[..nv.len().min(24)]),
                String::from_utf8_lossy(&mv[..mv.len().min(24)])
            )
        })
    }));
    let deadlines = sample(&mut node.iter().filter_map(|(k, (_, nd))| {
        let (_, md) = model.get(k)?;
        (nd != md).then(|| format!("{} node {nd:?} vs model {md:?}", entry_name(k.0, &k.1)))
    }));
    Some(format!(
        "content reconciliation failed ({label}): node {} entries vs model {} \
         (node-only: [{node_only}] model-only: [{model_only}] value mismatches: [{values}] \
         deadline mismatches: [{deadlines}])",
        node.len(),
        model.len(),
    ))
}

/// ADR-0008 A1 O3 at the shipped topology: after the drain, a cell's
/// arena live allocations — read first, a count that reaps nothing — must
/// equal the entries the fold then emits at the same frozen instant. The
/// fold reaps expired records (as reads and `SCAN` do), so a record the
/// drain left past its deadline shows up as the difference. Independent
/// of the schedule: it reads arena counts and the checkpoint walk only.
/// The fold does not walk tiered namespaces, whose arena count it could
/// then not match, so a cell with one cannot be checked: that is
/// `VACUOUS`, never a pass (no scenario of this harness registers one).
pub(super) fn expired_retained(
    cell: usize,
    live: u64,
    plane: &Plane,
    now: Nanos,
) -> Option<String> {
    let mut emitted = 0u64;
    let skipped = plane.fold_live_entries(now, |_, _, _, _| emitted += 1);
    if skipped > 0 {
        return Some(format!(
            "VACUOUS: cell {cell}: the expired-retained check cannot run: the fold skipped \
             {skipped} tiered namespaces"
        ));
    }
    (live != emitted).then(|| {
        format!(
            "EXPIRED RETAINED: cell {cell}: {live} live records vs {emitted} live entries \
             after the expiry drain (a record outlived its deadline without a reap)"
        )
    })
}

/// The small-cap run's engagement (ADR-0008 A1 O3): a run under
/// `--wheel-nodes-max` must have refused a placement and reaped through
/// the sweep node-wide, or it proved nothing — `VACUOUS`, never green.
pub(super) fn small_cap_engagement(scenario: &Scenario, cells: &Cells) -> Option<String> {
    scenario.wheel_nodes_max?;
    let (mut refused, mut swept) = (0u64, 0u64);
    for (_, plane) in cells {
        let stats = plane.keyspace().stats();
        refused += stats.wheel_fallback;
        swept += stats.expired_swept;
    }
    (refused == 0 || swept == 0).then(|| {
        format!(
            "VACUOUS: the small-cap expiry run refused {refused} placements and swept \
             {swept} records node-wide (both must be > 0)"
        )
    })
}

/// Reaps every already-expired entry on both sides at `now` (active vs lazy
/// expiry equalized) so served counts are comparable.
fn equalize_expiry(cells: &Cells, model: &mut Keyspace, now: Nanos) {
    for (_, plane) in cells {
        plane.drain_expiry(now);
    }
    drain_model_expiry(model, now);
}

/// The model's side of ADR-0008 A1 O3's drain: time frozen at `now`,
/// every budget unbounded, until every wheel has caught up and every sweep
/// is idle or completed a pass that began at `now`.
pub(super) fn drain_model_expiry(model: &mut Keyspace, now: Nanos) {
    for _ in 0..inf_store::limits::EXPIRY_DRAIN_SLICES_MAX {
        model.expire_tick(now, ExpiryBudget::UNBOUNDED);
        if model.expiry_settled(now) {
            return;
        }
    }
}

/// Sends one command on an auditor connection and drives the cells until
/// its reply is complete. The clock does **not** advance: nothing else is
/// in flight, and a frozen `now` keeps every served count comparable with
/// the model folded at the same instant. Bounded by the stall detector's
/// step budget.
fn audit_roundtrip(
    cells: &mut Cells,
    nets: &[Rc<RefCell<CellNet>>],
    auditor: &mut Auditor,
    wire: &[u8],
) -> Result<Vec<u8>, String> {
    nets[auditor.cell].borrow_mut().client_send(auditor.fd, wire);
    for _ in 0..STALL_STEPS {
        for (cell_loop, plane) in cells.iter_mut() {
            cell_loop.run_iteration(plane).expect("audit iteration");
        }
        let rx = nets[auditor.cell].borrow_mut().client_recv(auditor.fd);
        auditor.rx.extend_from_slice(&rx);
        if let Some(n) = reply_len(&auditor.rx) {
            return Ok(auditor.rx.drain(..n).collect());
        }
    }
    Err(format!(
        "auditor {} did not answer within {STALL_STEPS} steps (command {:?})",
        auditor.scope.name(),
        String::from_utf8_lossy(wire)
    ))
}

/// Bulks of a `KEYS` / `SCAN` page reply.
fn bulk_set(items: &[Reply]) -> Option<BTreeSet<Vec<u8>>> {
    items
        .iter()
        .map(|item| match item {
            Reply::Bulk(key) => Some(key.clone()),
            Reply::Simple(_) | Reply::Error(_) | Reply::Int(_) | Reply::Nil | Reply::Array(_) => {
                None
            }
        })
        .collect()
}

/// Pages one audit `SCAN` walk may take (a full node walk at `COUNT 1` over
/// a few hundred keys stays far below it).
const AUDIT_SCAN_PAGES_MAX: u32 = 65_536;

/// One quiescent audit: expiry equalized, stored content reconciled, then
/// per scope the served surface over the wire against the model's entries
/// for that scope, then (seeded) a flush replayed on the model and a second
/// content pass.
#[allow(clippy::too_many_arguments)] // one linear audit script over the run's state
pub(super) fn run_audit(
    cells: &mut Cells,
    nets: &[Rc<RefCell<CellNet>>],
    clock: &Rc<VirtualClock>,
    rng: &mut SplitMix64,
    oracle: &SharedOracle,
    auditors: &mut [Auditor],
    label: &str,
    report: &mut SimReport,
    violations: &mut Vec<String>,
) {
    let now = clock.now();
    report.audits += 1;
    // The oracle is borrowed only between roundtrips: driving the cells
    // re-enters `on_execute`, which takes the same `RefCell`.
    let model = {
        let mut oracle = oracle.0.borrow_mut();
        equalize_expiry(cells, &mut oracle.model, now);
        fold_model_entries(&mut oracle.model, now)
    };
    let node = fold_node_entries(cells, now, violations);
    if let Some(violation) = reconcile_entries(&node, &model, label) {
        violations.push(violation);
    }
    // Served surface per scope — the plane's scatter programs end to end.
    for auditor in auditors.iter_mut() {
        let who = format!("{label}, auditor {}", auditor.scope.name());
        if !auditor.bound
            && let Some(bind) = auditor.scope.bind_wire()
        {
            match audit_roundtrip(cells, nets, auditor, &bind) {
                Ok(reply) if reply == b"+OK\r\n" => {}
                Ok(reply) => violations
                    .push(format!("{who}: binding refused: {:?}", String::from_utf8_lossy(&reply))),
                Err(e) => violations.push(format!("{who}: {e}")),
            }
        }
        auditor.bound = true;
        let scope = auditor.scope.exec_scope();
        let expected: BTreeSet<Vec<u8>> =
            model.keys().filter(|(s, _)| *s == scope).map(|(_, k)| k.clone()).collect();
        // DBSIZE: the scope's node-wide count.
        match audit_roundtrip(cells, nets, auditor, &encode(&[b"DBSIZE".to_vec()])) {
            Ok(reply) => {
                let want = Reply::Int(expected.len() as i64);
                let got = parse_reply(&reply);
                if got != want {
                    violations.push(format!("{who}: DBSIZE {got:?}, model {want:?}"));
                }
            }
            Err(e) => violations.push(format!("{who}: {e}")),
        }
        // SCAN: a full walk at a seeded COUNT must enumerate the scope's set.
        let count = [1u64, 7, 50][(rng.next_u64() % 3) as usize];
        let mut cursor = 0u64;
        let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut pages = 0u32;
        loop {
            let argv = vec![
                b"SCAN".to_vec(),
                cursor.to_string().into_bytes(),
                b"COUNT".to_vec(),
                count.to_string().into_bytes(),
            ];
            let reply = match audit_roundtrip(cells, nets, auditor, &encode(&argv)) {
                Ok(reply) => parse_reply(&reply),
                Err(e) => {
                    violations.push(format!("{who}: {e}"));
                    break;
                }
            };
            let page = match &reply {
                Reply::Array(items) if items.len() == 2 => match (&items[0], &items[1]) {
                    (Reply::Bulk(digits), Reply::Array(keys)) => core::str::from_utf8(digits)
                        .ok()
                        .and_then(|text| text.parse::<u64>().ok())
                        .zip(bulk_set(keys)),
                    _ => None,
                },
                Reply::Simple(_)
                | Reply::Error(_)
                | Reply::Int(_)
                | Reply::Bulk(_)
                | Reply::Nil
                | Reply::Array(_) => None,
            };
            let Some((next, keys)) = page else {
                violations.push(format!("{who}: SCAN answered {reply:?}"));
                break;
            };
            seen.extend(keys);
            pages += 1;
            cursor = next;
            if cursor == 0 {
                break;
            }
            if pages >= AUDIT_SCAN_PAGES_MAX {
                violations.push(format!("{who}: SCAN walk did not terminate"));
                break;
            }
        }
        if cursor == 0 && seen != expected {
            violations.push(format!(
                "{who}: SCAN COUNT {count} walk enumerated {} keys, model holds {} \
                 (missing: [{}] extra: [{}])",
                seen.len(),
                expected.len(),
                sample_keys(expected.difference(&seen)),
                sample_keys(seen.difference(&expected)),
            ));
        }
        // KEYS: a seeded glob over the alphabet.
        let glob = auditor.scope.glob(rng.next_u64() % 10);
        let want: BTreeSet<Vec<u8>> =
            expected.iter().filter(|k| glob_match(&glob, k, false)).cloned().collect();
        match audit_roundtrip(cells, nets, auditor, &encode(&[b"KEYS".to_vec(), glob.clone()])) {
            Ok(reply) => match parse_reply(&reply) {
                Reply::Array(items) if bulk_set(&items).is_some_and(|got| got == want) => {}
                other @ (Reply::Simple(_)
                | Reply::Error(_)
                | Reply::Int(_)
                | Reply::Bulk(_)
                | Reply::Nil
                | Reply::Array(_)) => violations.push(format!(
                    "{who}: KEYS {:?} answered {other:?}, model set has {} keys",
                    String::from_utf8_lossy(&glob),
                    want.len()
                )),
            },
            Err(e) => violations.push(format!("{who}: {e}")),
        }
        // RANDOMKEY: membership (the two-level draw is the recorded deviation).
        match audit_roundtrip(cells, nets, auditor, &encode(&[b"RANDOMKEY".to_vec()])) {
            Ok(reply) => match parse_reply(&reply) {
                Reply::Nil if expected.is_empty() => {}
                Reply::Bulk(key) if expected.contains(&key) => {}
                other @ (Reply::Simple(_)
                | Reply::Error(_)
                | Reply::Int(_)
                | Reply::Bulk(_)
                | Reply::Nil
                | Reply::Array(_)) => violations.push(format!(
                    "{who}: RANDOMKEY answered {other:?} against a {}-key scope",
                    expected.len()
                )),
            },
            Err(e) => violations.push(format!("{who}: {e}")),
        }
    }
    // A seeded flush, replayed on the model at this quiescent point (the
    // apply seam never replays flush legs), then the content pass again.
    if !auditors.is_empty() && rng.next_u64().is_multiple_of(4) {
        let index = (rng.next_u64() as usize) % auditors.len();
        let auditor = &mut auditors[index];
        let argv: Vec<Vec<u8>> = if rng.next_u64().is_multiple_of(2) {
            vec![b"FLUSHALL".to_vec()]
        } else {
            vec![b"FLUSHDB".to_vec()]
        };
        let who = format!("{label}, auditor {}", auditor.scope.name());
        let mut expected = Vec::new();
        {
            let slices: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
            let mut cx = model_cx(auditor.scope.exec_scope());
            let mut oracle = oracle.0.borrow_mut();
            execute_slices(&slices, &mut oracle.model, &mut cx, now, &mut expected);
            // The independent model follows an acknowledged flush only.
            if expected == b"+OK\r\n" {
                if argv[0] == b"FLUSHALL" {
                    oracle.shadow.flush_all();
                } else {
                    oracle.shadow.flush_db(auditor.scope.exec_scope());
                }
            }
        }
        match audit_roundtrip(cells, nets, auditor, &encode(&argv)) {
            Ok(reply) if reply == expected => {}
            Ok(reply) => violations.push(format!(
                "{who}: {:?} answered {:?}, model {:?}",
                String::from_utf8_lossy(&argv[0]),
                String::from_utf8_lossy(&reply),
                String::from_utf8_lossy(&expected)
            )),
            Err(e) => violations.push(format!("{who}: {e}")),
        }
        report.flushes += 1;
        let node = fold_node_entries(cells, now, violations);
        let model = fold_model_entries(&mut oracle.0.borrow_mut().model, now);
        if let Some(violation) = reconcile_entries(
            &node,
            &model,
            &format!("{label} after {}", String::from_utf8_lossy(&argv[0])),
        ) {
            violations.push(violation);
        }
    }
}

fn sample_keys<'k>(keys: impl Iterator<Item = &'k Vec<u8>>) -> String {
    keys.take(5).map(|k| String::from_utf8_lossy(k).into_owned()).collect::<Vec<_>>().join(", ")
}

/// Damages the model per `canary` (tests only) and names the entry it
/// touched; `None` when nothing was planted or no suitable entry exists.
pub(super) fn plant_canary(
    model: &mut Keyspace,
    canary: Canary,
    now: Nanos,
) -> Option<(ExecScope, Vec<u8>)> {
    if canary == Canary::None {
        return None;
    }
    let entries = fold_model_entries(model, now);
    let target = entries
        .iter()
        .find(|(_, (_, deadline))| canary != Canary::DropDeadline || deadline.is_some())
        .map(|((scope, key), _)| (*scope, key.clone()))?;
    let (scope, key) = &target;
    let store = match scope {
        ExecScope::Db(db) => model.db_mut(usize::from(*db)),
        ExecScope::Ns(ns) => model.ns_store_mut(*ns).expect("folded entry's namespace exists"),
        ExecScope::Unavailable => unreachable!("no entries fold under an unavailable scope"),
    };
    match canary {
        Canary::None => unreachable!("handled above"),
        Canary::DropKey => {
            store.del(key, now);
        }
        Canary::CorruptValue => {
            store.set(key, b"canary", SetOptions::default(), now).expect("in-bounds canary value");
        }
        Canary::DropDeadline => {
            store.expire(key, None, ExpireCond::Always, now);
        }
    }
    Some(target)
}
