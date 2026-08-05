//! Write amplification in the report (M4-S16, ADR-0060): every row a
//! campaign publishes carries a WA disposition, and the harness computes
//! the ratio **itself** from the per-namespace counters rather than
//! trusting the server's derived field.
//!
//! Two rules this module exists to enforce, both L10:
//!
//! - **No row without a disposition.** A row that measured no WA is not a
//!   passing row, it is an *unreported* row — and a missing number reads
//!   like a good one. Either a ratio, or the named reason there is none
//!   (`no tiered namespace on this node` is a legitimate reason; "the
//!   harness forgot" is not, and [`crate::gaterun::finish_report`] refuses
//!   the report).
//! - **Per namespace, never blended.** The gate reads the **worst**
//!   namespace. A node-wide `written/user` average is exactly the shape
//!   that hides one runaway tiered namespace behind a quiet cache one, so
//!   this module never computes one.
//!
//! The independent recomputation is a pair assertion across a trust
//! boundary: the server publishes `write_amp_milli` per namespace and
//! `tiering_write_amp_milli_max` per cell, and the harness divides the raw
//! counters again with the same ceiling rule. A divergence means one of
//! the two is wrong, and neither is entitled to the benefit of the doubt —
//! the row is refused.

use std::collections::BTreeMap;

/// Ratio scale, matching `INFO`'s milli-units (1_999 == 1.999×).
const MILLI: u64 = 1_000;

/// One tiered namespace's write-amplification inputs, as scraped.
struct NsRow {
    cell: usize,
    ns: String,
    user_bytes: u64,
    written_bytes: u64,
    /// The server's own `write_amp_milli` token: an integer, or
    /// `undefined` when the namespace admitted no user byte.
    reported: String,
}

/// The ratio recomputed from raw counters — the same ceiling rule the
/// engine uses (`inf_store::WriteAccounting`), reimplemented here on
/// purpose: a harness that calls the code under test cannot contradict
/// it. `None` when there is no denominator.
fn milli_ceil(written_bytes: u64, user_bytes: u64) -> Option<u64> {
    if user_bytes == 0 {
        return None;
    }
    Some(
        u64::try_from(
            u128::from(written_bytes)
                .saturating_mul(u128::from(MILLI))
                .div_ceil(u128::from(user_bytes)),
        )
        .unwrap_or(u64::MAX),
    )
}

fn token(recomputed: Option<u64>) -> String {
    recomputed.map_or_else(|| "undefined".to_string(), |m| m.to_string())
}

impl NsRow {
    fn recomputed(&self) -> Option<u64> {
        milli_ceil(self.written_bytes, self.user_bytes)
    }

    fn token(&self) -> String {
        token(self.recomputed())
    }
}

/// One tiered namespace's blob-leg inputs (M4-S18, ADR-0061 D8), as
/// scraped: `blob_bytes / blob_user_bytes` — the disjoint device leg's
/// own ratio, never folded into [`NsRow`]'s.
struct BlobNsRow {
    cell: usize,
    ns: String,
    blob_user_bytes: u64,
    blob_bytes: u64,
    /// The server's own `blob_write_amp_milli` token.
    reported: String,
}

impl BlobNsRow {
    fn recomputed(&self) -> Option<u64> {
        milli_ceil(self.blob_bytes, self.blob_user_bytes)
    }

    fn token(&self) -> String {
        token(self.recomputed())
    }
}

/// What a row reports about write amplification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// No tiered namespace existed on the node during the row — the
    /// memory-mode rows' honest answer, and structurally checked
    /// (`tiering_tables == 0`), not assumed.
    NoTieredNamespace,
    /// Measured: the worst namespace's ratio in milli-units, over
    /// `namespaces` namespaces that had a denominator.
    Measured { milli_max: u64, namespaces: usize },
    /// At least one namespace wrote bytes while admitting none: its
    /// amplification is unbounded, so no maximum over the others
    /// describes the row. Never a pass.
    Unbounded { namespaces: usize },
}

