//! Seeded M3 reference-document corpus (M3-S20).
//!
//! This module deliberately uses only `std`: it is measurement input, not
//! document-engine code. M3 tests and benches include this one source file
//! directly so every memory/latency row consumes the same bytes without a
//! new shipping dependency edge. The checked-in manifest records byte
//! witnesses; generated JSON files do not live in git.

use std::fmt::Write as _;
use std::path::Path;

#[allow(dead_code)] // consumed by M3 benches/tests that include this source
pub const CANONICAL_SEED: u64 = 0x1D0C_2026;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CorpusDoc {
    pub name: &'static str,
    pub file: &'static str,
    pub contract: &'static str,
    pub json: String,
}

impl CorpusDoc {
    #[inline]
    pub fn byte_witness(&self) -> u64 {
        fnv1a64(self.json.as_bytes())
    }
}

/// SplitMix64: fixed-width integer operations only, deterministic on every
/// target Rust supports. Shape-local streams keep one shape stable if a
/// later manifest version adds another shape.
struct Rng(u64);

impl Rng {
    fn new(seed: u64, stream: u64) -> Rng {
        Rng(seed ^ stream)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        debug_assert!(bound > 0);
        self.next() % bound
    }

    fn word(&mut self) -> &'static str {
        const WORDS: &[&str] = &[
            "alpha", "systems", "catalog", "optics", "lens", "vector", "engine", "durable",
            "stream", "record", "packet", "orange", "printer", "keyboard", "monitor", "quartz",
        ];
        WORDS[self.below(WORDS.len() as u64) as usize]
    }

    fn push_ascii(&mut self, out: &mut String, bytes: usize) {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        for _ in 0..bytes {
            out.push(ALPHABET[self.below(ALPHABET.len() as u64) as usize] as char);
        }
    }
}

fn finish_padded(mut json: String, target: usize, rng: &mut Rng) -> String {
    const SUFFIX: &str = "\"}";
    assert!(json.len() + SUFFIX.len() <= target, "shape base exceeds target");
    let padding = target - json.len() - SUFFIX.len();
    rng.push_ascii(&mut json, padding);
    json.push_str(SUFFIX);
    assert_eq!(json.len(), target);
    json
}

fn small(seed: u64) -> String {
    let mut rng = Rng::new(seed, 0x534D_414C_4C20_0200);
    let json = format!(
        "{{\"id\":{},\"name\":\"{}\",\"active\":true,\"score\":{},\"tags\":[\"{}\",\"{}\"],\"pad\":\"",
        rng.below(1_000_000),
        rng.word(),
        rng.below(10_000),
        rng.word(),
        rng.word(),
    );
    finish_padded(json, 200, &mut rng)
}

fn gate_level(depth: usize, rng: &mut Rng) -> String {
    let mut json = format!(
        "{{\"id\":{},\"name\":\"{}\",\"active\":{},\"score\":0.815,\"note\":\"{}\"",
        rng.below(1_000_000),
        rng.word(),
        if rng.below(2) == 0 { "false" } else { "true" },
        rng.word(),
    );
    if depth > 0 {
        json.push_str(",\"child\":");
        json.push_str(&gate_level(depth - 1, rng));
    }
    json.push('}');
    json
}

fn gate(seed: u64) -> String {
    let mut rng = Rng::new(seed, 0x4741_5445_0000_0400);
    let child = gate_level(3, &mut rng);
    let json = format!(
        "{{\"kind\":\"gate\",\"id\":{},\"score\":0.815,\"child\":{child},\"pad\":\"",
        rng.below(1_000_000),
    );
    finish_padded(json, 1_024, &mut rng)
}

fn item(index: usize, rng: &mut Rng) -> String {
    format!(
        "{{\"id\":{index},\"name\":\"{}-{index}\",\"qty\":{},\"active\":{}}}",
        rng.word(),
        rng.below(97) + 1,
        if rng.below(2) == 0 { "false" } else { "true" },
    )
}

fn sized_array(seed: u64, stream: u64, target: usize, items: usize, kind: &str) -> String {
    let mut rng = Rng::new(seed, stream);
    let mut json = format!("{{\"kind\":\"{kind}\",\"items\":[");
    for index in 0..items {
        if index > 0 {
            json.push(',');
        }
        json.push_str(&item(index, &mut rng));
    }
    json.push_str("],\"pad\":\"");
    finish_padded(json, target, &mut rng)
}

