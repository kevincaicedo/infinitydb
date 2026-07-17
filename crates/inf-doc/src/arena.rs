//! Arena form: the RAM node tree for large/edit-hot documents (ADR-0036
//! D7). Nodes live in the per-cell document arena (`inf-alloc`), children
//! are 8-byte `DocRef`s — node surgery never moves siblings or children.
//! This form is a **projection**: it never crosses a durability or fabric
//! boundary; `freeze()` re-derives canonical tape bytes on demand, and
//! `freeze(morph(t)) == t` byte-exact is the contract checkpoints and M4
//! demotion stand on (proptest + fuzz enforced).
//!
//! Accounting is exact and cheap (L5): every alloc/free/grow updates the
//! owning document's `{node_bytes, slack_bytes}` at the call site — the
//! S19 `doc_arena_bytes`/`doc_slack_bytes` feeds.

use inf_alloc::arena::{Arena, ArenaAddr};

use crate::apply::{ApplyError, ApplyOp, Number, ScalarPatch, number_op};
use crate::build::{BFrame, TapeBuilder};
use crate::error::DocError;
use crate::path::{PathProgram, SimpleStep};
use crate::tape::{self, DocStr, TapeDoc};

const TAG_SHIFT: u32 = 60;
const PAYLOAD_MASK: u64 = (1 << TAG_SHIFT) - 1;
const ADDR_MASK: u64 = (1 << 48) - 1;
/// Inline i60 range: wider integers spill to an 8-byte heap cell.
const INLINE_INT_MIN: i64 = -(1 << 59);
const INLINE_INT_MAX: i64 = (1 << 59) - 1;

const NODE_HDR: usize = 8; // count: u32 | cap: u32
const OBJ_ENTRY: usize = 16; // key: DocRef | val: DocRef
const ARR_SLOT: usize = 8; // DocRef
const STR_HDR: usize = 4; // len: u32
const NUM_CELL: usize = 8;

/// Value kind carried in a `DocRef`'s top nibble. RAM-only tag space,
/// frozen by ADR-0036 (the M5 collections precedent).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum RefTag {
    Null = 0,
    False = 1,
    True = 2,
    IntInline = 3,
    IntHeap = 4,
    F64 = 5,
    Str = 6,
    Obj = 7,
    Arr = 8,
}

/// 8-byte tagged reference: tag in bits 63..60, payload in bits 59..0 —
/// an i60 immediate (`IntInline`) or a 48-bit `ArenaAddr` (upper payload
/// bits zero, asserted). Explicit bitfields, no NaN-boxing (D7).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct DocRef(u64);

impl DocRef {
    pub const NULL: DocRef = DocRef((RefTag::Null as u64) << TAG_SHIFT);

    #[inline]
    pub fn bool_ref(v: bool) -> DocRef {
        let tag = if v { RefTag::True } else { RefTag::False };
        DocRef((tag as u64) << TAG_SHIFT)
    }

    fn with_addr(tag: RefTag, addr: ArenaAddr) -> DocRef {
        let raw = addr.to_raw();
        debug_assert_eq!(raw & !ADDR_MASK, 0, "ArenaAddr is 48-bit");
        DocRef(((tag as u64) << TAG_SHIFT) | raw)
    }

    fn inline_int(v: i64) -> Option<DocRef> {
        if (INLINE_INT_MIN..=INLINE_INT_MAX).contains(&v) {
            Some(DocRef(((RefTag::IntInline as u64) << TAG_SHIFT) | ((v as u64) & PAYLOAD_MASK)))
        } else {
            None
        }
    }

    /// The raw 64-bit encoding — what a store record's tree handle carries
    /// (ADR-0037 D1). Round-trips through [`DocRef::from_raw`].
    #[inline]
    pub fn to_raw(self) -> u64 {
        self.0
    }

    /// Rebuilds a ref from its raw encoding, refusing malformed bits: an
    /// unknown tag nibble, a nonzero payload on a payload-less tag, or
    /// address bits above 48 on an address-carrying tag. The store's
    /// handle decode is the only caller; a `None` there is an
    /// arena-lifecycle bug surfaced as a typed condition, never a
    /// misread tree.
    pub fn from_raw(raw: u64) -> Option<DocRef> {
        let payload = raw & PAYLOAD_MASK;
        match raw >> TAG_SHIFT {
            t if t == RefTag::Null as u64
                || t == RefTag::False as u64
                || t == RefTag::True as u64 =>
            {
                (payload == 0).then_some(DocRef(raw))
            }
            t if t == RefTag::IntInline as u64 => Some(DocRef(raw)),
            t if t <= RefTag::Arr as u64 => (payload & !ADDR_MASK == 0).then_some(DocRef(raw)),
            _ => None,
        }
    }

    #[inline]
    pub fn tag(self) -> RefTag {
        match self.0 >> TAG_SHIFT {
            0 => RefTag::Null,
            1 => RefTag::False,
            2 => RefTag::True,
            3 => RefTag::IntInline,
            4 => RefTag::IntHeap,
            5 => RefTag::F64,
            6 => RefTag::Str,
            7 => RefTag::Obj,
            8 => RefTag::Arr,
            t => unreachable!("invalid DocRef tag {t} — refs are constructed, never decoded"),
        }
    }

