#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "benchmark: fixture files outside cell code (ADR-0144 D5)"
)]
//! The serializer's rows, by instructions per operation.
//!
//! One operation is one call into a cleared buffer that keeps its capacity;
//! every fixture is built outside the counted operations. The reply rows go
//! through `JSON.GET`'s path, `serialize_reply_into_bounded` at the default
//! reply budget (128 MiB):
//!
//! | Row | Value | Reply shape | Options |
//! |---|---|---|---|
//! | `tiny` | `{"a":1}` | legacy | compact |
//! | `tiny10` | `{"a":10}` | legacy | compact |
//! | `small` | `small-200B` | legacy | compact |
//! | `kib` | `gate-1KiB` | legacy | compact |
//! | `kib-formatted` | `gate-1KiB` | legacy | formatted |
//! | `kib-dollar` | `gate-1KiB` | one `$` path | formatted |
//! | `kib-two-paths` | `gate-1KiB` | two `$` paths | formatted |
//! | `mib` | 16 `large-64KiB` in one array | legacy | compact |
//! | `mib-formatted` | the same | legacy | formatted |
//! | `arrpop-kib` | `gate-1KiB` | `serialize_into_bounded` | compact |
//! | `validate-kib` | `gate-1KiB` | `CanonicalDoc::validate` | — |
//! | `validate-mib` | the 1 MiB array | `CanonicalDoc::validate` | — |
//!
//! Formatted is `INDENT` two spaces, `NEWLINE` a newline, `SPACE` a space.
//!
//! The estimator is `instructions:u` per operation on one pinned core,
//! `(I(n) − I(0)) / n`; a leg is this binary under `--variant <row><plant>`:
//!
//!   cargo bench -p inf-doc --bench ser --no-run
//!   taskset -c <core> perf stat -e instructions:u -x, -- <binary> \
//!       --variant <row>+0 --ops <n>
//!
//! Two builds of this file are compared, legs A B B A twice, each `<row>+0`.
//! The plants are judged against their own build: `<row>+1` runs one `nop`
//! more per operation, and `<row>+4` runs 4 % more operations, one more
//! for every 25. The three suffixes are of one length, so a row's legs
//! start from one heap: a check row's count moves with where the
//! allocator grows the walk's stack. Each leg prints the bytes its
//! operations wrote and a checksum of one operation's output, so two
//! builds that count different work read apart before their counts do. A
//! check row writes nothing: its checksum is of the document it checked.
//!
//! `tiny` and `tiny10` are the stack rows: one container, one key, and a
//! number of one digit or of two, so the walk's own cost a call is most of
//! the count, with and without a one-digit number's own path.
//!
//! Without `--variant` the binary runs the same rows under criterion.

use criterion::{Criterion, criterion_group};
use std::hint::black_box;
use std::process::ExitCode;

use inf_doc::ser::{Reply, SerializeOpts, serialize_into_bounded, serialize_reply_into_bounded};
use inf_doc::{CanonicalDoc, DocValue, JsonParser, TapeDoc};

#[allow(dead_code, unused_imports)] // shared generator also contains its CLI and witness tests
#[path = "../../../bins/inf-bench/src/doc_corpus.rs"]
mod doc_corpus;

/// The default reply budget, `doc-max-reply-bytes`.
const REPLY_BYTES_MAX: usize = 128 << 20;

const FORMATTED: SerializeOpts<'static> =
    SerializeOpts { indent: b"  ", newline: b"\n", space: b" " };
const COMPACT: SerializeOpts<'static> = SerializeOpts { indent: b"", newline: b"", space: b"" };

/// What one operation calls.
#[derive(Copy, Clone)]
enum Call {
    Legacy,
    Dollar,
    TwoPaths,
    ArrPop,
    Validate,
}

/// The one table of rows: `--variant` is parsed against it.
const ROWS: [(&str, &str, Call, bool); 12] = [
    ("tiny", "tiny", Call::Legacy, false),
    ("tiny10", "tiny10", Call::Legacy, false),
    ("small", "small-200B", Call::Legacy, false),
    ("kib", "gate-1KiB", Call::Legacy, false),
    ("kib-formatted", "gate-1KiB", Call::Legacy, true),
    ("kib-dollar", "gate-1KiB", Call::Dollar, true),
    ("kib-two-paths", "gate-1KiB", Call::TwoPaths, true),
    ("mib", "mib", Call::Legacy, false),
    ("mib-formatted", "mib", Call::Legacy, true),
    ("arrpop-kib", "gate-1KiB", Call::ArrPop, false),
    ("validate-kib", "gate-1KiB", Call::Validate, false),
    ("validate-mib", "mib", Call::Validate, false),
];

