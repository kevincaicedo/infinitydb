//! Connection sensitivity is a disposition, not proof of server capacity (ADR-0135).
use super::Measurements;
use crate::load::{LoadMode, LoadReport, LoadSpec, render, run};

const MAX_PROBE_CONNECTIONS: usize = 1024;
const SENSITIVITY_PERCENT: f64 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    Plateau,
    GeneratorLimited,
    Inconclusive,
    Unmeasured,
}

impl Disposition {
    fn label(self) -> &'static str {
        match self {
            Self::Plateau => "PLATEAU",
            Self::GeneratorLimited => "GENERATOR-LIMITED",
            Self::Inconclusive => "INCONCLUSIVE",
            Self::Unmeasured => "UNMEASURED",
        }
    }
}

pub(super) struct GeneratorProbe {
    name: String,
    disposition: Disposition,
    detail: String,
}

fn probe_spec(baseline: &LoadSpec) -> Result<LoadSpec, String> {
    if baseline.duration.is_zero() || baseline.conns == 0 || baseline.pipeline == 0 {
        return Err("duration, connections and pipeline must be positive".into());
    }
    if baseline.fill.is_some() || baseline.target_ops_per_sec.is_some_and(|rate| rate > 0) {
        return Err("connection probe requires a steady closed-loop workload".into());
    }
    let connections = baseline.conns.checked_add(baseline.conns.div_ceil(2));
    let Some(conns) = connections.filter(|count| *count <= MAX_PROBE_CONNECTIONS) else {
        return Err(format!("probe exceeds {MAX_PROBE_CONNECTIONS} connections"));
    };
    Ok(LoadSpec { conns, ..baseline.clone() })
}

fn validate_sample(sample: &LoadReport) -> Result<(), String> {
    if sample.ops == 0 || !sample.ops_per_sec.is_finite() || sample.ops_per_sec <= 0.0 {
        return Err("no finite positive throughput sample".into());
    }
    if !sample.elapsed_s.is_finite() || sample.elapsed_s <= 0.0 {
        return Err("no finite positive measurement interval".into());
    }
    sample.require_no_errors()?;
    if sample.mode != LoadMode::ClosedLoop {
        return Err("sample was not closed-loop".into());
    }
    Ok(())
}

fn classify(baseline: &LoadReport, probe: &LoadReport) -> Result<(Disposition, f64), String> {
    validate_sample(baseline).map_err(|error| format!("baseline: {error}"))?;
    validate_sample(probe).map_err(|error| format!("probe: {error}"))?;
    let delta = (probe.ops_per_sec - baseline.ops_per_sec) / baseline.ops_per_sec * 100.0;
    if !delta.is_finite() {
        return Err("non-finite throughput change".into());
    }
    let disposition = if delta >= SENSITIVITY_PERCENT {
        Disposition::GeneratorLimited
    } else if delta <= -SENSITIVITY_PERCENT {
        Disposition::Inconclusive
    } else {
        Disposition::Plateau
    };
    Ok((disposition, delta))
}

impl Measurements {
    /// Call after the measured leg's counter scrape, while its workload state still exists.
    pub(crate) fn probe_generator(&mut self, name: &str, spec: &LoadSpec, baseline: &LoadReport) {
        let result = probe_spec(spec).and_then(|probe| {
            validate_sample(baseline)?;
            println!("== generator saturation: {name}, {} -> {} conns ==", spec.conns, probe.conns);
            run(&probe)
        });
        self.record_generator_probe(name, spec, baseline, result);
    }

    fn record_generator_probe(
        &mut self,
        name: &str,
        spec: &LoadSpec,
        baseline: &LoadReport,
        result: Result<LoadReport, String>,
    ) {
        let outcome = probe_spec(spec).and_then(|_| {
            let probe = result.as_ref().map_err(Clone::clone)?;
            classify(baseline, probe)
        });
        let (disposition, detail) = match outcome {
            Ok((disposition, delta)) => (disposition, format!("throughput change {delta:+.3}%")),
            Err(reason) => (Disposition::Unmeasured, reason),
        };
        let probe_connections = spec.conns.saturating_add(spec.conns.div_ceil(2));
        let detail = format!(
            "{detail}; connections {} -> {probe_connections}, P={}, duration={}s, warmup={}s",
            spec.conns,
            spec.pipeline,
            spec.duration.as_secs_f64(),
            spec.warmup.as_secs_f64()
        );
        println!("generator saturation ({name}): {} — {detail}", disposition.label());
        self.raw_section(
            &format!("generator {name} baseline"),
            &format!("{spec:?}\n{}", render(baseline)),
        );
        if let Ok(probe) = &result {
            self.raw_section(&format!("generator {name} +50% connections"), &render(probe));
        }
        self.generator_probes.push(GeneratorProbe { name: name.into(), disposition, detail });
    }

    pub(super) fn generator_verdict(&self, milestone: &str) -> (String, Option<String>) {
        let required = matches!(milestone, "m0" | "m1" | "m2");
        if self.generator_probes.is_empty() {
            return if required {
                let reason = "generator saturation: UNMEASURED (no required steady-row probe)";
                (format!("{reason}\n\n"), Some(reason.into()))
            } else {
                (String::new(), None)
            };
        }
        let mut text = String::from(
            "generator saturation (+50% connections, 5% sensitivity):\n\n\
             | row / arm | disposition | observation |\n|---|---|---|\n",
        );
        let mut failures = Vec::new();
        for probe in &self.generator_probes {
            text.push_str(&format!(
                "| {} | {} | {} |\n",
                probe.name,
                probe.disposition.label(),
                probe.detail
            ));
            if probe.disposition != Disposition::Plateau {
                failures.push(format!(
                    "{}: {} ({})",
                    probe.name,
                    probe.disposition.label(),
                    probe.detail
                ));
            }
        }
        text.push_str(
            "\nPLATEAU means insensitive to this connection probe; CPU headroom and the \
             limiting resource are not established. Probe samples do not replace gate samples.\n\n",
        );
        let error = (!failures.is_empty())
            .then(|| format!("generator saturation: {}", failures.join("; ")));
        (text, error)
    }
}

#[cfg(test)]
mod tests;