    #[inline]
    fn addr(self) -> ArenaAddr {
        debug_assert!(!matches!(
            self.tag(),
            RefTag::Null | RefTag::False | RefTag::True | RefTag::IntInline
        ));
        ArenaAddr::from_raw(self.0 & ADDR_MASK).expect("payload holds a 48-bit address")
    }

    #[inline]
    fn as_inline_int(self) -> i64 {
        debug_assert_eq!(self.tag(), RefTag::IntInline);
        // Sign-extend the i60 payload.
        ((self.0 << 4) as i64) >> 4
    }
}

/// Per-document memory attribution (the S19 domain feeds).
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub struct DocMemReport {
    /// Requested bytes across all cells and nodes of this document.
    pub node_bytes: usize,
    /// Reserved-but-unused capacity bytes (array/object growth slack).
    pub slack_bytes: usize,
}

/// Recycled buffers for canonical arena-tree freeze. Checkpoint walkers
/// retain one per store so document-heavy walks do not allocate per entry.
#[derive(Default)]
pub struct FreezeScratch {
    out: Vec<u8>,
    builder_stack: Vec<BFrame>,
    walk_stack: Vec<FreezeFrame>,
}

impl FreezeScratch {
    /// Retained heap bytes owned by this reusable checkpoint/freeze scratch.
    /// The buffers are per store and therefore part of the document memory
    /// domain even while they are empty (L5).
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.out.capacity()
            + self.builder_stack.capacity() * size_of::<BFrame>()
            + self.walk_stack.capacity() * size_of::<FreezeFrame>()
    }
}

/// An arena-form document: a root ref plus exact accounting. All
/// operations take the owning cell's arena explicitly — the store owns
/// the arena (S03); this type owns the tree and its books.
#[derive(Debug)]
pub struct ArenaDoc {
    root: DocRef,
    mem: DocMemReport,
}

// ---- raw node access ------------------------------------------------------

fn read_header(arena: &Arena, addr: ArenaAddr) -> (u32, u32) {
    let b = arena.bytes(addr, NODE_HDR);
    let count = u32::from_le_bytes(b[0..4].try_into().expect("4-byte field"));
    let cap = u32::from_le_bytes(b[4..8].try_into().expect("4-byte field"));
    debug_assert!(count <= cap);
    (count, cap)
}

fn node_len(cap: u32, slot: usize) -> usize {
    NODE_HDR + cap as usize * slot
}

fn read_ref_at(arena: &Arena, addr: ArenaAddr, node_bytes: usize, offset: usize) -> DocRef {
    let b = arena.bytes(addr, node_bytes);
    DocRef(u64::from_le_bytes(b[offset..offset + 8].try_into().expect("8-byte slot")))
}

fn str_bytes(arena: &Arena, r: DocRef) -> &[u8] {
    debug_assert_eq!(r.tag(), RefTag::Str);
    let addr = r.addr();
    let len_bytes = arena.bytes(addr, STR_HDR);
    let len = u32::from_le_bytes(len_bytes[0..4].try_into().expect("4-byte field")) as usize;
    &arena.bytes(addr, STR_HDR + len)[STR_HDR..]
}

fn num_bits(arena: &Arena, r: DocRef) -> u64 {
    let b = arena.bytes(r.addr(), NUM_CELL);
    u64::from_le_bytes(b[0..8].try_into().expect("8-byte cell"))
}

// ---- allocation helpers (every one accounts at the call site) --------------

fn alloc_str(arena: &mut Arena, s: &[u8], mem: &mut DocMemReport) -> Result<DocRef, DocError> {
    let len = STR_HDR + s.len();
    let addr = arena.alloc(len).ok_or(DocError::ArenaExhausted)?;
    let cell = arena.bytes_mut(addr, len);
    cell[0..4].copy_from_slice(&(s.len() as u32).to_le_bytes());
    cell[4..].copy_from_slice(s);
    mem.node_bytes += len;
    Ok(DocRef::with_addr(RefTag::Str, addr))
}

fn alloc_num_cell(
    arena: &mut Arena,
    bits: u64,
    tag: RefTag,
    mem: &mut DocMemReport,
) -> Result<DocRef, DocError> {
    let addr = arena.alloc(NUM_CELL).ok_or(DocError::ArenaExhausted)?;
    arena.bytes_mut(addr, NUM_CELL).copy_from_slice(&bits.to_le_bytes());
    mem.node_bytes += NUM_CELL;
    Ok(DocRef::with_addr(tag, addr))
}

fn alloc_int(arena: &mut Arena, v: i64, mem: &mut DocMemReport) -> Result<DocRef, DocError> {
    match DocRef::inline_int(v) {
        Some(r) => Ok(r),
        None => alloc_num_cell(arena, v as u64, RefTag::IntHeap, mem),
    }
}

