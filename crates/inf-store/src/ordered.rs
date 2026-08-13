//! Per-cell ordered key-range projection — the M4.5-S01 B+-tree
//! (`docs/milestones/m4.5-indexes-query.md` §3.2 freeze row; ADR-0072 D2).
//!
//! One tree holds the entries of one secondary index: `(typed key bytes,
//! entry_ref)` pairs in memcmp order, `entry_ref` as the final tie-break —
//! duplicate keys (many documents sharing one indexed value) are ordinary,
//! and remove/lower-bound address the exact pair (ADR-0072 D5's idempotent
//! entry ops need pair identity). Single-threaded per cell (L1): no `Sync`
//! bounds, no atomics, no native pointers — nodes live in typed pools
//! addressed by `u32` ids, in safe Rust.
//!
//! Two monomorphized key schemes, chosen per tree by the registry's key
//! type (the napkin in the S01 ledger entry killed the one-layout
//! alternative at ~37 B/entry on numeric corpora):
//!
//! - [`Fixed8`] — numeric keys, exactly 8 bytes; the big-endian `u64`
//!   prefix *is* the key. 16 B/entry SoA arrays, no heap.
//! - [`VarKey`] — arbitrary keys ≤ [`ORDERED_KEY_MAX`]; a zero-padded
//!   `u64` prefix plus the suffix (bytes past 8) in a size-classed heap.
//!   Order is `(prefix, suffix, len)`, which equals memcmp order: equal
//!   zero-padded prefixes mean the byte-8 tails decide, and when both
//!   tails are empty the shorter key is a strict prefix of the longer
//!   (proptested against `Vec<u8>` order).
//!
//! Leaf/branch search is [`inf_simd::lower_bound_u64`] over the node's
//! contiguous prefix array (branchless count-less-than); equal-prefix runs
//! resolve scalar, bounded by the fanout. Cursors **re-seek, never pin**
//! (the §3.2 freeze): a cursor owns its resume pair and a cached
//! (leaf, slot, epoch) hint — `next()` is an in-leaf advance while the
//! tree epoch is unchanged, else one re-seek to the first pair past the
//! resume point, which is what makes the M1-SCAN-shaped property (every
//! pair present throughout a scan is returned at least once) hold by
//! construction under interleaved mutation.
//!
//! Mutations are plan-then-commit at this layer too (the ADR-0072 D7
//! property S04's reservation stands on): every allocation an operation
//! needs — split nodes, separator copies — happens before any structure
//! moves, so a capacity `Err` always leaves the tree exactly as it was.

use core::cmp::Ordering;
use core::marker::PhantomData;

use inf_simd::lower_bound_u64;

/// Structural cap on a tree key (u16-safe; the DynamoDB key-cap
/// precedent). S02's encoding ADR owns the semantic cap and the
/// too-long ⇒ no-entry (counted) rule — the tree only refuses.
pub const ORDERED_KEY_MAX: usize = 1024;

/// Tree height bound for the iterative descent path (fanout ≥ 8 makes
/// 16 levels astronomically sufficient; violated ⇒ corrupt tree).
const MAX_HEIGHT: usize = 16;

/// Null node id (pools never reach `u32::MAX` nodes — checked on alloc).
const NONE: u32 = u32::MAX;

/// Capacity failures are operating conditions, not invariants: the caller
/// (S04's plan-then-commit reservation) turns them into typed refusals.
/// On `Err` the tree is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderedMapError {
    /// The variable-key heap would exceed its u32 address space.
    HeapFull,
    /// A node pool would exceed its u32 id space.
    NodeLimit,
}

/// Per-tree memory attribution (L5) — every byte the tree holds, split
/// into reserved and slack so the S03 `idx_tree_bytes`/`idx_slack_bytes`
/// domains can be assembled without probing internals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OrderedMapMemory {
    /// Bytes reserved by the leaf pool (capacity, not just live nodes).
    pub leaf_bytes: u64,
    /// Bytes reserved by the internal-node pool.
    pub internal_bytes: u64,
    /// Bytes reserved by the variable-key heap (zero for `Fixed8`).
    pub heap_bytes: u64,
    /// The slack inside the above: free-listed nodes, unallocated pool
    /// capacity, free heap classes, and size-class rounding.
    pub slack_bytes: u64,
    /// Live entry count (equals `OrderedMap::len`).
    pub entries: u64,
}

impl OrderedMapMemory {
    /// Total reserved bytes.
    pub fn total_bytes(&self) -> u64 {
        self.leaf_bytes + self.internal_bytes + self.heap_bytes
    }
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Fixed8 {}
    impl Sealed for super::VarKey {}
}

/// A key scheme fixes the per-entry layout at compile time (sealed —
/// the two schemes below are the vocabulary the registry chooses from).
/// The meta accessors monomorphize to nothing for [`Fixed8`].
pub trait KeyScheme: sealed::Sealed + 'static {
    /// Per-entry sideband beyond the u64 prefix (nothing for `Fixed8`).
    #[doc(hidden)]
    type Meta: Copy + Default + core::fmt::Debug;
    /// True when the prefix is the whole key.
    #[doc(hidden)]
    const FIXED: bool;
    #[doc(hidden)]
    fn meta_new(addr: u32, len: u16) -> Self::Meta;
    #[doc(hidden)]
    fn meta_addr(meta: Self::Meta) -> u32;
    #[doc(hidden)]
    fn meta_len(meta: Self::Meta) -> u16;
}

/// Exactly-8-byte keys (S02 numeric encodings): the prefix is the key.
#[derive(Debug)]
pub struct Fixed8;

/// Arbitrary keys up to [`ORDERED_KEY_MAX`] bytes.
#[derive(Debug)]
pub struct VarKey;

/// `VarKey` sideband: heap address of the suffix + full key length.
/// Packed: the meta array is 6 B/entry, not 8 — fields are only ever
/// copied by value (references into packed layouts don't compile).
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C, packed)]
pub struct VarMeta {
    addr: u32,
    len: u16,
}

impl KeyScheme for Fixed8 {
    type Meta = ();
    const FIXED: bool = true;

    fn meta_new(_addr: u32, _len: u16) -> Self::Meta {}

    fn meta_addr(_meta: Self::Meta) -> u32 {
        0
    }

    fn meta_len(_meta: Self::Meta) -> u16 {
        8
    }
}

impl KeyScheme for VarKey {
    type Meta = VarMeta;
    const FIXED: bool = false;

    fn meta_new(addr: u32, len: u16) -> Self::Meta {
        VarMeta { addr, len }
    }

    fn meta_addr(meta: Self::Meta) -> u32 {
        meta.addr
    }

    fn meta_len(meta: Self::Meta) -> u16 {
        meta.len
    }
}

/// A probe key, parsed once per operation.
struct Probe<'a> {
    prefix: u64,
    suffix: &'a [u8],
    len: u16,
}

/// Zero-padded big-endian prefix of `key` (order-preserving together
/// with the suffix + len tie-breaks — see the module doc).
fn key_prefix(key: &[u8]) -> u64 {
    let take = key.len().min(8);
    let mut buf = [0u8; 8];
    buf[..take].copy_from_slice(&key[..take]);
    u64::from_be_bytes(buf)
}

fn make_probe<S: KeyScheme>(key: &[u8]) -> Probe<'_> {
    if S::FIXED {
        assert!(key.len() == 8, "Fixed8 keys are exactly 8 bytes");
    } else {
        assert!(key.len() <= ORDERED_KEY_MAX, "key exceeds ORDERED_KEY_MAX");
    }
    let head = key.len().min(8);
    Probe { prefix: key_prefix(key), suffix: &key[head..], len: key.len() as u16 }
}

// --- node pools ---------------------------------------------------------

/// Leaf node: SoA arrays so the prefix region is one contiguous SIMD
/// scan. `next` chains leaves left-to-right for range cursors.
struct Leaf<S: KeyScheme, const F: usize> {
    count: u16,
    next: u32,
    prefixes: [u64; F],
    metas: [S::Meta; F],
    refs: [u64; F],
}

