//! The index-range page step (M4.5-S09, ADR-0080 D4, A1, A2): one place
//! owns the frozen form's paging semantics — resume-key length gate,
//! seek (resume pair clamped to the lower edge), upper-edge check,
//! scan-budget check, LIMIT countdown, resume production. The caller (S11's query future; the S09
//! tests) resolves
//! each candidate's pk ref, evaluates the residual VM, and reports
//! matches back — evaluation stays with the owner of doc custody, the
//! interpretation of the bounds does not fork.
//!
//! Pages are bounded by entries **scanned**, not entries matched — a
//! selective residual must never make one page unbounded work (L6; the
//! DynamoDB `Limit` rule). Resume is strictly-after a `(key, ref)`
//! pair, never a tree position (the S01 freeze: cursors re-seek, so
//! rebalancing cannot break them); mid-key resume matters because a
//! multi-valued equality range holds many refs under one key.

use inf_store::{IndexKeyType, IndexTree, OrderedCursor};

use crate::access::{RangeEdge, edge_len_ok};

/// Why a page could not start. The resume pair is client input (S11's
/// cursor token), so its shape is checked here, at the layer that
/// owns the seek — never by the tree's probe asserts (ADR-0080 A2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageError {
    /// The resume key's length does not fit the index key type: a
    /// `Fixed8` type is exactly 8 bytes, `Utf8` is `1..=ORDERED_KEY_MAX`
    /// — the same rule the access program's edges obey (`BadEdge`).
    BadResumeKey,
}

/// A page's resume point: the last `(encoded key, pk ref)` pair served.
/// S11's cursor wire format embeds this (plus the `{index id,
/// generation}` binding that makes staleness a typed error).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageResume {
    pub key: Vec<u8>,
    pub entry_ref: u64,
}

/// What one page did. `more == false` means the statement is complete
/// (range exhausted or LIMIT reached) — no further page exists. With
/// `more == true`, `resume` carries the next page's start; `None`
/// there means the pager never consumed a pair (the caller suspended
/// immediately) and the caller's own input state still describes the
/// page start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageOutcome {
    pub matched: u32,
    pub scanned: u32,
    pub more: bool,
    pub resume: Option<PageResume>,
}

/// Drives one page over an [`IndexTree`] range. The cursor owns its
/// resume pair and never borrows the tree between calls — mutation
/// between pages (and between candidates) is the operating condition,
/// not a hazard.
pub struct RangePager {
    cursor: OrderedCursor,
    hi: RangeEdge,
    scan_budget: u32,
    scanned: u32,
    matched: u32,
    limit_remaining: Option<u32>,
    /// Range end or LIMIT reached — the statement is complete.
    done: bool,
    last_key: Vec<u8>,
    last_ref: u64,
}

impl RangePager {
    /// A page starting at `resume` (strictly after that pair) or at the
    /// range's lower edge — whichever is later. A resume pair is client
    /// input (S11's cursor binds index + generation, not the statement's
    /// range), so a pair below `lo` seeds from `lo` instead: the tree
    /// iterates in key order, so one clamp at the seek is exactly a
    /// lower-edge check on every candidate (ADR-0080 A1). `limit_remaining`
    /// is the statement `LIMIT` minus prior pages' matches (`None` =
    /// unlimited); `scan_budget` bounds this page's work in entries
    /// scanned (≥ 1 — a zero-work page is a caller bug). `key_type` is
    /// the index's: a resume key of the wrong length for it is refused
    /// typed (A2) before any tree probe sees it.
    pub fn new(
        lo: &RangeEdge,
        hi: &RangeEdge,
        resume: Option<&PageResume>,
        key_type: IndexKeyType,
        scan_budget: u32,
        limit_remaining: Option<u32>,
    ) -> Result<RangePager, PageError> {
        debug_assert!(scan_budget >= 1, "a page scans at least one entry");
        if let Some(r) = resume
            && !edge_len_ok(r.key.len(), key_type)
        {
            return Err(PageError::BadResumeKey);
        }
        let cursor = match resume {
            Some(r) if lo.admits_from_below(&r.key) => {
                OrderedCursor::resume_after(&r.key, r.entry_ref)
            }
            _ => match lo {
                RangeEdge::Unbounded => OrderedCursor::from_start(),
                RangeEdge::Included(key) => OrderedCursor::from_key(key, true),
                RangeEdge::Excluded(key) => OrderedCursor::from_key(key, false),
            },
        };
        Ok(RangePager {
            cursor,
            hi: hi.clone(),
            scan_budget,
            scanned: 0,
            matched: 0,
            limit_remaining,
            done: false,
            last_key: Vec::new(),
            last_ref: 0,
        })
    }

    /// The next in-range candidate `(encoded key, pk ref)`, or `None`
    /// when the page ends (budget, range end, or LIMIT). The caller
    /// resolves the ref, applies the residual, and reports a match via
    /// [`RangePager::count_match`].
    pub fn next(&mut self, tree: &IndexTree) -> Option<(&[u8], u64)> {
        if self.done || self.scanned == self.scan_budget || self.limit_remaining == Some(0) {
            return None;
        }
        let Some((key, entry_ref)) = tree.cursor_next(&mut self.cursor) else {
            self.done = true;
            return None;
        };
        if !self.hi.admits_from_above(key) {
            self.done = true;
            return None;
        }
        self.scanned += 1;
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.last_ref = entry_ref;
        Some((self.last_key.as_slice(), self.last_ref))
    }

    /// The last candidate satisfied the statement (residual verdict
    /// true, document present) — counts toward the page's result and
    /// the statement LIMIT.
    pub fn count_match(&mut self) {
        self.matched += 1;
        if let Some(remaining) = &mut self.limit_remaining {
            debug_assert!(*remaining >= 1, "matches beyond LIMIT are a caller bug");
            *remaining -= 1;
            if *remaining == 0 {
                self.done = true;
            }
        }
    }

    /// Close the page (normally, or as an early suspension — S11's
    /// byte budgets stop driving and take the resume the same way).
    pub fn finish(self) -> PageOutcome {
        let more = !self.done;
        let resume = (more && self.scanned > 0)
            .then_some(PageResume { key: self.last_key, entry_ref: self.last_ref });
        PageOutcome { matched: self.matched, scanned: self.scanned, more, resume }
    }
}
