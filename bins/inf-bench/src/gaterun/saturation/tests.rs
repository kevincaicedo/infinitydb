use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn sample(rate: f64) -> LoadReport {
    LoadReport { ops: 100, ops_per_sec: rate, elapsed_s: 1.0, ..Default::default() }
}

#[test]
fn probe_changes_only_connections_and_rounds_up_with_a_hard_bound() {
    let baseline = LoadSpec {
        conns: 3,
        pipeline: 7,
        duration: Duration::from_millis(1500),
        warmup: Duration::from_millis(333),
        seed: 42,
        key_size: 55,
        value_size: 321,
        ttl_range_ms: Some((10, 300)),
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"durable".to_vec()]],
        ..Default::default()
    };
    for (connections, expected) in [(1, 2), (3, 5), (64, 96), (512, 768), (682, 1023)] {
        let baseline = LoadSpec { conns: connections, ..baseline.clone() };
        let mut probe = probe_spec(&baseline).unwrap();
        assert_eq!(probe.conns, expected);
        probe.conns = connections;
        assert_eq!(format!("{probe:?}"), format!("{baseline:?}"));
    }
    for connections in [0, 683, usize::MAX] {
        assert!(probe_spec(&LoadSpec { conns: connections, ..baseline.clone() }).is_err());
    }
    for spec in [
        LoadSpec { duration: Duration::ZERO, ..baseline.clone() },
        LoadSpec { pipeline: 0, ..baseline.clone() },
        LoadSpec { fill: Some(100), ..baseline.clone() },
        LoadSpec { target_ops_per_sec: Some(100), ..baseline },
    ] {
        assert!(probe_spec(&spec).is_err());
    }
}

#[test]
fn sensitivity_boundaries_and_direction_have_distinct_dispositions() {
    for (rate, expected) in [
        (100.0, Disposition::Plateau),
        (104.999, Disposition::Plateau),
        (95.001, Disposition::Plateau),
        (105.0, Disposition::GeneratorLimited),
        (150.0, Disposition::GeneratorLimited),
        (95.0, Disposition::Inconclusive),
        (50.0, Disposition::Inconclusive),
    ] {
        assert_eq!(classify(&sample(100.0), &sample(rate)).unwrap().0, expected);
    }
}

#[test]
fn invalid_samples_never_classify_as_a_plateau() {
    let mut invalid = vec![
        LoadReport { ops: 0, ..sample(100.0) },
        LoadReport { errors: 1, ..sample(100.0) },
        LoadReport { errors: 100, busy_retryable: 100, ..sample(100.0) },
    ];
    for value in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        invalid.push(LoadReport { ops_per_sec: value, ..sample(100.0) });
        invalid.push(LoadReport { elapsed_s: value, ..sample(100.0) });
    }
    for invalid in invalid {
        assert!(classify(&invalid, &sample(100.0)).is_err());
        assert!(classify(&sample(100.0), &invalid).is_err());
    }
    assert!(classify(&sample(f64::MIN_POSITIVE), &sample(f64::MAX)).is_err());
}

fn report(milestone: &str, measurements: &Measurements, reference: bool) -> (bool, String) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "inf-generator-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = crate::gaterun::finish_report(
        milestone,
        &[],
        measurements,
        true,
        reference,
        root.to_str().unwrap(),
        "probe test",
    );
    let dir = std::fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
    let text = std::fs::read_to_string(dir.join("report.md")).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    (result.is_ok(), text)
}

#[test]
fn every_gate_entry_point_invalidates_bad_probes_in_both_tiers() {
    for milestone in ["m0", "m1", "m2"] {
        for reference in [false, true] {
            for (probe, valid, label) in [
                (Ok(sample(100.0)), true, "PLATEAU"),
                (Ok(sample(105.0)), false, "GENERATOR-LIMITED"),
                (Ok(sample(95.0)), false, "INCONCLUSIVE"),
                (Ok(sample(0.0)), false, "UNMEASURED"),
                (Err("probe connection failed".into()), false, "UNMEASURED"),
            ] {
                let mut measurements = Measurements::new();
                measurements.set("loadgen:unchanged", 123.0);
                measurements.record_generator_probe(
                    "fixture",
                    &LoadSpec::default(),
                    &sample(100.0),
                    probe,
                );
                let (success, text) = report(milestone, &measurements, reference);
                assert_eq!(success, valid, "{text}");
                assert!(text.contains(label), "{text}");
                assert_eq!(text.contains("status: COMPLETE"), valid);
                assert_eq!(text.contains("NOT citation-grade"), !valid);
                assert!(text.contains("64 -> 96"));
                assert!(text.contains("limiting resource are not established"));
                assert_eq!(measurements.values["loadgen:unchanged"], 123.0);
                assert!(text.contains("generator fixture baseline"));
            }
        }
    }
}

#[test]
fn one_healthy_probe_cannot_hide_an_invalid_row_or_a_failed_probe() {
    let mut measurements = Measurements::new();
    measurements.record_generator_probe(
        "good",
        &LoadSpec::default(),
        &sample(100.0),
        Ok(sample(100.0)),
    );
    measurements.record_generator_probe(
        "bad",
        &LoadSpec::default(),
        &sample(100.0),
        Err("offline".into()),
    );
    let (success, text) = report("m2", &measurements, false);
    assert!(!success);
    assert!(text.contains("bad: UNMEASURED (offline;"));
}