/// Internal node: up to `F - 1` separators and `F` children. The
/// separator at `i` fences child `i` from child `i + 1`: pairs strictly
/// below it live left, pairs at or above it live right.
struct Internal<S: KeyScheme, const F: usize> {
    count: u16,
    prefixes: [u64; F],
    metas: [S::Meta; F],
    refs: [u64; F],
    children: [u32; F],
}

impl<S: KeyScheme, const F: usize> Leaf<S, F> {
    fn new() -> Self {
        Leaf {
            count: 0,
            next: NONE,
            prefixes: [0; F],
            metas: [S::Meta::default(); F],
            refs: [0; F],
        }
    }
}

impl<S: KeyScheme, const F: usize> Internal<S, F> {
    fn new() -> Self {
        Internal {
            count: 0,
            prefixes: [0; F],
            metas: [S::Meta::default(); F],
            refs: [0; F],
            children: [NONE; F],
        }
    }
}

/// Typed node pool: `u32` ids, free-list recycling, no native pointers.
struct Pool<T> {
    nodes: Vec<T>,
    free: Vec<u32>,
}

impl<T> Pool<T> {
    fn new() -> Self {
        Pool { nodes: Vec::new(), free: Vec::new() }
    }

    fn alloc(&mut self, node: T) -> Result<u32, OrderedMapError> {
        if let Some(id) = self.free.pop() {
            self.nodes[id as usize] = node;
            return Ok(id);
        }
        let id = self.nodes.len();
        if id >= NONE as usize {
            return Err(OrderedMapError::NodeLimit);
        }
        // Bounded growth: Vec's amortized doubling would leave up to
        // 100% capacity slack against the 40 B/entry gate (L5) — grow
        // in ~12.5% steps instead (slack is measured, so the bound is
        // visible in `memory()`, not folklore).
        if self.nodes.len() == self.nodes.capacity() {
            self.nodes.reserve_exact((self.nodes.capacity() / 8).max(64));
        }
        self.nodes.push(node);
        Ok(id as u32)
    }

    fn release(&mut self, id: u32) {
        debug_assert!((id as usize) < self.nodes.len());
        self.free.push(id);
    }

    fn get(&self, id: u32) -> &T {
        &self.nodes[id as usize]
    }

    fn get_mut(&mut self, id: u32) -> &mut T {
        &mut self.nodes[id as usize]
    }

    /// Two distinct nodes mutably (the safe split-borrow pattern).
    fn get2_mut(&mut self, a: u32, b: u32) -> (&mut T, &mut T) {
        assert!(a != b, "get2_mut requires distinct ids");
        let (a, b) = (a as usize, b as usize);
        if a < b {
            let (lo, hi) = self.nodes.split_at_mut(b);
            (&mut lo[a], &mut hi[0])
        } else {
            let (lo, hi) = self.nodes.split_at_mut(a);
            (&mut hi[0], &mut lo[b])
        }
    }

    fn reserved_bytes(&self) -> u64 {
        (self.nodes.capacity() * size_of::<T>()) as u64
    }

    fn slack_bytes(&self) -> u64 {
        let spare = self.nodes.capacity() - self.nodes.len() + self.free.len();
        (spare * size_of::<T>()) as u64
    }
}

// --- variable-key heap ----------------------------------------------------

/// Suffix classes round up to 8-byte steps: class `c` holds blocks of
/// `c * 8` bytes, `c` in `1..=127` (suffixes are 1..=1016 bytes).
const HEAP_CLASSES: usize = ORDERED_KEY_MAX / 8;

/// Size-classed byte heap for `VarKey` suffixes. Addresses are byte
/// offsets into one `Vec<u8>` — stable across growth, `u32`-bounded
/// (the cap is a typed operating error, never a panic).
struct KeyHeap {
    bytes: Vec<u8>,
    free: Vec<Vec<u32>>,
    free_bytes: u64,
    pad_bytes: u64,
}

impl KeyHeap {
    fn new() -> Self {
        KeyHeap { bytes: Vec::new(), free: Vec::new(), free_bytes: 0, pad_bytes: 0 }
    }

    fn class_of(suffix_len: usize) -> usize {
        debug_assert!((1..=ORDERED_KEY_MAX - 8).contains(&suffix_len));
        suffix_len.div_ceil(8)
    }

    fn alloc(&mut self, suffix: &[u8]) -> Result<u32, OrderedMapError> {
        let class = Self::class_of(suffix.len());
        let block = class * 8;
        if self.free.is_empty() {
            self.free = vec![Vec::new(); HEAP_CLASSES + 1];
        }
        let addr = match self.free[class].pop() {
            Some(addr) => {
                self.free_bytes -= block as u64;
                addr
            }
            None => {
                let addr = self.bytes.len();
                if addr + block > NONE as usize {
                    return Err(OrderedMapError::HeapFull);
                }
                // Same bounded-growth rule as the node pools (L5).
                if addr + block > self.bytes.capacity() {
                    let step = (self.bytes.capacity() / 8).max(4096).max(block);
                    self.bytes.reserve_exact(step);
                }
                self.bytes.resize(addr + block, 0);
                addr as u32
            }
        };
        self.bytes[addr as usize..addr as usize + suffix.len()].copy_from_slice(suffix);
        self.pad_bytes += (block - suffix.len()) as u64;
        Ok(addr)
    }

    fn release(&mut self, addr: u32, suffix_len: usize) {
        let class = Self::class_of(suffix_len);
        self.free[class].push(addr);
        self.free_bytes += (class * 8) as u64;
        self.pad_bytes -= (class * 8 - suffix_len) as u64;
    }

    fn get(&self, addr: u32, suffix_len: usize) -> &[u8] {
        &self.bytes[addr as usize..addr as usize + suffix_len]
    }
}

/// An owned separator: `(prefix, meta, entry_ref)` whose heap bytes (if
/// any) belong to the node that holds it, not to any leaf entry.
type Sep<S> = (u64, <S as KeyScheme>::Meta, u64);

// --- the tree ---------------------------------------------------------------

/// One secondary index's ordered `(key bytes, entry_ref)` set. `F` is
/// the node fanout. Default 32 — the S01 L4 A/B verdict
/// (`.artifacts/m4.5/s01/`): fanout 32 won the two binding hot paths
/// (hot point probe −9%, insert −4% random / −11% sequential) against
/// fanout 64's +5% memory and +2.8 ns amortized `next()`; every §4.1
/// budget passes at either fanout, so the gate-relevant rows decided.
pub struct OrderedMap<S: KeyScheme, const F: usize = 32> {
    leaves: Pool<Leaf<S, F>>,
    internals: Pool<Internal<S, F>>,
    heap: KeyHeap,
    /// Root node id (`NONE` = empty tree). At `height == 0` the root is
    /// a leaf; otherwise an internal whose subtrees bottom out in
    /// leaves after exactly `height` hops (all leaves share a depth).
    root: u32,
    height: u16,
    len: u64,
    /// Bumped on every successful mutation; cursors use it to tell an
    /// intact position hint from one that must re-seek.
    epoch: u64,
    _scheme: PhantomData<S>,
}

impl<S: KeyScheme, const F: usize> Default for OrderedMap<S, F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: KeyScheme, const F: usize> OrderedMap<S, F> {
    const LEAF_MIN: usize = F / 2;
    const SEP_MAX: usize = F - 1;
    const SEP_MIN: usize = (F - 1) / 2;

