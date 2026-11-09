//! `m3-document`'s `JSON.MGET` audits: one account for the whole reply
//! (ADR-0173 D1) at the shipped topology, keys split over the cells.
//!
//! Each writer owns one audit key per cell, chosen by the harness's own
//! router, and sets each once per life to one fixed pad document, on the
//! sends after its life's first root set. At three fixed send ordinals of
//! every root-set window it sends one audit of each shape:
//!
//! | Shape | Command | Under the scenario's budget |
//! |---|---|---|
//! | pair | `JSON.MGET a₀ a₁ $` | served |
//! | repeated | `JSON.MGET a₀ ×6 a₁ ×6 $` | each owner's part fits, the whole does not: refused |
//! | stream | `JSON.MGET k a₀ a₁ $`, `k` the writer's document | as its rendered length says |
//!
//! The expected reply comes from the writer's model and the budget alone,
//! and the owners from the harness's router, never from the product. Four
//! engagement counters, summed over the writers and kept across lives, say
//! what the audits reached; a run in which no writer was answered on its
//! window's last audit is `Unreached`, which no seed is red for alone.

use inf_foundation::CellId;
use inf_store::{SlotRouter, StoreConfig};

use super::{DurableReport, DurableScenario, Pending, Writer, encode};
use crate::lift::local_key;

/// The audits a scenario runs: the reply budget every cell's stores take,
/// which the model applies to each rendered reply.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MgetAudits {
    pub reply_budget_bytes: usize,
}

/// What a run's audits reached, node sums over every life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MgetAuditReport {
    /// The scenario ran audits.
    pub ran: bool,
    /// Answered audits the model served.
    pub served: u64,
    /// Answered audits the model refused.
    pub refused: u64,
    /// Answered audits whose keys have two owners.
    pub split: u64,
    /// Answered audits with two owners, every owner's part at or under the
    /// budget and the whole over it: the case only the coordinator's
    /// account refuses.
    pub split_sum_refused: u64,
    /// No writer was answered on its window's last audit.
    pub unreached: bool,
}

impl MgetAuditReport {
    /// The single run's report line.
    pub fn describe(&self) -> String {
        format!(
            "mget audits [served:{} refused:{} split:{} split_sum_refused:{} unreached:{}]",
            self.served, self.refused, self.split, self.split_sum_refused, self.unreached
        )
    }
}

/// A sweep shard's audits: the counters summed over the seeds that ran
/// them, and how many of those seeds reported `Unreached`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MgetAuditFold {
    seeds: u64,
    unreached: u64,
    sum: MgetAuditReport,
}

impl MgetAuditFold {
    pub fn absorb(&mut self, report: &MgetAuditReport) {
        if !report.ran {
            return;
        }
        self.seeds += 1;
        self.unreached += u64::from(report.unreached);
        self.sum.served += report.served;
        self.sum.refused += report.refused;
        self.sum.split += report.split;
        self.sum.split_sum_refused += report.split_sum_refused;
    }

    /// The shard's coverage, empty when no seed ran the audits.
    pub fn coverage(&self) -> String {
        if self.seeds == 0 {
            return String::new();
        }
        let MgetAuditReport { served, refused, split, split_sum_refused, .. } = self.sum;
        format!(
            ", mget audits [seeds:{} unreached:{} served:{served} refused:{refused} \
             split:{split} split_sum_refused:{split_sum_refused}]",
            self.seeds, self.unreached
        )
    }

    /// A shard every seed of which reported `Unreached` proves nothing of
    /// the audits: red.
    pub fn shard_end_clauses(&self) -> Vec<String> {
        if self.seeds > 0 && self.unreached == self.seeds {
            return vec![format!(
                "VACUOUS mget-audits: all {} seeds of the shard were Unreached — no writer \
                 was answered on its window's last audit",
                self.seeds
            )];
        }
        Vec::new()
    }
}

/// One writer's audits: the shape in flight and what was answered.
#[derive(Clone, Debug, Default)]
pub(crate) struct MgetTally {
    inflight: Option<AuditClass>,
    served: u64,
    refused: u64,
    split: u64,
    split_sum_refused: u64,
    /// The writer was answered on its window's last audit.
    reached: bool,
}

