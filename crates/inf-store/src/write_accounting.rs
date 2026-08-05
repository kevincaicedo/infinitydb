//! Per-namespace write-path accounting (M4-S13, plan §5 E4) — the four
//! byte counters write amplification is computed from, and (M4-S16,
//! ADR-0060) the ratio itself.
//!
//! With tiering a user byte is written **twice by design**: once as a WAL
//! record (the durability mechanism — L2, unchanged from M2) and once as
//! tier-file bytes when the record's range flushes (the storage
//! mechanism). Compaction adds more later. That is the honest baseline
//! the operator guide states plainly
//! (`infinitydb/docs/ops-tiered-storage.md`); these counters exist so it
//! is *measured*, never estimated, and so a runaway namespace is visible
//! before the disk fills (L10 — no silent anything).
//!
//! ## What each counter counts (frozen with this story — S16 divides
//! exactly these units)
//!
//! | Counter | Charged where | Includes | Excludes |
//! |---|---|---|---|
//! | `user_bytes` | record boundary, `inf-store` | key + value bytes of every record image the namespace admitted | record headers, WAL framing, protocol bytes |
//! | `wal_bytes` | WAL staging, [`TieredTable::stage_wal`](crate::TieredTable::stage_wal) | encoded log-record bytes, length prefix included | the shared frame header/trailer (see below) |
//! | `flush_bytes` | tier device writes, `inf-log` | header blocks, frame writes (partial-tail rewrites included), footers, **and the re-flush of every relocated record** | nothing the block layer sees for this pipeline |
//! | `compaction_bytes` | copy-forward, `inf-store` | bytes copy-forward relocated to the tail | the device write those bytes cause — that is `flush_bytes`' (ADR-0060 D2) |
//!
//! Three deliberate asymmetries, each a judgement rather than an
//! oversight:
//!
//! - **User bytes are counted at the record boundary, never at the
//!   wire.** Wire counting folds protocol overhead into the denominator
//!   and skews write amplification permanently in the flattering
//!   direction.
//! - **Deletes contribute no user bytes and do contribute WAL bytes.** A
//!   tombstone-heavy workload therefore reports write amplification above
//!   the write-twice baseline. That is a true statement about the
//!   workload, not an accounting artifact — the guide says so.
//! - **Frame header/trailer bytes are not pro-rated across namespaces.**
//!   One WAL frame carries records from every namespace the cell wrote
//!   that iteration; splitting its 20-odd envelope bytes per namespace
//!   would be a lie dressed as precision. They are a global term, named
//!   and measured in the S13 block-layer validation artifact.
//!
//! Counters are cumulative **per boot life** and reset on restart, like
//! every other tiering counter (§3.1 "addresses are per-life"): recovery
//! builds a fresh table and a fresh flush pipeline. Two boot-path device
//! writes are deliberately outside `flush_bytes` — the recovery reseal of
//! an unsealed tier file (`recover_seal_existing`, one footer per
//! recovered file) and checkpoint/MANIFEST writes (they belong to the
//! checkpoint domain, and `INFO persistence` already reports it).
//!
//! Memory-mode namespaces have no `TieredTable`, therefore no
//! `WriteAccounting` object — the ADR-0051 degenerate case is type-level
//! absence, and the §3.3 zero contract over `INFO tiering` is what proves
//! it from outside.
//!
//! ## The ratio (M4-S16, ADR-0060)
//!
//! `WA = (wal_bytes + flush_bytes) / user_bytes`, per namespace, per boot
//! life — [`WriteAccounting::write_amplification`].
//!
//! **`compaction_bytes` is deliberately not a term** (ADR-0060 D2, and it
//! is the one surprise in this module). Copy-forward does not write to the
//! device: it re-appends the live record into the RAM tail (ADR-0059 D2),
//! and the ordinary demotion/flush leg carries those bytes to disk — where
//! `flush_bytes` already counts them. Adding `compaction_bytes` would
//! count every relocated byte twice and put the accounting outside the
//! S13 ±10% block-layer window exactly when compaction runs; the S16
//! reconciliation measured both candidate numerators against
//! `/proc/diskstats` and the block layer chose this one. So compaction's
//! cost is fully inside the reported ratio — it arrives through
//! `flush_bytes` (plus the D9 origin markers' extra `ColdDisplace` records
//! through `wal_bytes`) — while `compaction_bytes` stays the *volume*
//! counter that explains **why** `flush_bytes` is high.
//!
//! Two properties the ratio is built to keep:
//!
//! - **Per namespace, never blended.** A node-wide average hides a
//!   runaway tiered namespace behind a quiet one, so the cell-level type
//!   ([`WriteAccountingTotals`]) carries the four sums and cannot produce
//!   a ratio at all.
//! - **Rounding never flatters.** Milli-units are ceiling-rounded and the
//!   64-bit conversion saturates upward: a reported figure may overstate
//!   amplification by one thousandth, never understate it (L10 — a
//!   rounding that turns a failing gate into a passing one is a silent
//!   cap).