    pub fn new() -> Self {
        const { assert!(F >= 8 && F <= 256 && F.is_multiple_of(2), "fanout: even, 8..=256") };
        OrderedMap {
            leaves: Pool::new(),
            internals: Pool::new(),
            heap: KeyHeap::new(),
            root: NONE,
            height: 0,
            len: 0,
            epoch: 0,
            _scheme: PhantomData,
        }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// L5 attribution snapshot — O(1) from maintained pool/heap state.
    pub fn memory(&self) -> OrderedMapMemory {
        let heap_free_list_bytes: u64 =
            self.heap.free.iter().map(|f| (f.capacity() * size_of::<u32>()) as u64).sum();
        OrderedMapMemory {
            leaf_bytes: self.leaves.reserved_bytes(),
            internal_bytes: self.internals.reserved_bytes(),
            heap_bytes: self.heap.bytes.capacity() as u64 + heap_free_list_bytes,
            slack_bytes: self.leaves.slack_bytes()
                + self.internals.slack_bytes()
                + self.heap.free_bytes
                + self.heap.pad_bytes
                + (self.heap.bytes.capacity() - self.heap.bytes.len()) as u64,
            entries: self.len,
        }
    }

    // -- comparisons ------------------------------------------------------

    /// Entry key vs probe key (no ref) — `(prefix, suffix, len)`.
    fn cmp_key(&self, prefix: u64, meta: S::Meta, probe: &Probe<'_>) -> Ordering {
        let by_prefix = prefix.cmp(&probe.prefix);
        if S::FIXED || by_prefix != Ordering::Equal {
            return by_prefix;
        }
        let len = S::meta_len(meta);
        let suffix_len = usize::from(len).saturating_sub(8);
        let suffix = self.heap.get(S::meta_addr(meta), suffix_len);
        suffix.cmp(probe.suffix).then(len.cmp(&probe.len))
    }

    /// Entry pair vs probe pair.
    fn cmp_pair(
        &self,
        prefix: u64,
        meta: S::Meta,
        entry_ref: u64,
        probe: &Probe<'_>,
        probe_ref: u64,
    ) -> Ordering {
        self.cmp_key(prefix, meta, probe).then(entry_ref.cmp(&probe_ref))
    }

    /// First slot in `leaf` whose pair is `>= (probe, probe_ref)` (or
    /// `>` when `allow_equal` is false). Prefix jump + a scalar walk
    /// bounded by the equal-prefix run. The prefix jump strategy is the
    /// S01 A/B's to pick (`.artifacts/m4.5/s01/`) — see
    /// [`Self::leaf_prefix_jump`].
    fn leaf_lower_bound(
        &self,
        leaf: &Leaf<S, F>,
        probe: &Probe<'_>,
        probe_ref: u64,
        allow_equal: bool,
    ) -> usize {
        let count = leaf.count as usize;
        let mut idx = Self::leaf_prefix_jump(&leaf.prefixes[..count], probe.prefix);
        while idx < count {
            let order = self.cmp_pair(
                leaf.prefixes[idx],
                leaf.metas[idx],
                leaf.refs[idx],
                probe,
                probe_ref,
            );
            let advance = order == Ordering::Less || (!allow_equal && order == Ordering::Equal);
            if !advance {
                break;
            }
            idx += 1;
        }
        idx
    }

    /// Early-exit prefix scan: touches only the cache lines up to the
    /// boundary (~half the leaf on average) at the price of one
    /// well-predicted mispredict per search, where the branchless
    /// count-scan streams the whole prefix array. Early-vs-count is an
    /// S01 A/B row (`.artifacts/m4.5/s01/`); the count-scan stays
    /// reachable through [`OrderedMap::contains_scalar_search`].
    fn leaf_prefix_jump(prefixes: &[u64], probe: u64) -> usize {
        let mut idx = 0;
        while idx < prefixes.len() && prefixes[idx] < probe {
            idx += 1;
        }
        idx
    }

    /// Child index for `(probe, probe_ref)` in an internal node: the
    /// number of separators `<=` the pair (pairs equal to a separator
    /// route right — "left strictly below, right at-or-above").
    fn route_child(&self, node: &Internal<S, F>, probe: &Probe<'_>, probe_ref: u64) -> usize {
        let count = node.count as usize;
        let mut idx = lower_bound_u64(&node.prefixes[..count], probe.prefix);
        while idx < count {
            let order = self.cmp_pair(
                node.prefixes[idx],
                node.metas[idx],
                node.refs[idx],
                probe,
                probe_ref,
            );
            if order == Ordering::Greater {
                break;
            }
            idx += 1;
        }
        idx
    }

    /// Descend to the leaf owning `(probe, probe_ref)`, recording the
    /// internal path (bounded — MAX_HEIGHT is an internal invariant).
    fn descend(
        &self,
        probe: &Probe<'_>,
        probe_ref: u64,
        path: &mut [(u32, u16); MAX_HEIGHT],
    ) -> u32 {
        assert!((self.height as usize) < MAX_HEIGHT, "tree height invariant violated");
        let mut node = self.root;
        for slot in path.iter_mut().take(self.height as usize) {
            let internal = self.internals.get(node);
            let child = self.route_child(internal, probe, probe_ref);
            *slot = (node, child as u16);
            node = internal.children[child];
        }
        node
    }

    // -- insert -------------------------------------------------------------

    /// Insert the pair; `Ok(false)` when it is already present (the
    /// ADR-0072 D5 insert-if-absent semantics). On `Err` (capacity) the
    /// tree is unchanged — allocations happen before structure moves.
    pub fn insert(&mut self, key: &[u8], entry_ref: u64) -> Result<bool, OrderedMapError> {
        let probe = make_probe::<S>(key);
        if self.root == NONE {
            return self.insert_first(&probe, entry_ref);
        }
        let mut path = [(NONE, 0u16); MAX_HEIGHT];
        let leaf_id = self.descend(&probe, entry_ref, &mut path);
        let leaf = self.leaves.get(leaf_id);
        let idx = self.leaf_lower_bound(leaf, &probe, entry_ref, true);
        let exact = idx < leaf.count as usize
            && self.cmp_pair(
                leaf.prefixes[idx],
                leaf.metas[idx],
                leaf.refs[idx],
                &probe,
                entry_ref,
            ) == Ordering::Equal;
        if exact {
            return Ok(false);
        }
        if (leaf.count as usize) < F {
            let meta = self.store_meta(&probe)?;
            let leaf = self.leaves.get_mut(leaf_id);
            shift_right(leaf, idx);
            write_slot(leaf, idx, probe.prefix, meta, entry_ref);
            leaf.count += 1;
        } else {
            self.insert_split(leaf_id, idx, &probe, entry_ref, &path)?;
        }
        self.len += 1;
        self.epoch += 1;
        Ok(true)
    }

    fn insert_first(&mut self, probe: &Probe<'_>, entry_ref: u64) -> Result<bool, OrderedMapError> {
        let meta = self.store_meta(probe)?;
        let mut leaf = Leaf::new();
        write_slot(&mut leaf, 0, probe.prefix, meta, entry_ref);
        leaf.count = 1;
        match self.leaves.alloc(leaf) {
            Ok(id) => self.root = id,
            Err(error) => {
                self.free_meta(meta);
                return Err(error);
            }
        }
        self.len = 1;
        self.epoch += 1;
        Ok(true)
    }

    /// Store the probe's suffix (VarKey) and build the entry meta.
    fn store_meta(&mut self, probe: &Probe<'_>) -> Result<S::Meta, OrderedMapError> {
        if S::FIXED || probe.suffix.is_empty() {
            return Ok(S::meta_new(0, probe.len));
        }
        let addr = self.heap.alloc(probe.suffix)?;
        Ok(S::meta_new(addr, probe.len))
    }

    /// Free an entry's heap suffix (remove and failure paths).
    fn free_meta(&mut self, meta: S::Meta) {
        if !S::FIXED {
            let suffix_len = usize::from(S::meta_len(meta)).saturating_sub(8);
            if suffix_len > 0 {
                self.heap.release(S::meta_addr(meta), suffix_len);
            }
        }
    }

    /// An owned separator copy of the pair at `(leaf_id, slot)`.
    fn copy_entry_as_sep(&mut self, leaf_id: u32, slot: usize) -> Result<Sep<S>, OrderedMapError> {
        let leaf = self.leaves.get(leaf_id);
        debug_assert!(slot < leaf.count as usize, "separator source in bounds");
        let (prefix, meta, entry_ref) = (leaf.prefixes[slot], leaf.metas[slot], leaf.refs[slot]);
        if S::FIXED {
            return Ok((prefix, meta, entry_ref));
        }
        let len = S::meta_len(meta);
        let suffix_len = usize::from(len).saturating_sub(8);
        let addr = if suffix_len == 0 {
            0
        } else {
            let copied = self.heap.get(S::meta_addr(meta), suffix_len).to_vec();
            self.heap.alloc(&copied)?
        };
        Ok((prefix, S::meta_new(addr, len), entry_ref))
    }

    /// An owned separator copy of the probe pair itself (the rightmost-
    /// split case, where the new entry becomes the right leaf's first).
    fn copy_probe_as_sep(
        &mut self,
        probe: &Probe<'_>,
        entry_ref: u64,
    ) -> Result<Sep<S>, OrderedMapError> {
        let meta = self.store_meta(probe)?;
        Ok((probe.prefix, meta, entry_ref))
    }

    /// Full-leaf insert: plan every allocation (entry meta, separator
    /// copy, split nodes, grown root), then commit with infallible
    /// moves only. The plan phase is fully rolled back on `Err`.
    fn insert_split(
        &mut self,
        leaf_id: u32,
        idx: usize,
        probe: &Probe<'_>,
        entry_ref: u64,
        path: &[(u32, u16); MAX_HEIGHT],
    ) -> Result<(), OrderedMapError> {
        // Rightmost-append heuristic: appending to the end of the
        // rightmost leaf splits full/empty, so ascending corpora
        // (timestamps) fill nodes instead of halving them.
        let rightmost = self.leaves.get(leaf_id).next == NONE;
        let at = if rightmost && idx == F { F } else { F / 2 };

        // Plan: entry meta, the separator copy (reads pre-split state),
        // the right leaf, one internal per full ancestor, the new root.
        let mut plan = InsertPlan::<S>::default();
        if let Err(error) = self.plan_split(&mut plan, leaf_id, at, probe, entry_ref, path) {
            self.rollback_plan(&plan);
            return Err(error);
        }

        // Commit (infallible): split the leaf, place the entry, thread
        // the separator upward through the pre-allocated nodes.
        self.split_leaf_into(leaf_id, plan.right_leaf, at);
        let (target, target_idx) =
            if at == F || idx > at { (plan.right_leaf, idx - at) } else { (leaf_id, idx) };
        {
            let leaf = self.leaves.get_mut(target);
            shift_right(leaf, target_idx);
            write_slot(leaf, target_idx, probe.prefix, plan.entry_meta, entry_ref);
            leaf.count += 1;
        }
        self.commit_sep_cascade(&plan, path);
        Ok(())
    }

    /// The allocation half of [`Self::insert_split`].
    fn plan_split(
        &mut self,
        plan: &mut InsertPlan<S>,
        leaf_id: u32,
        at: usize,
        probe: &Probe<'_>,
        entry_ref: u64,
        path: &[(u32, u16); MAX_HEIGHT],
    ) -> Result<(), OrderedMapError> {
        plan.entry_meta_planned = Some(self.store_meta(probe)?);
        plan.sep = Some(if at == F {
            self.copy_probe_as_sep(probe, entry_ref)?
        } else {
            self.copy_entry_as_sep(leaf_id, at)?
        });
        plan.right_leaf = self.leaves.alloc(Leaf::new())?;
        // One internal split per consecutively-full ancestor, walking
        // from the immediate parent toward the root. A leaf-only tree
        // has no ancestors: the split itself grows the first root.
        plan.grow_root = self.height == 0;
        for level in (0..self.height as usize).rev() {
            if (self.internals.get(path[level].0).count as usize) < Self::SEP_MAX {
                plan.full_run = (self.height as usize - 1) - level;
                plan.grow_root = false;
                break;
            }
            plan.full_run = self.height as usize - level;
            plan.grow_root = level == 0;
        }
        for _ in 0..plan.full_run {
            plan.push_internal(self.internals.alloc(Internal::new())?);
        }
        if plan.grow_root {
            plan.new_root = self.internals.alloc(Internal::new())?;
        }
        plan.entry_meta_final();
        Ok(())
    }

    /// Release everything a partially-built plan allocated.
    fn rollback_plan(&mut self, plan: &InsertPlan<S>) {
        if let Some(meta) = plan.entry_meta_planned {
            self.free_meta(meta);
        }
        if let Some((_, meta, _)) = plan.sep {
            self.free_meta(meta);
        }
        if plan.right_leaf != NONE {
            self.leaves.release(plan.right_leaf);
        }
        for i in 0..plan.internal_count {
            self.internals.release(plan.internals[i]);
        }
        if plan.new_root != NONE {
            self.internals.release(plan.new_root);
        }
    }

    /// Move entries `at..` of `leaf_id` into the pre-allocated right
    /// sibling and stitch the chain.
    fn split_leaf_into(&mut self, leaf_id: u32, right_id: u32, at: usize) {
        let (left, right) = self.leaves.get2_mut(leaf_id, right_id);
        let moved = left.count as usize - at;
        right.prefixes[..moved].copy_from_slice(&left.prefixes[at..at + moved]);
        right.metas[..moved].copy_from_slice(&left.metas[at..at + moved]);
        right.refs[..moved].copy_from_slice(&left.refs[at..at + moved]);
        right.count = moved as u16;
        right.next = left.next;
        left.next = right_id;
        left.count = at as u16;
    }

    /// Thread the separator upward: insert into the first non-full
    /// ancestor, splitting the full run through pre-allocated nodes.
    fn commit_sep_cascade(&mut self, plan: &InsertPlan<S>, path: &[(u32, u16); MAX_HEIGHT]) {
        let mut sep = plan.sep.expect("planned separator");
        let mut right_child = plan.right_leaf;
        let mut planned_splits = plan.internals[..plan.internal_count].iter();
        for level in (0..self.height as usize).rev() {
            let (node_id, child_idx) = path[level];
            let at = child_idx as usize;
            if (self.internals.get(node_id).count as usize) < Self::SEP_MAX {
                let node = self.internals.get_mut(node_id);
                shift_right_internal(node, at);
                write_sep(node, at, sep, right_child);
                node.count += 1;
                return;
            }
            let new_right = *planned_splits.next().expect("plan covered the full run");
            let evicted = self.split_internal_into(node_id, new_right);
            let mid = Self::SEP_MAX / 2;
            let (target, target_at) =
                if at <= mid { (node_id, at) } else { (new_right, at - (mid + 1)) };
            let node = self.internals.get_mut(target);
            shift_right_internal(node, target_at);
            write_sep(node, target_at, sep, right_child);
            node.count += 1;
            sep = evicted;
            right_child = new_right;
        }
        // Every level split: grow a pre-allocated root.
        let root = self.internals.get_mut(plan.new_root);
        write_sep_only(root, 0, sep);
        root.children[0] = self.root;
        root.children[1] = right_child;
        root.count = 1;
        self.root = plan.new_root;
        self.height += 1;
    }

    /// Split a full internal into the pre-allocated right node; the
    /// middle separator is evicted (ownership moves to the caller).
    fn split_internal_into(&mut self, node_id: u32, right_id: u32) -> Sep<S> {
        let (left, right) = self.internals.get2_mut(node_id, right_id);
        let mid = Self::SEP_MAX / 2;
        let moved = Self::SEP_MAX - mid - 1;
        right.prefixes[..moved].copy_from_slice(&left.prefixes[mid + 1..mid + 1 + moved]);
        right.metas[..moved].copy_from_slice(&left.metas[mid + 1..mid + 1 + moved]);
        right.refs[..moved].copy_from_slice(&left.refs[mid + 1..mid + 1 + moved]);
        right.children[..moved + 1].copy_from_slice(&left.children[mid + 1..mid + 2 + moved]);
        right.count = moved as u16;
        left.count = mid as u16;
        (left.prefixes[mid], left.metas[mid], left.refs[mid])
    }

    // -- point lookup ---------------------------------------------------------

    /// Exact-pair membership (ADR-0072 D5's remove-if-present twin).
    pub fn contains(&self, key: &[u8], entry_ref: u64) -> bool {
        if self.root == NONE {
            return false;
        }
        let probe = make_probe::<S>(key);
        let mut path = [(NONE, 0u16); MAX_HEIGHT];
        let leaf_id = self.descend(&probe, entry_ref, &mut path);
        let leaf = self.leaves.get(leaf_id);
        let idx = self.leaf_lower_bound(leaf, &probe, entry_ref, true);
        idx < leaf.count as usize
            && self.cmp_pair(leaf.prefixes[idx], leaf.metas[idx], leaf.refs[idx], &probe, entry_ref)
                == Ordering::Equal
    }

    /// Bench-only count-scan twin of [`Self::contains`]: the leaf
    /// prefix jump streams the whole prefix array branchlessly instead
    /// of exiting early — the other arm of the S01 leaf-search A/B
    /// (L4). Hidden, not frozen API.
    #[doc(hidden)]
    pub fn contains_scalar_search(&self, key: &[u8], entry_ref: u64) -> bool {
        if self.root == NONE {
            return false;
        }
        let probe = make_probe::<S>(key);
        let mut node = self.root;
        for _ in 0..self.height {
            let internal = self.internals.get(node);
            let count = internal.count as usize;
            let mut idx =
                inf_simd::scalar_lower_bound_u64(&internal.prefixes[..count], probe.prefix);
            while idx < count
                && self.cmp_pair(
                    internal.prefixes[idx],
                    internal.metas[idx],
                    internal.refs[idx],
                    &probe,
                    entry_ref,
                ) != Ordering::Greater
            {
                idx += 1;
            }
            node = internal.children[idx];
        }
        let leaf = self.leaves.get(node);
        let count = leaf.count as usize;
        let mut idx = inf_simd::scalar_lower_bound_u64(&leaf.prefixes[..count], probe.prefix);
        while idx < count
            && self.cmp_pair(leaf.prefixes[idx], leaf.metas[idx], leaf.refs[idx], &probe, entry_ref)
                == Ordering::Less
        {
            idx += 1;
        }
        idx < count
            && self.cmp_pair(leaf.prefixes[idx], leaf.metas[idx], leaf.refs[idx], &probe, entry_ref)
                == Ordering::Equal
    }

    // -- remove -------------------------------------------------------------

    /// Remove the exact pair; `false` when absent (remove-if-present).
    /// Removal never allocates on its structural path — a borrow that
    /// cannot copy its new fence falls back to a merge, which only
    /// frees — so it cannot fail.
    pub fn remove(&mut self, key: &[u8], entry_ref: u64) -> bool {
        if self.root == NONE {
            return false;
        }
        let probe = make_probe::<S>(key);
        let mut path = [(NONE, 0u16); MAX_HEIGHT];
        let leaf_id = self.descend(&probe, entry_ref, &mut path);
        let leaf = self.leaves.get(leaf_id);
        let idx = self.leaf_lower_bound(leaf, &probe, entry_ref, true);
        let exact = idx < leaf.count as usize
            && self.cmp_pair(
                leaf.prefixes[idx],
                leaf.metas[idx],
                leaf.refs[idx],
                &probe,
                entry_ref,
            ) == Ordering::Equal;
        if !exact {
            return false;
        }
        let meta = leaf.metas[idx];
        {
            let leaf = self.leaves.get_mut(leaf_id);
            shift_left(leaf, idx);
            leaf.count -= 1;
        }
        self.free_meta(meta);
        self.len -= 1;
        self.epoch += 1;
        self.rebalance_leaf(leaf_id, &path);
        true
    }

    /// Restore the leaf minimum after a removal, then walk the path
    /// upward fixing internal underflow (bounded by height).
    fn rebalance_leaf(&mut self, leaf_id: u32, path: &[(u32, u16); MAX_HEIGHT]) {
        if self.height == 0 {
            if self.leaves.get(leaf_id).count == 0 {
                self.leaves.release(leaf_id);
                self.root = NONE;
            }
            return;
        }
        if self.leaves.get(leaf_id).count as usize >= Self::LEAF_MIN {
            return;
        }
        let level = self.height as usize - 1;
        let (parent_id, child_idx) = path[level];
        if !self.leaf_borrow(parent_id, child_idx as usize, leaf_id) {
            self.leaf_merge(parent_id, child_idx as usize, leaf_id);
            self.rebalance_internals(path, level);
        }
    }

    /// Try to borrow one entry from a leaf sibling. The new fence is
    /// copied *before* anything moves (plan-then-commit in miniature) —
    /// if that copy cannot allocate, report `false` and let the caller
    /// merge, which frees instead of allocating.
    fn leaf_borrow(&mut self, parent_id: u32, child_idx: usize, leaf_id: u32) -> bool {
        let parent_count = self.internals.get(parent_id).count as usize;
        if child_idx > 0 {
            let left_id = self.internals.get(parent_id).children[child_idx - 1];
            let left_count = self.leaves.get(left_id).count as usize;
            if left_count > Self::LEAF_MIN {
                // The moved entry (left's last) becomes our first — it
                // is the new fence between left and us.
                let Ok(sep) = self.copy_entry_as_sep(left_id, left_count - 1) else {
                    return false;
                };
                let (left, cur) = self.leaves.get2_mut(left_id, leaf_id);
                let last = left.count as usize - 1;
                shift_right(cur, 0);
                write_slot(cur, 0, left.prefixes[last], left.metas[last], left.refs[last]);
                cur.count += 1;
                left.count -= 1;
                self.replace_sep(parent_id, child_idx - 1, sep);
                return true;
            }
        }
        if child_idx < parent_count {
            let right_id = self.internals.get(parent_id).children[child_idx + 1];
            if self.leaves.get(right_id).count as usize > Self::LEAF_MIN {
                // Right's second entry becomes right's first — the new
                // fence between us and right.
                let Ok(sep) = self.copy_entry_as_sep(right_id, 1) else {
                    return false;
                };
                let (right, cur) = self.leaves.get2_mut(right_id, leaf_id);
                let at = cur.count as usize;
                write_slot(cur, at, right.prefixes[0], right.metas[0], right.refs[0]);
                cur.count += 1;
                shift_left(right, 0);
                right.count -= 1;
                self.replace_sep(parent_id, child_idx, sep);
                return true;
            }
        }
        false
    }

    /// Swap a parent fence for a pre-copied one, freeing the old copy.
    fn replace_sep(&mut self, parent_id: u32, sep_idx: usize, sep: Sep<S>) {
        let node = self.internals.get_mut(parent_id);
        let old = node.metas[sep_idx];
        write_sep_only(node, sep_idx, sep);
        self.free_meta(old);
    }

    /// Merge the underflowing leaf with a sibling (always possible when
    /// borrowing was not) and drop the fence from the parent.
    fn leaf_merge(&mut self, parent_id: u32, child_idx: usize, leaf_id: u32) {
        // Canonical form: merge the RIGHT node into the LEFT node.
        let (left_id, right_id, sep_idx) = if child_idx > 0 {
            (self.internals.get(parent_id).children[child_idx - 1], leaf_id, child_idx - 1)
        } else {
            (leaf_id, self.internals.get(parent_id).children[child_idx + 1], child_idx)
        };
        let (left, right) = self.leaves.get2_mut(left_id, right_id);
        let (lc, rc) = (left.count as usize, right.count as usize);
        debug_assert!(lc + rc <= F, "leaf merge must fit one node");
        left.prefixes[lc..lc + rc].copy_from_slice(&right.prefixes[..rc]);
        left.metas[lc..lc + rc].copy_from_slice(&right.metas[..rc]);
        left.refs[lc..lc + rc].copy_from_slice(&right.refs[..rc]);
        left.count = (lc + rc) as u16;
        left.next = right.next;
        self.leaves.release(right_id);
        let dropped = self.drop_sep(parent_id, sep_idx);
        self.free_meta(dropped);
    }

    /// Remove separator `sep_idx` and the child pointer to its right,
    /// returning the separator's meta for the caller to free or move.
    fn drop_sep(&mut self, node_id: u32, sep_idx: usize) -> S::Meta {
        let node = self.internals.get_mut(node_id);
        let count = node.count as usize;
        let meta = node.metas[sep_idx];
        node.prefixes.copy_within(sep_idx + 1..count, sep_idx);
        node.metas.copy_within(sep_idx + 1..count, sep_idx);
        node.refs.copy_within(sep_idx + 1..count, sep_idx);
        node.children.copy_within(sep_idx + 2..count + 1, sep_idx + 1);
        node.count -= 1;
        meta
    }

    /// Fix internal underflow from `level` upward after a child merge.
    fn rebalance_internals(&mut self, path: &[(u32, u16); MAX_HEIGHT], mut level: usize) {
        loop {
            let node_id = path[level].0;
            if node_id == self.root {
                if self.internals.get(node_id).count == 0 {
                    // A fence-less root has exactly one child: collapse.
                    self.root = self.internals.get(node_id).children[0];
                    self.internals.release(node_id);
                    self.height -= 1;
                }
                return;
            }
            if self.internals.get(node_id).count as usize >= Self::SEP_MIN {
                return;
            }
            let (parent_id, child_idx) = path[level - 1];
            if self.internal_borrow(parent_id, child_idx as usize, node_id) {
                return;
            }
            self.internal_merge(parent_id, child_idx as usize, node_id);
            level -= 1;
        }
    }

    /// Rotate one separator through the parent from a richer sibling
    /// (pure ownership moves — no allocation, no copies to fail).
    fn internal_borrow(&mut self, parent_id: u32, child_idx: usize, node_id: u32) -> bool {
        let parent_count = self.internals.get(parent_id).count as usize;
        if child_idx > 0 {
            let left_id = self.internals.get(parent_id).children[child_idx - 1];
            if self.internals.get(left_id).count as usize > Self::SEP_MIN {
                // Parent fence comes down in front; left's last goes up.
                let down = read_sep(self.internals.get(parent_id), child_idx - 1);
                let (left, cur) = self.internals.get2_mut(left_id, node_id);
                let last = left.count as usize - 1;
                let moved_child = left.children[last + 1];
                let up = (left.prefixes[last], left.metas[last], left.refs[last]);
                left.count -= 1;
                let count = cur.count as usize;
                cur.prefixes.copy_within(0..count, 1);
                cur.metas.copy_within(0..count, 1);
                cur.refs.copy_within(0..count, 1);
                cur.children.copy_within(0..count + 1, 1);
                write_sep_only(cur, 0, down);
                cur.children[0] = moved_child;
                cur.count += 1;
                write_sep_only(self.internals.get_mut(parent_id), child_idx - 1, up);
                return true;
            }
        }
        if child_idx < parent_count {
            let right_id = self.internals.get(parent_id).children[child_idx + 1];
            if self.internals.get(right_id).count as usize > Self::SEP_MIN {
                // Parent fence comes down at our end; right's first goes up.
                let down = read_sep(self.internals.get(parent_id), child_idx);
                let (right, cur) = self.internals.get2_mut(right_id, node_id);
                let at = cur.count as usize;
                let moved_child = right.children[0];
                let up = (right.prefixes[0], right.metas[0], right.refs[0]);
                write_sep_only(cur, at, down);
                cur.children[at + 1] = moved_child;
                cur.count += 1;
                let rc = right.count as usize;
                right.prefixes.copy_within(1..rc, 0);
                right.metas.copy_within(1..rc, 0);
                right.refs.copy_within(1..rc, 0);
                right.children.copy_within(1..rc + 1, 0);
                right.count -= 1;
                write_sep_only(self.internals.get_mut(parent_id), child_idx, up);
                return true;
            }
        }
        false
    }

    /// Merge an underflowing internal with a minimum sibling: the parent
    /// fence comes down between them (ownership moves — no allocation).
    fn internal_merge(&mut self, parent_id: u32, child_idx: usize, node_id: u32) {
        let (left_id, right_id, sep_idx) = if child_idx > 0 {
            (self.internals.get(parent_id).children[child_idx - 1], node_id, child_idx - 1)
        } else {
            (node_id, self.internals.get(parent_id).children[child_idx + 1], child_idx)
        };
        let down = read_sep(self.internals.get(parent_id), sep_idx);
        let (left, right) = self.internals.get2_mut(left_id, right_id);
        let (lc, rc) = (left.count as usize, right.count as usize);
        debug_assert!(lc + 1 + rc <= Self::SEP_MAX, "internal merge must fit");
        write_sep_only(left, lc, down);
        left.prefixes[lc + 1..lc + 1 + rc].copy_from_slice(&right.prefixes[..rc]);
        left.metas[lc + 1..lc + 1 + rc].copy_from_slice(&right.metas[..rc]);
        left.refs[lc + 1..lc + 1 + rc].copy_from_slice(&right.refs[..rc]);
        left.children[lc + 1..lc + 2 + rc].copy_from_slice(&right.children[..rc + 1]);
        left.count = (lc + 1 + rc) as u16;
        self.internals.release(right_id);
        // The fence moved down (ownership transferred) — drop the
        // parent slot WITHOUT freeing its meta.
        let _ = self.drop_sep(parent_id, sep_idx);
    }

    // -- cursor support ---------------------------------------------------

    /// Leftmost leaf id (`NONE` when empty).
    fn leftmost_leaf(&self) -> u32 {
        if self.root == NONE {
            return NONE;
        }
        let mut node = self.root;
        for _ in 0..self.height {
            node = self.internals.get(node).children[0];
        }
        node
    }

    /// Position of the first pair `>=` (`>` when `allow_equal` is
    /// false) the probe pair, hopping to the next leaf when the owning
    /// leaf ends below it.
    fn seek(&self, probe: &Probe<'_>, probe_ref: u64, allow_equal: bool) -> Option<(u32, usize)> {
        if self.root == NONE {
            return None;
        }
        let mut path = [(NONE, 0u16); MAX_HEIGHT];
        let leaf_id = self.descend(probe, probe_ref, &mut path);
        let leaf = self.leaves.get(leaf_id);
        let idx = self.leaf_lower_bound(leaf, probe, probe_ref, allow_equal);
        if idx < leaf.count as usize {
            return Some((leaf_id, idx));
        }
        let next = leaf.next;
        if next == NONE {
            return None;
        }
        debug_assert!(self.leaves.get(next).count > 0, "chained leaves are non-empty");
        Some((next, 0))
    }

    /// Write the key at `(leaf, slot)` into `out`; returns the ref.
    fn emit_key(&self, leaf_id: u32, slot: usize, out: &mut Vec<u8>) -> u64 {
        let leaf = self.leaves.get(leaf_id);
        out.clear();
        if S::FIXED {
            out.extend_from_slice(&leaf.prefixes[slot].to_be_bytes());
        } else {
            let meta = leaf.metas[slot];
            let len = usize::from(S::meta_len(meta));
            let head = len.min(8);
            out.extend_from_slice(&leaf.prefixes[slot].to_be_bytes()[..head]);
            if len > 8 {
                out.extend_from_slice(self.heap.get(S::meta_addr(meta), len - 8));
            }
        }
        leaf.refs[slot]
    }

    /// Test-only deep invariant check: pairs strictly ascending across
    /// the whole leaf chain, chain covers `len` exactly, min occupancy
    /// once the tree is past trivial size.
    #[cfg(test)]
    fn check_invariants(&self) {
        if self.root == NONE {
            assert_eq!(self.len, 0);
            return;
        }
        let mut leaf = self.leftmost_leaf();
        let mut total = 0u64;
        let mut prev: Option<(Vec<u8>, u64)> = None;
        let mut buf = Vec::new();
        while leaf != NONE {
            let node = self.leaves.get(leaf);
            assert!(node.count > 0, "empty leaf in chain");
            for slot in 0..node.count as usize {
                let entry_ref = self.emit_key(leaf, slot, &mut buf);
                if let Some((prev_key, prev_ref)) = &prev {
                    assert!(
                        (prev_key.as_slice(), *prev_ref) < (buf.as_slice(), entry_ref),
                        "pairs must be strictly ascending"
                    );
                }
                prev = Some((buf.clone(), entry_ref));
                total += 1;
            }
            leaf = node.next;
        }
        assert_eq!(total, self.len, "leaf chain must cover len exactly");
    }
}

/// The allocations an [`OrderedMap::insert_split`] needs, gathered
/// before any structure moves (rolled back wholesale on failure).
struct InsertPlan<S: KeyScheme> {
    entry_meta_planned: Option<S::Meta>,
    entry_meta: S::Meta,
    sep: Option<Sep<S>>,
    right_leaf: u32,
    internals: [u32; MAX_HEIGHT],
    internal_count: usize,
    full_run: usize,
    grow_root: bool,
    new_root: u32,
}

impl<S: KeyScheme> Default for InsertPlan<S> {
    fn default() -> Self {
        InsertPlan {
            entry_meta_planned: None,
            entry_meta: S::Meta::default(),
            sep: None,
            right_leaf: NONE,
            internals: [NONE; MAX_HEIGHT],
            internal_count: 0,
            full_run: 0,
            grow_root: false,
            new_root: NONE,
        }
    }
}

impl<S: KeyScheme> InsertPlan<S> {
    fn push_internal(&mut self, id: u32) {
        self.internals[self.internal_count] = id;
        self.internal_count += 1;
    }

