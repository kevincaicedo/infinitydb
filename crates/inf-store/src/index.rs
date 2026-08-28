//! Index **v0** (M0-S14, master plan §7.3): per-cell open-addressing table,
//! Swiss-style — 1 control byte per slot (7-bit hash fragment, SIMD
//! group-probed 16 at a time via `inf-simd`) + an 8-byte slot
//! `{addr:48, fp:15, used:1}`. No keys, no TTLs, no values in the table:
//! key verification fetches the record (the store provides the comparison
//! closure), which the batch prefetch pipeline overlaps (L3/L4).
//!
//! Probing is hashbrown-shape: triangular group stride over a power-of-two
//! group count (visits every group). Deletion writes tombstones; tombstones
//! recycle on insert and are swept by rehash. Growth doubles capacity and
//! re-places every live slot via the caller's `hash_of` (the index stores
//! only 22 fingerprint bits, deliberately — §7.3 keeps it dense).
//!
//! **M4-S02 reinterpretation (§3.2):** the slot layout is frozen; what the
//! 48-bit field *means* is a property of the table's namespace mode —
//! [`MemoryMode`] tables store `ArenaAddr` (the M0/M1 path, byte-identical
//! by monomorphization), [`TieredMode`] tables store `LogicalAddr` into
//! the M4 address space. The discrimination is table-granular and
//! type-level: one table serves one namespace (M1), and the two meanings
//! can never coexist in one table by construction. Tiered tables carry a
//! full-hash sidecar (`Ext = u64`) because `grow` must re-place slots
//! whose records are cold — rehash-by-record-read would turn growth into
//! a disk storm (the §3.3 index-only rule); memory tables' sidecar is
//! zero-sized and every touch of it compiles away.
//!
//! M1 reserve: incremental split-order migration replaces the stop-and-copy
//! `grow` below; the `(live, tombstones, growth_left)` bookkeeping is
//! already per-table so the migration can move one group per MAINTAIN slice.

use inf_alloc::ArenaAddr;
use inf_foundation::LogicalAddr;
use inf_simd::{eq_mask16, high_bit_mask16, prefetch_read};

pub(crate) const GROUP: usize = 16;
const CTRL_EMPTY: u8 = 0x80;
const CTRL_TOMB: u8 = 0xFE;
/// Numerator of the maximum load factor (live + tombstones ≤ 85% of slots).
const LOAD_NUM: usize = 85;
const LOAD_DEN: usize = 100;

/// Table-granular meaning of the frozen 48-bit slot field (M4-S02).
/// Monomorphized — never a runtime branch: memory-mode tables compile to
/// the identical pre-M4 instruction path AND the identical struct layout
/// (`ExtStore = ()` occupies zero bytes, so field offsets do not move —
/// the S02 asm-diff artifact checks byte identity, not intent).
pub trait SlotMode {
    /// What the 48-bit slot payload denotes.
    type Addr: Copy + Eq;
    /// Per-slot sidecar storage. `()` for memory tables — zero bytes in
    /// the struct, every touch compiles away. A full-hash array for
    /// tiered tables: grow re-places cold-addressed slots from it, never
    /// from a record read (§3.3 index-only rule).
    type ExtStore;
    /// Sidecar bytes per slot (feeds `memory_bytes`, L5 — the tiered
    /// sidecar is attributed, not hidden).
    const EXT_BYTES_PER_SLOT: usize;
    fn addr_to_raw(addr: Self::Addr) -> u64;
    fn addr_from_raw(raw: u64) -> Self::Addr;
    fn ext_new(capacity: usize) -> Self::ExtStore;
    fn ext_set(store: &mut Self::ExtStore, pos: usize, hash: u64);
    /// The stored hash for `pos` (grow's re-placement source). Memory
    /// mode has none — callers pass the record-derived hash instead.
    fn ext_hash(store: &Self::ExtStore, pos: usize) -> u64;
    /// Whether the slot at `pos` may belong to `hash`, beyond the
    /// ctrl-tag filter — the exact-pair discipline `position_of` stands
    /// on (ADR-0057 D4, enforced since M4-S14). Tiered tables compare
    /// the full sidecar hash: addresses are per-life (§3.1), so after a
    /// recovery a `(tag, addr)` match alone can name a *different key's*
    /// slot — a displacement replay would then remove a live key
    /// (never-none violation, found by the m4-recovery sweep). Memory
    /// tables have no sidecar and no cross-life addresses (arena offsets
    /// are unique among live records), so tag + addr stays exact there
    /// and this compiles to `true` (the S02 identity untouched).
    fn ext_matches(store: &Self::ExtStore, pos: usize, hash: u64) -> bool;
}