/// Allocate a container node from collected child refs, `cap == count`
/// (zero slack at build — slack appears only through growth, D7).
fn alloc_node(
    arena: &mut Arena,
    tag: RefTag,
    slot: usize,
    refs: &[u64],
    mem: &mut DocMemReport,
) -> Result<DocRef, DocError> {
    debug_assert!(matches!(tag, RefTag::Obj | RefTag::Arr));
    let count = (if slot == OBJ_ENTRY { refs.len() / 2 } else { refs.len() }) as u32;
    let len = NODE_HDR + refs.len() * 8;
    debug_assert_eq!(len, node_len(count, slot));
    let addr = arena.alloc(len).ok_or(DocError::ArenaExhausted)?;
    let node = arena.bytes_mut(addr, len);
    node[0..4].copy_from_slice(&count.to_le_bytes());
    node[4..8].copy_from_slice(&count.to_le_bytes());
    for (i, r) in refs.iter().enumerate() {
        node[NODE_HDR + i * 8..NODE_HDR + i * 8 + 8].copy_from_slice(&r.to_le_bytes());
    }
    mem.node_bytes += len;
    Ok(DocRef::with_addr(tag, addr))
}

/// Release one subtree, updating `mem` downward. Iterative — children are
/// extracted before their node frees.
fn free_ref(arena: &mut Arena, root: DocRef, mem: &mut DocMemReport) {
    let mut stack: Vec<DocRef> = vec![root];
    while let Some(r) = stack.pop() {
        match r.tag() {
            RefTag::Null | RefTag::False | RefTag::True | RefTag::IntInline => {}
            RefTag::IntHeap | RefTag::F64 => {
                arena.free(r.addr(), NUM_CELL);
                mem.node_bytes -= NUM_CELL;
            }
            RefTag::Str => {
                let len = STR_HDR + str_bytes(arena, r).len();
                arena.free(r.addr(), len);
                mem.node_bytes -= len;
            }
            RefTag::Obj | RefTag::Arr => {
                let slot = if r.tag() == RefTag::Obj { OBJ_ENTRY } else { ARR_SLOT };
                let (count, cap) = read_header(arena, r.addr());
                let len = node_len(cap, slot);
                let used = count as usize * (slot / 8);
                for i in 0..used {
                    stack.push(read_ref_at(arena, r.addr(), len, NODE_HDR + i * 8));
                }
                arena.free(r.addr(), len);
                mem.node_bytes -= len;
                mem.slack_bytes -= (cap - count) as usize * slot;
            }
        }
    }
}

impl ArenaDoc {
    /// Morph a validated tape into arena form (one-way, D8). On arena
    /// exhaustion mid-build every allocation is released — no partial
    /// documents, no leaks (the accounting proptest is the proof).
    pub fn from_tape(doc: &TapeDoc<'_>, arena: &mut Arena) -> Result<ArenaDoc, DocError> {
        let mut mem = DocMemReport::default();
        let mut stack: Vec<MorphFrame<'_>> = Vec::new();
        match morph_walk(doc.root(), arena, &mut mem, &mut stack) {
            Ok(root) => Ok(ArenaDoc { root, mem }),
            Err(e) => {
                // Leak-free abort: release everything staged in open frames.
                for frame in stack {
                    let (refs, pending) = match frame {
                        MorphFrame::Obj { refs, pending_key, .. } => (refs, pending_key),
                        MorphFrame::Arr { refs, .. } => (refs, None),
                    };
                    for raw in refs {
                        free_ref(arena, DocRef(raw), &mut mem);
                    }
                    if let Some(k) = pending {
                        free_ref(arena, k, &mut mem);
                    }
                }
                debug_assert_eq!(mem, DocMemReport::default(), "morph abort leaks nothing");
                Err(e)
            }
        }
    }

    /// Rehydrate a document from a stored handle (ADR-0037 D1): `root` and
    /// `mem` must be exactly what an earlier `root_ref()`/`report()` pair
    /// yielded against the same arena — the store is the only caller and
    /// never duplicates a handle (RENAME transfers, COPY deep-copies), so
    /// double-ownership is unrepresentable one layer up.
    #[inline]
    pub fn from_parts(root: DocRef, mem: DocMemReport) -> ArenaDoc {
        ArenaDoc { root, mem }
    }

    #[inline]
    pub fn report(&self) -> DocMemReport {
        self.mem
    }

