use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn required_generator_disposition_cannot_be_omitted() {
    let mut measurements = Measurements::new();
    measurements.set("probe:value", 0.5);
    for milestone in ["m0", "m1", "m2"] {
        let (result, body) = report_for(milestone, &[gate("any", false)], &measurements, true);
        assert!(result.unwrap_err().contains("generator saturation"));
        assert!(body.contains("UNMEASURED"));
        assert!(!body.contains("status: COMPLETE"));
    }
}

fn gate(tier: &str, informational: bool) -> gates::Gate {
    gates::Gate {
        id: "probe".into(),
        name: "probe".into(),
        source: "probe:value".into(),
        threshold: 1.0,
        comparator: "<=".into(),
        unit: "x".into(),
        tier: tier.into(),
        informational,
    }
}

fn report(
    gates: &[gates::Gate],
    m: &Measurements,
    reference: bool,
) -> (Result<(), String>, String) {
    // These tests isolate gate thresholds; M0/M1/M2 probe validity has its own fixtures.
    report_for("m4", gates, m, reference)
}

fn report_for(
    milestone: &str,
    gates: &[gates::Gate],
    m: &Measurements,
    reference: bool,
) -> (Result<(), String>, String) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "inf-bench-verdict-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result =
        finish_report(milestone, gates, m, true, reference, root.to_str().unwrap(), "test");
    let dir = std::fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
    let body = std::fs::read_to_string(dir.join("report.md")).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    (result, body)
}

#[test]
fn every_missing_stop_lists_its_identity_and_source_in_the_report() {
    let mut gates = vec![gate("any", false), gate("linux-reference-box", false)];
    gates[1].id = "reference".into();
    gates[1].source = "external:absent".into();
    for reference in [false, true] {
        let (result, body) = report(&gates, &Measurements::new(), reference);
        let error = result.unwrap_err();
        for expected in ["probe (probe:value)", "reference (external:absent)"] {
            assert!(error.contains(expected), "{error}");
            assert!(body.contains(expected), "{body}");
        }
        assert!(body.contains("INCOMPLETE / FAILED — NOT citation-grade"));
        assert!(!body.contains("status: COMPLETE"));
    }
}

#[test]
fn nan_and_infinite_measurements_cannot_satisfy_stop_gates() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut m = Measurements::new();
        m.set("probe:value", value);
        let (result, body) = report(&[gate("any", false)], &m, true);
        assert!(result.unwrap_err().contains("unmeasured STOP"));
        assert!(body.contains("UNMEASURED STOP"));
    }
}

#[test]
fn measured_thresholds_keep_their_tier_and_informational_rules() {
    for reference in [false, true] {
        for (tier, informational) in [("any", false), ("linux-reference-box", false), ("any", true)]
        {
            for value in [0.5, 2.0] {
                let mut m = Measurements::new();
                m.set("probe:value", value);
                let (result, _) = report(&[gate(tier, informational)], &m, reference);
                let fails = value > 1.0 && !informational && (tier == "any" || reference);
                assert_eq!(result.is_err(), fails, "{tier} {informational} {value} {reference}");
            }
        }
    }
}

#[test]
fn missing_informational_measurement_does_not_fail_a_complete_run() {
    let (result, body) = report(&[gate("any", true)], &Measurements::new(), true);
    result.unwrap();
    assert!(body.contains("UNMEASURED (informational)"));
    assert!(body.contains("status: COMPLETE"));
}

#[test]
fn operational_failure_is_fatal_even_when_every_gate_passes() {
    for reference in [false, true] {
        let mut m = Measurements::new();
        m.set("probe:value", 0.5);
        m.fail("Dragonfly comparator startup failed: absent");
        m.fail("Redis RSS startup failed: permission denied");
        let (result, body) = report(&[gate("any", false)], &m, reference);
        let error = result.unwrap_err();
        for reason in &m.failures {
            assert!(error.contains(reason));
            assert!(body.contains(reason));
        }
        assert!(body.contains("NOT citation-grade"));
    }
}
