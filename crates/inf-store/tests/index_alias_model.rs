//! ARCH-W0.2 — the model-based oracle for index identity under hash
//! aliases (ADR-0139 D2/D4; record `docs/drr/ARCH-W0.2.md` §6).
//!
//! A `BTreeMap<pk, record>` shadow over a **collision-dense** key
//! universe — every key belongs to a real alias group (`collision-oracle`,
//! ADR-0094 D3) and values come from a four-value pool, so shared entries
//! are the norm, not the corner. After every op, and after a forced
//! expiry drain (so "physically present" and "live" coincide), the
//! expected tree is `{(enc(v), hash(pk))}` over the shadow and is
//! compared entry for entry with the tree.
//!
//! Independence: the model shares the keyed hasher (it must — the ref is
//! that hash) and `index_key_encode` (proven by the S02 golden vectors);
//! it shares **nothing** of `index_maint` and does not evaluate paths.
//! Liveness: a run that never suppressed a removal, never forgot a
//! gate-dead key, never met an alias of a write-set key — or never let a
//! removal *through* beside an alias — proved one path of five, so each
//! tally is asserted non-zero. Canary: `inf_canary_alias_blind`.

#![cfg(feature = "doc")]

use std::collections::{BTreeMap, BTreeSet};

use inf_doc::JsonParser;
use inf_doc::path::compile;
use inf_foundation::time::Nanos;
use inf_store::{
    COLLISION_KEY_PREFIX, CellStore, EvictionPolicy, ExpiryBudget, IndexId, IndexKeyBuf,
    IndexKeyType, IndexScalar, IndexSpec, IndexState, JsonSetOptions, KeyHasher, Keyspace, NsId,
    OrderedCursor, PkRef, PressureConfig, SetExpire, SetOptions, StoreConfig, index_key_encode,
};

const NS: NsId = NsId(0);
const IDX_V: IndexId = IndexId(1);
const IDX_TAGS: IndexId = IndexId(2);
const GROUPS: u64 = 5;
const MEMBERS: u8 = 3;
const VALUES: i64 = 4;

struct Rng(u64);

impl Rng {
    /// SplitMix64 — seeds in the test, never ambient randomness (L7).
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn alias_key(group: u64, member: u8) -> Vec<u8> {
    let mut out = vec![0u8; 48];
    out[..16].copy_from_slice(COLLISION_KEY_PREFIX);
    out[16..24].copy_from_slice(&group.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
    out[24..32].copy_from_slice(&group.to_le_bytes());
    out[32..].copy_from_slice(&[b'a' + member; 16]);
    out
}

fn random_key(rng: &mut Rng) -> Vec<u8> {
    alias_key(rng.below(GROUPS), rng.below(u64::from(MEMBERS)) as u8)
}

/// What the shadow knows about one physically present record.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Shadow {
    Doc { v: i64, tags: [i64; 2], deadline_ms: Option<u64> },
    Str { deadline_ms: Option<u64> },
}

impl Shadow {
    fn deadline_ms(&self) -> Option<u64> {
        match self {
            Shadow::Doc { deadline_ms, .. } | Shadow::Str { deadline_ms } => *deadline_ms,
        }
    }

    /// The `(index, value)` facts this record holds.
    fn holds(&self) -> BTreeSet<(IndexId, i64)> {
        match self {
            Shadow::Doc { v, tags, .. } => {
                [(IDX_V, *v), (IDX_TAGS, tags[0]), (IDX_TAGS, tags[1])].into_iter().collect()
            }
            Shadow::Str { .. } => BTreeSet::new(),
        }
    }
}

#[derive(Default)]
struct Tallies {
    /// Entries a record stopped holding while a group-mate still held
    /// them: the engine must suppress the removal.
    kept_expected: u64,
    /// Entries a record stopped holding beside a group-mate that did
    /// **not** hold them: the engine must let the removal through.
    let_through_beside_an_alias: u64,
    gate_passes: u64,
}

struct Harness {
    ks: Keyspace,
    model: BTreeMap<Vec<u8>, Shadow>,
    now_ms: u64,
    tallies: Tallies,
}

fn declare(ks: &mut Keyspace, id: IndexId, path: &str) {
    let program = compile(path.as_bytes()).expect("valid path").as_bytes().to_vec();
    ks.idx_create(IndexSpec {
        id,
        generation: u64::from(id.0),
        ns: NS,
        name: format!("idx-{}", id.0).into_bytes(),
        program,
        key_type: IndexKeyType::I64,
        state: IndexState::Declared,
    })
    .expect("declare");
}

impl Harness {
    fn new() -> Harness {
        let mut ks = Keyspace::new(StoreConfig::default());
        declare(&mut ks, IDX_V, "$.v");
        declare(&mut ks, IDX_TAGS, "$.tags[*]");
        ks.db_mut(0);
        // Converged: the `Strict` found/fresh asserts are part of the oracle.
        ks.idx_set_converged(NS, IDX_V, true);
        ks.idx_set_converged(NS, IDX_TAGS, true);
        Harness { ks, model: BTreeMap::new(), now_ms: 1_000, tallies: Tallies::default() }
    }

