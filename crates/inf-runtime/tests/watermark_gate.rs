#![allow(
    clippy::disallowed_types,
    reason = "test target: the model's std containers, outside cell code (ADR-0163 D2)"
)]
//! The watermark gate against an independent model (ADR-0177 D5 with A2): seeded waits made,
//! polled, advanced over, woken a budget at a time and dropped, against a `BTreeMap` of the
//! queued waits by LSN and a set of the dropped ones, which share no code with the gate or its
//! heap. A wait's waker records its id when woken, so a call's wakes are read in order. Every
//! call wakes exactly the model's covered waits, in LSN order, at most its budget; no wait is
//! Ready before the watermark covers it; a dropped wait leaves the heap and the pool at once.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_runtime::gate::ListWait;
use inf_runtime::{WaitList, WaitNodes, WatermarkGate, WatermarkWait};

/// The pool the model's cases run on: `T` nodes.
const T: u32 = 64;
const POOL: Cap = Cap::entries::<T>("watermark-model-pool", CapFill::Serving);
/// The drop row's pool: its four kinds of 1,000 waits and the 1,000 it keeps.
const DROPS: Cap = Cap::entries::<5_000>("watermark-drop-pool", CapFill::Serving);
/// The one-LSN row's pool.
const TIES: Cap = Cap::entries::<1_000>("watermark-tie-pool", CapFill::Serving);
/// The starved row's pool: one node.
const ONE: Cap = Cap::entries::<1>("watermark-one-pool", CapFill::Serving);

thread_local! {
    /// The ids the test's wakers woke, in order: a wake log the gate never sees.
    static WOKEN: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// A waker that records its wait's id in [`WOKEN`].
struct Recorder(u32);

impl Wake for Recorder {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        WOKEN.with(|log| log.borrow_mut().push(self.0));
    }
}

/// The ids woken since the last read.
fn woken() -> Vec<u32> {
    WOKEN.with(|log| std::mem::take(&mut *log.borrow_mut()))
}

/// One poll of `wait` with id `id`'s recording waker.
fn poll(wait: &mut WatermarkWait, id: u32) -> Poll<()> {
    let waker = Waker::from(Arc::new(Recorder(id)));
    Pin::new(wait).poll(&mut Context::from_waker(&waker))
}

/// A gate on a pool of `cap` on a census of its own, and the pool's handle.
fn gate(cap: Cap) -> (WatermarkGate, WaitNodes) {
    let (census, _mark) = CapCensus::new(CellId(0));
    let pool = WaitNodes::build(cap, &census).expect("the test pool");
    (WatermarkGate::build(pool.seat), pool.nodes)
}

/// The order a case makes its waits in.
#[derive(Clone, Copy, Debug)]
enum Order {
    Rising,
    Falling,
    Shuffled(u64),
}

/// `0..n` in `order`.
fn ordered(n: u32, order: Order) -> Vec<u32> {
    let mut ids: Vec<u32> = (0..n).collect();
    match order {
        Order::Rising => {}
        Order::Falling => ids.reverse(),
        Order::Shuffled(seed) => shuffle(&mut ids, seed),
    }
    ids
}

/// A seeded Fisher-Yates shuffle.
fn shuffle<V>(items: &mut [V], seed: u64) {
    let mut rng = SplitMix64::new(seed);
    for at in (1..items.len()).rev() {
        let other = usize::try_from(rng.next_u64() % (at as u64 + 1)).expect("an index");
        items.swap(at, other);
    }
}

/// Wait `id`'s LSN in a case of `n`: 10, 20, … and the last at `u64::MAX`, so an advance can
/// land below every wait, at one, between two and on the largest LSN there is.
fn lsn_of(id: u32, n: u32) -> u64 {
    if id + 1 == n { u64::MAX } else { 10 * (u64::from(id) + 1) }
}