/// Memory-mode tables: the 48-bit field is an [`ArenaAddr`] (M0 freeze).
pub struct MemoryMode;

impl SlotMode for MemoryMode {
    type Addr = ArenaAddr;
    type ExtStore = ();
    const EXT_BYTES_PER_SLOT: usize = 0;

    #[inline]
    fn addr_to_raw(addr: ArenaAddr) -> u64 {
        addr.to_raw()
    }

    #[inline]
    fn addr_from_raw(raw: u64) -> ArenaAddr {
        ArenaAddr::from_raw(raw).expect("slot addr is 48-bit by masking")
    }

    #[inline]
    fn ext_new(_capacity: usize) {}

    #[inline]
    fn ext_set(_store: &mut (), _pos: usize, _hash: u64) {}

    #[inline]
    fn ext_hash(_store: &(), _pos: usize) -> u64 {
        0 // never consulted: memory-mode grow hashes the record's key
    }

    #[inline]
    fn ext_matches(_store: &(), _pos: usize, _hash: u64) -> bool {
        true // no sidecar: tag + addr is already exact within one life
    }
}

/// Tiered-mode tables: the 48-bit field is a [`LogicalAddr`] into the
/// namespace's address space (M4-S01); the sidecar keeps the full key
/// hash so rehash never touches a record.
pub struct TieredMode;

impl SlotMode for TieredMode {
    type Addr = LogicalAddr;
    type ExtStore = Box<[u64]>;
    const EXT_BYTES_PER_SLOT: usize = size_of::<u64>();

    #[inline]
    fn addr_to_raw(addr: LogicalAddr) -> u64 {
        addr.to_raw()
    }

    #[inline]
    fn addr_from_raw(raw: u64) -> LogicalAddr {
        LogicalAddr::from_raw(raw).expect("slot addr is 48-bit by masking")
    }

    #[inline]
    fn ext_new(capacity: usize) -> Box<[u64]> {
        vec![0u64; capacity].into_boxed_slice()
    }

    #[inline]
    fn ext_set(store: &mut Box<[u64]>, pos: usize, hash: u64) {
        store[pos] = hash;
    }

    #[inline]
    fn ext_hash(store: &Box<[u64]>, pos: usize) -> u64 {
        store[pos]
    }

    #[inline]
    fn ext_matches(store: &Box<[u64]>, pos: usize, hash: u64) -> bool {
        store[pos] == hash
    }
}

/// 8-byte slot: `addr:48 | fp:15 | used:1` (frozen layout, §3.2).
#[derive(Copy, Clone, Default)]
struct Slot(u64);

impl Slot {
    const ADDR_MASK: u64 = (1 << 48) - 1;

    #[inline]
    fn new(addr_raw: u64, fp15: u16) -> Slot {
        debug_assert!(addr_raw <= Self::ADDR_MASK);
        debug_assert!(fp15 < (1 << 15));
        Slot(addr_raw | (u64::from(fp15) << 48) | (1 << 63))
    }

    #[inline]
    fn addr_raw(self) -> u64 {
        self.0 & Self::ADDR_MASK
    }

    #[inline]
    fn fp15(self) -> u16 {
        ((self.0 >> 48) & 0x7FFF) as u16
    }
}

/// Hash fragments: group index from the low bits, 7-bit control tag from the
/// top, 15-bit slot fingerprint from the bits between — disjoint, so the
/// effective filter is 22 bits before a record fetch.
#[inline]
fn h2(hash: u64) -> u8 {
    (hash >> 57) as u8 & 0x7F
}

#[inline]
fn fp15(hash: u64) -> u16 {
    ((hash >> 42) & 0x7FFF) as u16
}

/// Per-cell record index. Stores 48-bit addresses only; their meaning is
/// the table's [`SlotMode`].
pub struct Index<M: SlotMode = MemoryMode> {
    ctrl: Box<[u8]>,
    slots: Box<[Slot]>,
    /// Sidecar, parallel to `slots`. Zero bytes (no field, no code) for
    /// [`MemoryMode`]; the full key hashes for [`TieredMode`].
    ext: M::ExtStore,
    /// Power-of-two slot count; `capacity / 16` groups.
    capacity: usize,
    live: usize,
    tombstones: usize,
}

/// The position of a resumable home-group walk
/// ([`Index::home_group_cursor`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HomeGroupCursor {
    mask: usize,
    home: usize,
    group: usize,
    stride: usize,
}

impl HomeGroupCursor {
    /// The home group this cursor walks.
    #[must_use]
    pub fn home(&self) -> usize {
        self.home
    }
}