    /// Root as a unified cursor value.
    pub fn root_value<'a>(&self, arena: &'a Arena) -> crate::cursor::DocValue<'a> {
        deref(arena, self.root)
    }

    #[inline]
    pub fn root_ref(&self) -> DocRef {
        self.root
    }

    /// Serialize to canonical tape bytes (header included). Single pass,
    /// u24 backpatch — children never move (D3). The M4 demotion and
    /// checkpoint-walker contract: `freeze(morph(t)) == t`.
    pub fn freeze(&self, arena: &Arena) -> Result<Vec<u8>, DocError> {
        let mut b = TapeBuilder::new();
        let mut stack: Vec<FreezeFrame> = Vec::new();
        freeze_walk(arena, self.root, &mut b, &mut stack)?;
        b.finish()
    }

    /// Canonical freeze into reusable output + frame buffers. The returned
    /// slice remains valid until the next use of `scratch`.
    pub fn freeze_recycled<'a>(
        &self,
        arena: &Arena,
        scratch: &'a mut FreezeScratch,
    ) -> Result<&'a [u8], DocError> {
        let out = core::mem::take(&mut scratch.out);
        let builder_stack = core::mem::take(&mut scratch.builder_stack);
        let mut walk_stack = core::mem::take(&mut scratch.walk_stack);
        walk_stack.clear();
        let mut builder =
            TapeBuilder::with_recycled(out, builder_stack, crate::limits::DOC_BYTES_MAX);
        let result = freeze_walk(arena, self.root, &mut builder, &mut walk_stack);
        let (out, builder_stack) =
            if result.is_ok() { builder.finish_recycled() } else { builder.into_recycled() };
        scratch.out = out;
        scratch.builder_stack = builder_stack;
        scratch.walk_stack = walk_stack;
        result?;
        Ok(&scratch.out)
    }

    /// Release the whole tree. Consumes the document; accounting must
    /// return to zero (asserted — a leak here is a bug, not a condition).
    pub fn free(mut self, arena: &mut Arena) {
        free_ref(arena, self.root, &mut self.mem);
        assert_eq!(self.mem, DocMemReport::default(), "document accounting reconciles to zero");
    }

    /// Append to an array node. Returns the (possibly relocated) array
    /// ref — refs are values, and growth may move the node (never its
    /// children); the caller repoints its parent slot. ×1.25 growth keeps
    /// slack ≤ 25% of array bytes by construction (D7 → the S13 budget).
    pub fn arr_push(
        &mut self,
        arena: &mut Arena,
        arr: DocRef,
        value: DocRef,
    ) -> Result<DocRef, DocError> {
        debug_assert_eq!(arr.tag(), RefTag::Arr);
        let new_arr = self.grow_if_full(arena, arr, ARR_SLOT)?;
        let (count, cap) = read_header(arena, new_arr.addr());
        debug_assert!(count < cap);
        let bytes = node_len(cap, ARR_SLOT);
        let node = arena.bytes_mut(new_arr.addr(), bytes);
        let at = NODE_HDR + count as usize * ARR_SLOT;
        node[at..at + 8].copy_from_slice(&value.0.to_le_bytes());
        node[0..4].copy_from_slice(&(count + 1).to_le_bytes());
        self.mem.slack_bytes -= ARR_SLOT;
        if arr == self.root {
            self.root = new_arr;
        }
        Ok(new_arr)
    }

    /// Append an entry to an object node (no key dedup here — RedisJSON
    /// path semantics own replacement; S16 wires them). Same relocation
    /// contract as `arr_push`.
    pub fn obj_push(
        &mut self,
        arena: &mut Arena,
        obj: DocRef,
        key: DocRef,
        value: DocRef,
    ) -> Result<DocRef, DocError> {
        debug_assert_eq!(obj.tag(), RefTag::Obj);
        debug_assert_eq!(key.tag(), RefTag::Str);
        let new_obj = self.grow_if_full(arena, obj, OBJ_ENTRY)?;
        let (count, cap) = read_header(arena, new_obj.addr());
        debug_assert!(count < cap);
        let bytes = node_len(cap, OBJ_ENTRY);
        let node = arena.bytes_mut(new_obj.addr(), bytes);
        let at = NODE_HDR + count as usize * OBJ_ENTRY;
        node[at..at + 8].copy_from_slice(&key.0.to_le_bytes());
        node[at + 8..at + 16].copy_from_slice(&value.0.to_le_bytes());
        node[0..4].copy_from_slice(&(count + 1).to_le_bytes());
        self.mem.slack_bytes -= OBJ_ENTRY;
        if obj == self.root {
            self.root = new_obj;
        }
        Ok(new_obj)
    }

    /// Allocation-free same-width scalar patch over the arena projection
    /// (ADR-0043 D1). Unsupported shapes return before a byte changes.
    pub fn patch_scalar(
        &mut self,
        arena: &mut Arena,
        program: &PathProgram,
        op: &ApplyOp<'_>,
    ) -> Result<ScalarPatch, ApplyError> {
        let Some(steps) = program.simple_steps() else {
            return Ok(ScalarPatch::Unsupported);
        };
        let Some((slot, current)) = locate_simple_ref(arena, self.root, steps) else {
            return Ok(ScalarPatch::Missing);
        };
        match *op {
            ApplyOp::NumIncrBy(operand) | ApplyOp::NumMultBy(operand) => {
                let current_number = match current.tag() {
                    RefTag::IntInline => Number::I64(current.as_inline_int()),
                    RefTag::IntHeap => Number::I64(num_bits(arena, current) as i64),
                    RefTag::F64 => Number::F64(f64::from_bits(num_bits(arena, current))),
                    _ => return Ok(ScalarPatch::Skipped),
                };
                let result =
                    number_op(current_number, operand, matches!(op, ApplyOp::NumMultBy(_)))?;
                if canonical_number_len(current_number) != canonical_number_len(result)
                    || !patch_number_ref(arena, &mut self.root, slot, current, result)
                {
                    return Ok(ScalarPatch::Unsupported);
                }
                Ok(ScalarPatch::Number(result))
            }
            ApplyOp::Toggle => {
                let toggled = match current.tag() {
                    RefTag::False => true,
                    RefTag::True => false,
                    _ => return Ok(ScalarPatch::Skipped),
                };
                write_slot(arena, &mut self.root, slot, DocRef::bool_ref(toggled));
                Ok(ScalarPatch::Toggled(toggled))
            }
            _ => Ok(ScalarPatch::Unsupported),
        }
    }

    /// Ensure one free slot: resize in place when the allocator can, else
    /// relocate the node (children are refs — they never move).
    fn grow_if_full(
        &mut self,
        arena: &mut Arena,
        node_ref: DocRef,
        slot: usize,
    ) -> Result<DocRef, DocError> {
        let (count, cap) = read_header(arena, node_ref.addr());
        if count < cap {
            return Ok(node_ref);
        }
        // ×1.25 (ceil), floor +4: amortized O(n) appends, bounded slack.
        let new_cap = cap + (cap / 4).max(4);
        let old_len = node_len(cap, slot);
        let new_len = node_len(new_cap, slot);
        let addr = node_ref.addr();
        let final_addr = if arena.resize_in_place(addr, old_len, new_len) {
            addr
        } else {
            let new_addr = arena.alloc(new_len).ok_or(DocError::ArenaExhausted)?;
            // Copy through a scratch buffer: the arena cannot lend two
            // regions at once. Growth is amortized-rare; S16 revisits with
            // an A/B if it ever profiles.
            let old = arena.bytes(addr, old_len).to_vec();
            arena.bytes_mut(new_addr, new_len)[..old_len].copy_from_slice(&old);
            arena.free(addr, old_len);
            new_addr
        };
        arena.bytes_mut(final_addr, new_len)[4..8].copy_from_slice(&new_cap.to_le_bytes());
        self.mem.node_bytes += new_len - old_len;
        self.mem.slack_bytes += (new_cap - cap) as usize * slot;
        Ok(DocRef::with_addr(node_ref.tag(), final_addr))
    }

    /// Scalar/string cell allocators for the push primitives (S16's
    /// mutation engine speaks these; the model never touches this module).
    pub fn alloc_i64(&mut self, arena: &mut Arena, v: i64) -> Result<DocRef, DocError> {
        alloc_int(arena, v, &mut self.mem)
    }

    pub fn alloc_f64(&mut self, arena: &mut Arena, v: f64) -> Result<DocRef, DocError> {
        if !v.is_finite() {
            return Err(DocError::NonFiniteNumber);
        }
        alloc_num_cell(arena, v.to_bits(), RefTag::F64, &mut self.mem)
    }

    /// `&str` parameters make UTF-8 a type-level fact (D6).
    pub fn alloc_str_value(&mut self, arena: &mut Arena, s: &str) -> Result<DocRef, DocError> {
        alloc_str(arena, s.as_bytes(), &mut self.mem)
    }

    pub fn alloc_key(&mut self, arena: &mut Arena, k: &str) -> Result<DocRef, DocError> {
        alloc_str(arena, k.as_bytes(), &mut self.mem)
    }
}

