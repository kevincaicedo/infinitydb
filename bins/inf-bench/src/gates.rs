//! Reader for `docs/milestones/m0-gates.toml` — a hand-rolled parser for
//! exactly the schema that file uses (`[[gate]]` tables of scalar fields).
//! No TOML dependency: the format is fixed and a real parser would be the
//! only consumer of the crate.

#[derive(Clone, Debug, Default)]
pub struct Gate {
    pub id: String,
    pub name: String,
    pub threshold: f64,
    pub comparator: String,
    pub unit: String,
    pub tier: String,
    pub source: String,
    pub informational: bool,
}

impl Gate {
    /// Applies the gate inequality to a measured value.
    pub fn passes(&self, measured: f64) -> bool {
        match self.comparator.as_str() {
            ">=" => measured >= self.threshold,
            "<=" => measured <= self.threshold,
            ">" => measured > self.threshold,
            "<" => measured < self.threshold,
            other => panic!("gates file has unknown comparator {other:?}"),
        }
    }
}

/// Parses the gates file.
///
/// # Errors
/// Malformed lines or a missing file.
pub fn load(path: &str) -> Result<Vec<Gate>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut gates: Vec<Gate> = Vec::new();
    let mut current: Option<Gate> = None;
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[gate]]" {
            if let Some(gate) = current.take() {
                gates.push(gate);
            }
            current = Some(Gate::default());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("{path}:{}: expected `key = value`", lineno + 1));
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        let Some(gate) = current.as_mut() else {
            continue; // top-level schema/milestone keys
        };
        match key {
            "id" => gate.id = value.into(),
            "name" => gate.name = value.into(),
            "threshold" => {
                gate.threshold =
                    value.parse().map_err(|e| format!("{path}:{}: threshold: {e}", lineno + 1))?;
            }
            "comparator" => gate.comparator = value.into(),
            "unit" => gate.unit = value.into(),
            "tier" => gate.tier = value.into(),
            "source" => gate.source = value.into(),
            "informational" => gate.informational = value == "true",
            "measured_by" => {}
            other => {
                return Err(format!("{path}:{}: unknown gate field {other}", lineno + 1));
            }
        }
    }
    if let Some(gate) = current.take() {
        gates.push(gate);
    }
    if gates.is_empty() {
        return Err(format!("{path}: no [[gate]] entries"));
    }
    Ok(gates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_checked_in_gates_file() {
        // The file lives outside the workspace root (repo docs/).
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../docs/milestones/m0-gates.toml");
        let gates = load(path).expect("gates file parses");
        assert_eq!(gates.len(), 9, "all nine §6 gates present");
        let sqes = gates.iter().find(|g| g.id == "sqes_per_submit").expect("sqes gate");
        assert!(sqes.passes(16.0) && !sqes.passes(15.9));
        let p999 = gates.iter().find(|g| g.id == "p999_latency").expect("p999 gate");
        assert!(p999.passes(2999.0) && !p999.passes(3000.0));
        assert!(gates.iter().any(|g| g.informational), "flamegraph row is informational");
    }
}