    fn now(&self) -> Nanos {
        Nanos(self.now_ms * 1_000_000)
    }

    fn live(&self, key: &[u8]) -> Option<&Shadow> {
        self.model.get(key).filter(|s| s.deadline_ms().is_none_or(|d| self.now_ms <= d))
    }

    fn expired_unreaped_doc(&self) -> Option<Vec<u8>> {
        self.model.iter().find_map(|(key, shadow)| match shadow {
            Shadow::Doc { deadline_ms: Some(d), .. } if *d < self.now_ms => Some(key.clone()),
            _ => None,
        })
    }

    /// Records the liveness tallies for `key` going from its current
    /// shadow to `next`: for every fact it stops holding, does a
    /// physically present group-mate hold it?
    fn note_transition(&mut self, key: &[u8], next: Option<&Shadow>) {
        let Some(current) = self.model.get(key) else { return };
        let after = next.map(Shadow::holds).unwrap_or_default();
        let head = &key[..32];
        for fact in current.holds().difference(&after) {
            let mates = self.model.iter().filter(|(k, _)| &k[..32] == head && k.as_slice() != key);
            let mut any_mate = false;
            let mut held = false;
            for (_, mate) in mates {
                any_mate = true;
                held |= mate.holds().contains(fact);
            }
            if held {
                self.tallies.kept_expected += 1;
            } else if any_mate {
                self.tallies.let_through_beside_an_alias += 1;
            }
        }
    }

    fn bracketed<R>(
        &mut self,
        keys: &[&[u8]],
        path: Option<&str>,
        gate: bool,
        body: impl FnOnce(&mut CellStore, Nanos) -> R,
    ) -> R {
        let program = path.map(|p| compile(p.as_bytes()).expect("valid path"));
        self.ks.idx_bracket_begin(NS, keys, program.as_ref()).expect("pre-half admits");
        if gate {
            self.gate_evict_all_volatile();
        }
        let now = self.now();
        let result = body(self.ks.db_mut(0), now);
        self.ks.idx_bracket_commit(NS, keys);
        result
    }

    /// The OOM gate under `volatile-ttl` and a 1-byte limit: every
    /// volatile record dies (expired ones as expirations), whatever
    /// bracket is open around it.
    fn gate_evict_all_volatile(&mut self) {
        self.ks.set_pressure(PressureConfig {
            limit_bytes: 1,
            policy: EvictionPolicy::VolatileTtl,
            samples: 5,
        });
        let _ = self.ks.free_for_write(self.now());
        self.ks.set_pressure(PressureConfig {
            limit_bytes: 0,
            policy: EvictionPolicy::NoEviction,
            samples: 5,
        });
        let volatile: Vec<Vec<u8>> = self
            .model
            .iter()
            .filter(|(_, s)| s.deadline_ms().is_some())
            .map(|(k, _)| k.clone())
            .collect();
        for key in volatile {
            self.note_transition(&key, None);
            self.model.remove(&key);
        }
        self.tallies.gate_passes += 1;
    }

    fn set_model(&mut self, key: &[u8], next: Option<Shadow>) {
        self.note_transition(key, next.as_ref());
        match next {
            Some(shadow) => self.model.insert(key.to_vec(), shadow),
            None => self.model.remove(key),
        };
    }

