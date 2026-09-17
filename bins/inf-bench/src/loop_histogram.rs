//! Schema-1 loop snapshots and checked loaded-window arithmetic (ADR-0136).

use std::collections::BTreeMap;

use inf_foundation::LogHistogram;

/// Dense decimal counts need at most 40319 bytes; metadata fits in the remainder.
pub const REPLY_BYTES_MAX: usize = 64 * 1024;

#[derive(Debug)]
pub struct Snapshot {
    pub cell: u16,
    pub cells: u16,
    pub run_id: String,
    pub samples: u64,
    submits: u64,
    sqes: u64,
    counts: Box<[u64; LogHistogram::BUCKET_COUNT]>,
}

fn field<'a>(info: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str, String> {
    info.get(name).map(String::as_str).ok_or_else(|| format!("missing {name}"))
}

fn integer(value: &str) -> Result<u64, String> {
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid histogram integer".into());
    }
    value.parse().map_err(|_| "histogram integer overflow".into())
}

fn number(info: &BTreeMap<String, String>, name: &str) -> Result<u64, String> {
    integer(field(info, name)?).map_err(|error| format!("{name}: {error}"))
}

/// Fixed width and bounded bytes; a truncated list is never a zero tail.
pub fn decode_counts(text: &str) -> Result<Box<[u64; LogHistogram::BUCKET_COUNT]>, String> {
    if text.len() > LogHistogram::BUCKET_COUNT * 21 - 1 {
        return Err("histogram counts exceed schema-1 byte bound".into());
    }
    let mut counts = Box::new([0; LogHistogram::BUCKET_COUNT]);
    let mut fields = text.split(',');
    for count in counts.iter_mut() {
        *count = integer(fields.next().ok_or("histogram buckets missing")?)?;
    }
    if fields.next().is_some() {
        return Err("extra histogram buckets".into());
    }
    Ok(counts)
}

impl Snapshot {
    /// Persist the arithmetic inputs with the run, independently of rounded gate values.
    pub fn render(&self) -> String {
        use core::fmt::Write;
        let mut text = format!(
            "cell={} cells={} run_id={} samples={} submits={} sqes={}\ncounts=",
            self.cell, self.cells, self.run_id, self.samples, self.submits, self.sqes
        );
        for (index, count) in self.counts.iter().enumerate() {
            if index != 0 {
                text.push(',');
            }
            let _ = write!(text, "{count}");
        }
        text.push('\n');
        text
    }

    /// Decode only the schema's bounded fields, rejecting duplicate INFO names.
    pub fn decode_info(reply: &[u8]) -> Result<Option<Self>, String> {
        if reply.len() > REPLY_BYTES_MAX {
            return Err("loop histogram reply exceeds byte budget".into());
        }
        let text = core::str::from_utf8(reply).map_err(|_| "invalid loop histogram UTF-8")?;
        let mut info = BTreeMap::new();
        for line in text.lines() {
            let Some((name, value)) = line.split_once(':') else { continue };
            if !matches!(name, "cell" | "cells" | "run_id") && !name.starts_with("loop_histogram_")
            {
                continue;
            }
            if info.len() == 16 || name.len() > 64 {
                return Err("too many loop histogram fields".into());
            }
            if info.insert(name.to_owned(), value.to_owned()).is_some() {
                return Err(format!("duplicate loop histogram field {name}"));
            }
        }
        Self::parse(&info)
    }

    /// `None` is the explicitly pending first request, never a usable measurement.
    pub fn parse(info: &BTreeMap<String, String>) -> Result<Option<Self>, String> {
        if field(info, "loop_histogram_schema")? != "1" {
            return Err("unsupported loop histogram schema".into());
        }
        let cell = u16::try_from(number(info, "cell")?).map_err(|_| "invalid cell")?;
        let cells = u16::try_from(number(info, "cells")?).map_err(|_| "invalid cells")?;
        let run_id = field(info, "run_id")?;
        if cell >= cells || run_id.len() != 40 || !run_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid loop histogram identity".into());
        }
        if info.contains_key("loop_histogram_pending") {
            if field(info, "loop_histogram_pending")? != "1"
                || info.keys().any(|key| {
                    key.starts_with("loop_histogram_")
                        && key != "loop_histogram_schema"
                        && key != "loop_histogram_pending"
                })
            {
                return Err("invalid pending loop histogram".into());
            }
            return Ok(None);
        }
        let counts = decode_counts(field(info, "loop_histogram_counts")?)?;
        let samples = number(info, "loop_histogram_samples")?;
        let sum = counts.iter().try_fold(0u64, |sum, count| sum.checked_add(*count));
        if sum != Some(samples) || samples != number(info, "loop_histogram_iterations")? {
            return Err("inconsistent loop histogram sample count".into());
        }
        Ok(Some(Self {
            cell,
            cells,
            run_id: run_id.to_owned(),
            samples,
            counts,
            submits: number(info, "loop_histogram_submits")?,
            sqes: number(info, "loop_histogram_sqes")?,
        }))
    }
}