impl<M: SlotMode> Index<M> {
    /// A table that can hold `at_least` entries without growing.
    pub fn with_capacity(at_least: usize) -> Index<M> {
        let slots = (at_least.max(1) * LOAD_DEN).div_ceil(LOAD_NUM);
        let capacity = slots.next_power_of_two().max(GROUP);
        Index {
            ctrl: vec![CTRL_EMPTY; capacity].into_boxed_slice(),
            slots: vec![Slot::default(); capacity].into_boxed_slice(),
            ext: M::ext_new(capacity),
            capacity,
            live: 0,
            tombstones: 0,
        }
    }

    /// Live entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.live
    }

    /// True when empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Slot capacity (the table grows itself; this is for reporting).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Exact table footprint in bytes (feeds `index_bytes`, L5) — the
    /// tiered sidecar is attributed here, not hidden.
    #[inline]
    pub fn memory_bytes(&self) -> usize {
        self.capacity * (1 + size_of::<Slot>() + M::EXT_BYTES_PER_SLOT)
    }

    #[inline]
    fn group_mask(&self) -> usize {
        self.capacity / GROUP - 1
    }

    #[inline]
    fn ctrl_group(&self, group: usize) -> &[u8; 16] {
        self.ctrl[group * GROUP..group * GROUP + GROUP].try_into().expect("group-aligned")
    }

    /// Prefetch the probe path for `hash` — the batch pipeline calls this
    /// for every key in a parse batch before any `find` (L3/L4).
    #[inline]
    pub fn prefetch(&self, hash: u64) {
        let group = (hash as usize) & self.group_mask();
        prefetch_read(&raw const self.ctrl[group * GROUP]);
        prefetch_read((&raw const self.slots[group * GROUP]).cast());
    }

    /// Finds the address whose record matches, where `verify(addr)` performs
    /// the key comparison (fingerprint false positives reach it; full-key
    /// equality is the store's job).
    #[inline]
    pub fn find(&self, hash: u64, mut verify: impl FnMut(M::Addr) -> bool) -> Option<M::Addr> {
        let (tag, fp) = (h2(hash), fp15(hash));
        let mask = self.group_mask();
        let mut group = (hash as usize) & mask;
        let mut stride = 0;
        loop {
            let ctrl = self.ctrl_group(group);
            let mut candidates = eq_mask16(ctrl, tag);
            while candidates != 0 {
                let i = candidates.trailing_zeros() as usize;
                candidates &= candidates - 1;
                let slot = self.slots[group * GROUP + i];
                if slot.fp15() == fp {
                    let addr = M::addr_from_raw(slot.addr_raw());
                    if verify(addr) {
                        return Some(addr);
                    }
                }
            }
            // An EMPTY anywhere in the group terminates the probe chain
            // (tombstones do not — deleted slots were once links).
            if eq_mask16(ctrl, CTRL_EMPTY) != 0 {
                return None;
            }
            stride += 1;
            if stride > mask {
                return None; // every group visited (full-of-tombstones guard)
            }
            group = (group + stride) & mask;
        }
    }

    /// Visits every slotted address whose **full** hash equals `hash`
    /// (the tiered sidecar — `ext_matches`; on memory tables every
    /// tag-matching slot, which is the same filter `find` applies), in
    /// probe order, until the chain ends. The M4.5-S37 shadow probe
    /// (ADR-0093 D2): a cold candidate is a *shadow* only when its
    /// 64-bit hash equals the key's — a fingerprint-only match is another
    /// key and is left alone. Diagnostics-class cost (one chain walk);
    /// the eligible write pays it once, after `lookup` reported a cold
    /// candidate.
    pub fn each_exact(&self, hash: u64, mut visit: impl FnMut(M::Addr)) {
        let (tag, fp) = (h2(hash), fp15(hash));
        let mask = self.group_mask();
        let mut group = (hash as usize) & mask;
        let mut stride = 0;
        loop {
            let ctrl = self.ctrl_group(group);
            let mut candidates = eq_mask16(ctrl, tag);
            while candidates != 0 {
                let i = candidates.trailing_zeros() as usize;
                candidates &= candidates - 1;
                let pos = group * GROUP + i;
                let slot = self.slots[pos];
                if slot.fp15() == fp && M::ext_matches(&self.ext, pos, hash) {
                    visit(M::addr_from_raw(slot.addr_raw()));
                }
            }
            if eq_mask16(ctrl, CTRL_EMPTY) != 0 {
                return;
            }
            stride += 1;
            if stride > mask {
                return;
            }
            group = (group + stride) & mask;
        }
    }

    /// Diagnostics: groups visited until the probe for `hash` terminates
    /// (found-and-verified, or empty slot). Feeds the probe-length
    /// histogram artifact (M0-S14 AC) — not a hot-path API.
    pub fn probe_groups(&self, hash: u64, mut verify: impl FnMut(M::Addr) -> bool) -> usize {
        let (tag, fp) = (h2(hash), fp15(hash));
        let mask = self.group_mask();
        let mut group = (hash as usize) & mask;
        let mut stride = 0;
        let mut visited = 1;
        loop {
            let ctrl = self.ctrl_group(group);
            let mut candidates = eq_mask16(ctrl, tag);
            while candidates != 0 {
                let i = candidates.trailing_zeros() as usize;
                candidates &= candidates - 1;
                let slot = self.slots[group * GROUP + i];
                if slot.fp15() == fp && verify(M::addr_from_raw(slot.addr_raw())) {
                    return visited;
                }
            }
            if eq_mask16(ctrl, CTRL_EMPTY) != 0 || stride >= mask {
                return visited;
            }
            stride += 1;
            visited += 1;
            group = (group + stride) & mask;
        }
    }

    /// True when the next insert must be preceded by [`grow`](Self::grow).
    /// Split out (rather than growing inside `insert`) because re-placement
    /// needs the caller's `hash_of` — the table doesn't store full hashes
    /// (memory mode) or wants the caller's say (tiered).
    #[inline]
    pub fn needs_grow(&self) -> bool {
        (self.live + self.tombstones + 1) * LOAD_DEN > self.capacity * LOAD_NUM
    }

    /// Inserts `addr` under `hash`. Precondition: the key is absent (the
    /// caller always `find`s first) and `needs_grow()` is false.
    pub fn insert(&mut self, hash: u64, addr: M::Addr) {
        debug_assert!(!self.needs_grow(), "caller must grow first");
        let mask = self.group_mask();
        let mut group = (hash as usize) & mask;
        let mut stride = 0;
        loop {
            // First special slot (empty or tombstone) in probe order.
            let specials = high_bit_mask16(self.ctrl_group(group));
            if specials != 0 {
                let i = specials.trailing_zeros() as usize;
                let pos = group * GROUP + i;
                if self.ctrl[pos] == CTRL_TOMB {
                    self.tombstones -= 1;
                }
                self.ctrl[pos] = h2(hash);
                self.slots[pos] = Slot::new(M::addr_to_raw(addr), fp15(hash));
                M::ext_set(&mut self.ext, pos, hash);
                self.live += 1;
                return;
            }
            stride += 1;
            assert!(stride <= mask, "insert found no slot — load invariant broken");
            group = (group + stride) & mask;
        }
    }

    /// Swaps the address stored for an existing entry (same key, record
    /// moved by an update). Panics if `(hash, old)` is not present — that is
    /// an index/store desync.
    pub fn replace(&mut self, hash: u64, old: M::Addr, new: M::Addr) {
        let pos = self.position_of(hash, old).expect("replace target present");
        self.slots[pos] = Slot::new(M::addr_to_raw(new), fp15(hash));
    }

    /// True when the exact `(hash, addr)` pair is slotted — the M4-S12
    /// ref-apply idempotency probe (ADR-0057 D4: the walker's at-least-
    /// once re-emission may duplicate a ref; a duplicated slot would
    /// outlive the single displacement removal and serve stale bytes).
    #[must_use]
    pub fn contains_pair(&self, hash: u64, addr: M::Addr) -> bool {
        self.position_of(hash, addr).is_some()
    }

    /// Removes the entry holding `addr` if present; false when absent.
    /// The M4-S12 displacement-replay primitive (ADR-0057 D4): a
    /// `ColdDisplace` names an old-life address that may or may not have
    /// been re-slotted by a checkpoint ref — absence is a legal
    /// interleaving (walked-late image), never a desync.
    pub fn remove_if_present(&mut self, hash: u64, addr: M::Addr) -> bool {
        let Some(pos) = self.position_of(hash, addr) else { return false };
        self.remove_at(pos);
        true
    }

    /// Removes the entry holding `addr`. Panics if absent (desync).
    pub fn remove(&mut self, hash: u64, addr: M::Addr) {
        let pos = self.position_of(hash, addr).expect("remove target present");
        self.remove_at(pos);
    }

    fn remove_at(&mut self, pos: usize) {
        // If the slot's group has an empty, no probe chain passes THROUGH
        // this group — the slot can return to EMPTY instead of tombstoning.
        let group = pos / GROUP;
        let has_empty = eq_mask16(self.ctrl_group(group), CTRL_EMPTY) != 0;
        if has_empty {
            self.ctrl[pos] = CTRL_EMPTY;
        } else {
            self.ctrl[pos] = CTRL_TOMB;
            self.tombstones += 1;
        }
        self.slots[pos] = Slot::default();
        self.live -= 1;
    }

    /// Home-group scan emitting `(addr, full hash)` from the sidecar —
    /// the M4-S12 checkpoint walker's enumeration primitive (ADR-0057
    /// D1): identical guarantee to [`scan_home_group`]
    /// (Self::scan_home_group), but no closure ever touches a record, so
    /// the cold majority walks at index speed with zero cold reads. Only
    /// meaningful for modes with a real sidecar ([`TieredMode`]); memory
    /// tables keep the record-hashing walk.
    pub fn scan_home_group_ext(&self, group: usize, mut emit: impl FnMut(M::Addr, u64)) {
        let mask = self.group_mask();
        let home = group & mask;
        let mut group = home;
        let mut stride = 0;
        loop {
            let ctrl = self.ctrl_group(group);
            for (i, &c) in ctrl.iter().enumerate() {
                if c & 0x80 == 0 {
                    let pos = group * GROUP + i;
                    let hash = M::ext_hash(&self.ext, pos);
                    if (hash as usize) & mask == home {
                        emit(M::addr_from_raw(self.slots[pos].addr_raw()), hash);
                    }
                }
            }
            if eq_mask16(ctrl, CTRL_EMPTY) != 0 {
                return;
            }
            stride += 1;
            if stride > mask {
                return;
            }
            group = (group + stride) & mask;
        }
    }

    /// A resumable home-group walk positioned at `group`'s home (M4.5-S37,
    /// ADR-0093 A4′): [`scan_home_group_step`](Self::scan_home_group_step)
    /// visits the chain one 16-slot probe group at a time, so a caller
    /// that mutates between steps holds a scratch bounded by the group,
    /// never by the chain.
    #[must_use]
    pub fn home_group_cursor(&self, group: usize) -> HomeGroupCursor {
        let mask = self.group_mask();
        let home = group & mask;
        HomeGroupCursor { mask, home, group: home, stride: 0 }
    }

    /// One step of a home-group walk: emits `(addr, full hash)` for the
    /// live entries of the cursor's current probe group whose home is
    /// the cursor's, then advances it; `false` once the chain has ended
    /// (this was its last group). Same guarantee as
    /// [`scan_home_group_ext`](Self::scan_home_group_ext) taken group by
    /// group. Valid across removals between steps: `remove` writes EMPTY
    /// only into groups that already hold one, so no chain shortens
    /// under a cursor — and never across growth (debug-asserted: the
    /// cursor carries the mask it was cut for).
    pub fn scan_home_group_step(
        &self,
        cursor: &mut HomeGroupCursor,
        mut emit: impl FnMut(M::Addr, u64),
    ) -> bool {
        let mask = self.group_mask();
        debug_assert_eq!(cursor.mask, mask, "a home-group cursor outlived a grow");
        let ctrl = self.ctrl_group(cursor.group);
        for (i, &c) in ctrl.iter().enumerate() {
            if c & 0x80 == 0 {
                let pos = cursor.group * GROUP + i;
                let hash = M::ext_hash(&self.ext, pos);
                if (hash as usize) & mask == cursor.home {
                    emit(M::addr_from_raw(self.slots[pos].addr_raw()), hash);
                }
            }
        }
        if eq_mask16(ctrl, CTRL_EMPTY) != 0 {
            return false;
        }
        cursor.stride += 1;
        if cursor.stride > mask {
            return false;
        }
        cursor.group = (cursor.group + cursor.stride) & mask;
        true
    }

    fn position_of(&self, hash: u64, addr: M::Addr) -> Option<usize> {
        let tag = h2(hash);
        let raw = M::addr_to_raw(addr);
        let mask = self.group_mask();
        let mut group = (hash as usize) & mask;
        let mut stride = 0;
        loop {
            let ctrl = self.ctrl_group(group);
            let mut candidates = eq_mask16(ctrl, tag);
            while candidates != 0 {
                let i = candidates.trailing_zeros() as usize;
                candidates &= candidates - 1;
                let pos = group * GROUP + i;
                if self.slots[pos].addr_raw() == raw && M::ext_matches(&self.ext, pos, hash) {
                    return Some(pos);
                }
            }
            if eq_mask16(ctrl, CTRL_EMPTY) != 0 {
                return None;
            }
            stride += 1;
            if stride > mask {
                return None;
            }
            group = (group + stride) & mask;
        }
    }

    /// Probe groups in the table (the SCAN cursor space — one cursor value
    /// per home group).
    #[inline]
    pub fn group_count(&self) -> usize {
        self.capacity / GROUP
    }

    /// Home-group scan (M1-S02): emits every live address whose **home**
    /// group (the group its full hash maps to, via `hash_of`) equals
    /// `group`, by walking the probe chain reachable from it. Triangular
    /// probing displaces entries off their home group, but every displaced
    /// entry lives within its home's find-chain (groups visited until the
    /// first one holding an EMPTY slot — `remove` only writes EMPTY into
    /// groups that already have one, so chains never shorten), which makes
    /// home-group enumeration exhaustive.
    ///
    /// Combined with reverse-binary cursor increments this gives the SCAN
    /// guarantee across doubling growth: a home group `g` at capacity `C`
    /// splits into exactly `{g, g + C/16}` at `2C`.
    pub fn scan_home_group(
        &self,
        group: usize,
        mut hash_of: impl FnMut(M::Addr) -> u64,
        mut emit: impl FnMut(M::Addr),
    ) {
        let mask = self.group_mask();
        let home = group & mask;
        let mut group = home;
        let mut stride = 0;
        loop {
            let ctrl = self.ctrl_group(group);
            for (i, &c) in ctrl.iter().enumerate() {
                if c & 0x80 == 0 {
                    let addr = M::addr_from_raw(self.slots[group * GROUP + i].addr_raw());
                    if (hash_of(addr) as usize) & mask == home {
                        emit(addr);
                    }
                }
            }
            if eq_mask16(ctrl, CTRL_EMPTY) != 0 {
                return;
            }
            stride += 1;
            if stride > mask {
                return;
            }
            group = (group + stride) & mask;
        }
    }

    /// Bounded clock-hand walk (M1-S06): emits every live address in the
    /// slot window `[start_slot, start_slot + max_slots)` (wrapping) and
    /// returns the next hand position. The eviction sweep owns the budget;
    /// the index only iterates — candidate policy lives in `evict.rs`
    /// (the §3.2 candidate-iterator seam).
    pub fn live_walk(
        &self,
        start_slot: usize,
        max_slots: usize,
        mut emit: impl FnMut(M::Addr),
    ) -> usize {
        let mask = self.capacity - 1;
        let start = start_slot & mask;
        let span = max_slots.min(self.capacity);
        for i in 0..span {
            let pos = (start + i) & mask;
            if self.ctrl[pos] & 0x80 == 0 {
                emit(M::addr_from_raw(self.slots[pos].addr_raw()));
            }
        }
        (start + span) & mask
    }

    /// First live address at or after `start_slot` (wrapping) — the
    /// RANDOMKEY probe (two-level random is the documented deviation; the
    /// caller rolls the slot).
    pub fn live_from(&self, start_slot: usize) -> Option<M::Addr> {
        if self.live == 0 {
            return None;
        }
        let start = start_slot & (self.capacity - 1);
        let (tail, head) = (&self.ctrl[start..], &self.ctrl[..start]);
        for (i, &c) in tail.iter().chain(head.iter()).enumerate() {
            if c & 0x80 == 0 {
                let slot = self.slots[(start + i) & (self.capacity - 1)];
                return Some(M::addr_from_raw(slot.addr_raw()));
            }
        }
        None
    }

    /// Doubles capacity (also sweeping tombstones), re-placing every live
    /// address via `hash_of(addr, stored_hash)` — memory-mode stores hash
    /// the record's key (`stored_hash` is 0, there is no sidecar); tiered
    /// tables return the sidecar hash and never touch a record (a
    /// cold-address record read inside grow would be a disk walk — the
    /// §3.3 index-only rule).
    /// M0 is stop-and-copy; M1 replaces this with split-order increments.
    pub fn grow(&mut self, mut hash_of: impl FnMut(M::Addr, u64) -> u64) {
        // Tombstone-heavy tables rehash at the same size (recycle), others double.
        let new_capacity =
            if self.tombstones >= self.live { self.capacity } else { self.capacity * 2 };
        let mut next = Index {
            ctrl: vec![CTRL_EMPTY; new_capacity].into_boxed_slice(),
            slots: vec![Slot::default(); new_capacity].into_boxed_slice(),
            ext: M::ext_new(new_capacity),
            capacity: new_capacity,
            live: 0,
            tombstones: 0,
        };
        for pos in 0..self.capacity {
            if self.ctrl[pos] & 0x80 == 0 {
                let addr = M::addr_from_raw(self.slots[pos].addr_raw());
                next.insert(hash_of(addr, M::ext_hash(&self.ext, pos)), addr);
            }
        }
        *self = next;
    }
}