#[derive(Copy, Clone)]
enum RefSlot {
    Root,
    Node { addr: ArenaAddr, bytes: usize, offset: usize },
}

fn locate_simple_ref<'a>(
    arena: &Arena,
    root: DocRef,
    steps: impl Iterator<Item = SimpleStep<'a>>,
) -> Option<(RefSlot, DocRef)> {
    let mut slot = RefSlot::Root;
    let mut current = root;
    for step in steps {
        (slot, current) = match step {
            SimpleStep::Child(key) => locate_obj_ref(arena, current, key)?,
            SimpleStep::Index(index) => locate_arr_ref(arena, current, index)?,
        };
    }
    Some((slot, current))
}

fn locate_obj_ref(arena: &Arena, obj: DocRef, key: &[u8]) -> Option<(RefSlot, DocRef)> {
    if obj.tag() != RefTag::Obj {
        return None;
    }
    let (count, cap) = read_header(arena, obj.addr());
    let bytes = node_len(cap, OBJ_ENTRY);
    for index in 0..count as usize {
        let at = NODE_HDR + index * OBJ_ENTRY;
        let candidate = read_ref_at(arena, obj.addr(), bytes, at);
        if str_bytes(arena, candidate) == key {
            let offset = at + 8;
            return Some((
                RefSlot::Node { addr: obj.addr(), bytes, offset },
                read_ref_at(arena, obj.addr(), bytes, offset),
            ));
        }
    }
    None
}