/// A case: `n` waits made and first-polled in `order` on a pool of `T`, then each advance of
/// the list followed by calls of `budget` while the gate owes. After every call the model's
/// covered waits, least LSN first, are the ones woken, the call's count is at most its budget
/// and is the wakes it recorded, and every wait the model holds queued polls Pending; after
/// the calls nothing is owed, each woken wait polls Ready once and gives its node back.
fn run_case(n: u32, order: Order, budget: usize) {
    let name = format!("{n} waits {order:?}, budget {budget}");
    let (gate, nodes) = gate(POOL);
    let mut waits: BTreeMap<u32, WatermarkWait> = BTreeMap::new();
    let mut queued: BTreeMap<u64, u32> = BTreeMap::new();
    for id in ordered(n, order) {
        let lsn = lsn_of(id, n);
        let mut wait = gate.wait_for(lsn);
        assert_eq!(poll(&mut wait, id), Poll::Pending, "{name}: wait {id} above the watermark");
        waits.insert(id, wait);
        queued.insert(lsn, id);
    }
    assert_eq!((gate.waiting(), nodes.len()), (n as usize, n), "{name}: each queued on a node");
    assert!(woken().is_empty(), "{name}: a first poll woke a wait");
    // At the watermark (0), below the least LSN, at an entry, between two, and to the largest.
    for to in [0, 5, 20, 35, u64::MAX] {
        gate.advance(to);
        assert!(woken().is_empty(), "{name}: the advance to {to} woke a wait");
        let covered: Vec<u32> = queued.range(..=to).map(|(_, id)| *id).collect();
        let mut order_woken = Vec::new();
        // At most a call a wait, and one that finds none: a gate still owing after them fails.
        for _ in 0..=n {
            if !gate.owes() {
                break;
            }
            let count = gate.wake_covered(budget);
            let call = woken();
            assert!(count <= budget, "{name}: a call woke {count}, past its budget");
            assert_eq!(call.len(), count, "{name}: the call's count is its wakes");
            order_woken.extend(call);
        }
        assert!(!gate.owes(), "{name}: still owed after the advance to {to}");
        assert_eq!(order_woken, covered, "{name}: the advance to {to} woke other than the model");
        queued.retain(|lsn, _| *lsn > to);
        for id in queued.values() {
            let wait = waits.get_mut(id).expect("a queued wait");
            assert_eq!(poll(wait, *id), Poll::Pending, "{name}: wait {id} Ready uncovered");
        }
        for id in &covered {
            let mut wait = waits.remove(id).expect("a woken wait");
            assert_eq!(poll(&mut wait, *id), Poll::Ready(()), "{name}: woken wait {id} read");
            assert_eq!(poll(&mut wait, *id), Poll::Ready(()), "{name}: Ready again, no node");
        }
        assert_eq!(gate.waiting(), queued.len(), "{name}: waiting() is the model's count");
        assert_eq!(nodes.len() as usize, queued.len(), "{name}: a node a queued wait");
    }
    assert!(queued.is_empty(), "{name}: the advance to u64::MAX covers every wait");
    assert_eq!(nodes.full(), 0, "{name}: no first poll found the pool empty");
}

/// The gate against its model in three orders, rising, falling and seeded, at 1, 2, `T - 1`
/// and `T` waits, each at budgets of 1, `k - 1`, `k` and `k + 1`, `k` the case's waits. A
/// wait at `u64::MAX` is woken by the advance to it.
#[test]
fn the_gate_wakes_exactly_the_models_covered_waits_in_lsn_order() {
    for n in [1, 2, T - 1, T] {
        for order in [Order::Rising, Order::Falling, Order::Shuffled(0x5EED_0000 + u64::from(n))] {
            let k = n as usize;
            for budget in [1, k.saturating_sub(1).max(1), k, k + 1] {
                run_case(n, order, budget);
            }
        }
    }
}

/// A wait covered when it first polls is Ready with no node, no count and no wake; a wait
/// made and never polled registers nothing, so `waiting()` and the pool read 0 with it alive.
#[test]
fn a_wait_made_and_not_polled_registers_nothing() {
    let (gate, nodes) = gate(POOL);
    let made = gate.wait_for(7);
    assert_eq!((gate.waiting(), nodes.len()), (0, 0), "a wait made and not polled holds nothing");
    gate.advance(7);
    let mut covered = gate.wait_for(7);
    assert_eq!(poll(&mut covered, 1), Poll::Ready(()), "a covered first poll is Ready");
    assert_eq!((gate.waiting(), nodes.len(), nodes.full()), (0, 0, 0), "no node, no count");
    drop(made);
    assert!(woken().is_empty(), "nothing was woken");
}