    /// The plan is complete: the entry meta is committed to use (no
    /// longer rolled back — commit consumes it).
    fn entry_meta_final(&mut self) {
        self.entry_meta = self.entry_meta_planned.take().expect("meta planned");
    }
}

// --- slot helpers (leaf) -----------------------------------------------

fn write_slot<S: KeyScheme, const F: usize>(
    leaf: &mut Leaf<S, F>,
    idx: usize,
    prefix: u64,
    meta: S::Meta,
    entry_ref: u64,
) {
    leaf.prefixes[idx] = prefix;
    leaf.metas[idx] = meta;
    leaf.refs[idx] = entry_ref;
}

fn shift_right<S: KeyScheme, const F: usize>(leaf: &mut Leaf<S, F>, idx: usize) {
    let count = leaf.count as usize;
    debug_assert!(count < F && idx <= count);
    leaf.prefixes.copy_within(idx..count, idx + 1);
    leaf.metas.copy_within(idx..count, idx + 1);
    leaf.refs.copy_within(idx..count, idx + 1);
}

fn shift_left<S: KeyScheme, const F: usize>(leaf: &mut Leaf<S, F>, idx: usize) {
    let count = leaf.count as usize;
    debug_assert!(idx < count);
    leaf.prefixes.copy_within(idx + 1..count, idx);
    leaf.metas.copy_within(idx + 1..count, idx);
    leaf.refs.copy_within(idx + 1..count, idx);
}

// --- slot helpers (internal) ---------------------------------------------

/// Open slot `idx` for a separator + right child (arrays shift right).
fn shift_right_internal<S: KeyScheme, const F: usize>(node: &mut Internal<S, F>, idx: usize) {
    let count = node.count as usize;
    debug_assert!(count < F - 1 && idx <= count);
    node.prefixes.copy_within(idx..count, idx + 1);
    node.metas.copy_within(idx..count, idx + 1);
    node.refs.copy_within(idx..count, idx + 1);
    node.children.copy_within(idx + 1..count + 1, idx + 2);
}

/// Write separator + right child at `idx` (arrays already shifted).
fn write_sep<S: KeyScheme, const F: usize>(
    node: &mut Internal<S, F>,
    idx: usize,
    sep: (u64, S::Meta, u64),
    right_child: u32,
) {
    write_sep_only(node, idx, sep);
    node.children[idx + 1] = right_child;
}

fn write_sep_only<S: KeyScheme, const F: usize>(
    node: &mut Internal<S, F>,
    idx: usize,
    sep: (u64, S::Meta, u64),
) {
    node.prefixes[idx] = sep.0;
    node.metas[idx] = sep.1;
    node.refs[idx] = sep.2;
}

fn read_sep<S: KeyScheme, const F: usize>(
    node: &Internal<S, F>,
    idx: usize,
) -> (u64, S::Meta, u64) {
    (node.prefixes[idx], node.metas[idx], node.refs[idx])
}

// --- cursor ------------------------------------------------------------------

/// Where the cursor resumes from on its next `next()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorPos {
    /// Before the first pair.
    Start,
    /// At or after `(key_buf, entry_ref)` — a fresh inclusive bound.
    AtOrAfter,
    /// Strictly after `(key_buf, entry_ref)` — the last returned pair.
    After,
}