    /// Drains the wheel at `now`, so that afterwards every physically
    /// present record is live and the shadow can be compared.
    fn drain_expiry(&mut self) {
        loop {
            let stats = self.ks.expire_tick(self.now(), ExpiryBudget::default());
            if stats.reaped == 0 && stats.lag_ms == 0 {
                break;
            }
        }
        let expired: Vec<Vec<u8>> = self
            .model
            .iter()
            .filter(|(_, s)| s.deadline_ms().is_some_and(|d| self.now_ms > d))
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            self.note_transition(&key, None);
            self.model.remove(&key);
        }
    }

    fn expected(&self, id: IndexId) -> BTreeSet<(Vec<u8>, PkRef)> {
        let mut buf = IndexKeyBuf::new();
        let mut out = BTreeSet::new();
        for (pk, shadow) in &self.model {
            for (fact_id, value) in shadow.holds() {
                if fact_id != id {
                    continue;
                }
                index_key_encode(IndexKeyType::I64, IndexScalar::I64(value), &mut buf)
                    .expect("i64");
                let pk_ref = PkRef::from_key_hash(KeyHasher::default().hash(pk));
                out.insert((buf.as_bytes().to_vec(), pk_ref));
            }
        }
        out
    }

    fn tree(&self, id: IndexId) -> BTreeSet<(Vec<u8>, PkRef)> {
        let mut out = BTreeSet::new();
        let tree = self.ks.idx_tree(NS, id).expect("declared");
        let mut cursor = OrderedCursor::from_start();
        while let Some((key, entry_ref)) = tree.cursor_next(&mut cursor) {
            out.insert((key.to_vec(), entry_ref));
        }
        out
    }

    fn check(&mut self, context: &str) {
        self.drain_expiry();
        for id in [IDX_V, IDX_TAGS] {
            assert_eq!(self.ks.idx_degraded(NS, id), Some(false), "{context}: index {} veto", id.0);
            assert_eq!(self.tree(id), self.expected(id), "{context}: index {} ≠ the model", id.0);
        }
    }
}