/// 1,000 waits at one LSN: the advance to it wakes none, and the calls it owes wake all 1,000,
/// each once, in whatever order the heap keeps ties.
#[test]
fn waits_at_one_lsn_are_all_woken_by_the_calls_their_advance_owes() {
    const WAITS: u32 = 1_000;
    let (gate, nodes) = gate(TIES);
    let mut waits: Vec<WatermarkWait> = Vec::new();
    for id in 0..WAITS {
        let mut wait = gate.wait_for(42);
        assert_eq!(poll(&mut wait, id), Poll::Pending, "wait {id} queued");
        waits.push(wait);
    }
    gate.advance(41);
    for _ in 0..2 {
        assert_eq!(gate.wake_covered(64), 0, "below the LSN no wait is covered");
    }
    assert!(!gate.owes(), "the call that found none cleared the debt");
    gate.advance(42);
    assert!(woken().is_empty(), "the advance woke a wait");
    let mut all = Vec::new();
    for _ in 0..=WAITS / 64 + 1 {
        if !gate.owes() {
            break;
        }
        let count = gate.wake_covered(64);
        assert!(count <= 64, "a call past its budget");
        all.extend(woken());
    }
    assert!(!gate.owes(), "still owed after every call it needs");
    let set: BTreeSet<u32> = all.iter().copied().collect();
    assert_eq!((all.len(), set.len()), (WAITS as usize, WAITS as usize), "each woken once");
    for (id, wait) in (0..).zip(waits.iter_mut()) {
        assert_eq!(poll(wait, id), Poll::Ready(()), "wait {id} read its wake");
    }
    assert_eq!((gate.waiting(), nodes.len()), (0, 0), "every node back in the pool");
}

/// What the drop row does to a wait.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fate {
    /// Dropped while queued, before any advance.
    Queued,
    /// Dropped covered and not yet woken: after the advance, before the wakes.
    Covered,
    /// Dropped woken and unread: after its wake, before its poll.
    Unread,
    /// Dropped before its first poll.
    Unpolled,
    /// Kept: woken once and read.
    Kept,
}

/// 1,000 waits of each fate at shuffled LSNs, so the queued drops remove entries from every
/// depth of the heap and the covered ones from its covered prefix: after the drops and the
/// wakes past them all, `waiting()` and the pool read 0, and every kept wait was woken once
/// and no dropped one at all.
#[test]
fn every_drop_leaves_the_heap_and_the_pool_at_once() {
    const EACH: u32 = 1_000;
    let fates = [Fate::Queued, Fate::Covered, Fate::Unread, Fate::Unpolled, Fate::Kept];
    let (gate, nodes) = gate(DROPS);
    let mut lsns: Vec<u64> = (1..=5 * u64::from(EACH)).collect();
    shuffle(&mut lsns, 0xD80_95E7);
    let mut waits: BTreeMap<u32, (Fate, WatermarkWait)> = BTreeMap::new();
    for (id, lsn) in (0u32..).zip(&lsns) {
        let fate = fates[(id % 5) as usize];
        let mut wait = gate.wait_for(*lsn);
        if fate != Fate::Unpolled {
            assert_eq!(poll(&mut wait, id), Poll::Pending, "wait {id} queued");
        }
        waits.insert(id, (fate, wait));
    }
    let drop_all = |waits: &mut BTreeMap<u32, (Fate, WatermarkWait)>, fate: Fate| {
        let mut ids: Vec<u32> =
            waits.iter().filter(|(_, w)| w.0 == fate).map(|(id, _)| *id).collect();
        shuffle(&mut ids, u64::from(fate as u8));
        for id in ids {
            drop(waits.remove(&id));
        }
    };
    drop_all(&mut waits, Fate::Unpolled);
    drop_all(&mut waits, Fate::Queued);
    gate.advance(u64::MAX);
    drop_all(&mut waits, Fate::Covered);
    for _ in 0..=5 * EACH / 256 + 1 {
        if !gate.owes() {
            break;
        }
        let _ = gate.wake_covered(256);
    }
    assert!(!gate.owes(), "still owed after the wakes past every wait");
    drop_all(&mut waits, Fate::Unread);
    let log = woken();
    let set: BTreeSet<u32> = log.iter().copied().collect();
    assert_eq!(log.len(), set.len(), "a wait woken twice");
    for id in &log {
        let fate = (id % 5) as usize;
        assert!(matches!(fates[fate], Fate::Unread | Fate::Kept), "dropped wait {id} woken");
    }
    assert_eq!(set.len(), 2 * EACH as usize, "every unread and kept wait woken once");
    for (id, (_, wait)) in &mut waits {
        assert_eq!(poll(wait, *id), Poll::Ready(()), "kept wait {id} read its wake");
    }
    waits.clear();
    assert_eq!((gate.waiting(), nodes.len()), (0, 0), "the heap and the pool empty");
    assert_eq!(nodes.full(), 0, "no first poll found the pool empty");
}