/// A range cursor over an [`OrderedMap`]. It owns its resume pair and
/// never borrows the tree between calls — mutate freely in between; the
/// cursor re-seeks (the pagination freeze, plan §3.2). Amortized
/// `next()` is an in-leaf advance while the tree is unmutated.
pub struct OrderedCursor {
    pos: CursorPos,
    key_buf: Vec<u8>,
    entry_ref: u64,
    hint_leaf: u32,
    hint_slot: u32,
    hint_epoch: u64,
}

impl OrderedCursor {
    /// Cursor over the whole tree, in `(key, ref)` order.
    pub fn from_start() -> OrderedCursor {
        OrderedCursor {
            pos: CursorPos::Start,
            key_buf: Vec::new(),
            entry_ref: 0,
            hint_leaf: NONE,
            hint_slot: 0,
            hint_epoch: 0,
        }
    }

    /// Cursor from `key`: `inclusive` starts at the first pair whose
    /// key is `>= key` (any ref); otherwise strictly past every ref of
    /// `key` (a `begins_with`/resume upper-edge shape).
    pub fn from_key(key: &[u8], inclusive: bool) -> OrderedCursor {
        let mut cursor = OrderedCursor::from_start();
        cursor.key_buf.extend_from_slice(key);
        if inclusive {
            cursor.pos = CursorPos::AtOrAfter;
            cursor.entry_ref = 0;
        } else {
            cursor.pos = CursorPos::After;
            cursor.entry_ref = u64::MAX;
        }
        cursor
    }