fn locate_arr_ref(arena: &Arena, arr: DocRef, index: i64) -> Option<(RefSlot, DocRef)> {
    if arr.tag() != RefTag::Arr {
        return None;
    }
    let (count, cap) = read_header(arena, arr.addr());
    let index = if index < 0 { index + i64::from(count) } else { index };
    if !(0..i64::from(count)).contains(&index) {
        return None;
    }
    let bytes = node_len(cap, ARR_SLOT);
    let offset = NODE_HDR + index as usize * ARR_SLOT;
    Some((
        RefSlot::Node { addr: arr.addr(), bytes, offset },
        read_ref_at(arena, arr.addr(), bytes, offset),
    ))
}

fn write_slot(arena: &mut Arena, root: &mut DocRef, slot: RefSlot, value: DocRef) {
    match slot {
        RefSlot::Root => *root = value,
        RefSlot::Node { addr, bytes, offset } => {
            arena.bytes_mut(addr, bytes)[offset..offset + 8]
                .copy_from_slice(&value.0.to_le_bytes());
        }
    }
}

fn patch_number_ref(
    arena: &mut Arena,
    root: &mut DocRef,
    slot: RefSlot,
    current: DocRef,
    result: Number,
) -> bool {
    match (current.tag(), result) {
        (RefTag::F64, Number::F64(value)) => {
            arena
                .bytes_mut(current.addr(), NUM_CELL)
                .copy_from_slice(&value.to_bits().to_le_bytes());
            true
        }
        (RefTag::IntInline, Number::I64(value)) => {
            let Some(next) = DocRef::inline_int(value) else { return false };
            write_slot(arena, root, slot, next);
            true
        }
        (RefTag::IntHeap, Number::I64(value)) => {
            if DocRef::inline_int(value).is_some() {
                return false;
            }
            arena
                .bytes_mut(current.addr(), NUM_CELL)
                .copy_from_slice(&(value as u64).to_le_bytes());
            true
        }
        _ => false,
    }
}

fn canonical_number_len(number: Number) -> usize {
    match number {
        Number::F64(_) => crate::emit::F64_LEN,
        Number::I64(value) => crate::emit::i64_len(value),
    }
}

// ---- freeze walk ------------------------------------------------------------

enum FreezeFrame {
    Obj { addr: ArenaAddr, count: u32, cap: u32, idx: u32 },
    Arr { addr: ArenaAddr, count: u32, cap: u32, idx: u32 },
}

enum FreezeStep {
    /// Top frame is exhausted: emit `end()` and pop.
    Close,
    /// Next child to emit; `key` is set for object entries.
    Entry { key: Option<DocRef>, value: DocRef },
}

fn freeze_walk(
    arena: &Arena,
    root: DocRef,
    builder: &mut TapeBuilder,
    stack: &mut Vec<FreezeFrame>,
) -> Result<(), DocError> {
    freeze_emit(arena, root, builder, stack)?;
    while !stack.is_empty() {
        match freeze_step(arena, stack) {
            FreezeStep::Close => {
                builder.end();
                stack.pop();
            }
            FreezeStep::Entry { key, value } => {
                if let Some(key) = key {
                    let key = str::from_utf8(str_bytes(arena, key))
                        .expect("arena key cells hold boundary-validated UTF-8");
                    builder.key(key)?;
                }
                freeze_emit(arena, value, builder, stack)?;
            }
        }
    }
    Ok(())
}

/// Pull the next step out of the top frame with a short borrow, so the
/// caller can push new frames without fighting the borrow checker.
fn freeze_step(arena: &Arena, stack: &mut [FreezeFrame]) -> FreezeStep {
    let top = stack.last_mut().expect("caller checked non-empty");
    match top {
        FreezeFrame::Obj { addr, count, cap, idx } => {
            if idx == count {
                return FreezeStep::Close;
            }
            let bytes = node_len(*cap, OBJ_ENTRY);
            let at = NODE_HDR + *idx as usize * OBJ_ENTRY;
            let key = read_ref_at(arena, *addr, bytes, at);
            let value = read_ref_at(arena, *addr, bytes, at + 8);
            *idx += 1;
            FreezeStep::Entry { key: Some(key), value }
        }
        FreezeFrame::Arr { addr, count, cap, idx } => {
            if idx == count {
                return FreezeStep::Close;
            }
            let bytes = node_len(*cap, ARR_SLOT);
            let value = read_ref_at(arena, *addr, bytes, NODE_HDR + *idx as usize * ARR_SLOT);
            *idx += 1;
            FreezeStep::Entry { key: None, value }
        }
    }
}