/// A pool of one node: a second wait above the watermark finds none at each of 100 polls,
/// Pending each time and counted; once the first is woken and read it queues on the freed
/// node, and is Ready only after its own wake.
#[test]
fn a_wait_with_no_node_stays_pending_until_one_frees_and_its_wake_lands() {
    let (gate, nodes) = gate(ONE);
    let mut first = gate.wait_for(10);
    assert_eq!(poll(&mut first, 0), Poll::Pending, "the first takes the one node");
    let mut second = gate.wait_for(20);
    for _ in 0..100 {
        assert_eq!(poll(&mut second, 1), Poll::Pending, "no node: Pending, never Ready");
    }
    let starved = nodes.full();
    assert!(starved > 0, "VACUOUS pool-never-empty: no first poll found the pool empty");
    assert_eq!(starved, 100, "each poll with no node counted");
    assert_eq!(woken(), vec![1; 100], "each starved poll woke its own task");
    gate.advance(10);
    assert_eq!(gate.wake_covered(1), 1, "the first is woken");
    assert_eq!(woken(), vec![0], "the first's wake");
    assert_eq!(poll(&mut first, 0), Poll::Ready(()), "the first reads its wake");
    assert_eq!(poll(&mut second, 1), Poll::Pending, "the second queues on the freed node");
    assert_eq!((gate.waiting(), nodes.len()), (1, 1), "the second holds the node");
    gate.advance(20);
    assert_eq!(poll(&mut second, 1), Poll::Pending, "covered, but not yet woken");
    assert_eq!(gate.wake_covered(1), 1, "the second is woken");
    assert_eq!(poll(&mut second, 1), Poll::Ready(()), "Ready after its wake");
    assert_eq!((gate.waiting(), nodes.len(), nodes.full()), (0, 0, 100), "the pool empty again");
}

thread_local! {
    /// What a re-entrant waker runs when woken: the test's own step, on the test's thread.
    static ON_WAKE: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
}

/// A waker whose wake runs [`ON_WAKE`]'s step, inside the gate's call.
struct Reentrant;

impl Wake for Reentrant {
    fn wake(self: Arc<Self>) {
        ON_WAKE.with(|step| {
            if let Some(step) = step.borrow_mut().as_mut() {
                step();
            }
        });
    }
}

/// One poll of a wait on the re-entrant waker.
fn poll_reentrant<F: Future + Unpin>(wait: &mut F) -> Poll<F::Output> {
    let waker = Waker::from(Arc::new(Reentrant));
    Pin::new(wait).poll(&mut Context::from_waker(&waker))
}