impl Disposition {
    /// The value the `write_amplification` gate reads, in × units.
    /// `None` for dispositions that carry no measurement — the gate then
    /// reports PENDING rather than a fabricated pass.
    #[must_use]
    pub fn gate_value(&self) -> Option<f64> {
        match self {
            Disposition::Measured { milli_max, .. } => Some(*milli_max as f64 / MILLI as f64),
            // Unbounded has no finite value to give, so the gate reports
            // PENDING — never PASS — and the row's rendered disposition
            // says `UNDEFINED` out loud in the report table. Mapping it to
            // a large number instead would make a gate verdict out of an
            // undefined quantity.
            Disposition::Unbounded { .. } | Disposition::NoTieredNamespace => None,
        }
    }

    /// One report line — what the reader sees in the WA-by-row table.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Disposition::NoTieredNamespace => {
                "n/a (no tiered namespace on the node — memory-mode row)".to_string()
            }
            Disposition::Measured { milli_max, namespaces } => format!(
                "{}.{:03}× worst of {namespaces} namespace(s)",
                milli_max / MILLI,
                milli_max % MILLI
            ),
            Disposition::Unbounded { namespaces } => format!(
                "UNDEFINED — {namespaces} namespace(s) wrote bytes and admitted none \
                 (unbounded amplification; not a pass)"
            ),
        }
    }
}

/// What a row reports about the blob leg (M4-S18): the split the report
/// carries **beside** the record disposition, never inside it. The blob
/// leg has no gate of its own — `blob WA ≈ 1×` is a construction the
/// split makes checkable, not a threshold — so this type carries no
/// gate value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobDisposition {
    /// No namespace stored a value out of line during the row (or no
    /// tiered namespace existed) — absence, not a fault.
    NoBlobActivity,
    /// The worst namespace's `blob_bytes / blob_user_bytes` in
    /// milli-units, over `namespaces` namespaces with blob activity.
    Measured { milli_max: u64, namespaces: usize },
    /// At least one namespace wrote extent device bytes while admitting
    /// no blob value byte — structurally unreachable through the sealed
    /// extent lifecycle, so seeing it means the accounting is broken.
    Unbounded { namespaces: usize },
}

impl BlobDisposition {
    /// One report clause — rendered after the record disposition in the
    /// WA-by-row table.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            BlobDisposition::NoBlobActivity => "n/a (no blob activity)".to_string(),
            BlobDisposition::Measured { milli_max, namespaces } => format!(
                "{}.{:03}× worst of {namespaces} namespace(s)",
                milli_max / MILLI,
                milli_max % MILLI
            ),
            BlobDisposition::Unbounded { namespaces } => format!(
                "UNDEFINED — {namespaces} namespace(s) wrote extent bytes with no blob \
                 denominator (broken accounting; not a pass)"
            ),
        }
    }
}

/// Parses the `tiering_ns<id>:` lines out of one cell's INFO map.
fn ns_rows(cell: usize, info: &BTreeMap<String, String>) -> Result<Vec<NsRow>, String> {
    let mut rows = Vec::new();
    for (key, value) in info {
        let Some(ns) = key.strip_prefix("tiering_ns") else { continue };
        let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
        for pair in value.split(',') {
            if let Some((name, v)) = pair.split_once('=') {
                fields.insert(name, v);
            }
        }
        let field = |name: &str| -> Result<u64, String> {
            fields
                .get(name)
                .ok_or_else(|| format!("cell {cell} ns {ns}: no `{name}` field"))?
                .parse::<u64>()
                .map_err(|e| format!("cell {cell} ns {ns}: {name}: {e}"))
        };
        let (wal, flush) = (field("wal_bytes")?, field("flush_bytes")?);
        rows.push(NsRow {
            cell,
            ns: ns.to_string(),
            user_bytes: field("user_bytes")?,
            written_bytes: wal + flush,
            reported: (*fields
                .get("write_amp_milli")
                .ok_or_else(|| format!("cell {cell} ns {ns}: no `write_amp_milli` field"))?)
            .to_string(),
        });
    }
    Ok(rows)
}