#[derive(Debug)]
pub struct CellWindow {
    pub cell: u16,
    pub samples: u64,
    pub p999_us: u64,
    submits: u64,
    sqes: u64,
}

fn delta(before: u64, after: u64, field: &str) -> Result<u64, String> {
    after.checked_sub(before).ok_or_else(|| format!("non-monotone {field}"))
}

impl CellWindow {
    fn between(before: &Snapshot, after: &Snapshot) -> Result<Self, String> {
        if before.cell != after.cell || before.cells != after.cells || before.run_id != after.run_id
        {
            return Err("loop histogram identity changed across window".into());
        }
        let samples = delta(before.samples, after.samples, "loop histogram samples")?;
        if samples == 0 {
            return Err("empty loop histogram window".into());
        }
        let rank = (u128::from(samples) * 999).div_ceil(1000);
        let mut seen = 0u128;
        let mut p999_us = None;
        for (index, (&begin, &end)) in before.counts.iter().zip(after.counts.iter()).enumerate() {
            seen += u128::from(delta(begin, end, "loop histogram bucket")?);
            if seen >= rank && p999_us.is_none() {
                p999_us = LogHistogram::bucket_upper_bound(index);
            }
        }
        if seen != u128::from(samples) {
            return Err("inconsistent loop histogram delta".into());
        }
        Ok(Self {
            cell: before.cell,
            samples,
            p999_us: p999_us.ok_or("missing loop histogram percentile")?,
            submits: delta(before.submits, after.submits, "raw_submits")?,
            sqes: delta(before.sqes, after.sqes, "raw_sqes")?,
        })
    }
}

#[derive(Debug)]
pub struct LoadWindow {
    pub cells: Vec<CellWindow>,
    pub sqes_per_submit: f64,
    pub p999_us: u64,
}

