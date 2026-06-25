//! Durability oracle core for M2-S19.
//!
//! The oracle is intentionally independent of `inf-log`: it models the
//! contract in per-cell log order, while adapters can later map concrete LSNs
//! to [`LogSeq`]. Ack time is tracked separately because `always` and
//! `everysec` writes can acknowledge at different times relative to log order.

use std::collections::BTreeMap;

use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::Nanos;

pub const EVERYSEC_LOSS_WINDOW: Nanos = Nanos::from_secs(1);

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct LogSeq(u64);

impl LogSeq {
    pub const ZERO: LogSeq = LogSeq(0);

    pub const fn new(value: u64) -> LogSeq {
        LogSeq(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn previous(self) -> Option<LogSeq> {
        if self.0 == 0 { None } else { Some(LogSeq(self.0 - 1)) }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DurabilityClass {
    Always,
    Everysec,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IterationWrite<'a> {
    MemorySet { key: &'a [u8], value: &'a [u8] },
    MemoryDelete { key: &'a [u8] },
    EverysecSet { key: &'a [u8], value: &'a [u8] },
    EverysecDelete { key: &'a [u8] },
    AlwaysSet { key: &'a [u8], value: &'a [u8] },
    AlwaysDelete { key: &'a [u8] },
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct IterationAppend {
    pub durable_records: u32,
    pub memory_ops: u32,
    pub first_seq: Option<LogSeq>,
    pub last_seq: Option<LogSeq>,
}

impl IterationAppend {
    fn note_memory(&mut self) {
        self.memory_ops = self.memory_ops.checked_add(1).expect("memory op count overflow");
    }

    fn note_durable(&mut self, seq: LogSeq) {
        if self.first_seq.is_none() {
            self.first_seq = Some(seq);
        }
        self.last_seq = Some(seq);
        self.durable_records =
            self.durable_records.checked_add(1).expect("durable record count overflow");
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct Ack {
    class: DurabilityClass,
    time: Nanos,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Mutation {
    seq: LogSeq,
    append_time: Nanos,
    ack: Option<Ack>,
    key: Vec<u8>,
    value: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct DurabilityOracle {
    loss_window: Nanos,
    mutations: Vec<Mutation>,
}

impl DurabilityOracle {
    pub fn new() -> DurabilityOracle {
        DurabilityOracle::with_loss_window(EVERYSEC_LOSS_WINDOW)
    }

    pub fn with_loss_window(loss_window: Nanos) -> DurabilityOracle {
        assert!(loss_window.0 > 0);
        DurabilityOracle { loss_window, mutations: Vec::new() }
    }

    pub fn append_set(&mut self, append_time: Nanos, key: &[u8], value: &[u8]) -> LogSeq {
        assert!(!key.is_empty());
        self.append_mutation(append_time, key, Some(value))
    }

    pub fn append_delete(&mut self, append_time: Nanos, key: &[u8]) -> LogSeq {
        assert!(!key.is_empty());
        self.append_mutation(append_time, key, None)
    }

    pub fn ack(&mut self, seq: LogSeq, class: DurabilityClass, ack_time: Nanos) {
        assert!(seq.0 > 0);
        let index = usize::try_from(seq.0 - 1).expect("ack seq must fit usize");
        let mutation = self.mutations.get_mut(index).expect("ack seq must exist");
        assert_eq!(mutation.seq, seq);
        assert!(mutation.ack.is_none(), "mutation must be acked once");
        assert!(ack_time >= mutation.append_time);
        mutation.ack = Some(Ack { class, time: ack_time });
    }

    pub fn append_acked_set(
        &mut self,
        class: DurabilityClass,
        ack_time: Nanos,
        key: &[u8],
        value: &[u8],
    ) -> LogSeq {
        let seq = self.append_set(ack_time, key, value);
        self.ack(seq, class, ack_time);
        seq
    }

    pub fn append_acked_delete(
        &mut self,
        class: DurabilityClass,
        ack_time: Nanos,
        key: &[u8],
    ) -> LogSeq {
        let seq = self.append_delete(ack_time, key);
        self.ack(seq, class, ack_time);
        seq
    }

    pub fn append_iteration_batch(
        &mut self,
        append_time: Nanos,
        always_fsync_time: Option<Nanos>,
        writes: &[IterationWrite<'_>],
    ) -> IterationAppend {
        assert!(!writes.is_empty());
        if let Some(sync_time) = always_fsync_time {
            assert!(sync_time >= append_time);
        }

        let mut appended = IterationAppend::default();
        for write in writes {
            match *write {
                IterationWrite::MemorySet { key, value } => {
                    assert!(!key.is_empty());
                    let _ = value;
                    appended.note_memory();
                }
                IterationWrite::MemoryDelete { key } => {
                    assert!(!key.is_empty());
                    appended.note_memory();
                }
                IterationWrite::EverysecSet { key, value } => {
                    let seq = self.append_set(append_time, key, value);
                    self.ack(seq, DurabilityClass::Everysec, append_time);
                    appended.note_durable(seq);
                }
                IterationWrite::EverysecDelete { key } => {
                    let seq = self.append_delete(append_time, key);
                    self.ack(seq, DurabilityClass::Everysec, append_time);
                    appended.note_durable(seq);
                }
                IterationWrite::AlwaysSet { key, value } => {
                    let sync_time = always_fsync_time.expect("always write requires an fsync time");
                    let seq = self.append_set(append_time, key, value);
                    self.ack(seq, DurabilityClass::Always, sync_time);
                    appended.note_durable(seq);
                }
                IterationWrite::AlwaysDelete { key } => {
                    let sync_time = always_fsync_time.expect("always write requires an fsync time");
                    let seq = self.append_delete(append_time, key);
                    self.ack(seq, DurabilityClass::Always, sync_time);
                    appended.note_durable(seq);
                }
            }
        }
        appended
    }

    pub fn mutation_count(&self) -> usize {
        self.mutations.len()
    }

    pub fn required_prefix(&self, crash_time: Nanos) -> LogSeq {
        let mut required = LogSeq::ZERO;
        for mutation in &self.mutations {
            let Some(ack) = mutation.ack else {
                continue;
            };
            if ack.time > crash_time {
                continue;
            }
            if !self.ack_requires_survival(ack, crash_time) {
                continue;
            }
            required = mutation.seq;
        }
        required
    }

    pub fn last_appended_prefix(&self, crash_time: Nanos) -> LogSeq {
        let mut last = LogSeq::ZERO;
        for mutation in &self.mutations {
            if mutation.append_time <= crash_time {
                last = mutation.seq;
            }
        }
        last
    }

    pub fn state_after_prefix(&self, prefix: LogSeq) -> BTreeMap<Vec<u8>, Vec<u8>> {
        assert!(prefix.0 <= self.mutations.len() as u64);
        let mut state = BTreeMap::new();
        for mutation in &self.mutations {
            if mutation.seq > prefix {
                break;
            }
            apply_mutation(&mut state, mutation);
        }
        state
    }

    pub fn audit_recovered<I, K, V>(&self, crash_time: Nanos, recovered: I) -> DurabilityAudit
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let (recovered_state, mut violations) = normalize_recovered_state(recovered);
        let recovered_digest = state_digest(&recovered_state);
        let required_prefix = self.required_prefix(crash_time);
        let last_appended_prefix = self.last_appended_prefix(crash_time);
        let mut state = BTreeMap::new();

        if required_prefix == LogSeq::ZERO && state == recovered_state {
            return DurabilityAudit {
                crash_time,
                required_prefix,
                last_appended_prefix,
                accepted_prefix: Some(LogSeq::ZERO),
                recovered_digest,
                expected_digest: Some(state_digest(&state)),
                violations,
            };
        }

        let mut accepted_prefix = None;
        let mut expected_digest = None;
        for mutation in &self.mutations {
            if mutation.append_time > crash_time {
                break;
            }
            apply_mutation(&mut state, mutation);
            if mutation.seq < required_prefix {
                continue;
            }
            if state == recovered_state {
                accepted_prefix = Some(mutation.seq);
                expected_digest = Some(state_digest(&state));
                break;
            }
        }

        if accepted_prefix.is_none() {
            violations.push(format!(
                "recovered state is not a valid log prefix: required_prefix={} \
                 last_appended_prefix={} recovered_digest={recovered_digest:#018x}",
                required_prefix.0, last_appended_prefix.0
            ));
        }

        DurabilityAudit {
            crash_time,
            required_prefix,
            last_appended_prefix,
            accepted_prefix,
            recovered_digest,
            expected_digest,
            violations,
        }
    }

    fn append_mutation(&mut self, append_time: Nanos, key: &[u8], value: Option<&[u8]>) -> LogSeq {
        if let Some(last) = self.mutations.last() {
            assert!(append_time >= last.append_time);
        }
        let seq = LogSeq(
            u64::try_from(self.mutations.len())
                .expect("mutation count must fit u64")
                .checked_add(1)
                .expect("mutation sequence must not overflow"),
        );
        self.mutations.push(Mutation {
            seq,
            append_time,
            ack: None,
            key: key.to_vec(),
            value: value.map(<[u8]>::to_vec),
        });
        seq
    }

    fn ack_requires_survival(&self, ack: Ack, crash_time: Nanos) -> bool {
        match ack.class {
            DurabilityClass::Always => true,
            DurabilityClass::Everysec => ack.time.saturating_add(self.loss_window) <= crash_time,
        }
    }
}

impl Default for DurabilityOracle {
    fn default() -> Self {
        DurabilityOracle::new()
    }
}

#[derive(Clone, Debug)]
pub struct DurabilityAudit {
    pub crash_time: Nanos,
    pub required_prefix: LogSeq,
    pub last_appended_prefix: LogSeq,
    pub accepted_prefix: Option<LogSeq>,
    pub recovered_digest: u64,
    pub expected_digest: Option<u64>,
    pub violations: Vec<String>,
}

impl DurabilityAudit {
    pub fn ok(&self) -> bool {
        self.violations.is_empty() && self.accepted_prefix.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct DurabilitySweepConfig {
    pub seed: u64,
    pub seed_offset: u64,
    pub seed_stride: u64,
    pub seeds: u64,
    pub writes_per_seed: u64,
    pub key_space: u64,
    pub loss_window: Nanos,
    pub mixed_policy_interval: u64,
}

impl DurabilitySweepConfig {
    pub fn ci(seed: u64) -> DurabilitySweepConfig {
        DurabilitySweepConfig {
            seed,
            seed_offset: 0,
            seed_stride: 1,
            seeds: 128,
            writes_per_seed: 96,
            key_space: 32,
            loss_window: EVERYSEC_LOSS_WINDOW,
            mixed_policy_interval: 8,
        }
    }

    pub fn apply_seed_shard(
        &mut self,
        shard_index: u64,
        shard_count: u64,
    ) -> Result<(), &'static str> {
        if self.seeds == 0 {
            return Err("sweep seed count must be greater than zero");
        }
        if shard_count == 0 {
            return Err("sweep shard count must be greater than zero");
        }
        if shard_index >= shard_count {
            return Err("sweep shard index must be less than shard count");
        }
        if shard_count > self.seeds {
            return Err("sweep shard count must not exceed sweep seed count");
        }

        self.seeds = 1 + (self.seeds - 1 - shard_index) / shard_count;
        self.seed_offset = shard_index;
        self.seed_stride = shard_count;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DurabilitySweepReport {
    pub seed: u64,
    pub seed_offset: u64,
    pub seed_stride: u64,
    pub seeds: u64,
    pub writes_per_seed: u64,
    pub cases_audited: u64,
    pub mixed_policy_batches: u64,
    pub manifest: Vec<u8>,
    pub manifest_hash: u64,
    pub violations: Vec<String>,
}

impl DurabilitySweepReport {
    pub fn ok(&self) -> bool {
        self.violations.is_empty() && self.cases_audited == self.seeds
    }
}

pub fn run_durability_sweep(config: &DurabilitySweepConfig) -> DurabilitySweepReport {
    assert!(config.seeds > 0);
    assert!(config.seed_stride > 0);
    assert!(config.writes_per_seed > 0);
    assert!(config.key_space > 0);

    let mut manifest = Vec::new();
    let mut violations = Vec::new();
    let mut mixed_policy_batches = 0;
    for offset in 0..config.seeds {
        let seed = config
            .seed
            .wrapping_add(config.seed_offset)
            .wrapping_add(offset.wrapping_mul(config.seed_stride));
        let seed_run = run_durability_seed(seed, config, false);
        let audit = seed_run.audit;
        mixed_policy_batches += seed_run.mixed_policy_batches;
        manifest.extend_from_slice(&seed.to_le_bytes());
        manifest.extend_from_slice(&audit.required_prefix.0.to_le_bytes());
        manifest.extend_from_slice(&audit.last_appended_prefix.0.to_le_bytes());
        manifest.extend_from_slice(&audit.accepted_prefix.unwrap_or(LogSeq::ZERO).0.to_le_bytes());
        manifest.extend_from_slice(&audit.recovered_digest.to_le_bytes());
        manifest.extend_from_slice(&seed_run.mixed_policy_batches.to_le_bytes());
        if !audit.ok() {
            violations.push(format!(
                "seed {seed:#x}: {}",
                audit.violations.first().map(String::as_str).unwrap_or("oracle violation")
            ));
        }
    }

    DurabilitySweepReport {
        seed: config.seed,
        seed_offset: config.seed_offset,
        seed_stride: config.seed_stride,
        seeds: config.seeds,
        writes_per_seed: config.writes_per_seed,
        cases_audited: config.seeds,
        mixed_policy_batches,
        manifest_hash: hash64(&manifest, 0xD2D2_0019),
        manifest,
        violations,
    }
}

pub fn find_planted_ack_before_fsync_canary(
    config: &DurabilitySweepConfig,
    max_seeds: u64,
) -> Option<u64> {
    assert!(max_seeds > 0);
    for offset in 0..max_seeds {
        let seed = config.seed.wrapping_add(offset);
        let seed_run = run_durability_seed(seed, config, true);
        if !seed_run.audit.ok() {
            return Some(seed);
        }
    }
    None
}

#[derive(Clone, Debug)]
struct DurabilitySeedRun {
    audit: DurabilityAudit,
    mixed_policy_batches: u64,
}

fn run_durability_seed(
    seed: u64,
    config: &DurabilitySweepConfig,
    planted_ack_before_fsync: bool,
) -> DurabilitySeedRun {
    let mut rng = SplitMix64::new(seed ^ 0xD2D2_0019);
    let mut oracle = DurabilityOracle::with_loss_window(config.loss_window);
    let mut now = Nanos::ZERO;
    let mut mixed_policy_batches = 0;

    let mut index = 0;
    while index < config.writes_per_seed {
        now = now.saturating_add(Nanos::from_millis(1 + rng.next_below(25)));
        if should_emit_mixed_policy_batch(config, index) {
            let everysec_key = format!("mixed:e:{}", rng.next_below(config.key_space));
            let everysec_value = format!("ve:{seed:x}:{index:x}:{}", rng.next_u64());
            let always_key = format!("mixed:a:{}", rng.next_below(config.key_space));
            let always_value = format!("va:{seed:x}:{index:x}:{}", rng.next_u64());
            let memory_key = format!("mixed:m:{}", rng.next_below(config.key_space));
            let memory_value = format!("vm:{seed:x}:{index:x}:{}", rng.next_u64());
            let fsync_time = now.saturating_add(Nanos::from_micros(100 + rng.next_below(5_000)));
            let writes = [
                IterationWrite::MemorySet {
                    key: memory_key.as_bytes(),
                    value: memory_value.as_bytes(),
                },
                IterationWrite::EverysecSet {
                    key: everysec_key.as_bytes(),
                    value: everysec_value.as_bytes(),
                },
                IterationWrite::AlwaysSet {
                    key: always_key.as_bytes(),
                    value: always_value.as_bytes(),
                },
            ];
            let appended = oracle.append_iteration_batch(now, Some(fsync_time), &writes);
            assert_eq!(appended.memory_ops, 1);
            assert_eq!(appended.durable_records, 2);
            mixed_policy_batches += 1;
            index += 2;
            continue;
        }

        let key = format!("key:{}", rng.next_below(config.key_space));
        let seq = if rng.next_below(10) == 0 {
            oracle.append_delete(now, key.as_bytes())
        } else {
            let value = format!("v:{seed:x}:{index:x}:{}", rng.next_u64());
            oracle.append_set(now, key.as_bytes(), value.as_bytes())
        };
        let class = if rng.next_below(4) == 0 {
            DurabilityClass::Always
        } else {
            DurabilityClass::Everysec
        };
        let ack_delay = match class {
            DurabilityClass::Always => Nanos::from_micros(100 + rng.next_below(5_000)),
            DurabilityClass::Everysec => Nanos::ZERO,
        };
        oracle.ack(seq, class, now.saturating_add(ack_delay));
        index += 1;
    }

    let crash_time = now.saturating_add(config.loss_window).saturating_add(Nanos::from_millis(1));
    let required = oracle.required_prefix(crash_time);
    let last = oracle.last_appended_prefix(crash_time);
    let cut = if planted_ack_before_fsync {
        required.previous().unwrap_or(LogSeq::ZERO)
    } else {
        let span = last.0 - required.0;
        LogSeq(required.0 + rng.next_below(span + 1))
    };
    let recovered = oracle.state_after_prefix(cut);
    let audit = oracle.audit_recovered(crash_time, recovered.iter());
    DurabilitySeedRun { audit, mixed_policy_batches }
}

fn should_emit_mixed_policy_batch(config: &DurabilitySweepConfig, index: u64) -> bool {
    if config.mixed_policy_interval == 0 {
        return false;
    }
    if index + 1 >= config.writes_per_seed {
        return false;
    }
    index.is_multiple_of(config.mixed_policy_interval)
}

fn apply_mutation(state: &mut BTreeMap<Vec<u8>, Vec<u8>>, mutation: &Mutation) {
    match &mutation.value {
        Some(value) => {
            state.insert(mutation.key.clone(), value.clone());
        }
        None => {
            state.remove(&mutation.key);
        }
    }
}

fn normalize_recovered_state<I, K, V>(recovered: I) -> (BTreeMap<Vec<u8>, Vec<u8>>, Vec<String>)
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<[u8]>,
    V: AsRef<[u8]>,
{
    let mut state = BTreeMap::new();
    let mut violations = Vec::new();
    for (key, value) in recovered {
        let key = key.as_ref();
        if key.is_empty() {
            violations.push("recovered image contains an empty key".to_string());
            continue;
        }
        if state.insert(key.to_vec(), value.as_ref().to_vec()).is_some() {
            violations.push(format!(
                "recovered image contains duplicate key {:?}",
                String::from_utf8_lossy(key)
            ));
        }
    }
    (state, violations)
}

fn state_digest(state: &BTreeMap<Vec<u8>, Vec<u8>>) -> u64 {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(state.len() as u64).to_le_bytes());
    for (key, value) in state {
        bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
        bytes.extend_from_slice(value);
    }
    hash64(&bytes, 0xD2D2_5A7E)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(state: &BTreeMap<Vec<u8>, Vec<u8>>) -> impl Iterator<Item = (&[u8], &[u8])> {
        state.iter().map(|(key, value)| (key.as_slice(), value.as_slice()))
    }

    #[test]
    fn always_ack_requires_recovered_prefix_at_or_after_the_ack() {
        let mut oracle = DurabilityOracle::new();
        oracle.append_acked_set(DurabilityClass::Always, Nanos::from_millis(1), b"a", b"1");
        oracle.append_acked_set(DurabilityClass::Everysec, Nanos::from_millis(2), b"b", b"2");

        let recovered = oracle.state_after_prefix(LogSeq::ZERO);
        let audit = oracle.audit_recovered(Nanos::from_millis(3), entries(&recovered));

        assert!(!audit.ok());
        assert_eq!(audit.required_prefix, LogSeq::new(1));
    }

    #[test]
    fn everysec_write_inside_loss_window_is_optional() {
        let mut oracle = DurabilityOracle::new();
        oracle.append_acked_set(DurabilityClass::Everysec, Nanos::from_millis(1), b"a", b"1");
        let optional = oracle.append_acked_set(
            DurabilityClass::Everysec,
            Nanos::from_millis(1_500),
            b"b",
            b"2",
        );
        let crash_time = Nanos::from_millis(2_000);

        let recovered = oracle.state_after_prefix(LogSeq::new(1));
        let audit = oracle.audit_recovered(crash_time, entries(&recovered));

        assert!(audit.ok(), "violations: {:?}", audit.violations);
        assert_eq!(audit.required_prefix, LogSeq::new(1));
        assert_eq!(audit.last_appended_prefix, optional);
    }

    #[test]
    fn everysec_write_outside_loss_window_is_required() {
        let mut oracle = DurabilityOracle::new();
        oracle.append_acked_set(DurabilityClass::Everysec, Nanos::from_millis(1), b"a", b"1");
        oracle.append_acked_set(DurabilityClass::Everysec, Nanos::from_millis(500), b"b", b"2");

        let recovered = oracle.state_after_prefix(LogSeq::new(1));
        let audit = oracle.audit_recovered(Nanos::from_millis(1_501), entries(&recovered));

        assert!(!audit.ok());
        assert_eq!(audit.required_prefix, LogSeq::new(2));
    }

    #[test]
    fn non_prefix_recovered_image_is_rejected() {
        let mut oracle = DurabilityOracle::new();
        oracle.append_acked_set(DurabilityClass::Always, Nanos::from_millis(1), b"a", b"1");
        oracle.append_acked_set(DurabilityClass::Always, Nanos::from_millis(2), b"b", b"2");
        let recovered = BTreeMap::from([(b"b".to_vec(), b"2".to_vec())]);

        let audit = oracle.audit_recovered(Nanos::from_millis(3), entries(&recovered));

        assert!(!audit.ok());
        assert_eq!(audit.required_prefix, LogSeq::new(2));
    }

    #[test]
    fn mixed_policy_iteration_requires_whole_synced_prefix_after_always_ack() {
        let mut oracle = DurabilityOracle::new();
        let append_time = Nanos::from_millis(10);
        let fsync_time = Nanos::from_millis(12);

        let appended = oracle.append_iteration_batch(
            append_time,
            Some(fsync_time),
            &[
                IterationWrite::MemorySet { key: b"m", value: b"1" },
                IterationWrite::EverysecSet { key: b"e", value: b"2" },
                IterationWrite::AlwaysSet { key: b"a", value: b"3" },
            ],
        );

        assert_eq!(appended.memory_ops, 1);
        assert_eq!(appended.durable_records, 2);
        assert_eq!(appended.first_seq, Some(LogSeq::new(1)));
        assert_eq!(appended.last_seq, Some(LogSeq::new(2)));

        let before_fsync = oracle.state_after_prefix(LogSeq::ZERO);
        let before_audit = oracle.audit_recovered(Nanos::from_millis(11), entries(&before_fsync));
        assert!(before_audit.ok(), "violations: {:?}", before_audit.violations);
        assert_eq!(before_audit.required_prefix, LogSeq::ZERO);

        let partial = oracle.state_after_prefix(LogSeq::new(1));
        let partial_audit = oracle.audit_recovered(fsync_time, entries(&partial));
        assert!(!partial_audit.ok());
        assert_eq!(partial_audit.required_prefix, LogSeq::new(2));

        let full = oracle.state_after_prefix(LogSeq::new(2));
        let full_audit = oracle.audit_recovered(fsync_time, entries(&full));
        assert!(full_audit.ok(), "violations: {:?}", full_audit.violations);
    }

    #[test]
    fn sweep_is_deterministic_and_green_for_valid_prefixes() {
        let config = DurabilitySweepConfig::ci(0xD2D2);
        let a = run_durability_sweep(&config);
        let b = run_durability_sweep(&config);

        assert!(a.ok(), "violations: {:?}", a.violations);
        assert_eq!(a.seed_offset, 0);
        assert_eq!(a.seed_stride, 1);
        assert!(a.mixed_policy_batches > 0);
        assert_eq!(a.mixed_policy_batches, b.mixed_policy_batches);
        assert_eq!(a.manifest, b.manifest);
        assert_eq!(a.manifest_hash, b.manifest_hash);
    }

    #[test]
    fn sweep_seed_shards_partition_one_campaign() {
        let mut shard_a = DurabilitySweepConfig { seeds: 10, ..DurabilitySweepConfig::ci(0xD2D2) };
        let mut shard_b = shard_a.clone();
        let mut shard_c = shard_a.clone();

        shard_a.apply_seed_shard(0, 3).unwrap();
        shard_b.apply_seed_shard(1, 3).unwrap();
        shard_c.apply_seed_shard(2, 3).unwrap();

        assert_eq!(shard_a.seeds, 4);
        assert_eq!(shard_b.seeds, 3);
        assert_eq!(shard_c.seeds, 3);
        assert_eq!(shard_a.seed_offset, 0);
        assert_eq!(shard_b.seed_offset, 1);
        assert_eq!(shard_c.seed_offset, 2);
        assert_eq!(shard_a.seed_stride, 3);
        assert_eq!(shard_b.seed_stride, 3);
        assert_eq!(shard_c.seed_stride, 3);

        let report_a = run_durability_sweep(&shard_a);
        let report_b = run_durability_sweep(&shard_b);
        let report_c = run_durability_sweep(&shard_c);

        assert!(report_a.ok(), "violations: {:?}", report_a.violations);
        assert!(report_b.ok(), "violations: {:?}", report_b.violations);
        assert!(report_c.ok(), "violations: {:?}", report_c.violations);
        assert_eq!(report_a.manifest.len(), 4 * 48);
        assert_eq!(report_b.manifest.len(), 3 * 48);
        assert_eq!(report_c.manifest.len(), 3 * 48);
        assert_ne!(report_a.manifest, report_b.manifest);
        assert_ne!(report_b.manifest, report_c.manifest);
    }

    #[test]
    fn planted_ack_before_fsync_canary_is_caught_within_1000_seeds() {
        let config = DurabilitySweepConfig {
            seeds: 16,
            writes_per_seed: 32,
            ..DurabilitySweepConfig::ci(0)
        };
        let caught = find_planted_ack_before_fsync_canary(&config, 1_000);

        assert!(caught.is_some(), "planted canary survived 1000 seeds");
    }
}