/// Derives the row's disposition from a full cell scrape, cross-checking
/// the server's derived fields against an independent division.
///
/// # Errors
/// A malformed per-namespace line, a namespace whose reported ratio
/// disagrees with the recomputation, or a cell aggregate that is not the
/// maximum of its own namespace lines. Each of those makes the row invalid
/// — the numbers cannot all be right, and a report is not the place to
/// average over a contradiction.
pub fn disposition(scrape: &[BTreeMap<String, String>]) -> Result<Disposition, String> {
    let mut rows: Vec<NsRow> = Vec::new();
    for (cell, info) in scrape.iter().enumerate() {
        rows.extend(ns_rows(cell, info)?);
    }
    if rows.is_empty() {
        let tables: u64 = scrape
            .iter()
            .filter_map(|i| i.get("tiering_tables"))
            .filter_map(|v| v.parse::<u64>().ok())
            .sum();
        if tables != 0 {
            return Err(format!(
                "{tables} tiered table(s) exist but no `tiering_ns<id>:` line was rendered — \
                 the write-amplification surface is broken, not absent"
            ));
        }
        return Ok(Disposition::NoTieredNamespace);
    }

    for row in &rows {
        if row.reported != row.token() {
            return Err(format!(
                "cell {} ns {}: server reports write_amp_milli={} but user {} / written {} \
                 recomputes to {} — the row is invalid",
                row.cell,
                row.ns,
                row.reported,
                row.user_bytes,
                row.written_bytes,
                row.token()
            ));
        }
    }

    // Per-cell aggregates must be the maximum of that cell's lines: the
    // gate reads the aggregate, so a wrong maximum is a wrong gate.
    for (cell, info) in scrape.iter().enumerate() {
        let expect_max =
            rows.iter().filter(|r| r.cell == cell).filter_map(NsRow::recomputed).max().unwrap_or(0);
        let expect_unbounded = rows
            .iter()
            .filter(|r| r.cell == cell && r.recomputed().is_none() && r.written_bytes > 0)
            .count();
        let reported_max = info
            .get("tiering_write_amp_milli_max")
            .ok_or_else(|| format!("cell {cell}: no tiering_write_amp_milli_max field"))?
            .parse::<u64>()
            .map_err(|e| format!("cell {cell}: tiering_write_amp_milli_max: {e}"))?;
        let reported_unbounded = info
            .get("tiering_write_amp_undefined_ns")
            .ok_or_else(|| format!("cell {cell}: no tiering_write_amp_undefined_ns field"))?
            .parse::<usize>()
            .map_err(|e| format!("cell {cell}: tiering_write_amp_undefined_ns: {e}"))?;
        if reported_max != expect_max || reported_unbounded != expect_unbounded {
            return Err(format!(
                "cell {cell}: aggregate write-amp fields (max {reported_max}, undefined \
                 {reported_unbounded}) disagree with its own namespace lines (max {expect_max}, \
                 undefined {expect_unbounded}) — the row is invalid"
            ));
        }
    }

    let unbounded = rows.iter().filter(|r| r.recomputed().is_none() && r.written_bytes > 0).count();
    if unbounded > 0 {
        return Ok(Disposition::Unbounded { namespaces: unbounded });
    }
    let measured: Vec<u64> = rows.iter().filter_map(NsRow::recomputed).collect();
    match measured.iter().max() {
        Some(&milli_max) => Ok(Disposition::Measured { milli_max, namespaces: measured.len() }),
        // Every namespace is untouched (no user bytes, no written bytes):
        // nothing wrote, so there is nothing to amplify.
        None => Ok(Disposition::NoTieredNamespace),
    }
}