fn random_doc(rng: &mut Rng) -> (i64, [i64; 2], Vec<u8>) {
    let v = rng.below(VALUES as u64) as i64;
    let tags = [rng.below(VALUES as u64) as i64, rng.below(VALUES as u64) as i64];
    let json = format!(r#"{{"v":{v},"tags":[{},{}],"w":{}}}"#, tags[0], tags[1], rng.below(9));
    (v, tags, JsonParser::new().parse(json.as_bytes()).expect("valid doc"))
}

/// Half the writes carry a short TTL, so records routinely cross their
/// deadline between two ops and meet the next bracket expired but
/// unreaped — the physical pre-image the coverage rules are about.
fn random_deadline(rng: &mut Rng, now_ms: u64) -> Option<u64> {
    (rng.below(2) == 0).then(|| now_ms + 1 + rng.below(16))
}

fn expire_of(deadline_ms: Option<u64>) -> SetExpire {
    deadline_ms.map_or(SetExpire::Clear, |ms| SetExpire::At(Nanos(ms * 1_000_000)))
}

/// One random op through the bracket, mirrored into the shadow.
fn step(h: &mut Harness, rng: &mut Rng) -> &'static str {
    h.now_ms += rng.below(4);
    let key = random_key(rng);
    // One op in twelve runs with the OOM gate firing inside its bracket.
    let gate = rng.below(12) == 0;
    // Writes outnumber deaths, so aliases coexist most of the time.
    match rng.below(9) {
        0..=3 => {
            let (v, tags, idoc) = random_doc(rng);
            let deadline_ms = random_deadline(rng, h.now_ms);
            // `WRONGTYPE` over a live string: a refusal, nothing changed.
            let applied = h.bracketed(&[&key], None, gate, |store, now| {
                let opts = JsonSetOptions { expire: expire_of(deadline_ms), ..Default::default() };
                store.json_set(&key, &idoc, opts, now).is_ok()
            });
            if applied {
                h.set_model(&key, Some(Shadow::Doc { v, tags, deadline_ms }));
            }
            "json_set"
        }
        4 => {
            h.bracketed(&[&key], None, gate, |store, now| store.del(&key, now));
            h.set_model(&key, None);
            "del"
        }
        5 => {
            let other = random_key(rng);
            h.bracketed(&[&key, &other], None, gate, |store, now| {
                store.del(&key, now);
                store.del(&other, now);
            });
            h.set_model(&key, None);
            h.set_model(&other, None);
            "del pair"
        }
        6 => {
            h.bracketed(&[&key], None, gate, |store, now| {
                store.set(&key, b"a string", SetOptions::default(), now).expect("set");
            });
            h.set_model(&key, Some(Shadow::Str { deadline_ms: None }));
            "set string"
        }
        7 => {
            let target = random_key(rng);
            let moved = h.bracketed(&[&key, &target], None, gate, |store, now| {
                store.rename(&key, &target, now).unwrap_or(false)
            });
            if moved && key != target {
                let record = h.model.get(&key).cloned().expect("a moved source was present");
                h.set_model(&key, None);
                h.set_model(&target, Some(record));
            }
            "rename"
        }
        _ => {
            // A non-root write at `$.w`: both indexes are pruned. Aimed
            // at a document that is expired but unreaped whenever one
            // exists — the state in which the body's own lookup kills
            // the pruned bracket's key.
            let key = h.expired_unreaped_doc().unwrap_or(key);
            let replacement = h.live(&key).and_then(|shadow| match shadow {
                Shadow::Doc { v, tags, .. } => Some((*v, *tags)),
                Shadow::Str { .. } => None,
            });
            let json = replacement.map_or_else(
                || r#"{"v":0,"tags":[0,0],"w":0}"#.to_string(),
                |(v, tags)| format!(r#"{{"v":{v},"tags":[{},{}],"w":77}}"#, tags[0], tags[1]),
            );
            let idoc = JsonParser::new().parse(json.as_bytes()).expect("valid doc");
            let is_doc = replacement.is_some();
            h.bracketed(&[&key], Some("$.w"), gate, |store, now| {
                if is_doc {
                    let _ = store.json_replace(&key, &idoc, now);
                } else {
                    // The body's own lookup: it reaps an expired record.
                    let _ = store.json_get(&key, now);
                }
            });
            "pruned write"
        }
    }
}

fn storm(seed: u64, ops: u64) {
    let mut h = Harness::new();
    let mut rng = Rng(seed);
    let mut population = 0u64;
    for op in 0..ops {
        let what = step(&mut h, &mut rng);
        h.check(&format!("seed {seed:#x} op {op} ({what})"));
        population += h.model.len() as u64;
    }
    // Collision-dense means aliases coexist: on average more records are
    // present than there are groups, so some group holds two or more.
    assert!(population / ops > GROUPS, "seed {seed:#x}: mean population {}", population / ops);
    let counters = h.ks.idx_counters_total();
    let t = &h.tallies;
    assert!(t.gate_passes > 0, "seed {seed:#x}: no gate pass ran");
    assert!(t.kept_expected > 0, "seed {seed:#x}: the model never expected a suppression");
    assert!(
        t.let_through_beside_an_alias > 0,
        "seed {seed:#x}: no removal went through beside an alias"
    );
    assert!(counters.alias_kept > 0, "seed {seed:#x}: idx_alias_kept never moved");
    assert!(counters.alias_groups > 0, "seed {seed:#x}: idx_alias_groups never moved");
    assert!(counters.cover_alias > 0, "seed {seed:#x}: idx_cover_alias never moved");
    assert!(counters.gate_forget > 0, "seed {seed:#x}: idx_gate_forget never moved");
    assert!(counters.prune_void > 0, "seed {seed:#x}: idx_prune_void never moved");
    assert_eq!(counters.alias_walk_over, 0, "three-member groups never cross a budget");
    assert_eq!(counters.degraded_trips, 0);
}

#[test]
fn tree_equals_the_model_over_a_collision_dense_universe() {
    for seed in [0xA11A_5001u64, 0xA11A_5002, 0xA11A_5003] {
        storm(seed, 4_000);
    }
}

#[test]
#[ignore = "10^6-op storm — run in release explicitly"]
fn million_op_alias_storm() {
    storm(0xA11A_5EED, 1_000_000);
}