impl LoadWindow {
    pub fn between(before: &[Snapshot], after: &[Snapshot]) -> Result<Self, String> {
        let expected = before.first().ok_or("no loop histogram cells")?.cells;
        if before.len() != usize::from(expected) || after.len() != before.len() {
            return Err("incomplete loop histogram cell set".into());
        }
        let mut cells = Vec::with_capacity(before.len());
        let (mut submits, mut sqes, mut p999_us) = (0u64, 0u64, 0);
        for (index, (begin, end)) in before.iter().zip(after).enumerate() {
            if usize::from(begin.cell) != index
                || begin.cells != expected
                || begin.run_id != before[0].run_id
            {
                return Err("inconsistent loop histogram cell set".into());
            }
            let window = CellWindow::between(begin, end)?;
            submits = submits.checked_add(window.submits).ok_or("raw_submits sum overflow")?;
            sqes = sqes.checked_add(window.sqes).ok_or("raw_sqes sum overflow")?;
            p999_us = p999_us.max(window.p999_us);
            cells.push(window);
        }
        if submits == 0 {
            return Err("empty submission window".into());
        }
        Ok(Self { cells, sqes_per_submit: sqes as f64 / submits as f64, p999_us })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(cell: u16, cells: u16, values: &[(u64, u64)]) -> Snapshot {
        let mut counts = Box::new([0u64; LogHistogram::BUCKET_COUNT]);
        for &(value, count) in values {
            let mut histogram = LogHistogram::new();
            histogram.record(value);
            let index = histogram.bucket_counts().iter().position(|&n| n == 1).unwrap();
            counts[index] += count;
        }
        Snapshot {
            cell,
            cells,
            run_id: "0".repeat(40),
            samples: counts.iter().sum(),
            submits: 0,
            sqes: 0,
            counts,
        }
    }

    fn wire(snapshot: &Snapshot) -> Vec<u8> {
        format!(
            "cell:{}\r\ncells:{}\r\nrun_id:{}\r\nloop_histogram_schema:1\r\n\
             loop_histogram_samples:{}\r\nloop_histogram_iterations:{}\r\n\
             loop_histogram_submits:{}\r\nloop_histogram_sqes:{}\r\n\
             loop_histogram_counts:{}\r\n",
            snapshot.cell,
            snapshot.cells,
            snapshot.run_id,
            snapshot.samples,
            snapshot.samples,
            snapshot.submits,
            snapshot.sqes,
            snapshot.counts.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
        )
        .into_bytes()
    }

    #[test]
    fn cheap_history_and_a_busy_peer_cannot_dilute_the_slow_cell() {
        let before = [snapshot(0, 2, &[(0, 1_000_000)]), snapshot(1, 2, &[(0, 1_000_000)])];
        let mut after =
            [snapshot(0, 2, &[(0, 11_000_000)]), snapshot(1, 2, &[(0, 1_000_000), (1000, 200)])];
        after[0].submits = 100;
        after[0].sqes = 1600;
        after[1].submits = 10;
        after[1].sqes = 160;
        let window = LoadWindow::between(&before, &after).unwrap();
        assert_eq!(window.p999_us, 1007);
        assert_eq!(window.cells[1].samples, 200);
        assert_eq!(window.cells[0].p999_us, 0);
        assert_eq!(window.sqes_per_submit, 16.0);
    }

    #[test]
    fn percentile_rank_uses_exact_ceiling_at_zero_one_and_extreme_counts() {
        let before = snapshot(0, 1, &[]);
        let rank = u64::MAX - u64::MAX / 1000;
        for (values, expected) in [
            (vec![(0, 999), (1000, 1)], 0),
            (vec![(0, 998), (1000, 2)], 1007),
            (vec![(500, 1)], 503),
            (vec![(0, rank - 1), (1000, u64::MAX - rank + 1)], 1007),
            (vec![(u64::MAX, u64::MAX)], u64::MAX),
        ] {
            let after = snapshot(0, 1, &values);
            assert_eq!(CellWindow::between(&before, &after).unwrap().p999_us, expected);
        }
        assert!(CellWindow::between(&before, &before).unwrap_err().contains("empty"));
    }

    #[test]
    fn bucket_rollback_is_refused_even_when_total_samples_increase() {
        let before = snapshot(0, 1, &[(0, 10), (1000, 10)]);
        let after = snapshot(0, 1, &[(0, 100), (1000, 9)]);
        assert!(CellWindow::between(&before, &after).unwrap_err().contains("bucket"));
        assert!(CellWindow::between(&after, &before).unwrap_err().contains("samples"));
    }

    #[test]
    fn every_cell_is_validated_before_counter_aggregation() {
        let mut before = [snapshot(0, 2, &[(0, 1)]), snapshot(1, 2, &[(0, 1)])];
        let mut after = [snapshot(0, 2, &[(0, 2)]), snapshot(1, 2, &[(0, 2)])];
        before[0].submits = 1;
        after[1].submits = 100;
        assert!(LoadWindow::between(&before, &after).unwrap_err().contains("raw_submits"));
        before[0].submits = 0;
        before[0].sqes = 1;
        after[1].sqes = 1600;
        assert!(LoadWindow::between(&before, &after).unwrap_err().contains("raw_sqes"));
        before[0].sqes = 0;
        after[0].submits = u64::MAX;
        assert!(LoadWindow::between(&before, &after).unwrap_err().contains("sum overflow"));
        after[0].submits = 0;
        after[1].submits = 0;
        assert!(LoadWindow::between(&before, &after).unwrap_err().contains("empty submission"));
    }

    #[test]
    fn missing_duplicate_and_restarted_cells_cannot_produce_a_window() {
        let before = [snapshot(0, 2, &[(0, 1)]), snapshot(1, 2, &[(0, 1)])];
        let mut after = [snapshot(0, 2, &[(0, 2)]), snapshot(1, 2, &[(0, 2)])];
        assert!(LoadWindow::between(&[], &[]).is_err());
        assert!(LoadWindow::between(&before, &after[..1]).is_err());
        after[1].cell = 0;
        assert!(LoadWindow::between(&before, &after).unwrap_err().contains("identity"));
        after[1].cell = 1;
        after[1].run_id = "1".repeat(40);
        assert!(LoadWindow::between(&before, &after).unwrap_err().contains("identity"));
    }

    #[test]
    fn decoder_rejects_truncation_excess_duplicates_overflow_and_missing_fields() {
        let valid = wire(&snapshot(0, 1, &[(0, 1000)]));
        assert_eq!(Snapshot::decode_info(&valid).unwrap().unwrap().samples, 1000);
        let text = String::from_utf8(valid).unwrap();
        for bad in [
            text.replace("schema:1", "schema:2"),
            text.replace("samples:1000", "samples:999"),
            text.replace("iterations:1000", "iterations:999"),
            text.replace("submits:0", "submits:+1"),
            text.replace("sqes:0", "sqes:18446744073709551616"),
            text.replace("cell:0", "cell:1"),
            text.replace("cells:1", "cells:0"),
            text.replace("loop_histogram_submits:0\r\n", ""),
            format!("{text}loop_histogram_samples:1000\r\n"),
            format!("{text}loop_histogram_pending:1\r\n"),
        ] {
            assert!(Snapshot::decode_info(bad.as_bytes()).is_err(), "{bad}");
        }
        let counts = "0,".repeat(LogHistogram::BUCKET_COUNT - 1) + "0";
        for bad in [
            counts[..counts.len() - 2].to_owned(),
            counts.clone() + ",0",
            counts.replacen('0', "-1", 1),
            "0".repeat(REPLY_BYTES_MAX),
        ] {
            assert!(decode_counts(&bad).is_err());
        }
        let mut overflow = snapshot(0, 1, &[(0, u64::MAX)]);
        overflow.counts[1] = 1;
        assert!(Snapshot::decode_info(&wire(&overflow)).unwrap_err().contains("sample count"));
        assert!(Snapshot::decode_info(&[255]).is_err());
        assert!(Snapshot::decode_info(&vec![0; REPLY_BYTES_MAX + 1]).is_err());
    }
}
