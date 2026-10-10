//! Scenario-major comparisons, with explicit replicate order and leg custody.

use std::io::Write;

use super::*;

pub struct Campaign<'a> {
    pub engines: &'a [EngineKind],
    pub attach: &'a BTreeMap<EngineKind, (String, u16)>,
    pub docker: bool,
    pub port_base: u16,
    pub load: &'a LoadParams,
    pub pin_start: Option<usize>,
    pub maxmemory_mb: Option<u64>,
    pub images: &'a Images,
    pub generators: Generators,
    pub run_dir: &'a Path,
    pub replicates: u16,
}

pub struct Results {
    pub cells: Vec<Cell>,
    pub memory: Vec<MemCell>,
    pub configs: Vec<EngineConfig>,
}

struct Leg<'a> {
    ordinal: u32,
    replicate: u16,
    engine_index: usize,
    workload: &'a Workload,
    pipeline: u32,
}

pub fn replicates(flags: &Flags) -> Result<u16, String> {
    let count = flags.u16_or("replicates", 3)?;
    if !(1..=5).contains(&count) {
        return Err("--replicates must be in 1–5".into());
    }
    if flags.bool("reference-box") && count < 3 {
        return Err("--reference-box requires 3–5 replicates (even with --unsafe-env)".into());
    }
    Ok(count)
}

pub fn validate(
    engines: &[EngineKind],
    workloads: &[Workload],
    pipelines: &[u32],
) -> Result<(), String> {
    if engines.is_empty() || workloads.is_empty() || pipelines.is_empty() {
        return Err("engine, workload and pipeline selections must be nonempty".into());
    }
    if pipelines.len() > 16 || pipelines.contains(&0) {
        return Err("select at most 16 nonzero pipelines".into());
    }
    for (index, pipeline) in pipelines.iter().enumerate() {
        if pipelines[..index].contains(pipeline) {
            return Err("duplicate pipeline selection".into());
        }
    }
    for (index, workload) in workloads.iter().enumerate() {
        if workloads[..index].iter().any(|other| other.name == workload.name) {
            return Err("duplicate workload selection".into());
        }
    }
    Ok(())
}

pub fn leg_count(
    engines: usize,
    workloads: &[Workload],
    pipelines: usize,
    replicates: u16,
) -> usize {
    let scenarios: usize = workloads
        .iter()
        .map(|workload| if matches!(workload.kind, Kind::Memory) { 1 } else { pipelines })
        .sum();
    engines * scenarios * usize::from(replicates)
}