/// One durable-tiered namespace's write-path byte counters, cell-local
/// and cumulative for this boot life (L1: no atomics, one owner).
///
/// Every field is a byte count; the module docs define each one exactly.
/// `Default` is the empty state a fresh namespace starts in.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct WriteAccounting {
    /// Key + value bytes of every record image admitted, charged at the
    /// record boundary (the write-amplification denominator).
    pub user_bytes: u64,
    /// Encoded WAL record bytes staged for this namespace (length prefix
    /// included; shared frame framing excluded — see the module docs).
    pub wal_bytes: u64,
    /// Device bytes the tier flush wrote for this namespace.
    pub flush_bytes: u64,
    /// Bytes copy-forward compaction relocated to the tail — the
    /// **relocation volume**, not a device-byte leg: the flush that
    /// follows writes them and `flush_bytes` counts them there
    /// (ADR-0060 D2). Reported so a high `flush_bytes` can be explained,
    /// never added to the numerator.
    pub compaction_bytes: u64,
    /// Value bytes stored out of line — the blob **denominator** leg
    /// (M4-S17, ADR-0061 D8). Deliberately not in `user_bytes`: extent
    /// values never flow through WAL or flush, so folding them into the
    /// record ratio would dilute it; blob WA is its own ratio,
    /// `blob_bytes / blob_user_bytes ≈ 1×` by construction —
    /// [`blob_write_amplification`](Self::blob_write_amplification)
    /// (M4-S18, the report split over exactly these two fields).
    pub blob_user_bytes: u64,
    /// Device bytes extent writers handed the device for this namespace
    /// (header blocks + frames, rewritten tails included) — the blob
    /// **numerator** leg. Disjoint from `wal_bytes`/`flush_bytes` by
    /// construction (a byte is written once and counted once —
    /// ADR-0060 D2, unchanged).
    pub blob_bytes: u64,
}

impl WriteAccounting {
    /// Bytes written on this namespace's behalf: the write-amplification
    /// **numerator** — WAL records plus tier-device bytes, each counted
    /// once. `compaction_bytes` is not a term here; the module docs argue
    /// why (ADR-0060 D2: relocated bytes reach the device through the
    /// flush leg, so adding them double-counts).
    #[must_use]
    pub fn written_bytes(&self) -> u64 {
        self.wal_bytes + self.flush_bytes
    }

    /// This namespace's write amplification for the current boot life
    /// (M4-S16). Undefined when no user byte was admitted — see
    /// [`WriteAmplification`].
    #[must_use]
    pub fn write_amplification(&self) -> WriteAmplification {
        Self::ratio_milli(self.written_bytes(), self.user_bytes)
    }

    /// The blob leg's own ratio (M4-S18, ADR-0061 D8):
    /// `blob_bytes / blob_user_bytes`, ≈ 1.001× by construction (frame
    /// CRC 4/4092 plus one header block per extent). Kept out of
    /// [`write_amplification`](Self::write_amplification) in both
    /// directions — extent bytes never ride WAL/flush/compaction, and
    /// folding the two ratios would let a quiet blob leg dilute a
    /// runaway record leg (the per-namespace-blend pitfall, one level
    /// down). A namespace with no blob activity reports `undefined`
    /// with zero written bytes — absence, not a fault.
    #[must_use]
    pub fn blob_write_amplification(&self) -> WriteAmplification {
        Self::ratio_milli(self.blob_bytes, self.blob_user_bytes)
    }

    /// One rounding rule for both ratios: ceiling, and saturating on the
    /// 64-bit narrowing — both round *against* the system. u128 is free
    /// here (control plane, once per scrape) and a u64 product overflows
    /// past ~18 PB written.
    fn ratio_milli(written_bytes: u64, user_bytes: u64) -> WriteAmplification {
        if user_bytes == 0 {
            return WriteAmplification::Undefined { written_bytes };
        }
        let milli = u128::from(written_bytes)
            .saturating_mul(WriteAmplification::MILLI)
            .div_ceil(u128::from(user_bytes));
        WriteAmplification::Measured { milli: u64::try_from(milli).unwrap_or(u64::MAX) }
    }
}