/// What the model decided for one audit, kept until its reply.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct AuditClass {
    shape: AuditShape,
    refused: bool,
    two_owners: bool,
    parts_fit: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum AuditShape {
    Pair,
    Repeated,
    Stream,
}

/// The root-set window, in sends: a writer's document is set whole at every
/// ordinal it divides (`document::next_document_command`).
const WINDOW_SENDS: u64 = 96;

/// Bytes of the pad document. Its `$` reply, the bulk `[pad]`, is 1,035 B,
/// so the repeated shape's six copies per owner (6,214 B) fit an 8,192 B
/// budget and its twelve (12,425 B) do not.
const PAD_DOCUMENT_BYTES: usize = 1024;

/// Copies of each audit key in the repeated shape.
const REPEATED_COPIES: usize = 6;

/// The refusal line the whole-reply rule answers (ADR-0099 D3's text).
const REPLY_TOO_LARGE: &[u8] = b"-ERR reply too large\r\n";

/// What a send ordinal of an audited writer carries.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum AuditSend {
    /// The pad set of the audit key on this cell.
    Pad(u16),
    Audit(AuditShape),
    Other,
}

/// The send at `sent` (this life's ordinal) on a node of `cells` cells: the
/// pads right after the life's first root set, then the three audits right
/// after each window's root set (and the first window's pads).
fn send_at(sent: u64, cells: u16) -> AuditSend {
    let cells_count = u64::from(cells);
    let pad = sent.checked_sub(1).filter(|&index| index < cells_count);
    if let Some(cell) = pad.and_then(|index| u16::try_from(index).ok()) {
        return AuditSend::Pad(cell);
    }
    match (sent % WINDOW_SENDS).checked_sub(cells_count + 1) {
        Some(0) => AuditSend::Audit(AuditShape::Pair),
        Some(1) => AuditSend::Audit(AuditShape::Repeated),
        Some(2) => AuditSend::Audit(AuditShape::Stream),
        Some(_) | None => AuditSend::Other,
    }
}