impl Campaign<'_> {
    pub fn run(&self, workloads: &[Workload], pipelines: &[u32]) -> Result<Results, String> {
        let mut results = Results { cells: Vec::new(), memory: Vec::new(), configs: Vec::new() };
        let mut manifest = std::fs::File::create(self.run_dir.join("schedule.tsv"))
            .map_err(|error| format!("create schedule: {error}"))?;
        writeln!(manifest, "ordinal\treplicate\tengine\tworkload\tpipeline\tartifact\tstatus")
            .map_err(|error| error.to_string())?;
        for leg in self.schedule(workloads, pipelines) {
            let kind = self.engines[leg.engine_index];
            let artifact = format!(
                "{:04}-rep{}-{}-{}-p{}",
                leg.ordinal,
                leg.replicate,
                kind.label(),
                leg.workload.name,
                leg.pipeline
            );
            if let Some(reason) = self.skip_reason(leg.engine_index, leg.workload) {
                self.record(&mut manifest, &leg, &artifact, reason)?;
                continue;
            }
            self.record(&mut manifest, &leg, &artifact, "running")?;
            let outcome = self.run_leg(&leg, &artifact, &mut results);
            let status = if outcome.is_ok() { "complete" } else { "failed" };
            self.record(&mut manifest, &leg, &artifact, status)?;
            outcome?;
        }
        if results.cells.is_empty() && results.memory.is_empty() {
            return Err("no selected engine/workload/generator combination can be measured".into());
        }
        Ok(results)
    }

    fn schedule<'a>(&self, workloads: &'a [Workload], pipelines: &[u32]) -> Vec<Leg<'a>> {
        let mut legs = Vec::new();
        let mut ordinal = 0;
        for workload in workloads {
            let (eligible, skipped): (Vec<_>, Vec<_>) = (0..self.engines.len())
                .partition(|&index| self.skip_reason(index, workload).is_none());
            let depths = if matches!(workload.kind, Kind::Memory) { &[0][..] } else { pipelines };
            for &pipeline in depths {
                for replicate in 1..=self.replicates {
                    let rotated = (0..eligible.len()).map(|position| {
                        eligible[(position + usize::from(replicate - 1)) % eligible.len()]
                    });
                    for engine_index in rotated.chain(skipped.iter().copied()) {
                        ordinal += 1;
                        legs.push(Leg { ordinal, replicate, engine_index, workload, pipeline });
                    }
                }
            }
        }
        legs
    }

    fn skip_reason(&self, engine_index: usize, workload: &Workload) -> Option<&'static str> {
        if workload.requires_json && !self.engines[engine_index].has_json() {
            return Some("skipped: no JSON surface");
        }
        if !matches!(workload.kind, Kind::Memory)
            && !self.generators.memtier
            && workload.redisbench_test.is_none()
        {
            return Some("skipped: no selected generator for workload");
        }
        None
    }

    fn record(
        &self,
        manifest: &mut std::fs::File,
        leg: &Leg<'_>,
        artifact: &str,
        status: &str,
    ) -> Result<(), String> {
        writeln!(
            manifest,
            "{}\t{}\t{}\t{}\t{}\t{artifact}\t{status}",
            leg.ordinal,
            leg.replicate,
            self.engines[leg.engine_index].label(),
            leg.workload.name,
            leg.pipeline
        )
        .and_then(|()| manifest.flush())
        .map_err(|error| format!("record schedule: {error}"))
    }

    fn run_leg(&self, leg: &Leg<'_>, artifact: &str, results: &mut Results) -> Result<(), String> {
        let raw_dir = self.run_dir.join("raw").join(artifact);
        let log_dir = self.run_dir.join("logs").join(artifact);
        std::fs::create_dir_all(&raw_dir).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(&log_dir).map_err(|error| error.to_string())?;
        let kind = self.engines[leg.engine_index];
        let port = if self.attach.contains_key(&kind) {
            self.port_base
        } else {
            self.port_base + (leg.ordinal - 1) as u16
        };
        let target = bring_up(
            kind,
            self.attach,
            self.docker,
            port,
            self.load,
            self.pin_start,
            self.maxmemory_mb,
            self.images,
            &log_dir,
        )?;
        std::fs::write(
            log_dir.join("config.txt"),
            format!(
                "engine={}\nversion={}\nmode={}\ndurability={:?}\ncommand={}\n",
                kind.label(),
                target.version,
                target.mode_label(),
                target.durability,
                target.launch_cmd
            ),
        )
        .map_err(|error| format!("write launch config: {error}"))?;
        let outcome = self.measure(&target, leg, &raw_dir);
        results.configs.push(EngineConfig {
            label: kind.label(),
            version: target.version.clone(),
            mode: target.mode_label(),
            durability: target.durability,
            launch_cmd: target.launch_cmd.clone(),
            peak_rss_mib: engine::rss_peak_mib(&target),
            artifact: artifact.into(),
        });
        engine::teardown(target);
        let (mut cells, mut memory) = outcome?;
        for cell in &mut cells {
            cell.replicate = leg.replicate;
            cell.ordinal = leg.ordinal;
        }
        for cell in &mut memory {
            cell.replicate = leg.replicate;
            cell.ordinal = leg.ordinal;
            std::fs::write(
                raw_dir.join("memory.tsv"),
                format!(
                    "replicate\tkeys\tvalue_bytes\tbaseline_mib\tafter_mib\tbytes_per_key\n\
                {}\t{}\t{}\t{:?}\t{:?}\t{:?}\n",
                    cell.replicate,
                    cell.keys,
                    cell.value_size,
                    cell.baseline_mib,
                    cell.after_mib,
                    cell.bytes_per_key
                ),
            )
            .map_err(|error| format!("write memory sample: {error}"))?;
        }
        results.cells.extend(cells);
        results.memory.extend(memory);
        Ok(())
    }

    fn measure(
        &self,
        target: &Target,
        leg: &Leg<'_>,
        raw: &Path,
    ) -> Result<(Vec<Cell>, Vec<MemCell>), String> {
        if target.kind == EngineKind::InfinityDb
            && target.mode != Mode::Attach
            && let Some(limit) = self.maxmemory_mb
        {
            engine::set_maxmemory(target, limit)?;
        }
        bench_engine(
            target,
            std::slice::from_ref(leg.workload),
            &[leg.pipeline],
            self.load,
            self.generators,
            raw,
            self.load.duration.clamp(2, 5),
            self.load.duration.clamp(3, 10),
            0.0,
        )
    }
}