impl<M: SlotMode> core::fmt::Debug for Index<M> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Index")
            .field("live", &self.live)
            .field("tombstones", &self.tombstones)
            .field("capacity", &self.capacity)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inf_foundation::hash64;
    use std::collections::HashMap;

    /// Test rig: "records" are entries in a Vec; ArenaAddr = index into it.
    struct Rig {
        keys: Vec<Vec<u8>>,
        index: Index,
    }

    impl Rig {
        fn new() -> Rig {
            Rig { keys: Vec::new(), index: Index::with_capacity(4) }
        }

        fn hash(key: &[u8]) -> u64 {
            hash64(key, 0xC0FFEE)
        }

        fn addr(i: usize) -> ArenaAddr {
            ArenaAddr::from_raw(i as u64).expect("small")
        }

        fn get(&self, key: &[u8]) -> Option<u64> {
            self.index
                .find(Self::hash(key), |a| self.keys[a.to_raw() as usize] == key)
                .map(|a| a.to_raw())
        }

        fn upsert(&mut self, key: &[u8]) -> u64 {
            let hash = Self::hash(key);
            let keys = &self.keys;
            if let Some(old) = self.index.find(hash, |a| keys[a.to_raw() as usize] == key) {
                return old.to_raw();
            }
            if self.index.needs_grow() {
                let keys = &self.keys;
                self.index.grow(|a, _| Self::hash(&keys[a.to_raw() as usize]));
            }
            self.keys.push(key.to_vec());
            let addr = Self::addr(self.keys.len() - 1);
            self.index.insert(hash, addr);
            addr.to_raw()
        }

        fn remove(&mut self, key: &[u8]) -> bool {
            let hash = Self::hash(key);
            let keys = &self.keys;
            match self.index.find(hash, |a| keys[a.to_raw() as usize] == key) {
                Some(addr) => {
                    self.index.remove(hash, addr);
                    true
                }
                None => false,
            }
        }
    }

    /// Tiered-mode rig (M4-S02): addresses are `LogicalAddr`s into a Vec;
    /// `grow` re-places from the hash sidecar — the rig can PROVE no
    /// record was read by never providing record access to the closure.
    struct TieredRig {
        keys: Vec<Vec<u8>>,
        index: Index<TieredMode>,
    }

    impl TieredRig {
        fn new() -> TieredRig {
            TieredRig { keys: Vec::new(), index: Index::with_capacity(4) }
        }

        fn get(&self, key: &[u8]) -> Option<u64> {
            self.index
                .find(Rig::hash(key), |a| self.keys[a.to_raw() as usize] == key)
                .map(|a| a.to_raw())
        }

        fn upsert(&mut self, key: &[u8]) -> u64 {
            let hash = Rig::hash(key);
            let keys = &self.keys;
            if let Some(old) = self.index.find(hash, |a| keys[a.to_raw() as usize] == key) {
                return old.to_raw();
            }
            if self.index.needs_grow() {
                // The sidecar is the only hash source: no key access here.
                self.index.grow(|_, ext| ext);
            }
            self.keys.push(key.to_vec());
            let addr = LogicalAddr::from_raw(self.keys.len() as u64 - 1).expect("small");
            self.index.insert(hash, addr);
            addr.to_raw()
        }

        fn remove(&mut self, key: &[u8]) -> bool {
            let hash = Rig::hash(key);
            let keys = &self.keys;
            match self.index.find(hash, |a| keys[a.to_raw() as usize] == key) {
                Some(addr) => {
                    self.index.remove(hash, addr);
                    true
                }
                None => false,
            }
        }
    }

    #[test]
    fn insert_find_remove_basics() {
        let mut rig = Rig::new();
        assert_eq!(rig.get(b"k1"), None);
        let a = rig.upsert(b"k1");
        assert_eq!(rig.get(b"k1"), Some(a));
        assert_eq!(rig.upsert(b"k1"), a, "upsert of present key is a find");
        assert!(rig.remove(b"k1"));
        assert_eq!(rig.get(b"k1"), None);
        assert!(!rig.remove(b"k1"));
        assert!(rig.index.is_empty());
    }

    #[test]
    fn replace_swaps_address_in_place() {
        let mut rig = Rig::new();
        rig.upsert(b"key");
        let hash = Rig::hash(b"key");
        rig.keys.push(b"key".to_vec()); // the "moved record"
        let new_addr = Rig::addr(rig.keys.len() - 1);
        rig.index.replace(hash, Rig::addr(0), new_addr);
        assert_eq!(rig.get(b"key"), Some(new_addr.to_raw()));
        assert_eq!(rig.index.len(), 1);
    }

    /// M0-S14 AC shape: random op sequence vs a HashMap oracle.
    #[test]
    fn storm_matches_hashmap_oracle() {
        let ops: usize = if cfg!(miri) { 2_000 } else { 100_000 };
        let mut rig = Rig::new();
        let mut oracle: HashMap<Vec<u8>, u64> = HashMap::new();
        let mut x: u64 = 0x1234_5678_9ABC_DEF1;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for op in 0..ops {
            let key = format!("key:{}", rand() % 512).into_bytes();
            match rand() % 3 {
                0 => {
                    let got = rig.upsert(&key);
                    let want = *oracle.entry(key.clone()).or_insert(got);
                    assert_eq!(got, want, "op {op}: upsert disagreed");
                }
                1 => {
                    let got = rig.remove(&key);
                    let want = oracle.remove(&key).is_some();
                    assert_eq!(got, want, "op {op}: remove disagreed");
                }
                _ => {
                    let got = rig.get(&key);
                    let want = oracle.get(&key).copied();
                    assert_eq!(got, want, "op {op}: get disagreed");
                }
            }
            assert_eq!(rig.index.len(), oracle.len(), "op {op}: len drift");
        }
        for (key, want) in &oracle {
            assert_eq!(rig.get(key), Some(*want), "final sweep");
        }
    }

    /// M4-S02: the same storm through a tiered-mode table — growth and
    /// tombstone recycling re-place from the sidecar, zero record reads
    /// (structural: the grow closure receives no record access).
    #[test]
    fn tiered_storm_matches_hashmap_oracle() {
        let ops: usize = if cfg!(miri) { 2_000 } else { 100_000 };
        let mut rig = TieredRig::new();
        let mut oracle: HashMap<Vec<u8>, u64> = HashMap::new();
        let mut x: u64 = 0x0FEE_D5EE_D000_0001;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for op in 0..ops {
            let key = format!("key:{}", rand() % 512).into_bytes();
            match rand() % 3 {
                0 => {
                    let got = rig.upsert(&key);
                    let want = *oracle.entry(key.clone()).or_insert(got);
                    assert_eq!(got, want, "op {op}: upsert disagreed");
                }
                1 => {
                    let got = rig.remove(&key);
                    let want = oracle.remove(&key).is_some();
                    assert_eq!(got, want, "op {op}: remove disagreed");
                }
                _ => {
                    let got = rig.get(&key);
                    let want = oracle.get(&key).copied();
                    assert_eq!(got, want, "op {op}: get disagreed");
                }
            }
            assert_eq!(rig.index.len(), oracle.len(), "op {op}: len drift");
        }
        for (key, want) in &oracle {
            assert_eq!(rig.get(key), Some(*want), "final sweep");
        }
    }

    #[test]
    fn tombstone_recycling_bounds_capacity() {
        let mut rig = Rig::new();
        // Insert/delete cycles over a fixed working set must not grow the
        // table unboundedly: rehash-in-place recycles tombstones.
        for round in 0..200 {
            for i in 0..64 {
                rig.upsert(format!("cycle:{i}").as_bytes());
            }
            for i in 0..64 {
                assert!(rig.remove(format!("cycle:{i}").as_bytes()), "round {round}");
            }
        }
        assert!(rig.index.capacity() <= 1024, "capacity ballooned: {:?}", rig.index);
    }

    #[test]
    fn tiered_tombstone_recycling_bounds_capacity() {
        let mut rig = TieredRig::new();
        for round in 0..200 {
            for i in 0..64 {
                rig.upsert(format!("cycle:{i}").as_bytes());
            }
            for i in 0..64 {
                assert!(rig.remove(format!("cycle:{i}").as_bytes()), "round {round}");
            }
        }
        assert!(rig.index.capacity() <= 1024, "capacity ballooned: {:?}", rig.index);
    }

    #[test]
    fn growth_keeps_every_key_findable() {
        let mut rig = Rig::new();
        let n = if cfg!(miri) { 300 } else { 50_000 };
        let mut addrs = Vec::new();
        for i in 0..n {
            addrs.push(rig.upsert(format!("grow:{i}").as_bytes()));
        }
        for (i, want) in addrs.iter().enumerate() {
            assert_eq!(rig.get(format!("grow:{i}").as_bytes()), Some(*want));
        }
        // Load factor honored after growth churn.
        assert!(rig.index.len() * 100 <= rig.index.capacity() * 85);
    }

    #[test]
    fn memory_bytes_is_nine_per_slot() {
        let index: Index = Index::with_capacity(10_000);
        assert_eq!(index.memory_bytes(), index.capacity() * 9);
    }

    #[test]
    fn tiered_sidecar_is_attributed_seventeen_per_slot() {
        let index: Index<TieredMode> = Index::with_capacity(10_000);
        assert_eq!(index.memory_bytes(), index.capacity() * 17);
    }
}