/// Parses the blob fields out of the `tiering_ns<id>:` lines (M4-S18).
/// The three fields ship together — a line missing one is malformed.
fn blob_ns_rows(cell: usize, info: &BTreeMap<String, String>) -> Result<Vec<BlobNsRow>, String> {
    let mut rows = Vec::new();
    for (key, value) in info {
        let Some(ns) = key.strip_prefix("tiering_ns") else { continue };
        let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
        for pair in value.split(',') {
            if let Some((name, v)) = pair.split_once('=') {
                fields.insert(name, v);
            }
        }
        let field = |name: &str| -> Result<u64, String> {
            fields
                .get(name)
                .ok_or_else(|| format!("cell {cell} ns {ns}: no `{name}` field"))?
                .parse::<u64>()
                .map_err(|e| format!("cell {cell} ns {ns}: {name}: {e}"))
        };
        rows.push(BlobNsRow {
            cell,
            ns: ns.to_string(),
            blob_user_bytes: field("blob_user_bytes")?,
            blob_bytes: field("blob_bytes")?,
            reported: (*fields
                .get("blob_write_amp_milli")
                .ok_or_else(|| format!("cell {cell} ns {ns}: no `blob_write_amp_milli` field"))?)
            .to_string(),
        });
    }
    Ok(rows)
}

