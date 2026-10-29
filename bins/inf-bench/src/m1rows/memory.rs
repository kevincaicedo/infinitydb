//! Eviction reports compare cell-owned logical bytes with one node total.
use std::collections::BTreeMap;

use crate::gaterun::{Measurements, sum_field};

pub(super) fn record_eviction_memory(
    measurements: &mut Measurements,
    infos: &[BTreeMap<String, String>],
    limit: u64,
) -> Result<(), String> {
    if infos.is_empty() || limit == 0 {
        return Err("eviction memory requires cell scrapes and a positive limit".into());
    }
    let mut resident = 0;
    for info in infos {
        if info.get("memory_scope").map(String::as_str) != Some("node") {
            return Err("eviction memory requires memory_scope:node on every scrape".into());
        }
        let bytes = info
            .get("used_memory")
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or("eviction memory requires an integer used_memory on every scrape")?;
        resident = resident.max(bytes);
    }
    let evicted = sum_field(infos, "evicted_keys");
    let logical = sum_field(infos, "records_live_bytes")
        + sum_field(infos, "index_bytes")
        + sum_field(infos, "wheel_bytes")
        + sum_field(infos, "evict_bytes");
    measurements.set("loadgen:eviction_used_over_limit", logical as f64 / limit as f64);
    measurements.note(format!(
        "eviction pressure: {evicted} evictions; logical {logical} B vs limit {limit} B \
         (resident incl. slack/buffers: {resident} B)"
    ));
    measurements
        .note("eviction resident: maximum observed node total; asynchronous INFO snapshots");
    if evicted == 0 {
        measurements.note("WARNING: zero evictions — the row did not generate pressure");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_snapshots_can_differ_but_their_maximum_is_not_a_sum() {
        let infos = [100, 99, 104, 102].map(|bytes| {
            [("memory_scope".into(), "node".into()), ("used_memory".into(), bytes.to_string())]
                .into()
        });
        let mut measurements = Measurements::new();
        record_eviction_memory(&mut measurements, &infos, 1024).unwrap();
        assert!(measurements.notes[0].contains("resident incl. slack/buffers: 104 B"));
    }

    #[test]
    fn invalid_node_memory_never_silently_becomes_zero() {
        let valid: BTreeMap<String, String> =
            [("memory_scope".into(), "node".into()), ("used_memory".into(), "100".into())].into();
        for scope in [None, Some("cell"), Some("unknown")] {
            let mut invalid = valid.clone();
            invalid.remove("memory_scope");
            if let Some(scope) = scope {
                invalid.insert("memory_scope".into(), scope.into());
            }
            let mut measurements = Measurements::new();
            assert!(
                record_eviction_memory(&mut measurements, &[valid.clone(), invalid], 1).is_err()
            );
            assert!(measurements.values.is_empty());
        }
        for value in [None, Some("-1"), Some("NaN"), Some("18446744073709551616")] {
            let mut invalid = valid.clone();
            invalid.remove("used_memory");
            if let Some(value) = value {
                invalid.insert("used_memory".into(), value.into());
            }
            assert!(record_eviction_memory(&mut Measurements::new(), &[invalid], 1).is_err());
        }
        assert!(record_eviction_memory(&mut Measurements::new(), &[], 1).is_err());
        assert!(record_eviction_memory(&mut Measurements::new(), &[valid], 0).is_err());
    }

    #[test]
    fn eviction_memory_counts_node_once_and_cell_domains_once_each() {
        for cells in [1, 2, 4, 8] {
            let infos: Vec<_> = (0..cells)
                .map(|_| {
                    [
                        ("memory_scope".into(), "node".into()),
                        ("used_memory".into(), "4096".into()),
                        ("records_live_bytes".into(), "100".into()),
                        ("index_bytes".into(), "20".into()),
                        ("wheel_bytes".into(), "3".into()),
                        ("evict_bytes".into(), "4".into()),
                    ]
                    .into()
                })
                .collect();
            let mut measurements = Measurements::new();
            record_eviction_memory(&mut measurements, &infos, 1024).unwrap();
            assert!(measurements.notes[0].contains("resident incl. slack/buffers: 4096 B"));
            assert_eq!(
                measurements.values["loadgen:eviction_used_over_limit"],
                f64::from(cells * 127) / 1024.0
            );
        }
    }
}