/// One of the re-entrant waker's polls, named, and what it answered.
type Answer = (&'static str, Poll<()>);

/// A waker that reaches the gate and the pool inside `wake_covered`'s wake: it polls a covered
/// wait, one above the watermark and a list wait of the same pool, drops a queued wait, then
/// advances and calls `wake_covered` itself. Both of the gate's borrows end before each wake,
/// so nothing panics: the covered wait is Ready with no node, the other two queue, the dropped
/// one leaves, and the nested call wakes what its advance covers.
#[test]
fn a_waker_that_reaches_the_gate_and_the_pool_inside_its_wake_finds_them_free() {
    let (gate, nodes) = gate(POOL);
    let list = WaitList::unit(&nodes);
    let mut doomed = gate.wait_for(100);
    assert_eq!(poll(&mut doomed, 9), Poll::Pending, "the wait the waker drops is queued");
    let doomed = Rc::new(RefCell::new(Some(doomed)));
    let answers: Rc<RefCell<Vec<Answer>>> = Rc::new(RefCell::new(Vec::new()));
    let made: Rc<RefCell<Vec<WatermarkWait>>> = Rc::new(RefCell::new(Vec::new()));
    let listed: Rc<RefCell<Vec<ListWait<()>>>> = Rc::new(RefCell::new(Vec::new()));
    let (step_gate, step_list) = (gate.clone(), list.clone());
    let step_doomed = Rc::clone(&doomed);
    let (step_answers, step_made, step_listed) =
        (Rc::clone(&answers), Rc::clone(&made), Rc::clone(&listed));
    ON_WAKE.with(|step| {
        *step.borrow_mut() = Some(Box::new(move || {
            let mut answers = step_answers.borrow_mut();
            let mut covered = step_gate.wait_for(5);
            answers.push(("covered", poll(&mut covered, 1)));
            let mut above = step_gate.wait_for(50);
            answers.push(("above", poll(&mut above, 2)));
            step_made.borrow_mut().push(above);
            let mut wait = step_list.wait(());
            let waker = Waker::from(Arc::new(Recorder(3)));
            answers.push(("listed", Pin::new(&mut wait).poll(&mut Context::from_waker(&waker))));
            step_listed.borrow_mut().push(wait);
            drop(step_doomed.borrow_mut().take());
            step_gate.advance(50);
            let nested = step_gate.wake_covered(8);
            answers.push(("nested", if nested == 1 { Poll::Ready(()) } else { Poll::Pending }));
        }));
    });
    let mut first = gate.wait_for(10);
    assert!(poll_reentrant(&mut first).is_pending(), "engagement: queued on the re-entrant waker");
    gate.advance(10);
    assert_eq!(gate.wake_covered(1), 1, "the outer call woke the re-entrant wait");
    ON_WAKE.with(|step| step.borrow_mut().take());
    let answers = answers.borrow();
    assert_eq!(answers.len(), 4, "engagement: the waker's step ran whole");
    assert_eq!(answers[0], ("covered", Poll::Ready(())), "a covered wait made in a wake is Ready");
    assert_eq!(answers[1], ("above", Poll::Pending), "a wait above it queues inside the wake");
    assert_eq!(answers[2], ("listed", Poll::Pending), "a list wait of the pool queues inside it");
    assert_eq!(answers[3], ("nested", Poll::Ready(())), "the nested call woke what it covers");
    assert_eq!(woken(), vec![2], "the nested call woke the wait made above the watermark");
    assert!(doomed.borrow().is_none(), "the queued wait was dropped inside the wake");
    assert!(poll(&mut first, 0).is_ready(), "the outer wait reads its wake");
    let mut above = made.borrow_mut().pop().expect("the wait made inside the wake");
    assert!(poll(&mut above, 2).is_ready(), "the nested call's wake is read");
    assert_eq!(gate.waiting(), 0, "the dropped wait left the heap");
    assert_eq!((list.waiting(), nodes.len()), (1, 1), "the list wait alone holds a node");
    listed.borrow_mut().clear();
    assert_eq!(nodes.len(), 0, "every node back in the pool");
}

/// A waker's own `advance` at the outer call's last budgeted pop: the call ends on its budget
/// with a covered wait queued, so the gate still owes, and the next call wakes it.
#[test]
fn an_advance_inside_the_last_budgeted_wake_leaves_the_gate_owing() {
    let (gate, nodes) = gate(POOL);
    let mut later = gate.wait_for(60);
    assert_eq!(poll(&mut later, 1), Poll::Pending, "a wait the nested advance covers");
    let step_gate = gate.clone();
    ON_WAKE.with(|step| {
        *step.borrow_mut() = Some(Box::new(move || step_gate.advance(60)));
    });
    let mut first = gate.wait_for(10);
    assert!(poll_reentrant(&mut first).is_pending(), "engagement: queued on the re-entrant waker");
    gate.advance(10);
    assert_eq!(gate.wake_covered(1), 1, "the budget's one pop");
    ON_WAKE.with(|step| step.borrow_mut().take());
    assert_eq!(gate.watermark(), 60, "engagement: the waker advanced");
    assert!(gate.owes(), "a covered wait is queued and the gate does not owe it");
    assert_eq!(gate.wake_covered(1), 1, "the next call wakes it");
    assert!(poll(&mut first, 0).is_ready() && poll(&mut later, 1).is_ready(), "both read");
    assert_eq!(gate.wake_covered(1), 0, "nothing left");
    assert!(!gate.owes(), "the call that found none cleared the debt");
    assert_eq!(nodes.len(), 0, "every node back in the pool");
}

/// Both kinds of wait on one pool: list and watermark waits interleaved up to the pool's `T`,
/// woken and dropped in a seeded order, against a model of the nodes each kind holds. The
/// pool's `len` is the model's after every step, and 0 at the end.
#[test]
fn list_and_watermark_waits_share_one_pool_as_its_model_holds() {
    let (gate, nodes) = gate(POOL);
    let list = WaitList::unit(&nodes);
    let mut rng = SplitMix64::new(0xB074_C1D5);
    let mut listed = Vec::new();
    let mut gated: Vec<(u64, WatermarkWait)> = Vec::new();
    let mut next_lsn = 1;
    let mut held_max = 0;
    let waker = Waker::noop();
    for step in 0..20_000 {
        let held = listed.len() + gated.len();
        held_max = held_max.max(held);
        match rng.next_u64() % 6 {
            0 | 1 if held < T as usize => {
                let mut wait = list.wait(());
                assert!(Pin::new(&mut wait).poll(&mut Context::from_waker(waker)).is_pending());
                listed.push(wait);
            }
            2 | 3 if held < T as usize => {
                let mut wait = gate.wait_for(next_lsn);
                assert!(Pin::new(&mut wait).poll(&mut Context::from_waker(waker)).is_pending());
                gated.push((next_lsn, wait));
                next_lsn += 1;
            }
            4 if !gated.is_empty() => {
                let at = usize::try_from(rng.next_u64() % gated.len() as u64).expect("index");
                let (_, wait) = gated.swap_remove(at);
                drop(wait);
            }
            5 if !listed.is_empty() => {
                assert!(list.wake_one(()), "a queued list wait is woken");
                let mut wait = listed.remove(0);
                assert!(Pin::new(&mut wait).poll(&mut Context::from_waker(waker)).is_ready());
            }
            _ => {
                let to = gated.iter().map(|(lsn, _)| *lsn).min().unwrap_or(0);
                gate.advance(to);
                for _ in 0..=T {
                    if gate.wake_covered(T as usize) == 0 {
                        break;
                    }
                }
                for (_, wait) in gated.iter_mut().filter(|(lsn, _)| *lsn <= to) {
                    let read = Pin::new(wait).poll(&mut Context::from_waker(waker));
                    assert!(read.is_ready(), "step {step}: a covered wait read its wake");
                }
                gated.retain(|(lsn, _)| *lsn > to);
            }
        }
        let model = listed.len() + gated.len();
        assert_eq!(nodes.len() as usize, model, "step {step}: the pool holds the model's nodes");
        assert_eq!(gate.waiting() + list.waiting(), model, "step {step}: each kind's count");
    }
    assert_eq!(held_max, T as usize, "engagement: the waits filled the pool");
    drop(gated);
    for mut wait in listed {
        assert!(list.wake_one(()), "the list's head is woken");
        assert!(Pin::new(&mut wait).poll(&mut Context::from_waker(waker)).is_ready());
    }
    assert_eq!(nodes.len(), 0, "every node back in the pool");
}