fn deep(seed: u64) -> String {
    let mut rng = Rng::new(seed, 0x4445_4550_0000_0020);
    // The leaf is container depth 1; 31 wrappers make the exact contract 32.
    let mut json = format!("{{\"leaf\":true,\"value\":\"{}\"}}", rng.word());
    for level in (1..32).rev() {
        json = format!("{{\"level\":{level},\"next\":{json}}}");
    }
    json
}

fn wide(seed: u64) -> String {
    let mut rng = Rng::new(seed, 0x5749_4445_0000_2710);
    let mut json = String::with_capacity(600_000);
    json.push('[');
    for index in 0..10_000 {
        if index > 0 {
            json.push(',');
        }
        // Keep elements intentionally small: this shape stresses breadth,
        // not per-element payload size.
        write!(
            json,
            "{{\"id\":{index},\"name\":\"{}-{index}\",\"qty\":{}}}",
            rng.word(),
            rng.below(97) + 1,
        )
        .expect("writing to String cannot fail");
    }
    json.push(']');
    json
}

pub fn generate(seed: u64) -> Vec<CorpusDoc> {
    vec![
        CorpusDoc {
            name: "small-200B",
            file: "small-200B.json",
            contract: "exactly 200 JSON-text bytes",
            json: small(seed),
        },
        CorpusDoc {
            name: "gate-1KiB",
            file: "gate-1KiB.json",
            contract: "exactly 1024 JSON-text bytes; child depth 4",
            json: gate(seed),
        },
        CorpusDoc {
            name: "medium-2KiB",
            file: "medium-2KiB.json",
            contract: "exactly 2048 JSON-text bytes; 12 small objects",
            json: sized_array(seed, 0x4D45_4449_0000_0800, 2_048, 12, "medium"),
        },
        CorpusDoc {
            name: "large-64KiB",
            file: "large-64KiB.json",
            contract: "exactly 65536 JSON-text bytes; 256 small objects",
            json: sized_array(seed, 0x4C41_5247_0001_0000, 65_536, 256, "large"),
        },
        CorpusDoc {
            name: "deep-32",
            file: "deep-32.json",
            contract: "exactly 32 nested object containers",
            json: deep(seed),
        },
        CorpusDoc {
            name: "wide-array",
            file: "wide-array.json",
            contract: "exactly 10000 small object elements",
            json: wide(seed),
        },
    ]
}

#[allow(dead_code)] // consumed by M3 benches/tests that include this source
pub fn shape(seed: u64, name: &str) -> CorpusDoc {
    generate(seed)
        .into_iter()
        .find(|doc| doc.name == name)
        .unwrap_or_else(|| panic!("unknown document corpus shape {name}"))
}

/// Corpus v2 (ADR-0046 D3): the `index`-th unique instance of a shape.
/// Same size/shape contracts as v1; a per-index derived seed makes every
/// key's bytes distinct. v1's one-file-per-shape corpus loaded identical
/// bytes into N keys, and pinned RedisJSON stores identical large strings
/// at ~0.39x their unique-content cost (measured 2026-07-16: 36.3 KB vs
/// 92.9 KB per 64 KiB document) — an exploit no real workload offers, so
/// the RSS gate binds on unique instances.
pub fn instance(seed: u64, name: &str, index: u64) -> String {
    // One splitmix step decorrelates instance seeds from raw indices.
    let instance_seed = Rng::new(seed, 0x4952_4E53_5443_0002 ^ index).next();
    match name {
        "small-200B" => small(instance_seed),
        "gate-1KiB" => gate(instance_seed),
        "medium-2KiB" => sized_array(instance_seed, 0x4D45_4449_0000_0800, 2_048, 12, "medium"),
        "large-64KiB" => sized_array(instance_seed, 0x4C41_5247_0001_0000, 65_536, 256, "large"),
        "deep-32" => deep_v2(instance_seed),
        "wide-array" => wide(instance_seed),
        other => panic!("unknown document corpus shape {other}"),
    }
}

/// v2-only deep shape: v1's `deep()` carries a single 16-word leaf — 16
/// possible documents, so unique instances need an entropy-bearing leaf.
/// v1 bytes stay untouched (the checked-in manifest witnesses them).
fn deep_v2(seed: u64) -> String {
    let mut rng = Rng::new(seed, 0x4445_4550_0000_0020);
    let mut json =
        format!("{{\"leaf\":true,\"value\":\"{}-{}\"}}", rng.word(), rng.below(1_000_000_000_000));
    for level in (1..32).rev() {
        json = format!("{{\"level\":{level},\"next\":{json}}}");
    }
    json
}