/// Emit one ref: scalars/strings write immediately; containers open a
/// builder scope and push a frame.
fn freeze_emit(
    arena: &Arena,
    r: DocRef,
    b: &mut TapeBuilder,
    stack: &mut Vec<FreezeFrame>,
) -> Result<(), DocError> {
    match r.tag() {
        RefTag::Null => b.null(),
        RefTag::False => b.bool(false),
        RefTag::True => b.bool(true),
        RefTag::IntInline => b.i64(r.as_inline_int()),
        RefTag::IntHeap => b.i64(num_bits(arena, r) as i64),
        RefTag::F64 => b.f64(f64::from_bits(num_bits(arena, r))),
        RefTag::Str => b.str_value(
            str::from_utf8(str_bytes(arena, r))
                .expect("arena str cells hold boundary-validated UTF-8 (ADR-0036 D6)"),
        ),
        RefTag::Obj => {
            let (count, cap) = read_header(arena, r.addr());
            b.begin_obj()?;
            stack.push(FreezeFrame::Obj { addr: r.addr(), count, cap, idx: 0 });
            Ok(())
        }
        RefTag::Arr => {
            let (count, cap) = read_header(arena, r.addr());
            b.begin_arr()?;
            stack.push(FreezeFrame::Arr { addr: r.addr(), count, cap, idx: 0 });
            Ok(())
        }
    }
}

// ---- morph walk (tape → arena) -----------------------------------------------

enum MorphFrame<'t> {
    Obj { refs: Vec<u64>, pending_key: Option<DocRef>, iter: tape::ObjIter<'t> },
    Arr { refs: Vec<u64>, iter: tape::ArrIter<'t> },
}

/// What the top frame wants next (extracted under a short borrow).
enum MorphStep<'t> {
    CloseArr,
    CloseObj,
    Child(tape::ValueRef<'t>),
    ObjEntry { key: DocStr<'t>, value: tape::ValueRef<'t> },
}

fn morph_walk<'t>(
    root: tape::ValueRef<'t>,
    arena: &mut Arena,
    mem: &mut DocMemReport,
    stack: &mut Vec<MorphFrame<'t>>,
) -> Result<DocRef, DocError> {
    let mut completed: Option<DocRef> = morph_begin(root, arena, mem, stack)?;
    loop {
        if stack.is_empty() {
            return Ok(completed.expect("walk ends with exactly the root ref"));
        }
        let step = {
            let top = stack.last_mut().expect("checked non-empty");
            match top {
                MorphFrame::Arr { refs, iter } => {
                    if let Some(r) = completed.take() {
                        refs.push(r.0);
                    }
                    match iter.next() {
                        Some(child) => MorphStep::Child(child),
                        None => MorphStep::CloseArr,
                    }
                }
                MorphFrame::Obj { refs, pending_key, iter } => {
                    if let Some(r) = completed.take() {
                        let key = pending_key.take().expect("value completes a pending key");
                        refs.push(key.0);
                        refs.push(r.0);
                    }
                    match iter.next() {
                        Some((key, value)) => MorphStep::ObjEntry { key, value },
                        None => MorphStep::CloseObj,
                    }
                }
            }
        };
        match step {
            MorphStep::Child(child) => completed = morph_begin(child, arena, mem, stack)?,
            MorphStep::ObjEntry { key, value } => {
                let key_ref = alloc_str(arena, key.as_bytes(), mem)?;
                let top = stack.last_mut().expect("frame still open");
                let MorphFrame::Obj { pending_key, .. } = top else {
                    unreachable!("obj step came from an obj frame")
                };
                *pending_key = Some(key_ref);
                completed = morph_begin(value, arena, mem, stack)?;
            }
            MorphStep::CloseArr | MorphStep::CloseObj => {
                let frame = stack.pop().expect("frame still open");
                let (tag, slot, refs) = match frame {
                    MorphFrame::Arr { refs, .. } => (RefTag::Arr, ARR_SLOT, refs),
                    MorphFrame::Obj { refs, .. } => (RefTag::Obj, OBJ_ENTRY, refs),
                };
                match alloc_node(arena, tag, slot, &refs, mem) {
                    Ok(node) => completed = Some(node),
                    Err(e) => {
                        // The frame left the stack: release its refs here;
                        // outer frames are released by `from_tape`.
                        for raw in refs {
                            free_ref(arena, DocRef(raw), mem);
                        }
                        return Err(e);
                    }
                }
            }
        }
    }
}

fn morph_begin<'t>(
    v: tape::ValueRef<'t>,
    arena: &mut Arena,
    mem: &mut DocMemReport,
    stack: &mut Vec<MorphFrame<'t>>,
) -> Result<Option<DocRef>, DocError> {
    Ok(match v {
        tape::ValueRef::Null => Some(DocRef::NULL),
        tape::ValueRef::Bool(b) => Some(DocRef::bool_ref(b)),
        tape::ValueRef::I64(i) => Some(alloc_int(arena, i, mem)?),
        tape::ValueRef::F64(f) => Some(alloc_num_cell(arena, f.to_bits(), RefTag::F64, mem)?),
        tape::ValueRef::Str(s) => Some(alloc_str(arena, s.as_bytes(), mem)?),
        tape::ValueRef::Arr(a) => {
            stack.push(MorphFrame::Arr { refs: Vec::new(), iter: a.iter() });
            None
        }
        tape::ValueRef::Obj(o) => {
            stack.push(MorphFrame::Obj { refs: Vec::new(), pending_key: None, iter: o.iter() });
            None
        }
    })
}

// ---- cursors over the arena form ----------------------------------------------

