//! `CellStore` record lifecycle: expiry ticks and the wheel, eviction
//! policy hooks, and the record write/free/touch internals every
//! mutation funnels through.

use super::*;

impl CellStore {
    // ---- active expiry (M1-E2) ----

    /// One budgeted expiry MAINTAIN slice (M1-S05): advance the wheel toward
    /// `now`, validating each fired entry against the index and reaping only
    /// records genuinely expired. Stale entries (TTL changed/persisted/key
    /// gone) drop with a counter. Bounded by `budget` on both fires and
    /// cursor steps so a 1M-same-second storm cannot cliff the loop.
    pub fn expire_tick(&mut self, now: Nanos, budget: ExpiryBudget) -> ExpiryStats {
        let now_ms = now.0 / 1_000_000;
        #[cfg(feature = "doc")]
        let max_matches = self.cfg.doc_max_path_matches;
        let hasher = self.cfg.hasher;
        let CellStore { arena, index, wheel, stats, docs, idx, .. } = self;
        #[cfg(not(feature = "doc"))]
        let _ = &idx;
        let mut out = ExpiryStats::default();
        let tick = wheel.tick(now_ms, budget, |hash, _deadline| {
            // Reap any record on this hash's probe path that is genuinely
            // expired (full-hash check keeps fingerprint collisions out;
            // reaping an expired record is correct regardless of which key
            // armed the entry).
            let found = index.find(hash, |addr| {
                let view = record_at(arena, addr);
                view.is_expired(now) && hasher.hash(view.key()) == hash
            });
            match found {
                Some(addr) => {
                    let len = record_at(arena, addr).encoded_len();
                    // The record-death hook (ADR-0072 D6): active expiry
                    // is a removal class like any other; the split-borrow
                    // reap deliberately bypasses `free_record`, so the
                    // hook is wired here too (the D6 structural
                    // exception). MAINTAIN slices never run inside a
                    // command, so no bracket can cover this death.
                    #[cfg(feature = "doc")]
                    if idx.death_hook_wanted(hash)
                        && let Some(root) = doc::doc_root_at(arena, docs, addr, len)
                    {
                        idx.remove_doc_entries(hash, root, max_matches);
                    }
                    let payload = doc::payload_of(arena, addr, len);
                    index.remove(hash, addr);
                    arena.free(addr, len);
                    docs.release(payload);
                    stats.expired_active += 1;
                    stats.ttl_live = stats.ttl_live.saturating_sub(1);
                    out.reaped += 1;
                }
                None => {
                    stats.wheel_stale += 1;
                    out.stale += 1;
                }
            }
        });
        out.steps = tick.steps;
        out.lag_ms = if tick.caught_up { 0 } else { now_ms.saturating_sub(wheel_cursor(wheel)) };
        out.armed = wheel.live();
        out
    }

    /// The wheel's standing debt in ms at `now` (0 with nothing armed —
    /// an idle wheel snaps to `now` on its next tick, so a stale cursor
    /// there is not backlog). Pure read, no tick: the per-store term of
    /// the keyspace-wide `expiry_debt` fold (F-L05-03).
    #[must_use]
    pub fn expiry_lag_ms(&self, now: Nanos) -> u64 {
        if self.wheel.live() == 0 {
            return 0;
        }
        (now.0 / 1_000_000).saturating_sub(self.wheel.cursor_ms())
    }

    // ---- eviction mechanism (M1-S06; policy logic lives in `evict.rs`) ----

    /// Applies an eviction policy: flips the access-tracking mode and
    /// allocates/frees the CMS (8 KiB only while LFU is selected).
    pub fn set_eviction_policy(&mut self, policy: EvictionPolicy) {
        self.evict.set_policy(policy);
    }

    #[inline]
    pub fn eviction_policy(&self) -> EvictionPolicy {
        self.evict.policy
    }

    /// Logical bytes this store costs (live records + index + wheel + CMS)
    /// — what `maxmemory` pressure compares against (M1-S07), the Redis
    /// `used_memory` shape. Live (not resident) bytes are the comparable:
    /// slab chunks stay mapped and recycle, so resident is monotone while
    /// eviction must be able to bring pressure *down*. The RSS story is the
    /// slack bound: resident ≤ live-at-peak + class slack, asserted by the
    /// M1-S07 pressure test and gated on the reference box.
    pub fn used_bytes(&self) -> u64 {
        let r = self.report();
        r.records_live_bytes
            + r.index_bytes
            + r.wheel_bytes
            + r.evict_bytes
            + r.doc_tape_bytes
            + r.doc_arena_bytes
    }

    /// Evicts at most one victim under the active policy (bounded candidate
    /// window, `samples` per selection). The pressure driver loops this.
    pub fn evict_step(&mut self, samples: u32, now: Nanos) -> EvictStats {
        evict::evict_one(self, samples, now)
    }