/// The pad document's canonical text: `{"pad":"x…x"}`, `PAD_DOCUMENT_BYTES`
/// long, which `JSON.GET` answers byte for byte.
fn pad_document() -> Vec<u8> {
    let fill = PAD_DOCUMENT_BYTES - br#"{"pad":""}"#.len();
    format!(r#"{{"pad":"{}"}}"#, "x".repeat(fill)).into_bytes()
}

/// `store` with the scenario's audit budget as its `doc-max-reply-bytes`.
pub(crate) fn with_reply_budget(scenario: &DurableScenario, store: StoreConfig) -> StoreConfig {
    match scenario.mget_audits {
        Some(audits) => StoreConfig { doc_max_reply_bytes: audits.reply_budget_bytes, ..store },
        None => store,
    }
}

/// The writer's audit key on `cell`, owned there by the harness's router.
fn audit_key(writer: usize, cell: u16, cells: u16) -> Vec<u8> {
    local_key(&format!("m{writer}"), usize::from(cell), cells)
}

/// A document writer's send when it is a pad set or an audit, `None`
/// otherwise. Every send of an audited writer passes here first, so the
/// shape in flight is always this send's.
pub(crate) fn next_audit_command(
    writer: &mut Writer,
    scenario: &DurableScenario,
    key: &[u8],
) -> Option<(Vec<u8>, Pending)> {
    let audits = scenario.mget_audits?;
    writer.mget.inflight = None;
    match send_at(writer.sent, scenario.cells) {
        AuditSend::Other => None,
        AuditSend::Pad(cell) => {
            let key = audit_key(writer.id, cell, scenario.cells);
            let pad = pad_document();
            let wire = encode(&[b"JSON.SET", &key, b"$", &pad]);
            let expect = b"+OK\r\n".to_vec();
            Some((
                wire,
                Pending { key, state_after: Some(pad), expect, mutates: true, taints: false },
            ))
        }
        AuditSend::Audit(shape) => Some(audit(writer, scenario, audits, shape, key)),
    }
}

/// One audit of `shape`: its command, and the reply the model expects.
fn audit(
    writer: &mut Writer,
    scenario: &DurableScenario,
    audits: MgetAudits,
    shape: AuditShape,
    stream_key: &[u8],
) -> (Vec<u8>, Pending) {
    let cells = scenario.cells;
    let pads: Vec<Vec<u8>> = (0..cells).map(|cell| audit_key(writer.id, cell, cells)).collect();
    let mut keys: Vec<Vec<u8>> = Vec::new();
    match shape {
        AuditShape::Pair => keys.extend(pads.iter().cloned()),
        AuditShape::Repeated => {
            for pad in &pads {
                keys.extend(std::iter::repeat_n(pad.clone(), REPEATED_COPIES));
            }
        }
        AuditShape::Stream => {
            keys.push(stream_key.to_vec());
            keys.extend(pads.iter().cloned());
        }
    }
    let pad = pad_document();
    let texts: Vec<Vec<u8>> = keys
        .iter()
        .map(|key| match writer.models.get(key) {
            Some(model) => model.render(),
            None => pad.clone(),
        })
        .collect();
    let owners = owners_of(&keys, cells);
    let (expect, class) = expected(shape, &texts, &owners, audits.reply_budget_bytes);
    writer.mget.inflight = Some(class);
    let mut argv: Vec<&[u8]> = vec![b"JSON.MGET"];
    argv.extend(keys.iter().map(Vec::as_slice));
    argv.push(b"$");
    let first = keys.first().cloned().unwrap_or_default();
    (
        encode(&argv),
        Pending { key: first, state_after: None, expect, mutates: false, taints: false },
    )
}

/// Each key's owning cell, by the harness's contiguous router (the node's).
fn owners_of(keys: &[Vec<u8>], cells: u16) -> Vec<CellId> {
    let router = SlotRouter::new_contiguous(cells);
    keys.iter().map(|key| router.cell_of(SlotRouter::slot_of(key))).collect()
}

/// The `$` element of a document whose canonical text is `text`: the bulk
/// of the one-match array `[text]`.
fn element(text: &[u8]) -> Vec<u8> {
    let mut out = format!("${}\r\n[", text.len() + 2).into_bytes();
    out.extend_from_slice(text);
    out.extend_from_slice(b"]\r\n");
    out
}

/// The RESP2 array of `elements`, as one reply.
fn array(elements: &[&Vec<u8>]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", elements.len()).into_bytes();
    for element in elements {
        out.extend_from_slice(element);
    }
    out
}

/// The reply the whole-reply rule answers for documents `texts` under
/// `budget`, and the audit's class: the array when it renders at or under
/// the budget, else the refusal line; each owner's part is the array of
/// that owner's elements alone.
fn expected(
    shape: AuditShape,
    texts: &[Vec<u8>],
    owners: &[CellId],
    budget: usize,
) -> (Vec<u8>, AuditClass) {
    let elements: Vec<Vec<u8>> = texts.iter().map(|text| element(text)).collect();
    let whole = array(&elements.iter().collect::<Vec<_>>());
    let mut distinct: Vec<CellId> = owners.to_vec();
    distinct.sort_unstable_by_key(|cell| cell.0);
    distinct.dedup();
    let parts_fit = distinct.iter().all(|owner| {
        let part: Vec<&Vec<u8>> =
            elements.iter().zip(owners).filter(|(_, o)| *o == owner).map(|(e, _)| e).collect();
        array(&part).len() <= budget
    });
    let refused = whole.len() > budget;
    let class = AuditClass { shape, refused, two_owners: distinct.len() > 1, parts_fit };
    let reply = if refused { REPLY_TOO_LARGE.to_vec() } else { whole };
    (reply, class)
}

impl MgetTally {
    /// The writer's in-flight send was answered: an audit's class counts.
    pub(crate) fn answered(&mut self) {
        let Some(class) = self.inflight.take() else { return };
        if class.refused {
            self.refused += 1;
        } else {
            self.served += 1;
        }
        if class.two_owners {
            self.split += 1;
            self.split_sum_refused += u64::from(class.parts_fit && class.refused);
        }
        self.reached |= class.shape == AuditShape::Stream;
    }
}

/// The run's audit report from its writers, with the engagement rule: in a
/// run where a writer was answered on its window's last audit, a counter
/// the rule names at zero is `VACUOUS`, which is red.
pub(crate) fn audit_report(
    scenario: &DurableScenario,
    writers: &[Writer],
    report: &mut DurableReport,
) {
    if scenario.mget_audits.is_none() {
        return;
    }
    let mut sum = MgetAuditReport { ran: true, ..MgetAuditReport::default() };
    let mut reached = false;
    for tally in writers.iter().map(|writer| &writer.mget) {
        sum.served += tally.served;
        sum.refused += tally.refused;
        sum.split += tally.split;
        sum.split_sum_refused += tally.split_sum_refused;
        reached |= tally.reached;
    }
    sum.unreached = !reached;
    if reached {
        for (name, count) in [("mget-served", sum.served), ("mget-split", sum.split)] {
            if count == 0 {
                report.violations.push(format!(
                    "VACUOUS seed {:#x}: {name} — a writer was answered on every audit shape \
                     and this counter stayed 0",
                    scenario.seed
                ));
            }
        }
    }
    report.mget = sum;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pads follow the life's first root set, and the audits follow
    /// each window's root set, in the order pair, repeated, stream.
    #[test]
    fn the_ordinals_place_pads_then_audits_after_each_root_set() {
        let sends: Vec<AuditSend> = (0..7).map(|sent| send_at(sent, 2)).collect();
        assert_eq!(
            sends,
            [
                AuditSend::Other,
                AuditSend::Pad(0),
                AuditSend::Pad(1),
                AuditSend::Audit(AuditShape::Pair),
                AuditSend::Audit(AuditShape::Repeated),
                AuditSend::Audit(AuditShape::Stream),
                AuditSend::Other,
            ]
        );
        let window: Vec<AuditSend> = (96..103).map(|sent| send_at(sent, 2)).collect();
        assert_eq!(&window[..3], [AuditSend::Other; 3]);
        assert_eq!(window[3], AuditSend::Audit(AuditShape::Pair));
        assert_eq!(window[5], AuditSend::Audit(AuditShape::Stream));
    }

    /// The record's arithmetic: a pad element is 1,035 B, the pair 2,074 B,
    /// one owner's six copies 6,214 B and the repeated whole 12,425 B, so
    /// at 8,192 B the repeated shape is the coordinator's refusal alone.
    #[test]
    fn the_shapes_render_the_records_byte_counts() {
        let pad = pad_document();
        assert_eq!(pad.len(), PAD_DOCUMENT_BYTES);
        assert_eq!(element(&pad).len(), 1_035);
        let (c0, c1) = (CellId(0), CellId(1));
        let pair = expected(AuditShape::Pair, &[pad.clone(), pad.clone()], &[c0, c1], 8_192);
        assert_eq!(pair.0.len(), 2_074);
        assert!(!pair.1.refused && pair.1.two_owners);
        let texts = vec![pad.clone(); 12];
        let owners: Vec<CellId> = (0..12).map(|i| if i < 6 { c0 } else { c1 }).collect();
        let repeated = expected(AuditShape::Repeated, &texts, &owners, 8_192);
        assert_eq!(repeated.0, REPLY_TOO_LARGE);
        assert!(repeated.1.refused && repeated.1.parts_fit && repeated.1.two_owners);
        let served = expected(AuditShape::Repeated, &texts, &owners, 12_425);
        assert_eq!(served.0.len(), 12_425);
        assert_eq!(&served.0[..5], b"*12\r\n");
        let one = element(&pad);
        assert_eq!(array(&[&one; 6]).len(), 6_214);
    }

    /// The tally counts answered audits only, each by its class, and the
    /// stream shape is the window's last.
    #[test]
    fn the_tally_counts_each_answered_class() {
        let mut tally = MgetTally::default();
        tally.answered();
        assert_eq!((tally.served, tally.refused), (0, 0), "a non-audit send counts nothing");
        let refused = AuditClass {
            shape: AuditShape::Repeated,
            refused: true,
            two_owners: true,
            parts_fit: true,
        };
        tally.inflight = Some(refused);
        tally.answered();
        tally.inflight = Some(AuditClass { shape: AuditShape::Stream, refused: false, ..refused });
        tally.answered();
        assert_eq!(
            (tally.served, tally.refused, tally.split, tally.split_sum_refused),
            (1, 1, 2, 1)
        );
        assert!(tally.reached);
        assert_eq!(tally.inflight, None);
    }
}