/// Object cursor (arena arm). `len()` is O(1) — the node stores its count.
#[derive(Copy, Clone, Debug)]
pub struct ObjRef<'a> {
    arena: &'a Arena,
    addr: ArenaAddr,
    count: u32,
    cap: u32,
}

/// Array cursor (arena arm). `index()` is O(1) — slots are an array.
#[derive(Copy, Clone, Debug)]
pub struct ArrRef<'a> {
    arena: &'a Arena,
    addr: ArenaAddr,
    count: u32,
    cap: u32,
}

/// Resolve a ref into a unified cursor value.
pub(crate) fn deref(arena: &Arena, r: DocRef) -> crate::cursor::DocValue<'_> {
    use crate::cursor::DocValue;
    match r.tag() {
        RefTag::Null => DocValue::Null,
        RefTag::False => DocValue::Bool(false),
        RefTag::True => DocValue::Bool(true),
        RefTag::IntInline => DocValue::I64(r.as_inline_int()),
        RefTag::IntHeap => DocValue::I64(num_bits(arena, r) as i64),
        RefTag::F64 => DocValue::F64(f64::from_bits(num_bits(arena, r))),
        RefTag::Str => DocValue::Str(DocStr(str_bytes(arena, r))),
        RefTag::Obj => {
            let (count, cap) = read_header(arena, r.addr());
            DocValue::Obj(crate::cursor::ObjCursor::Arena(ObjRef {
                arena,
                addr: r.addr(),
                count,
                cap,
            }))
        }
        RefTag::Arr => {
            let (count, cap) = read_header(arena, r.addr());
            DocValue::Arr(crate::cursor::ArrCursor::Arena(ArrRef {
                arena,
                addr: r.addr(),
                count,
                cap,
            }))
        }
    }
}

impl<'a> ObjRef<'a> {
    #[inline]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn entry(&self, i: u32) -> (DocStr<'a>, crate::cursor::DocValue<'a>) {
        debug_assert!(i < self.count);
        let bytes = node_len(self.cap, OBJ_ENTRY);
        let at = NODE_HDR + i as usize * OBJ_ENTRY;
        let key = read_ref_at(self.arena, self.addr, bytes, at);
        let val = read_ref_at(self.arena, self.addr, bytes, at + 8);
        (DocStr(str_bytes(self.arena, key)), deref(self.arena, val))
    }

    pub fn iter(&self) -> ObjIter<'a> {
        ObjIter { obj: *self, idx: 0 }
    }

    /// First entry whose key equals `key` — the same pinned rule as the
    /// tape arm (ADR-0036 D5).
    /// First entry whose key equals `key`. Scans keys only — the value
    /// slot is read and dereferenced once, on the match: `entry(i)` per
    /// scanned entry paid a second slot read plus a full value `deref`
    /// (`DocValue` construction, header reads for containers) that the
    /// non-matching case throws away (~49%/17% of the S02 arena
    /// criterion row in `entry`/`deref` — perf annotate; A/B −38%:
    /// `.artifacts/m3/s02-traverse-opt-20260711/`).
    #[inline]
    pub fn get(&self, key: &[u8]) -> Option<crate::cursor::DocValue<'a>> {
        let bytes = node_len(self.cap, OBJ_ENTRY);
        for i in 0..self.count {
            let at = NODE_HDR + i as usize * OBJ_ENTRY;
            let key_ref = read_ref_at(self.arena, self.addr, bytes, at);
            if str_bytes(self.arena, key_ref) == key {
                let val_ref = read_ref_at(self.arena, self.addr, bytes, at + 8);
                return Some(deref(self.arena, val_ref));
            }
        }
        None
    }
}

#[derive(Clone, Debug)]
pub struct ObjIter<'a> {
    obj: ObjRef<'a>,
    idx: u32,
}

impl<'a> Iterator for ObjIter<'a> {
    type Item = (DocStr<'a>, crate::cursor::DocValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx == self.obj.count {
            return None;
        }
        let entry = self.obj.entry(self.idx);
        self.idx += 1;
        Some(entry)
    }
}

impl<'a> ArrRef<'a> {
    #[inline]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn index(&self, i: usize) -> Option<crate::cursor::DocValue<'a>> {
        if i >= self.count as usize {
            return None;
        }
        let bytes = node_len(self.cap, ARR_SLOT);
        let slot = read_ref_at(self.arena, self.addr, bytes, NODE_HDR + i * ARR_SLOT);
        Some(deref(self.arena, slot))
    }

    pub fn iter(&self) -> ArrIter<'a> {
        ArrIter { arr: *self, idx: 0 }
    }
}

#[derive(Clone, Debug)]
pub struct ArrIter<'a> {
    arr: ArrRef<'a>,
    idx: u32,
}

impl<'a> Iterator for ArrIter<'a> {
    type Item = crate::cursor::DocValue<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx == self.arr.count {
            return None;
        }
        let v = self.arr.index(self.idx as usize).expect("idx < count");
        self.idx += 1;
        Some(v)
    }
}