    /// The next pair, or `None` past the end. A cursor at the end stays
    /// valid: pairs inserted later beyond the resume point will be
    /// returned by subsequent calls (the M1 SCAN posture).
    pub fn next<'c, S: KeyScheme, const F: usize>(
        &'c mut self,
        map: &OrderedMap<S, F>,
    ) -> Option<(&'c [u8], u64)> {
        let hint_ok =
            self.pos == CursorPos::After && self.hint_leaf != NONE && self.hint_epoch == map.epoch;
        let (leaf, slot) = if hint_ok { self.advance_hint(map) } else { self.seek_position(map) }?;
        self.entry_ref = map.emit_key(leaf, slot, &mut self.key_buf);
        self.pos = CursorPos::After;
        self.hint_leaf = leaf;
        self.hint_slot = slot as u32;
        self.hint_epoch = map.epoch;
        Some((self.key_buf.as_slice(), self.entry_ref))
    }

    /// Fast path: the tree is unmutated since the last return, so the
    /// cached (leaf, slot) is still exact — step within/into leaves.
    fn advance_hint<S: KeyScheme, const F: usize>(
        &self,
        map: &OrderedMap<S, F>,
    ) -> Option<(u32, usize)> {
        let leaf = map.leaves.get(self.hint_leaf);
        let next_slot = self.hint_slot as usize + 1;
        if next_slot < leaf.count as usize {
            return Some((self.hint_leaf, next_slot));
        }
        if leaf.next == NONE {
            return None;
        }
        Some((leaf.next, 0))
    }

    /// Slow path: re-seek from the owned resume pair (one descent).
    fn seek_position<S: KeyScheme, const F: usize>(
        &self,
        map: &OrderedMap<S, F>,
    ) -> Option<(u32, usize)> {
        match self.pos {
            CursorPos::Start => {
                let leaf = map.leftmost_leaf();
                (leaf != NONE).then_some((leaf, 0))
            }
            CursorPos::AtOrAfter => {
                let probe = make_probe::<S>(&self.key_buf);
                map.seek(&probe, self.entry_ref, true)
            }
            CursorPos::After => {
                let probe = make_probe::<S>(&self.key_buf);
                map.seek(&probe, self.entry_ref, false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    type FixedMap = OrderedMap<Fixed8, 16>;
    type VarMap = OrderedMap<VarKey, 16>;

    fn key8(v: u64) -> [u8; 8] {
        v.to_be_bytes()
    }

    #[test]
    fn fixed8_basics() {
        let mut map = FixedMap::new();
        assert!(map.insert(&key8(5), 1).unwrap());
        assert!(map.insert(&key8(5), 2).unwrap(), "same key, new ref is a new pair");
        assert!(!map.insert(&key8(5), 1).unwrap(), "exact pair is idempotent");
        assert!(map.contains(&key8(5), 1));
        assert!(!map.contains(&key8(5), 3));
        assert!(map.remove(&key8(5), 1));
        assert!(!map.remove(&key8(5), 1), "remove-if-present");
        assert_eq!(map.len(), 1);
        map.check_invariants();
    }

    #[test]
    fn var_key_order_is_memcmp() {
        // Keys engineered around the zero-padding edge: "a" < "a\0" <
        // "a\0\0" < "a\x01" < "ab" < 9-byte keys sharing a prefix.
        let keys: Vec<&[u8]> =
            vec![b"a", b"a\0", b"a\0\0", b"a\x01", b"ab", b"abcdefgh", b"abcdefgh\0", b"abcdefghi"];
        let mut map = VarMap::new();
        for (i, key) in keys.iter().enumerate() {
            assert!(map.insert(key, i as u64).unwrap());
        }
        let mut cursor = OrderedCursor::from_start();
        let mut got = Vec::new();
        while let Some((key, entry_ref)) = cursor.next(&map) {
            got.push((key.to_vec(), entry_ref));
        }
        let mut want: Vec<(Vec<u8>, u64)> =
            keys.iter().enumerate().map(|(i, key)| (key.to_vec(), i as u64)).collect();
        want.sort();
        assert_eq!(got, want);
        map.check_invariants();
    }

    #[test]
    fn split_merge_round_trip() {
        let mut map = FixedMap::new();
        let n = 10_000u64;
        for v in 0..n {
            assert!(map.insert(&key8(v.wrapping_mul(0x9E37_79B9_7F4A_7C15)), v).unwrap());
        }
        assert_eq!(map.len(), n);
        map.check_invariants();
        for v in 0..n {
            assert!(map.remove(&key8(v.wrapping_mul(0x9E37_79B9_7F4A_7C15)), v));
        }
        assert_eq!(map.len(), 0);
        assert!(map.is_empty());
        map.check_invariants();
        assert_eq!(map.memory().entries, 0);
    }

    #[test]
    fn sequential_fill_uses_rightmost_split() {
        let mut map = FixedMap::new();
        for v in 0..2_000u64 {
            map.insert(&key8(v), v).unwrap();
        }
        map.check_invariants();
        // Ascending inserts with the rightmost heuristic leave interior
        // leaves full: reserved bytes stay near the entry payload.
        let memory = map.memory();
        let per_entry = memory.total_bytes() as f64 / memory.entries as f64;
        assert!(per_entry < 40.0, "sequential fill per-entry {per_entry:.1} B");
    }

    #[test]
    fn cursor_resumes_across_mutations() {
        let mut map = FixedMap::new();
        for v in 0..500u64 {
            map.insert(&key8(v * 2), v).unwrap();
        }
        let mut cursor = OrderedCursor::from_start();
        let mut seen = Vec::new();
        for _ in 0..250 {
            let (key, _) = cursor.next(&map).unwrap();
            seen.push(u64::from_be_bytes(key.try_into().unwrap()));
        }
        // Mutate mid-scan: delete everything already seen, insert odds.
        for v in seen.clone() {
            assert!(map.remove(&key8(v), v / 2));
        }
        for v in 0..100u64 {
            map.insert(&key8(v * 2 + 1), 1_000 + v).unwrap();
        }
        while let Some((key, _)) = cursor.next(&map) {
            seen.push(u64::from_be_bytes(key.try_into().unwrap()));
        }
        // Every pair present for the whole scan (the surviving evens
        // past the resume point) was returned exactly once here.
        for survivor in (250..500u64).map(|v| v * 2) {
            assert_eq!(seen.iter().filter(|&&k| k == survivor).count(), 1, "key {survivor}");
        }
        map.check_invariants();
    }

    #[test]
    fn from_key_bounds() {
        let mut map = FixedMap::new();
        for v in [10u64, 20, 20, 30] {
            let next_ref = map.len();
            map.insert(&key8(v), next_ref).unwrap();
        }
        let mut cursor = OrderedCursor::from_key(&key8(20), true);
        let (key, _) = cursor.next(&map).unwrap();
        assert_eq!(u64::from_be_bytes(key.try_into().unwrap()), 20);
        let mut cursor = OrderedCursor::from_key(&key8(20), false);
        let (key, _) = cursor.next(&map).unwrap();
        assert_eq!(u64::from_be_bytes(key.try_into().unwrap()), 30);
    }

    #[derive(Clone, Debug)]
    enum Op {
        Insert(Vec<u8>, u64),
        Remove(Vec<u8>, u64),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        // Small key/ref spaces force duplicates, deep splits and merges.
        let key = prop::collection::vec(0u8..4, 0..12);
        let entry_ref = 0u64..4;
        prop_oneof![
            3 => (key.clone(), entry_ref.clone()).prop_map(|(k, r)| Op::Insert(k, r)),
            2 => (key, entry_ref).prop_map(|(k, r)| Op::Remove(k, r)),
        ]
    }

    /// One model step: mirror the op into a `BTreeSet` of pairs.
    fn apply_op(map: &mut VarMap, model: &mut BTreeSet<(Vec<u8>, u64)>, op: &Op) {
        match op {
            Op::Insert(key, entry_ref) => {
                let inserted = map.insert(key, *entry_ref).unwrap();
                assert_eq!(inserted, model.insert((key.clone(), *entry_ref)));
            }
            Op::Remove(key, entry_ref) => {
                let removed = map.remove(key, *entry_ref);
                assert_eq!(removed, model.remove(&(key.clone(), *entry_ref)));
            }
        }
    }

    fn full_scan(map: &VarMap) -> Vec<(Vec<u8>, u64)> {
        let mut cursor = OrderedCursor::from_start();
        let mut out = Vec::new();
        while let Some((key, entry_ref)) = cursor.next(map) {
            out.push((key.to_vec(), entry_ref));
        }
        out
    }

    proptest! {
        // The model equivalence AC: random op sequences, identical
        // contents AND iteration order vs the BTreeSet model. The
        // 10⁶-op storm variant lives in tests/ordered_storm.rs.
        #[test]
        fn matches_btree_model(ops in prop::collection::vec(op_strategy(), 1..400)) {
            let mut map = VarMap::new();
            let mut model = BTreeSet::new();
            for op in &ops {
                apply_op(&mut map, &mut model, op);
            }
            map.check_invariants();
            let scanned = full_scan(&map);
            let want: Vec<(Vec<u8>, u64)> = model.iter().cloned().collect();
            prop_assert_eq!(scanned, want);
        }

        // Cursor-under-mutation: pairs present for the WHOLE scan are
        // returned at least once regardless of interleaved mutations.
        // "Present throughout" = in the initial set and never named by
        // any Remove op (a removed-and-reinserted pair may legitimately
        // dodge the cursor).
        #[test]
        fn cursor_under_mutation_returns_survivors(
            initial in prop::collection::btree_set(
                (prop::collection::vec(0u8..4, 0..10), 0u64..4), 1..120),
            ops in prop::collection::vec(op_strategy(), 0..120),
            stride in 1usize..8,
        ) {
            let mut map = VarMap::new();
            let mut model: BTreeSet<(Vec<u8>, u64)> = initial.iter().cloned().collect();
            for (key, entry_ref) in &initial {
                map.insert(key, *entry_ref).unwrap();
            }
            let mut cursor = OrderedCursor::from_start();
            let mut returned = Vec::new();
            let mut op_iter = ops.iter();
            loop {
                let mut progressed = false;
                for _ in 0..stride {
                    if let Some((key, entry_ref)) = cursor.next(&map) {
                        returned.push((key.to_vec(), entry_ref));
                        progressed = true;
                    }
                }
                if let Some(op) = op_iter.next() {
                    apply_op(&mut map, &mut model, op);
                } else if !progressed {
                    break;
                }
            }
            let touched: BTreeSet<(Vec<u8>, u64)> = ops.iter().filter_map(|op| match op {
                Op::Remove(key, entry_ref) => Some((key.clone(), *entry_ref)),
                Op::Insert(..) => None,
            }).collect();
            for pair in initial.iter().filter(|p| !touched.contains(*p)) {
                prop_assert!(
                    returned.contains(pair),
                    "survivor pair {:?} missing from scan", pair
                );
            }
            map.check_invariants();
        }
    }
}