pub fn manifest(seed: u64, corpus: &[CorpusDoc]) -> String {
    let mut out = String::new();
    writeln!(out, "version = 1").unwrap();
    writeln!(out, "seed = {seed}").unwrap();
    writeln!(out, "seed_hex = \"0x{seed:08X}\"").unwrap();
    writeln!(out, "generator = \"splitmix64-shape-streams-v1\"").unwrap();
    writeln!(out, "encoding = \"UTF-8 minified JSON\"").unwrap();
    writeln!(out, "bytes_checked_in = false").unwrap();
    for doc in corpus {
        writeln!(out).unwrap();
        writeln!(out, "[[shape]]").unwrap();
        writeln!(out, "name = \"{}\"", doc.name).unwrap();
        writeln!(out, "file = \"{}\"", doc.file).unwrap();
        writeln!(out, "contract = \"{}\"", doc.contract).unwrap();
        writeln!(out, "json_bytes = {}", doc.json.len()).unwrap();
        writeln!(out, "fnv1a64 = \"{:016x}\"", doc.byte_witness()).unwrap();
    }
    out
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

fn parse_seed(raw: &str) -> Result<u64, String> {
    match raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).map_err(|_| format!("invalid --seed `{raw}`")),
        None => raw.parse().map_err(|_| format!("invalid --seed `{raw}`")),
    }
}

/// `--counts small-200B=20000,gate-1KiB=20000,...` — the corpus-v2 count
/// vector. Order is preserved: the pipe loads shapes in the given order.
fn parse_counts(raw: &str) -> Result<Vec<(String, u64)>, String> {
    let mut counts = Vec::new();
    for part in raw.split(',') {
        let (name, n) = part.split_once('=').ok_or_else(|| format!("bad count `{part}`"))?;
        let n: u64 = n.parse().map_err(|_| format!("bad count `{part}`"))?;
        if n == 0 {
            return Err(format!("zero count `{part}`"));
        }
        counts.push((name.to_string(), n));
    }
    Ok(counts)
}

