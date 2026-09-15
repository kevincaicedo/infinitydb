//! `CellStore` cursors and images: `SCAN`, the checkpoint/post-image
//! walks, key reservation, digests, and `RANDOMKEY`.

use super::*;

impl CellStore {
    // ---- keyspace iteration (M1-S02) ----

    /// `SCAN` over one cell: home-group enumeration in reverse-binary cursor
    /// order. Guarantee: every key present for the whole scan is emitted at
    /// least once, across doubling growth and tombstone-recycling rehashes
    /// (same-capacity rebuilds keep home groups fixed; doublings split a
    /// home group `g` into `{g, g + groups}` — exactly the split the
    /// reverse-binary order tolerates). Keys written or removed mid-scan may
    /// or may not appear (Redis contract). Expired records encountered are
    /// reaped, never emitted. Returns the next cursor (0 = done).
    pub fn scan(
        &mut self,
        cursor: u64,
        count: usize,
        now: Nanos,
        mut emit: impl FnMut(&[u8]),
    ) -> u64 {
        let mask = self.index.group_count() as u64 - 1;
        let mut cursor = cursor & mask;
        let mut emitted = 0usize;
        let mut batch: Vec<ArenaAddr> = Vec::new();
        loop {
            batch.clear();
            {
                let hasher = self.cfg.hasher;
                let arena = &self.arena;
                self.index.scan_home_group(
                    cursor as usize,
                    |addr| hasher.hash(record_at(arena, addr).key()),
                    |addr| batch.push(addr),
                );
            }
            for &addr in &batch {
                let view = record_at(&self.arena, addr);
                if view.is_expired(now) {
                    let (hash, len) = (self.hash_key(view.key()), view.encoded_len());
                    self.free_record(hash, addr, len);
                    self.note_reap_lazy();
                } else {
                    emit(view.key());
                    emitted += 1;
                }
            }
            cursor = next_rev_cursor(cursor, mask);
            if cursor == 0 || emitted >= count {
                return cursor;
            }
        }
    }

    /// The fuzzy-checkpoint walk (M2-S10, ADR-0016 D2): the same
    /// resize-stable home-group enumeration as [`scan`](Self::scan), but
    /// emitting each live entry's post-image `(key, value, expire_at_ms)`
    /// instead of the key — expiry deadline in *internal* ms (the caller
    /// converts through its `WallAnchor` when encoding records). Inherits
    /// the SCAN guarantee: every entry present for the whole walk is
    /// emitted at least once across doublings and tombstone rehashes;
    /// entries written mid-walk may appear zero or more times (harmless —
    /// checkpoint replay is a blind idempotent upsert and the log tail
    /// from `ckpt-begin` re-covers them). Expired records encountered are
    /// reaped, never emitted. No access-tracking side effects on emitted
    /// entries. Returns the next cursor (0 = done).
    pub fn scan_post_images(
        &mut self,
        cursor: u64,
        count: usize,
        now: Nanos,
        mut emit: impl FnMut(&[u8], &[u8], Option<u64>),
    ) -> u64 {
        self.scan_checkpoint_images(cursor, count, now, |key, image, expire_at_ms| match image {
            CheckpointImage::String(value) => emit(key, value, expire_at_ms),
            #[cfg(feature = "doc")]
            CheckpointImage::JsonDoc { .. } => {
                panic!("string-only post-image walker encountered a document")
            }
        })
    }