/// One namespace's write amplification (M4-S16, ADR-0060) — a ratio in
/// **milli-units** or the honest statement that there is no denominator.
///
/// The undefined arm is not a defensive nicety: a namespace can write
/// bytes while admitting none (a delete-only workload stages WAL
/// tombstones and charges no user bytes — S13's second asymmetry), and
/// that state's amplification is unbounded, not zero. Reporting `0` there
/// would read as "no amplification", which is the opposite of the truth,
/// so the type refuses to produce a number and callers must say
/// `undefined` out loud.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WriteAmplification {
    /// No user byte was admitted this life: the ratio has no denominator.
    /// `written_bytes` is the numerator that exists regardless — zero for
    /// an untouched namespace, nonzero for the unbounded case
    /// ([`is_unbounded`](Self::is_unbounded)).
    Undefined { written_bytes: u64 },
    /// The measured ratio in milli-units: `1_999` is 1.999×.
    Measured { milli: u64 },
}

impl WriteAmplification {
    /// Fixed-point scale: reported ratios are integers in thousandths.
    /// Integer milli-units keep every `INFO` field parseable as an
    /// integer and keep the gate comparison exact — a float in a counter
    /// surface is a formatting decision masquerading as a measurement.
    pub const MILLI: u128 = 1_000;

    /// The measured ratio in milli-units, or `None` when undefined.
    #[must_use]
    pub fn milli(self) -> Option<u64> {
        match self {
            WriteAmplification::Measured { milli } => Some(milli),
            WriteAmplification::Undefined { .. } => None,
        }
    }

    /// The namespace wrote bytes without admitting a single user byte:
    /// amplification is unbounded. The state a gate must never read as a
    /// pass — `INFO` counts these namespaces in their own field so a
    /// harness can refuse the row instead of averaging them away.
    #[must_use]
    pub fn is_unbounded(self) -> bool {
        matches!(self, WriteAmplification::Undefined { written_bytes } if written_bytes > 0)
    }
}

impl core::fmt::Display for WriteAmplification {
    /// The one spelling of the field value, so `INFO`, the operator guide,
    /// and the harness cannot drift on the undefined token.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WriteAmplification::Measured { milli } => write!(f, "{milli}"),
            WriteAmplification::Undefined { .. } => f.write_str("undefined"),
        }
    }
}

/// A cell's field-wise write-counter totals across its tiered namespaces
/// — the `INFO tiering` aggregate fields.
///
/// Deliberately a **different type** from [`WriteAccounting`]: it has no
/// `write_amplification`, because a blended ratio hides a runaway tiered
/// namespace behind a quiet one (the M4-S16 pitfall, made unwritable
/// rather than reviewed for). The worst-namespace summary is
/// [`WriteAmpSummary`], which the cell computes per namespace and then
/// takes the maximum of.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct WriteAccountingTotals {
    /// Sum of every namespace's `user_bytes`.
    pub user_bytes: u64,
    /// Sum of every namespace's `wal_bytes`.
    pub wal_bytes: u64,
    /// Sum of every namespace's `flush_bytes`.
    pub flush_bytes: u64,
    /// Sum of every namespace's `compaction_bytes` (relocation volume).
    pub compaction_bytes: u64,
    /// Sum of every namespace's `blob_user_bytes` (M4-S17, ADR-0061 D8).
    pub blob_user_bytes: u64,
    /// Sum of every namespace's `blob_bytes` — the extent device leg,
    /// disjoint from `written_bytes()` by construction.
    pub blob_bytes: u64,
}

impl WriteAccountingTotals {
    /// Folds one namespace's counters in. Saturating: a scrape must never
    /// panic a serving cell, and the per-namespace lines carry the exact
    /// values either way.
    pub fn add(&mut self, ns: WriteAccounting) {
        self.user_bytes = self.user_bytes.saturating_add(ns.user_bytes);
        self.wal_bytes = self.wal_bytes.saturating_add(ns.wal_bytes);
        self.flush_bytes = self.flush_bytes.saturating_add(ns.flush_bytes);
        self.compaction_bytes = self.compaction_bytes.saturating_add(ns.compaction_bytes);
        self.blob_user_bytes = self.blob_user_bytes.saturating_add(ns.blob_user_bytes);
        self.blob_bytes = self.blob_bytes.saturating_add(ns.blob_bytes);
    }

    /// Bytes written across the cell's tiered namespaces (the aggregate
    /// `tiering_written_bytes`). A total, never a ratio — see the type
    /// docs.
    #[must_use]
    pub fn written_bytes(&self) -> u64 {
        self.wal_bytes.saturating_add(self.flush_bytes)
    }
}