/// Emit the corpus-v2 RESP load pipe: `JSON.SET <shape>:<i> $ <doc_i>`
/// with per-index unique documents. Prints the serialized-byte total per
/// shape and overall plus an fnv1a64 pipe witness — the numbers the RSS
/// verdict divides by, produced by the same pinned generator.
fn emit_pipe(seed: u64, path: &Path, counts: &[(String, u64)]) -> Result<(), String> {
    use std::io::Write as _;
    let file = std::fs::File::create(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    let mut out = std::io::BufWriter::new(file);
    let mut witness = 0xCBF2_9CE4_8422_2325u64;
    let mut total_bytes = 0u64;
    let mut total_docs = 0u64;
    println!("pipe_version = 2");
    println!("seed_hex = \"0x{seed:08X}\"");
    for (name, n) in counts {
        let mut shape_bytes = 0u64;
        for index in 0..*n {
            let doc = instance(seed, name, index);
            shape_bytes += doc.len() as u64;
            let key = format!("{name}:{index}");
            let mut frame = Vec::with_capacity(doc.len() + key.len() + 64);
            frame.extend_from_slice(b"*4\r\n$8\r\nJSON.SET\r\n");
            let arg = |bytes: &[u8], frame: &mut Vec<u8>| {
                frame.extend_from_slice(format!("${}\r\n", bytes.len()).as_bytes());
                frame.extend_from_slice(bytes);
                frame.extend_from_slice(b"\r\n");
            };
            arg(key.as_bytes(), &mut frame);
            arg(b"$", &mut frame);
            arg(doc.as_bytes(), &mut frame);
            for byte in &frame {
                witness ^= u64::from(*byte);
                witness = witness.wrapping_mul(0x0000_0100_0000_01B3);
            }
            out.write_all(&frame).map_err(|error| format!("write pipe: {error}"))?;
        }
        total_bytes += shape_bytes;
        total_docs += n;
        println!("shape.{name} = {{ documents = {n}, serialized_bytes = {shape_bytes} }}");
    }
    out.flush().map_err(|error| format!("flush pipe: {error}"))?;
    println!("documents_total = {total_docs}");
    println!("serialized_bytes_total = {total_bytes}");
    println!("pipe_fnv1a64 = \"{witness:016x}\"");
    Ok(())
}

pub fn cmd_doc_corpus(args: &[String]) -> Result<(), String> {
    let mut seed = None;
    let mut out_dir = None;
    let mut pipe_path = None;
    let mut counts = None;
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            "--seed" => {
                let raw = args.get(at + 1).ok_or("--seed needs a value")?;
                seed = Some(parse_seed(raw)?);
                at += 2;
            }
            "--out" => {
                out_dir = Some(args.get(at + 1).ok_or("--out needs a directory")?.clone());
                at += 2;
            }
            "--pipe" => {
                pipe_path = Some(args.get(at + 1).ok_or("--pipe needs a file path")?.clone());
                at += 2;
            }
            "--counts" => {
                let raw = args.get(at + 1).ok_or("--counts needs shape=N[,shape=N...]")?;
                counts = Some(parse_counts(raw)?);
                at += 2;
            }
            flag => {
                return Err(format!(
                    "unknown doc-corpus flag `{flag}` (--seed, --out, --pipe, --counts)"
                ));
            }
        }
    }
    let seed = seed.ok_or("doc-corpus requires --seed")?;
    match (&pipe_path, &counts) {
        (Some(path), Some(counts)) => return emit_pipe(seed, Path::new(path), counts),
        (Some(_), None) | (None, Some(_)) => return Err("--pipe and --counts go together".into()),
        (None, None) => {}
    }
    let corpus = generate(seed);
    let rendered = manifest(seed, &corpus);
    if let Some(dir) = out_dir {
        let dir = Path::new(&dir);
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("create {}: {error}", dir.display()))?;
        for doc in &corpus {
            let path = dir.join(doc.file);
            std::fs::write(&path, doc.json.as_bytes())
                .map_err(|error| format!("write {}: {error}", path.display()))?;
        }
        let path = dir.join("manifest.toml");
        std::fs::write(&path, rendered.as_bytes())
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        println!("doc-corpus: wrote {} generated documents to {}", corpus.len(), dir.display());
    }
    print!("{rendered}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_shapes_are_byte_deterministic_and_contract_sized() {
        let a = generate(CANONICAL_SEED);
        let b = generate(CANONICAL_SEED);
        assert_eq!(a, b);
        assert_eq!(shape(CANONICAL_SEED, "small-200B").json.len(), 200);
        assert_eq!(shape(CANONICAL_SEED, "gate-1KiB").json.len(), 1_024);
        assert_eq!(shape(CANONICAL_SEED, "medium-2KiB").json.len(), 2_048);
        assert_eq!(shape(CANONICAL_SEED, "large-64KiB").json.len(), 65_536);
        assert_eq!(shape(CANONICAL_SEED, "deep-32").json.matches("\"next\":").count(), 31);
        assert_eq!(shape(CANONICAL_SEED, "wide-array").json.matches("\"id\":").count(), 10_000);
    }

    #[test]
    fn checked_manifest_is_the_generator_witness() {
        let corpus = generate(CANONICAL_SEED);
        assert_eq!(manifest(CANONICAL_SEED, &corpus), include_str!("../m3-doc-corpus.toml"));
    }

    #[test]
    fn corpus_v2_instances_are_unique_deterministic_and_contract_sized() {
        // Determinism: same (seed, shape, index) → same bytes.
        assert_eq!(
            instance(CANONICAL_SEED, "large-64KiB", 7),
            instance(CANONICAL_SEED, "large-64KiB", 7)
        );
        // Uniqueness across indices (the ADR-0046 D3 comparator-exploit
        // fix): no two keys of a shape hold identical bytes.
        let mut seen = std::collections::HashSet::new();
        for index in 0..64 {
            for shape in
                ["small-200B", "gate-1KiB", "medium-2KiB", "large-64KiB", "deep-32", "wide-array"]
            {
                assert!(seen.insert(instance(CANONICAL_SEED, shape, index)), "duplicate instance");
            }
        }
        // Size/shape contracts carry over from v1.
        assert_eq!(instance(CANONICAL_SEED, "small-200B", 3).len(), 200);
        assert_eq!(instance(CANONICAL_SEED, "gate-1KiB", 3).len(), 1_024);
        assert_eq!(instance(CANONICAL_SEED, "medium-2KiB", 3).len(), 2_048);
        assert_eq!(instance(CANONICAL_SEED, "large-64KiB", 3).len(), 65_536);
        assert_eq!(instance(CANONICAL_SEED, "deep-32", 3).matches("\"next\":").count(), 31);
        assert_eq!(instance(CANONICAL_SEED, "wide-array", 3).matches("\"id\":").count(), 10_000);
    }
}
