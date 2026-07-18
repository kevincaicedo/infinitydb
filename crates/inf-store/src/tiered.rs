//! Tiered record table (M4-S02): the §7.3 index over the M4-S01 address
//! space. The index does not change shape — the frozen 8 B slot's 48-bit
//! field is reinterpreted as a [`LogicalAddr`] at table granularity
//! (`Index<TieredMode>`, monomorphized — memory-mode tables are untouched
//! by construction), and record fetch routes through the resolver.
//!
//! This module is **mechanism**; suspension policy stays with the caller
//! (L6): a lookup that lands on a cold candidate returns
//! [`TieredLookup::Cold`] — the command layer (S04's steel thread, S08's
//! hardened path) fetches the bytes through the tier store, verifies the
//! key, and on the ≈2⁻²² fingerprint false positive retries with the
//! address excluded. Nothing here reads a disk byte or holds anything
//! across a suspension: every entry point takes and returns plain
//! addresses, so the resumed command re-resolves by contract (the M0
//! custody rule).
//!
//! Mutation surface in S02 is the index+space contract only — insert /
//! overwrite (copy-to-tail, the S06 shape) / delete. In-place mutable-
//! region updates are M4-S05; TTL, eviction pressure, and WAL wiring
//! arrive with the stories that own them.

use inf_foundation::{LogicalAddr, hash64};

use crate::address_space::{AddrClass, AddressSpace, AddressSpaceConfig};
use crate::index::{Index, TieredMode};
use crate::record::{RecordKind, RecordSpec, RecordView};
use crate::store::{HASH_SEED, OpError};

/// Answer of a tiered lookup. `Cold` is a *candidate*: the 22-bit
/// fingerprint matched but the key is on disk — the caller fetches,
/// verifies with [`TieredTable::decode_record`], and on mismatch retries
/// via `lookup` with the address added to `exclude`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TieredLookup {
    /// RAM-resident, key verified.
    Ram(LogicalAddr),
    /// Cold candidate (addr < head) — fetch + verify, never trust.
    Cold(LogicalAddr),
    /// No live entry for this key.
    Miss,
}

/// Decoded view of one record's parts — the RAM read result and the
/// cold-fetch deserialization result share this shape.
#[derive(Copy, Clone, Debug)]
pub struct RecordParts<'a> {
    pub key: &'a [u8],
    pub value: &'a [u8],
    pub version: u32,
    pub encoded_len: usize,
}

impl<'a> RecordParts<'a> {
    fn of(view: RecordView<'a>) -> RecordParts<'a> {
        RecordParts {
            key: view.key(),
            value: view.value(),
            version: view.version(),
            encoded_len: view.encoded_len(),
        }
    }
}

/// One durable-tiered namespace's record table on one cell (L1).
pub struct TieredTable {
    index: Index<TieredMode>,
    space: AddressSpace,
    live_bytes: u64,
}

impl TieredTable {
    /// `None` when the ring reservation fails (namespace creation surfaces
    /// it typed).
    pub fn new(config: AddressSpaceConfig, initial_keys: usize) -> Option<TieredTable> {
        Some(TieredTable {
            index: Index::with_capacity(initial_keys.max(64)),
            space: AddressSpace::new(config)?,
            live_bytes: 0,
        })
    }

    /// Stable key hash — same seed as the memory-mode store, so batch
    /// pipelines hash once regardless of table mode.
    #[inline]
    pub fn hash_key(key: &[u8]) -> u64 {
        hash64(key, HASH_SEED)
    }

    /// Probe-line prefetch (the batch pipeline's phase 1 — L3).
    #[inline]
    pub fn prefetch(&self, hash: u64) {
        self.index.prefetch(hash);
    }

    /// Unverified-candidate record prefetch (phase 2): prefetches the
    /// candidate's record head lines **only when RAM-resident** — cold
    /// addresses are skipped (they suspend in S08 anyway; prefetching
    /// disk is meaningless).
    #[inline]
    pub fn prefetch_candidate(&self, hash: u64) {
        if let Some(addr) = self.index.find(hash, |_| true)
            && self.space.resolve(addr) != AddrClass::Cold
        {
            let head = self.space.bytes(addr, crate::record::HEADER_LEN).as_ptr();
            inf_simd::prefetch_read(head);
            inf_simd::prefetch_read(head.wrapping_add(64));
        }
    }

    /// Index lookup through the resolver. RAM candidates verify the key
    /// here; cold candidates return for the caller to fetch + verify
    /// (`exclude` carries fetched-and-mismatched addresses on retry — the
    /// false-positive path, ≈2⁻²² per candidate).
    pub fn lookup(&self, key: &[u8], hash: u64, exclude: &[LogicalAddr]) -> TieredLookup {
        debug_assert_eq!(hash, Self::hash_key(key));
        let mut cold_candidate = None;
        let ram_hit = self.index.find(hash, |addr| match self.space.resolve(addr) {
            AddrClass::Mutable | AddrClass::ReadOnly => self.record(addr).key == key,
            AddrClass::Cold => {
                if cold_candidate.is_none() && !exclude.contains(&addr) {
                    cold_candidate = Some(addr);
                }
                false // keep probing — a RAM entry may still verify
            }
        });
        match (ram_hit, cold_candidate) {
            (Some(addr), _) => TieredLookup::Ram(addr),
            (None, Some(addr)) => TieredLookup::Cold(addr),
            (None, None) => TieredLookup::Miss,
        }
    }