    /// Periodic eviction maintenance: CMS Morris-counter decay on the
    /// injected clock (MAINTAIN slice).
    pub fn evict_maintain(&mut self, now: Nanos) {
        evict::maybe_decay(&mut self.evict, now);
    }

    /// `OBJECT FREQ` under an LFU policy: the CMS estimate (Morris-scaled —
    /// recorded deviation: Redis reports its own log-counter scale).
    pub fn object_freq(&mut self, key: &[u8], now: Nanos) -> Option<u8> {
        self.resolve(key, now)?;
        let hash = self.hash_key(key);
        Some(self.evict.cms.as_ref().map_or(0, |cms| cms.estimate(hash)))
    }

    /// CLOCK aging: drop one reference generation (eviction sweep).
    pub(crate) fn age_record(&mut self, addr: ArenaAddr) {
        let head = self.arena.bytes_mut(addr, 1);
        head[0] = flags_ref_decrement(head[0]);
    }

    /// Reaps a record the eviction sweep found already expired.
    pub(crate) fn reap_expired_at(&mut self, hash: u64, addr: ArenaAddr, len: usize) {
        self.free_record(hash, addr, len);
        self.note_reap_lazy();
    }

    /// Removes an eviction victim (counted separately from expirations).
    pub(crate) fn evict_record(&mut self, hash: u64, addr: ArenaAddr, len: usize, had_ttl: bool) {
        self.free_record(hash, addr, len);
        self.note_ttl(had_ttl, false);
        self.stats.evicted_keys += 1;
    }

    // ---- internals ----

    /// Free one record completely: index entry, record bytes, and any
    /// document payload behind it (the ADR-0037 D3 choke point). Every
    /// reap/delete/evict site funnels here; the only deliberate bypass is
    /// RENAME's source removal (the handle transferred to the destination).
    ///
    /// Record-death hook (M4.5-S04, ADR-0072 D6): a dying document's
    /// index entries are removed here — the last moment its values are
    /// readable — unless this death is bracket-covered (a write-set key
    /// dying inside its own command; the bracket's diff owns it,
    /// ADR-0076 D4). Zero-index stores pay one cached branch.
    pub(crate) fn free_record(&mut self, hash: u64, addr: ArenaAddr, len: usize) {
        #[cfg(feature = "doc")]
        if self.idx.death_hook_wanted(hash) {
            let max_matches = self.cfg.doc_max_path_matches;
            let CellStore { arena, docs, idx, .. } = self;
            if let Some(root) = doc::doc_root_at(arena, docs, addr, len) {
                idx.remove_doc_entries(hash, root, max_matches);
            }
        }
        let payload = doc::payload_of(&self.arena, addr, len);
        self.index.remove(hash, addr);
        self.arena.free(addr, len);
        self.docs.release(payload);
    }

    /// Files the record's expiry at `deadline_ms + 1` — the first
    /// millisecond `is_expired` is true (the deadline millisecond itself
    /// still serves the key, as Redis's `now > when`; F-L05-05). A fire at
    /// the deadline would fail validation and strand the entry as a stale
    /// drop, leaving the record to lazy expiry alone.
    pub(crate) fn arm_wheel(&mut self, hash: u64, deadline_ms: u64) {
        if self.wheel.arm(hash, deadline_ms.saturating_add(1)) == ArmOutcome::PoolFull {
            self.stats.wheel_fallback += 1;
        }
    }

    /// TTL-record census transition (`INFO keyspace` `expires=`).
    #[inline]
    pub(crate) fn note_ttl(&mut self, old: bool, new: bool) {
        match (old, new) {
            (false, true) => self.stats.ttl_live += 1,
            (true, false) => self.stats.ttl_live = self.stats.ttl_live.saturating_sub(1),
            _ => {}
        }
    }

    #[inline]
    pub(crate) fn note_reap_lazy(&mut self) {
        self.stats.expired_lazy += 1;
        self.stats.ttl_live = self.stats.ttl_live.saturating_sub(1);
    }

    /// Index lookup + expire-on-read: returns the live record's address and
    /// encoded length, reaping it if its deadline passed.
    pub(crate) fn resolve(&mut self, key: &[u8], now: Nanos) -> Option<(ArenaAddr, usize)> {
        self.resolve_hashed(key, self.hash_key(key), now)
    }

    #[inline]
    pub(super) fn resolve_hashed(
        &mut self,
        key: &[u8],
        hash: u64,
        now: Nanos,
    ) -> Option<(ArenaAddr, usize)> {
        match self.lookup_hashed(key, hash, now) {
            Lookup::Live(addr, len) => Some((addr, len)),
            Lookup::Reaped(_) | Lookup::Absent => None,
        }
    }