/// Derives the row's blob disposition (M4-S18) from the same scrape as
/// [`disposition`], with the same pair assertions: per-namespace reported
/// vs recomputed, and the cell aggregates vs their own lines. Call after
/// [`disposition`] — the tables-without-lines broken-surface check lives
/// there and is not repeated here.
///
/// # Errors
/// A malformed line, a reported blob ratio that disagrees with the
/// recomputation, or an aggregate that is not the maximum of its own
/// lines — the row is refused, never averaged over.
pub fn blob_disposition(scrape: &[BTreeMap<String, String>]) -> Result<BlobDisposition, String> {
    let mut rows: Vec<BlobNsRow> = Vec::new();
    for (cell, info) in scrape.iter().enumerate() {
        rows.extend(blob_ns_rows(cell, info)?);
    }
    for row in &rows {
        if row.reported != row.token() {
            return Err(format!(
                "cell {} ns {}: server reports blob_write_amp_milli={} but blob_user {} / \
                 blob_bytes {} recomputes to {} — the row is invalid",
                row.cell,
                row.ns,
                row.reported,
                row.blob_user_bytes,
                row.blob_bytes,
                row.token()
            ));
        }
    }
    for (cell, info) in scrape.iter().enumerate() {
        let expect_max =
            rows.iter().filter(|r| r.cell == cell).filter_map(BlobNsRow::recomputed).max();
        let expect_max = expect_max.unwrap_or(0);
        let expect_unbounded = rows
            .iter()
            .filter(|r| r.cell == cell && r.recomputed().is_none() && r.blob_bytes > 0)
            .count();
        let aggregate = |name: &str| -> Result<u64, String> {
            info.get(name)
                .ok_or_else(|| format!("cell {cell}: no {name} field"))?
                .parse::<u64>()
                .map_err(|e| format!("cell {cell}: {name}: {e}"))
        };
        let reported_max = aggregate("tiering_blob_write_amp_milli_max")?;
        let reported_unbounded = aggregate("tiering_blob_write_amp_undefined_ns")?;
        if reported_max != expect_max || reported_unbounded as usize != expect_unbounded {
            return Err(format!(
                "cell {cell}: aggregate blob write-amp fields (max {reported_max}, undefined \
                 {reported_unbounded}) disagree with its own namespace lines (max {expect_max}, \
                 undefined {expect_unbounded}) — the row is invalid"
            ));
        }
    }
    let unbounded = rows.iter().filter(|r| r.recomputed().is_none() && r.blob_bytes > 0).count();
    if unbounded > 0 {
        return Ok(BlobDisposition::Unbounded { namespaces: unbounded });
    }
    let measured: Vec<u64> = rows.iter().filter_map(BlobNsRow::recomputed).collect();
    match measured.iter().max() {
        Some(&milli_max) => Ok(BlobDisposition::Measured { milli_max, namespaces: measured.len() }),
        None => Ok(BlobDisposition::NoBlobActivity),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(
        tables: u64,
        max: u64,
        undefined: u64,
        lines: &[(&str, &str)],
    ) -> BTreeMap<String, String> {
        let mut info = BTreeMap::new();
        info.insert("tiering_tables".to_string(), tables.to_string());
        info.insert("tiering_write_amp_milli_max".to_string(), max.to_string());
        info.insert("tiering_write_amp_undefined_ns".to_string(), undefined.to_string());
        // The blob aggregates ship on every scrape (M4-S18); tests that
        // exercise nonzero blob legs overwrite these two keys.
        info.insert("tiering_blob_write_amp_milli_max".to_string(), "0".to_string());
        info.insert("tiering_blob_write_amp_undefined_ns".to_string(), "0".to_string());
        for (ns, value) in lines {
            info.insert(format!("tiering_ns{ns}"), (*value).to_string());
        }
        info
    }

    fn ns_line(user: u64, wal: u64, flush: u64, reported: &str) -> String {
        format!(
            "head=0,flushed=0,ro_boundary=0,tail=0,committed_bytes=0,budget_bytes=0,\
             live_bytes=0,dead_bytes=0,user_bytes={user},wal_bytes={wal},flush_bytes={flush},\
             compaction_bytes=999999,write_amp_milli={reported},blob_user_bytes=0,blob_bytes=0,\
             blob_write_amp_milli=undefined"
        )
    }

    /// A line with blob activity: quiet record leg, parameterized blob leg.
    fn blob_ns_line(blob_user: u64, blob_bytes: u64, reported: &str) -> String {
        format!(
            "head=0,flushed=0,ro_boundary=0,tail=0,committed_bytes=0,budget_bytes=0,\
             live_bytes=0,dead_bytes=0,user_bytes=10,wal_bytes=10,flush_bytes=10,\
             compaction_bytes=0,write_amp_milli=2000,blob_user_bytes={blob_user},\
             blob_bytes={blob_bytes},blob_write_amp_milli={reported}"
        )
    }

    /// A memory-mode node has no namespace line and no tables: the honest
    /// disposition, not a fabricated zero.
    #[test]
    fn memory_mode_row_reports_no_tiered_namespace() {
        let scrape = vec![cell(0, 0, 0, &[])];
        assert_eq!(disposition(&scrape).expect("valid"), Disposition::NoTieredNamespace);
    }

    /// Tables exist but no line was rendered: broken surface, refused row
    /// — the failure mode a bare "0 namespaces" answer would have hidden.
    #[test]
    fn tables_without_lines_is_a_broken_surface() {
        let scrape = vec![cell(2, 0, 0, &[])];
        let err = disposition(&scrape).expect_err("refused");
        assert!(err.contains("broken, not absent"), "{err}");
    }

    /// The worst namespace wins, across cells, and the relocation volume
    /// (`compaction_bytes`) never enters the ratio (ADR-0060 D2).
    #[test]
    fn worst_namespace_across_cells_is_the_gate_value() {
        let scrape = vec![
            cell(1, 1_500, 0, &[("1", &ns_line(1_000, 1_000, 500, "1500"))]),
            cell(1, 6_000, 0, &[("2", &ns_line(100, 300, 300, "6000"))]),
        ];
        let d = disposition(&scrape).expect("valid");
        assert_eq!(d, Disposition::Measured { milli_max: 6_000, namespaces: 2 });
        assert!((d.gate_value().expect("measured") - 6.0).abs() < 1e-9);
        assert!(d.render().contains("6.000×"), "{}", d.render());
    }

    /// A namespace that wrote bytes and admitted none makes the row
    /// undefined — and undefined carries no gate value, so it can never
    /// arrive as a pass.
    #[test]
    fn unbounded_namespace_refuses_to_become_a_number() {
        let scrape = vec![cell(1, 0, 1, &[("3", &ns_line(0, 4_096, 0, "undefined"))])];
        let d = disposition(&scrape).expect("valid");
        assert_eq!(d, Disposition::Unbounded { namespaces: 1 });
        assert_eq!(d.gate_value(), None);
        assert!(d.render().starts_with("UNDEFINED"), "{}", d.render());
    }

    /// The pair assertion: a server-derived field that disagrees with the
    /// raw counters invalidates the row instead of being averaged in.
    #[test]
    fn reported_ratio_must_match_the_recomputation() {
        let scrape = vec![cell(1, 1_200, 0, &[("4", &ns_line(1_000, 1_000, 500, "1200"))])];
        let err = disposition(&scrape).expect_err("refused");
        assert!(err.contains("recomputes to 1500"), "{err}");
    }

    /// …and so does an aggregate that is not the maximum of its own lines.
    #[test]
    fn aggregate_must_be_the_max_of_its_lines() {
        let scrape = vec![cell(
            1,
            1_500,
            0,
            &[
                ("5", &ns_line(1_000, 1_000, 500, "1500")),
                ("6", &ns_line(1_000, 2_000, 500, "2500")),
            ],
        )];
        let err = disposition(&scrape).expect_err("refused");
        assert!(err.contains("disagree with its own namespace lines"), "{err}");
    }

    /// M4-S18: a row whose namespaces stored nothing out of line reports
    /// blob absence honestly — and the record-leg fixtures carry exactly
    /// that shape, so every record test above doubles as this arm's.
    #[test]
    fn blob_split_reports_absence_when_nothing_is_out_of_line() {
        let scrape = vec![cell(1, 1_500, 0, &[("1", &ns_line(1_000, 1_000, 500, "1500"))])];
        let d = blob_disposition(&scrape).expect("valid");
        assert_eq!(d, BlobDisposition::NoBlobActivity);
        assert!(d.render().contains("no blob activity"), "{}", d.render());
    }

    /// The blob split is its own worst-of ratio: the record leg's 2.0×
    /// stays in its own column, and the ≈ 1× blob construction is visible
    /// as a number, not a hope.
    #[test]
    fn blob_split_measures_the_worst_blob_namespace() {
        let mut info = cell(
            1,
            2_000,
            0,
            &[
                ("1", &blob_ns_line(1_000_000, 1_000_988, "1001")),
                ("2", &blob_ns_line(1_000, 1_004, "1004")),
            ],
        );
        info.insert("tiering_blob_write_amp_milli_max".to_string(), "1004".to_string());
        let d = blob_disposition(&[info]).expect("valid");
        assert_eq!(d, BlobDisposition::Measured { milli_max: 1_004, namespaces: 2 });
        assert!(d.render().contains("1.004×"), "{}", d.render());
    }

    /// The pair assertion holds on the blob leg too: a server-derived
    /// blob ratio that disagrees with the raw counters refuses the row.
    #[test]
    fn blob_reported_ratio_must_match_the_recomputation() {
        let mut info = cell(1, 2_000, 0, &[("3", &blob_ns_line(1_000_000, 1_000_988, "1000"))]);
        info.insert("tiering_blob_write_amp_milli_max".to_string(), "1000".to_string());
        let err = blob_disposition(&[info]).expect_err("refused");
        assert!(err.contains("recomputes to 1001"), "{err}");
    }

    /// …and so does a blob aggregate that is not the maximum of its own
    /// lines.
    #[test]
    fn blob_aggregate_must_be_the_max_of_its_lines() {
        let info = cell(1, 2_000, 0, &[("4", &blob_ns_line(1_000, 1_004, "1004"))]);
        // The helper's default aggregate (0) understates the line's 1004.
        let err = blob_disposition(&[info]).expect_err("refused");
        assert!(err.contains("disagree with its own namespace lines"), "{err}");
    }

    /// Extent bytes with no blob denominator is broken accounting (the
    /// sealed-extent lifecycle cannot produce it) — surfaced, never a
    /// pass.
    #[test]
    fn blob_bytes_without_a_denominator_is_unbounded() {
        let mut info = cell(1, 2_000, 0, &[("5", &blob_ns_line(0, 4_096, "undefined"))]);
        info.insert("tiering_blob_write_amp_undefined_ns".to_string(), "1".to_string());
        let d = blob_disposition(&[info]).expect("valid");
        assert_eq!(d, BlobDisposition::Unbounded { namespaces: 1 });
        assert!(d.render().starts_with("UNDEFINED"), "{}", d.render());
    }
}