    /// Reads a RAM-resident record (header first, then the exact slice —
    /// the `record_at` shape over the resolver).
    ///
    /// # Panics
    /// Panics when `addr` is cold — RAM access below the head is a
    /// resolver bypass (the address space enforces it).
    pub fn record(&self, addr: LogicalAddr) -> RecordParts<'_> {
        let head = self.space.bytes(addr, crate::record::HEADER_LEN);
        let full_len = crate::record::encoded_len_from_header(head);
        RecordParts::of(RecordView::new(self.space.bytes(addr, full_len)))
    }

    /// Deserializes a cold-fetched record image (the S04/S08 resume path
    /// and the test oracle's simulated tier store). The bytes come from a
    /// CRC-protected tier page (S11); this trusts them like the arena
    /// trusts its own writes.
    pub fn decode_record(bytes: &[u8]) -> RecordParts<'_> {
        RecordParts::of(RecordView::new(bytes))
    }

    /// Inserts a record for an absent key (the caller looked up first —
    /// the memory-mode `write_record` precondition, kept).
    pub fn insert(&mut self, key: &[u8], value: &[u8], hash: u64) -> Result<LogicalAddr, OpError> {
        // Absence precondition: no RAM-verified hit. A `Cold` answer is
        // NOT presence — a 2⁻²² fingerprint collision with a cold slot
        // legally reports a candidate for an absent key, so asserting
        // `Miss` here would panic on legal input.
        debug_assert!(
            !matches!(self.lookup(key, hash, &[]), TieredLookup::Ram(_)),
            "insert of a RAM-verified present key"
        );
        if self.index.needs_grow() {
            // Sidecar-only re-placement: cold-addressed slots re-place
            // without a record read (§3.3 — the closure has no record
            // access, so this is structural, not reviewed-for).
            self.index.grow(|_, ext| ext);
        }
        let addr = self.append(key, value, 0)?;
        self.index.insert(hash, addr);
        Ok(addr)
    }

    /// Overwrites the record at `old` (key-verified by the caller's
    /// lookup): copy-to-tail + index repoint + version bump — the S06
    /// shape, address never rewritten in place (§3.1). `old_len` and
    /// `old_version` come from the caller's verified view (RAM read or
    /// cold fetch) — for a cold `old`, reading it here would be a
    /// synchronous disk touch.
    pub fn overwrite(
        &mut self,
        key: &[u8],
        value: &[u8],
        hash: u64,
        old: LogicalAddr,
        old_len: usize,
        old_version: u32,
    ) -> Result<LogicalAddr, OpError> {
        let new_addr = self.append(key, value, old_version.wrapping_add(1))?;
        self.index.replace(hash, old, new_addr);
        // Dead-byte attribution at the repoint moment (the S14 live-set
        // hook site — S06 formalizes per-range accounting on it).
        self.space.note_dead_bytes(old_len as u64);
        self.live_bytes -= old_len as u64;
        Ok(new_addr)
    }

    /// Deletes the record at `addr` (key-verified by the caller's lookup).
    /// For a cold record this touches the index and accounting only —
    /// never a cold read (§3.3).
    pub fn delete(&mut self, hash: u64, addr: LogicalAddr, len: usize) {
        self.index.remove(hash, addr);
        self.space.note_dead_bytes(len as u64);
        self.live_bytes -= len as u64;
    }

    /// Live entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// True when empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The underlying address space (watermark advancement, counters,
    /// attribution). Flush/demotion slices (S07/S11) drive it through
    /// here; tests observe it.
    #[inline]
    pub fn space(&self) -> &AddressSpace {
        &self.space
    }

    /// Mutable space access for the lifecycle drivers (seal, flush
    /// confirmation, release — the §3.1 order is enforced inside).
    #[inline]
    pub fn space_mut(&mut self) -> &mut AddressSpace {
        &mut self.space
    }

    /// Raw RAM bytes of an address range — the flush pipeline's page
    /// source (S04/S11) and the test oracle's capture hook.
    #[inline]
    pub fn record_bytes(&self, addr: LogicalAddr, len: usize) -> &[u8] {
        self.space.bytes(addr, len)
    }

    /// Exact index + live-record accounting (L5): index bytes include the
    /// tiered hash sidecar; `live_bytes + space dead = space allocated`.
    pub fn index_bytes(&self) -> u64 {
        self.index.memory_bytes() as u64
    }

    /// Live record bytes (the accounting identity's left half).
    #[inline]
    pub fn live_bytes(&self) -> u64 {
        self.live_bytes
    }

    fn append(&mut self, key: &[u8], value: &[u8], version: u32) -> Result<LogicalAddr, OpError> {
        if key.len() > crate::record::MAX_KEY_LEN || value.len() > crate::record::MAX_VAL_LEN {
            return Err(OpError::TooLarge);
        }
        let spec = RecordSpec {
            key,
            value,
            version,
            expire_at_ms: None,
            kind: RecordKind::String { raw: false },
        };
        let len = spec.encoded_len();
        let addr = self.space.alloc(len).ok_or(OpError::OutOfMemory)?;
        spec.write(self.space.bytes_mut(addr, len));
        self.live_bytes += len as u64;
        Ok(addr)
    }
}

impl core::fmt::Debug for TieredTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TieredTable")
            .field("live", &self.index.len())
            .field("live_bytes", &self.live_bytes)
            .field("space", &self.space)
            .finish()
    }
}