    /// [`resolve_hashed`](Self::resolve_hashed) that also names the address
    /// it freed: a batched caller holding unverified candidate addresses
    /// must invalidate the one that was reaped, not the one it probed to
    /// (F-L05-04).
    pub(super) fn lookup_hashed(&mut self, key: &[u8], hash: u64, now: Nanos) -> Lookup {
        let arena = &self.arena;
        let Some(addr) = self.index.find(hash, |addr| record_at(arena, addr).key() == key) else {
            return Lookup::Absent;
        };
        let view = record_at(arena, addr);
        let len = view.encoded_len();
        if view.is_expired(now) {
            self.free_record(hash, addr, len);
            self.note_reap_lazy();
            return Lookup::Reaped(addr);
        }
        self.touch_access(hash, addr);
        Lookup::Live(addr, len)
    }

    /// Eviction access tracking (M1-S06): one cached branch when no LRU/LFU
    /// policy is active (the M1-S07 hot-path rule). CLOCK saturates the
    /// in-record reference bits (one OR on a line the access already
    /// pulled); LFU Morris-bumps the CMS with one injected-stream roll.
    #[inline]
    pub(super) fn touch_access(&mut self, hash: u64, addr: ArenaAddr) {
        match self.evict.tracking {
            Tracking::None => {}
            Tracking::Clock => {
                let head = self.arena.bytes_mut(addr, 1);
                head[0] = flags_ref_saturate(head[0]);
            }
            Tracking::Lfu => {
                let roll = self.evict.next_roll();
                if let Some(cms) = self.evict.cms.as_mut() {
                    cms.touch(hash, roll);
                }
            }
        }
    }

    pub(super) fn write_record(
        &mut self,
        key: &[u8],
        existing: Option<(ArenaAddr, usize)>,
        spec: RecordSpec<'_>,
    ) -> Result<(), OpError> {
        self.write_record_releasing(key, existing, spec)
    }

    /// [`write_record_carrying`](Self::write_record_carrying) plus the
    /// ADR-0037 D3 overwrite rule: any document payload behind `existing`
    /// is captured first and released only after the write succeeds — a
    /// failed write leaves the old record and its payload untouched.
    pub(crate) fn write_record_releasing(
        &mut self,
        key: &[u8],
        existing: Option<(ArenaAddr, usize)>,
        spec: RecordSpec<'_>,
    ) -> Result<(), OpError> {
        let old_payload = existing.map(|(addr, len)| doc::payload_of(&self.arena, addr, len));
        self.write_record_carrying(key, existing, spec)?;
        if let Some(payload) = old_payload {
            self.docs.release(payload);
        }
        Ok(())
    }

    /// Writes `spec`, reusing `existing`'s slot when the size class allows,
    /// else alloc-copy-free with an index address swap. **Carries** any
    /// document payload referenced by both old and new value bytes: no
    /// release happens here — TTL rewrites move handle bytes verbatim
    /// (ADR-0037 D3's RENAME/EXPIRE transfer rule; the in-place blob
    /// overwrite in `doc::json_write_value` relies on the same contract).
    pub(crate) fn write_record_carrying(
        &mut self,
        key: &[u8],
        existing: Option<(ArenaAddr, usize)>,
        spec: RecordSpec<'_>,
    ) -> Result<(), OpError> {
        let new_len = spec.encoded_len();
        let hash = self.hash_key(key);
        // Writes count as accesses (Redis updates LRU/LFU on write), at
        // write strength: one CLOCK generation / one CMS baseline bump —
        // repeated reads are what saturate recency, so churn cannot
        // impersonate a hot set.
        match existing {
            Some((addr, old_len)) if self.arena.resize_in_place(addr, old_len, new_len) => {
                spec.write(self.arena.bytes_mut(addr, new_len));
                self.touch_write(hash, addr);
                Ok(())
            }
            Some((addr, old_len)) => {
                let new_addr = self.arena.alloc(new_len).ok_or(OpError::OutOfMemory)?;
                spec.write(self.arena.bytes_mut(new_addr, new_len));
                self.index.replace(hash, addr, new_addr);
                self.arena.free(addr, old_len);
                self.touch_write(hash, new_addr);
                Ok(())
            }
            None => {
                if self.index.needs_grow() {
                    let hasher = self.cfg.hasher;
                    let arena = &self.arena;
                    self.index.grow(|addr, _| hasher.hash(record_at(arena, addr).key()));
                    self.stats.index_grows += 1;
                }
                let new_addr = self.arena.alloc(new_len).ok_or(OpError::OutOfMemory)?;
                spec.write(self.arena.bytes_mut(new_addr, new_len));
                self.index.insert(hash, new_addr);
                self.touch_write(hash, new_addr);
                Ok(())
            }
        }
    }

    /// Write-strength access mark (see `write_record_at`).
    #[inline]
    fn touch_write(&mut self, hash: u64, addr: ArenaAddr) {
        match self.evict.tracking {
            Tracking::None => {}
            Tracking::Clock => {
                let head = self.arena.bytes_mut(addr, 1);
                head[0] = flags_ref_write(head[0]);
            }
            Tracking::Lfu => {
                let roll = self.evict.next_roll();
                if let Some(cms) = self.evict.cms.as_mut() {
                    cms.touch(hash, roll);
                }
            }
        }
    }
}