/// A cell's write-amplification summary for `INFO tiering` (M4-S16): the
/// **worst** namespace, plus a count of the namespaces that cannot answer.
///
/// Two numbers rather than one average, for the reason the story names: an
/// average is exactly the shape that hides a runaway namespace. A gate
/// reads `milli_max`; a harness refuses the row when
/// `unbounded_namespaces` is nonzero, because that namespace's
/// amplification is unbounded and no maximum over the others describes it.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct WriteAmpSummary {
    /// Highest per-namespace ratio in milli-units among namespaces that
    /// have a denominator; `0` when none do (including a cell with no
    /// tiered namespace at all — the §3.3 zero contract).
    pub milli_max: u64,
    /// Namespaces that wrote bytes while admitting none.
    pub unbounded_namespaces: u32,
}

impl WriteAmpSummary {
    /// Folds one namespace's ratio in.
    pub fn add(&mut self, wa: WriteAmplification) {
        match wa {
            WriteAmplification::Measured { milli } => self.milli_max = self.milli_max.max(milli),
            WriteAmplification::Undefined { written_bytes } => {
                if written_bytes > 0 {
                    self.unbounded_namespaces += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numerator is the two written-byte legs and nothing else: user
    /// bytes are the denominator and must never leak into it,
    /// `compaction_bytes` is a volume counter whose device cost is
    /// already inside `flush_bytes` (ADR-0060 D2), and the blob legs are
    /// a disjoint device path with their own ratio (ADR-0061 D8) — a
    /// byte written once is counted once, in exactly one leg.
    #[test]
    fn written_bytes_is_wal_plus_flush_only() {
        let acct = WriteAccounting {
            user_bytes: 1_000,
            wal_bytes: 1_100,
            flush_bytes: 1_200,
            compaction_bytes: 300,
            blob_user_bytes: 50_000,
            blob_bytes: 50_200,
        };
        assert_eq!(acct.written_bytes(), 2_300);
    }

    /// Aggregation is exactly field-wise, so a cell total can always be
    /// reconciled against the per-namespace lines it summed.
    #[test]
    fn totals_are_field_wise() {
        let a = WriteAccounting {
            user_bytes: 1,
            wal_bytes: 2,
            flush_bytes: 3,
            compaction_bytes: 4,
            blob_user_bytes: 5,
            blob_bytes: 6,
        };
        let b = WriteAccounting {
            user_bytes: 10,
            wal_bytes: 20,
            flush_bytes: 30,
            compaction_bytes: 40,
            blob_user_bytes: 50,
            blob_bytes: 60,
        };
        let mut totals = WriteAccountingTotals::default();
        totals.add(a);
        totals.add(b);
        assert_eq!(
            totals,
            WriteAccountingTotals {
                user_bytes: 11,
                wal_bytes: 22,
                flush_bytes: 33,
                compaction_bytes: 44,
                blob_user_bytes: 55,
                blob_bytes: 66,
            }
        );
        assert_eq!(totals.written_bytes(), 55);
    }

    /// The write-twice baseline, exactly: 1.999× is the S13 measured
    /// shape, and the ratio ignores the relocation volume counter.
    #[test]
    fn ratio_is_milli_and_ignores_the_relocation_volume() {
        let acct = WriteAccounting {
            user_bytes: 1_000,
            wal_bytes: 1_010,
            flush_bytes: 989,
            compaction_bytes: 5_000,
            ..WriteAccounting::default()
        };
        assert_eq!(acct.write_amplification(), WriteAmplification::Measured { milli: 1_999 });
        assert_eq!(acct.write_amplification().milli(), Some(1_999));
        assert!(!acct.write_amplification().is_unbounded());
    }

    /// Rounding is ceiling: a ratio one thousandth over the gate must
    /// read as over it. Flooring here would let 3.0004× report 3.000×
    /// and pass a `< 3×` gate — a silent cap by arithmetic.
    #[test]
    fn rounding_never_flatters() {
        let acct =
            WriteAccounting { user_bytes: 10_000, wal_bytes: 30_004, ..WriteAccounting::default() };
        assert_eq!(acct.write_amplification().milli(), Some(3_001));
        // Exact ratios are not inflated by the ceiling.
        let exact = WriteAccounting { user_bytes: 10, wal_bytes: 20, ..WriteAccounting::default() };
        assert_eq!(exact.write_amplification().milli(), Some(2_000));
    }

    /// A namespace that wrote bytes and admitted none has unbounded
    /// amplification. It renders as `undefined`, never as a number, and
    /// the summary counts it instead of averaging it away.
    #[test]
    fn no_user_bytes_is_undefined_not_zero() {
        let idle = WriteAccounting::default();
        assert_eq!(idle.write_amplification(), WriteAmplification::Undefined { written_bytes: 0 });
        assert!(
            !idle.write_amplification().is_unbounded(),
            "an untouched namespace is not a fault"
        );
        assert_eq!(idle.write_amplification().milli(), None);
        assert_eq!(idle.write_amplification().to_string(), "undefined");

        let tombstones = WriteAccounting { wal_bytes: 4_096, ..WriteAccounting::default() };
        let wa = tombstones.write_amplification();
        assert_eq!(wa, WriteAmplification::Undefined { written_bytes: 4_096 });
        assert!(wa.is_unbounded(), "wrote bytes, admitted none");
        assert_eq!(wa.to_string(), "undefined");
    }

    /// The cell summary reports the worst namespace, never a blend: two
    /// namespaces at 1.5× and 6× summarize as 6×, not 3.75×.
    #[test]
    fn summary_takes_the_worst_namespace_not_the_average() {
        let quiet =
            WriteAccounting { user_bytes: 1_000, wal_bytes: 1_500, ..WriteAccounting::default() };
        let runaway =
            WriteAccounting { user_bytes: 100, wal_bytes: 600, ..WriteAccounting::default() };
        let tombstones = WriteAccounting { wal_bytes: 64, ..WriteAccounting::default() };
        let mut summary = WriteAmpSummary::default();
        for acct in [quiet, runaway, tombstones] {
            summary.add(acct.write_amplification());
        }
        assert_eq!(summary, WriteAmpSummary { milli_max: 6_000, unbounded_namespaces: 1 });

        let mut blend = WriteAccountingTotals::default();
        blend.add(quiet);
        blend.add(runaway);
        assert_eq!(blend.written_bytes(), 2_100);
        assert_eq!(blend.user_bytes, 1_100);
        // The blended ratio the type refuses to compute would be 1.91× —
        // it would hide the 6× namespace completely. This assertion is
        // the reason `WriteAccountingTotals` has no ratio method.
        assert!(blend.written_bytes() * 1_000 / blend.user_bytes < 2_000);
    }

    /// A one-byte denominator cannot panic or wrap a scrape: the
    /// narrowing saturates in the direction that reports bad news.
    #[test]
    fn pathological_denominator_saturates_upward() {
        let acct =
            WriteAccounting { user_bytes: 1, wal_bytes: u64::MAX, ..WriteAccounting::default() };
        assert_eq!(acct.write_amplification().milli(), Some(u64::MAX));
    }

    /// The blob ratio divides exactly its two disjoint legs (M4-S18,
    /// ADR-0061 D8) — the record counters never leak into it, and it
    /// never leaks into the record ratio. The S17 AC2 measurement
    /// (1 GiB round trip, 1.001×) is the shape pinned here.
    #[test]
    fn blob_ratio_is_its_own_disjoint_leg() {
        let acct = WriteAccounting {
            user_bytes: 100,
            wal_bytes: 120,
            flush_bytes: 110,
            blob_user_bytes: 1_000_000,
            blob_bytes: 1_000_988,
            ..WriteAccounting::default()
        };
        assert_eq!(acct.blob_write_amplification().milli(), Some(1_001));
        assert_eq!(acct.write_amplification().milli(), Some(2_300), "record legs only");
        // Ceiling rounds the blob leg against the system too.
        let over = WriteAccounting {
            blob_user_bytes: 10_000,
            blob_bytes: 30_004,
            ..WriteAccounting::default()
        };
        assert_eq!(over.blob_write_amplification().milli(), Some(3_001));
    }

    /// A namespace with no blob activity reports the undefined arm with
    /// zero written bytes — absence, never an alarm; blob bytes without
    /// a blob denominator would be the unbounded arm, same as the record
    /// leg's delete-only shape.
    #[test]
    fn blob_ratio_absence_is_undefined_not_zero() {
        let no_blobs =
            WriteAccounting { user_bytes: 10, wal_bytes: 20, ..WriteAccounting::default() };
        let wa = no_blobs.blob_write_amplification();
        assert_eq!(wa, WriteAmplification::Undefined { written_bytes: 0 });
        assert!(!wa.is_unbounded(), "no blob activity is not a fault");
        assert_eq!(wa.to_string(), "undefined");

        let orphaned = WriteAccounting { blob_bytes: 4_096, ..WriteAccounting::default() };
        assert!(orphaned.blob_write_amplification().is_unbounded());
    }
}