    /// Type-aware fuzzy-checkpoint walk. It has the same resize/expiry
    /// guarantees as [`scan_post_images`](Self::scan_post_images), but
    /// freezes documents into canonical idoc bytes at the store boundary.
    pub fn scan_checkpoint_images(
        &mut self,
        cursor: u64,
        count: usize,
        now: Nanos,
        mut emit: impl FnMut(&[u8], CheckpointImage<'_>, Option<u64>),
    ) -> u64 {
        let mut at = WalkCursor { group: cursor, chain: None };
        let done = self.scan_checkpoint_images_bounded(&mut at, count, now, |key, image, exp| {
            emit(key, image, exp);
            true
        });
        if done { 0 } else { at.group }
    }

    /// [`scan_checkpoint_images`](Self::scan_checkpoint_images) with an
    /// emission the caller may refuse (ADR-0117 D2): `emit` returns
    /// `false` to stop *before* that entry — the section it would not fit
    /// seals, and the next call resumes at exactly that entry through the
    /// cursor's in-chain position. Returns `true` once the index is fully
    /// walked (the cursor resets to [`WalkCursor::START`]).
    pub fn scan_checkpoint_images_bounded(
        &mut self,
        cursor: &mut WalkCursor,
        count: usize,
        now: Nanos,
        mut emit: impl FnMut(&[u8], CheckpointImage<'_>, Option<u64>) -> bool,
    ) -> bool {
        let mask = self.index.group_count() as u64 - 1;
        let mut group = cursor.group & mask;
        let mut resume = cursor.chain.take();
        let mut emitted = 0usize;
        let mut batch: Vec<(ArenaAddr, ChainPos)> = Vec::new();
        loop {
            batch.clear();
            {
                let hasher = self.cfg.hasher;
                let arena = &self.arena;
                self.index.scan_home_group_from(
                    group as usize,
                    resume.take(),
                    |addr| hasher.hash(record_at(arena, addr).key()),
                    |addr, pos| {
                        batch.push((addr, pos));
                        true
                    },
                );
            }
            for &(addr, pos) in &batch {
                let view = record_at(&self.arena, addr);
                if view.is_expired(now) {
                    let (hash, len) = (self.hash_key(view.key()), view.encoded_len());
                    self.free_record(hash, addr, len);
                    self.note_reap_lazy();
                    continue;
                }
                let taken = match view.type_tag() {
                    TypeTag::String => {
                        emit(view.key(), CheckpointImage::String(view.value()), view.expire_at_ms())
                    }
                    TypeTag::StringExtent => {
                        unreachable!("StringExtent records exist only in tiered namespaces")
                    }
                    TypeTag::JsonDoc => {
                        #[cfg(feature = "doc")]
                        {
                            let idoc = doc::checkpoint_idoc(&mut self.docs, view)
                                .expect("store-owned document freezes within its format bound");
                            emit(
                                view.key(),
                                CheckpointImage::JsonDoc {
                                    lineage: doc::lineage_of_record(view),
                                    version: view.version(),
                                    idoc,
                                },
                                view.expire_at_ms(),
                            )
                        }
                        #[cfg(not(feature = "doc"))]
                        unreachable!("JsonDoc records cannot exist without the doc feature");
                    }
                };
                if !taken {
                    // Refused: resume at this very entry (the slot never
                    // moves; a rebuild in between restarts the group).
                    *cursor = WalkCursor { group, chain: Some(pos) };
                    return false;
                }
                emitted += 1;
            }
            group = next_rev_cursor(group, mask);
            if group == 0 {
                *cursor = WalkCursor::START;
                return true;
            }
            if emitted >= count {
                *cursor = WalkCursor { group, chain: None };
                return false;
            }
        }
    }

    /// M2-S13: presize the index for `keys` live entries — recovery's
    /// hint from the `.ick` footer's per-ns counts, applied before the
    /// bulk replay so it avoids the doubling-rehash storm (each doubling
    /// is a stop-and-copy over the whole table). Only effective while the
    /// store is empty; a populated index keeps its geometry (growth on
    /// insert remains correct either way). The hint is clamped defensively
    /// — it may come from a damaged file, and a wrong hint may only cost
    /// memory geometry, never correctness.
    pub fn reserve_keys(&mut self, keys: usize) {
        const MAX_RESERVE: usize = 1 << 28;
        if self.is_empty() && keys > 64 {
            self.index = Index::with_capacity(keys.min(MAX_RESERVE));
        }
    }

    /// M2-S13 (ADR-0018): read-only sibling of
    /// [`scan_post_images`](Self::scan_post_images) for the recovery state
    /// digest — emits each live entry's `(key, value, expire_at_ms)`
    /// **without reaping** expired entries, so the walk performs no
    /// structural mutation and a full cursor sweep emits every live entry
    /// exactly once (the digest oracle needs exactly-once; the mutating
    /// walk guarantees only at-least-once across rehashes). Expired
    /// entries are skipped: they are logically dead at `now` whatever
    /// their physical residue.
    pub fn digest_post_images(
        &self,
        cursor: u64,
        count: usize,
        now: Nanos,
        mut emit: impl FnMut(&[u8], &[u8], Option<u64>),
    ) -> u64 {
        self.digest_checkpoint_images(cursor, count, now, |key, image, expire_at_ms| match image {
            CheckpointImage::String(value) => emit(key, value, expire_at_ms),
            #[cfg(feature = "doc")]
            CheckpointImage::JsonDoc { .. } => {
                panic!("string-only digest walker encountered a document")
            }
        })
    }

    /// Type-aware, read-only state-digest walk. Documents contribute
    /// canonical idoc bytes and their exact logical version; physical form
    /// and volatile cadence state are intentionally absent.
    pub fn digest_checkpoint_images(
        &self,
        cursor: u64,
        count: usize,
        now: Nanos,
        mut emit: impl FnMut(&[u8], CheckpointImage<'_>, Option<u64>),
    ) -> u64 {
        let mask = self.index.group_count() as u64 - 1;
        let mut cursor = cursor & mask;
        let mut emitted = 0usize;
        loop {
            let hasher = self.cfg.hasher;
            let arena = &self.arena;
            self.index.scan_home_group(
                cursor as usize,
                |addr| hasher.hash(record_at(arena, addr).key()),
                |addr| {
                    let view = record_at(arena, addr);
                    if view.is_expired(now) {
                        return;
                    }
                    match view.type_tag() {
                        TypeTag::String => emit(
                            view.key(),
                            CheckpointImage::String(view.value()),
                            view.expire_at_ms(),
                        ),
                        TypeTag::StringExtent => {
                            unreachable!("StringExtent records exist only in tiered namespaces")
                        }
                        TypeTag::JsonDoc => {
                            #[cfg(feature = "doc")]
                            {
                                let idoc = self
                                    .frozen_bytes_of(view)
                                    .expect("store-owned document freezes within its format bound");
                                emit(
                                    view.key(),
                                    CheckpointImage::JsonDoc {
                                        lineage: doc::lineage_of_record(view),
                                        version: view.version(),
                                        idoc: &idoc,
                                    },
                                    view.expire_at_ms(),
                                );
                            }
                            #[cfg(not(feature = "doc"))]
                            unreachable!("JsonDoc records cannot exist without the doc feature");
                        }
                    }
                    emitted += 1;
                },
            );
            cursor = next_rev_cursor(cursor, mask);
            if cursor == 0 || emitted >= count {
                return cursor;
            }
        }
    }
}