/// The plants: a suffix of `--variant`, the `nop`s per operation, and
/// whether one operation more runs for every 25. Every suffix has the same
/// length (module docs).
const PLANTS: [(&str, u8, bool); 3] = [("+0", 0, false), ("+1", 1, false), ("+4", 0, true)];

fn document_text(shape: &str) -> String {
    match shape {
        "tiny" => r#"{"a":1}"#.to_owned(),
        "tiny10" => r#"{"a":10}"#.to_owned(),
        "mib" => {
            let instance = |i| doc_corpus::instance(doc_corpus::CANONICAL_SEED, "large-64KiB", i);
            format!("[{}]", (0..16).map(instance).collect::<Vec<_>>().join(","))
        }
        named => doc_corpus::shape(doc_corpus::CANONICAL_SEED, named).json,
    }
}

/// One row's fixture: the document's bytes, parsed once.
struct Fixture {
    idoc: Vec<u8>,
    call: Call,
    opts: SerializeOpts<'static>,
}

impl Fixture {
    fn new(shape: &str, call: Call, formatted: bool) -> Fixture {
        let text = document_text(shape);
        let idoc = JsonParser::new().parse(text.as_bytes()).expect("fixture parses");
        Fixture { idoc, call, opts: if formatted { FORMATTED } else { COMPACT } }
    }

    fn reply<'a>(&self, root: DocValue<'a>) -> Reply<'a> {
        let matches = |value| Reply::Array(vec![Reply::Value(value)]);
        match self.call {
            Call::Dollar => matches(root),
            Call::TwoPaths => Reply::Object(vec![
                (b"$".as_slice(), matches(root)),
                (b"$.*".as_slice(), matches(root)),
            ]),
            Call::Legacy | Call::ArrPop | Call::Validate => Reply::Value(root),
        }
    }
}

