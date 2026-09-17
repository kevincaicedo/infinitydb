//! Diagnostic reports preserve missing measurements and operational failures.
use std::time::SystemTime;

use super::{Measurements, gates};

struct Verdicts {
    table: String,
    missing: Vec<String>,
    failed: Vec<String>,
}

impl Verdicts {
    fn error(&self, failures: &[String]) -> Option<String> {
        let mut errors = Vec::new();
        if !self.missing.is_empty() {
            errors.push(format!("unmeasured STOP gate(s): {}", self.missing.join(", ")));
        }
        if !self.failed.is_empty() {
            errors.push(format!("binding gate(s) FAILED: {}", self.failed.join(", ")));
        }
        errors.extend_from_slice(failures);
        if errors.is_empty() { None } else { Some(errors.join("; ")) }
    }
}

fn gate_verdicts(gates: &[gates::Gate], m: &Measurements, reference_box: bool) -> Verdicts {
    let mut result = Verdicts {
        table: "\n| gate | threshold | measured | verdict |\n|---|---|---|---|\n".into(),
        missing: Vec::new(),
        failed: Vec::new(),
    };
    println!("\n== gate verdicts ==");
    for gate in gates {
        let measured = m.values.get(gate.source.as_str()).filter(|v| v.is_finite());
        let (value, verdict) = match measured {
            None if gate.informational => ("—".into(), "UNMEASURED (informational)".into()),
            None => {
                result.missing.push(format!("{} ({})", gate.id, gate.source));
                ("—".into(), "UNMEASURED STOP — INCOMPLETE".into())
            }
            Some(value) => {
                let pass = gate.passes(*value);
                let tag = if pass { "PASS" } else { "FAIL" };
                let verdict = if gate.informational {
                    format!("{tag} (informational)")
                } else if gate.tier == "linux-reference-box" && !reference_box {
                    format!("{tag} (DEV-TIER, non-binding)")
                } else {
                    if !pass {
                        result.failed.push(gate.id.clone());
                    }
                    tag.into()
                };
                (format!("{value:.2}"), verdict)
            }
        };
        println!("  {:<38} {}", gate.name, verdict);
        result.table.push_str(&format!(
            "| {} | {} {} {} | {} | {} |\n",
            gate.name, gate.comparator, gate.threshold, gate.unit, value, verdict
        ));
    }
    result
}

/// Missing STOP measurements and operational failures fail in every tier.
/// Missing write-amplification dispositions refuse before writing any file.
pub(crate) fn finish_report(
    milestone: &str,
    gates: &[gates::Gate],
    m: &Measurements,
    env_ok: bool,
    reference_box: bool,
    artifacts_root: &str,
    header_facts: &str,
) -> Result<(), String> {
    let unreported: Vec<&str> =
        m.rows.iter().filter(|r| r.disposition.is_none()).map(|r| r.name.as_str()).collect();
    if !unreported.is_empty() {
        return Err(format!(
            "row(s) [{}] finished without a write-amplification disposition — an M4 report row \
             without WA is an invalid row (M4-S16); measure it or name why there is none",
            unreported.join(", ")
        ));
    }
    let verdicts = gate_verdicts(gates, m, reference_box);
    let (generator_report, generator_error) = m.generator_verdict(milestone);
    let mut failures = m.failures.clone();
    failures.extend(generator_error);
    let error = verdicts.error(&failures);
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| format!("report timestamp: {e}"))?
        .as_secs();
    let mut report = format!(
        "# {} gate-run report\n\ndate: {stamp} (unix) · {header_facts}\n\
         env-check: {}\ntier: {}\n\n",
        milestone.to_uppercase(),
        if env_ok { "OK" } else { "FAILED (overridden — NOT citation-grade)" },
        if reference_box { "reference-box (binding thresholds)" } else { "dev (non-binding)" },
    );
    report.push_str(&generator_report);
    if let Some(reason) = &error {
        report.push_str(&format!("**INCOMPLETE / FAILED — NOT citation-grade**\n\n{reason}\n\n"));
    } else {
        report.push_str("status: COMPLETE — all required gate measurements present\n\n");
    }
    report.push_str("notes:\n");
    for note in &m.notes {
        report.push_str(&format!("- {note}\n"));
    }
    report.push_str(&verdicts.table);
    append_rows(&mut report, m);
    write_report(artifacts_root, stamp, &report, m)?;
    error.map_or(Ok(()), Err)
}

fn append_rows(report: &mut String, m: &Measurements) {
    if !m.rows.is_empty() {
        report.push_str(
            "\n## write amplification by row\n\n\
             Per namespace, worst first — never a node-wide blend (M4-S16).\n\n\
             | row | write amplification |\n|---|---|\n",
        );
        for row in &m.rows {
            let disposition = row.disposition.as_deref().unwrap_or("MISSING");
            report.push_str(&format!("| {} | {} |\n", row.name, disposition));
        }
    }
    report.push_str(&m.raw);
}

fn write_report(root: &str, stamp: u64, report: &str, m: &Measurements) -> Result<(), String> {
    let dir = format!("{root}/{stamp}-gate-run");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
    let path = format!("{dir}/report.md");
    std::fs::write(&path, report).map_err(|e| format!("{path}: {e}"))?;
    println!("\ngate-run: report written to {path}");
    for (name, body) in &m.sidecars {
        let path = format!("{dir}/{name}");
        std::fs::write(&path, body).map_err(|e| format!("{path}: {e}"))?;
        println!("gate-run: sidecar written to {path}");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