/// The plants' `nop`s: `NOPS` instructions per operation, each with no
/// operand, so the count reads exactly `NOPS` more.
#[inline(always)]
fn plant<const NOPS: u8>() {
    // With no `nop` the leg emits no `asm!` at all.
    if NOPS > 0 {
        // SAFETY: a `nop` touches no register, memory or flag.
        unsafe {
            std::arch::asm!(
                ".rept {count}",
                "nop",
                ".endr",
                count = const NOPS,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
}

// One operation per call, never inlined into the loop that counts it.
#[inline(never)]
fn reply_op<const NOPS: u8>(
    reply: &Reply<'_>,
    opts: &SerializeOpts<'_>,
    out: &mut Vec<u8>,
) -> usize {
    plant::<NOPS>();
    out.clear();
    serialize_reply_into_bounded(reply, opts, out, REPLY_BYTES_MAX).expect("within the budget");
    out.len()
}

#[inline(never)]
fn arrpop_op<const NOPS: u8>(
    value: DocValue<'_>,
    opts: &SerializeOpts<'_>,
    out: &mut Vec<u8>,
) -> usize {
    plant::<NOPS>();
    out.clear();
    serialize_into_bounded(value, opts, out, REPLY_BYTES_MAX).expect("within the budget");
    out.len()
}

#[inline(never)]
fn validate_op<const NOPS: u8>(idoc: &[u8]) -> usize {
    plant::<NOPS>();
    CanonicalDoc::validate(idoc).expect("the fixture is canonical").as_bytes().len()
}

/// The three operations of one plant, called through pointers: the loop
/// that counts them is one function for every plant, so a planted leg
/// differs from its plain one by the plant alone.
#[derive(Copy, Clone)]
struct Ops {
    reply: fn(&Reply<'_>, &SerializeOpts<'_>, &mut Vec<u8>) -> usize,
    arrpop: fn(DocValue<'_>, &SerializeOpts<'_>, &mut Vec<u8>) -> usize,
    validate: fn(&[u8]) -> usize,
}

impl Ops {
    const fn of<const NOPS: u8>() -> Ops {
        Ops { reply: reply_op::<NOPS>, arrpop: arrpop_op::<NOPS>, validate: validate_op::<NOPS> }
    }
}

/// Runs `ops` operations of `fixture`, and one more for every 25 when
/// `doubled`: the same loop over 4 % more operations, so the plant adds no
/// instruction of its own. Returns the bytes they wrote and a checksum of
/// the last one's output.
#[inline(never)]
fn drive(fixture: &Fixture, plant: Ops, ops: u64, doubled: bool) -> (u64, u64) {
    let doc = TapeDoc::from_bytes(&fixture.idoc).expect("fixture validates");
    let root = DocValue::from(doc.root());
    let reply = fixture.reply(root);
    let plant = black_box(plant);
    let mut out = Vec::new();
    let one = |out: &mut Vec<u8>| match fixture.call {
        Call::Legacy | Call::Dollar | Call::TwoPaths => {
            (plant.reply)(black_box(&reply), black_box(&fixture.opts), out)
        }
        Call::ArrPop => (plant.arrpop)(black_box(root), black_box(&fixture.opts), out),
        Call::Validate => (plant.validate)(black_box(&fixture.idoc)),
    };
    // The buffer reaches its capacity before the counted operations.
    one(&mut out);
    let runs = if doubled { ops + ops / 25 } else { ops };
    let mut bytes = 0u64;
    for _ in 0..runs {
        bytes = bytes.wrapping_add(one(&mut out) as u64);
    }
    // A check writes no text: its work is the document it passed.
    let checked = CanonicalDoc::validate(&fixture.idoc).expect("the fixture is canonical");
    let work = match fixture.call {
        Call::Validate => checked.as_bytes(),
        Call::Legacy | Call::Dollar | Call::TwoPaths | Call::ArrPop => out.as_slice(),
    };
    (bytes, checksum(work))
}

/// FNV-1a over one operation's output.
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

fn bench_ser(c: &mut Criterion) {
    let mut group = c.benchmark_group("ser");
    for (name, shape, call, formatted) in ROWS {
        let fixture = Fixture::new(shape, call, formatted);
        let doc = TapeDoc::from_bytes(&fixture.idoc).expect("fixture validates");
        let root = DocValue::from(doc.root());
        let reply = fixture.reply(root);
        let mut out = Vec::new();
        group.bench_function(name, |b| {
            b.iter(|| match fixture.call {
                Call::Legacy | Call::Dollar | Call::TwoPaths => {
                    reply_op::<0>(black_box(&reply), black_box(&fixture.opts), &mut out)
                }
                Call::ArrPop => arrpop_op::<0>(black_box(root), black_box(&fixture.opts), &mut out),
                Call::Validate => validate_op::<0>(black_box(&fixture.idoc)),
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_ser);

fn main() -> ExitCode {
    if std::env::args().any(|arg| arg == "--variant") {
        return legs::main();
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
    ExitCode::SUCCESS
}

/// The legs of the A/B, by instructions per operation.
mod legs {
    use std::process::ExitCode;

    use super::{Fixture, Ops, PLANTS, ROWS, drive};

    fn variants() -> Vec<String> {
        let names = |plant: &str| ROWS.map(|(row, ..)| format!("{row}{plant}"));
        PLANTS.iter().flat_map(|(plant, ..)| names(plant)).collect()
    }

    fn parse() -> Result<(String, u64), String> {
        let mut variant = None;
        let mut ops = None;
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            if flag == "--bench" {
                continue; // cargo bench passes it to a harness-free binary
            }
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--variant" => variant = Some(value),
                "--ops" => ops = Some(value.parse::<u64>().map_err(|e| format!("--ops: {e}"))?),
                other => return Err(format!("unknown flag {other}")),
            }
        }
        Ok((variant.ok_or("--variant is required")?, ops.ok_or("--ops is required")?))
    }

    pub(super) fn main() -> ExitCode {
        let (variant, ops) = match parse() {
            Ok(args) => args,
            Err(error) => {
                eprintln!("ser legs: {error}");
                return ExitCode::from(2);
            }
        };
        let found = PLANTS.iter().find_map(|&(plant, nops, doubled)| {
            let row = variant.strip_suffix(plant)?;
            let &(_, shape, call, formatted) = ROWS.iter().find(|(name, ..)| *name == row)?;
            Some((Fixture::new(shape, call, formatted), nops, doubled))
        });
        let Some((fixture, nops, doubled)) = found else {
            eprintln!("ser legs: --variant {variant}: one of {}", variants().join(", "));
            return ExitCode::from(2);
        };
        let plant = match nops {
            0 => Ops::of::<0>(),
            1 => Ops::of::<1>(),
            other => {
                eprintln!("ser legs: {variant}: no plant of {other} nops");
                return ExitCode::from(2);
            }
        };
        let (bytes, checksum) = drive(&fixture, plant, ops, doubled);
        println!("{variant} ops={ops} bytes={bytes} checksum={checksum:016x}");
        ExitCode::SUCCESS
    }
}
