//! One byte account per `JSON.*` reply (ADR-0099 A1).
//!
//! `execute_json` opens a [`JsonReply`] over the connection's writer, sized
//! from the namespace's `doc-max-reply-bytes`, and hands it to the handler
//! by value. Every non-error byte the reply writes is charged to the one
//! account, framing included. A write that would cross the account writes
//! nothing and returns [`ReplyTooLarge`]; the handler then consumes the
//! reply with [`JsonReply::refuse`], which rolls the whole reply back and
//! answers `ERR reply too large`. Every end of a reply consumes it and
//! returns a [`Settled`], so a handler settles exactly once, and nothing
//! can be written after it.
//!
//! A reply known only after its effect reserves its [`FixedShape`]'s
//! maximum first ([`JsonReply::reserve_fixed`]) and then makes one
//! [`FixedReply::write`]. The in-place scalar probe's `Unsupported` arm
//! gets the account back only with `inf-doc`'s [`Unapplied`] proof.
//!
//! | End | Rolls back | Writes | Verdict |
//! |---|---|---|---|
//! | `finish` | nothing | nothing more | `Served` |
//! | `refuse` | to the mark | `-ERR reply too large` | `Refused` |
//! | `decline` | to the mark | the command's error line | `Declined` |
//! | `FixedReply::write` | nothing | the one fixed value | `Served` |

use inf_doc::DocValue;
use inf_doc::apply::{ApplyError, Number, Unapplied};
use inf_doc::json::{JsonErrorKind, JsonParseError};
use inf_doc::path::{EvalError, PathError};
use inf_doc::ser::{
    Reply, ReplyTooLarge, SerializeOpts, serialize_into_bounded, serialize_reply_into_bounded,
};
use inf_store::OpError;
use inf_wire::limits::PATCHED_HEADER_SLACK_BYTES;
use inf_wire::{Protocol, ReplyMark, RespWriter};

use crate::exec::op_error;
use crate::limits::FixedShape;

/// Pinned phrasing for a reply over the namespace's `doc-max-reply-bytes`
/// (ADR-0099 D3, beside ADR-0039 D5's `ERR document too large`).
pub(super) const REPLY_TOO_LARGE: &str = "ERR reply too large";

/// A handler's one reply, charged against one account.
pub(super) struct JsonReply<'w, 'b> {
    writer: &'w mut RespWriter<'b>,
    mark: ReplyMark,
    budget_bytes: usize,
    /// The buffer length the reply may reach: the mark plus the budget.
    limit_at: usize,
    /// Bytes a crossing document bulk built before the account refused it:
    /// with what the rollback discards, the refusal's cost in bytes.
    discarded_bytes: usize,
}

/// How a handler's reply ended (ADR-0099 A1). Only the consuming ends of
/// [`JsonReply`], [`FixedReply`] and [`PerElementArray`] make one.
#[must_use]
#[derive(Debug)]
pub(super) struct Settled {
    verdict: ReplyVerdict,
    refusals: u64,
    refused_bytes: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum ReplyVerdict {
    /// The built reply stands (an `MGET` with refused elements included).
    Served,
    /// The account refused: `-ERR reply too large` replaced the reply.
    Refused,
    /// The command's own error line replaced the reply.
    Declined,
}

impl Settled {
    pub(super) fn verdict(&self) -> ReplyVerdict {
        self.verdict
    }

    /// Refusals this reply counted: one for a refused reply, one per
    /// refused `MGET` element.
    pub(super) fn refusals(&self) -> u64 {
        self.refusals
    }

    /// Reply bytes the refusals built and then discarded.
    pub(super) fn refused_bytes(&self) -> u64 {
        self.refused_bytes
    }
}

/// Why a reply builder stopped before its reply was complete.
pub(super) enum ReplyStop<'a> {
    /// A charged write would have crossed the account.
    Refused(ReplyTooLarge),
    /// The command answers its own error line instead.
    Declined(ReplyError<'a>),
}

impl From<ReplyTooLarge> for ReplyStop<'_> {
    fn from(refusal: ReplyTooLarge) -> Self {
        ReplyStop::Refused(refusal)
    }
}

impl<'a> From<ReplyError<'a>> for ReplyStop<'a> {
    fn from(error: ReplyError<'a>) -> Self {
        ReplyStop::Declined(error)
    }
}

/// A builder's outcome (ADR-0099 A1's `ReplyBuild`): `Ok(())` is `Built`,
/// `Err(ReplyStop::Refused)` and `Err(ReplyStop::Declined)` the other two.
/// A builder holds only `&mut JsonReply`; it cannot write an error line,
/// because every error write consumes the reply, so it returns the error.
pub(super) type ReplyBuild<'a> = Result<(), ReplyStop<'a>>;

/// An error line a command answers in place of its reply. Each is bounded
/// by the argv it quotes; error lines are not charged (ADR-0099 A1).
pub(super) enum ReplyError<'a> {
    /// A pinned line (`ERR syntax error`, the missing-key line, …).
    Line(&'static str),
    Path(PathError),
    Eval(EvalError),
    Json(JsonParseError),
    Op(OpError),
    Apply(ApplyError),
    /// `ERR Path '<path>' does not exist`.
    PathMissing(&'a [u8]),
    /// `ERR Path '<path>' does not contain <noun>`.
    NotContaining {
        path: &'a [u8],
        noun: &'static str,
    },
}

impl ReplyError<'_> {
    fn write(self, w: &mut RespWriter<'_>) {
        match self {
            ReplyError::Line(line) => w.error(line),
            ReplyError::Path(error) => w.error(&format!("ERR {error}")),
            ReplyError::Eval(error) => w.error(&format!("ERR {error}")),
            ReplyError::Json(error) => json_error(&error, w),
            ReplyError::Op(error) => op_error(error, w),
            ReplyError::Apply(ApplyError::TooLarge) => w.error("ERR document too large"),
            // The parser's nesting text (ADR-0041 D11), for a composed depth
            // or an array operand past `DEPTH_MAX` (ADR-0169 D3).
            ReplyError::Apply(ApplyError::DepthExceeded) => {
                w.error("ERR document nesting too deep");
            }
            ReplyError::Apply(ApplyError::Eval(inner)) => w.error(&format!("ERR {inner}")),
            ReplyError::Apply(
                other @ (ApplyError::Overflow
                | ApplyError::NotANumber
                | ApplyError::OutOfBounds
                | ApplyError::RootDelete),
            ) => w.error(&format!("ERR {other}")),
            ReplyError::PathMissing(path) => {
                let path = String::from_utf8_lossy(path);
                w.error(&format!("ERR Path '{path}' does not exist"));
            }
            ReplyError::NotContaining { path, noun } => {
                let path = String::from_utf8_lossy(path);
                w.error(&format!("ERR Path '{path}' does not contain {noun}"));
            }
        }
    }
}

/// The two limit rejections carry their ADR-0039 D5 pinned phrasing;
/// everything else reports the typed offset line.
fn json_error(error: &JsonParseError, w: &mut RespWriter<'_>) {
    match error.kind {
        JsonErrorKind::DocumentTooLarge => w.error("ERR document too large"),
        JsonErrorKind::DepthExceeded => w.error("ERR document nesting too deep"),
        JsonErrorKind::UnexpectedCharacter(_)
        | JsonErrorKind::UnexpectedEnd
        | JsonErrorKind::TrailingCharacters
        | JsonErrorKind::InvalidNumber
        | JsonErrorKind::NumberOutOfRange
        | JsonErrorKind::InvalidEscape
        | JsonErrorKind::InvalidUnicodeEscape
        | JsonErrorKind::LoneSurrogate
        | JsonErrorKind::InvalidUtf8
        | JsonErrorKind::ControlCharacter
        | JsonErrorKind::UnterminatedString => w.error(&format!("ERR invalid JSON: {error}")),
    }
}

impl<'w, 'b> JsonReply<'w, 'b> {
    /// Opens the command's account at the writer's current end. Called by
    /// `execute_json` alone, once per command (ADR-0099 A1).
    pub(super) fn open(writer: &'w mut RespWriter<'b>, budget_bytes: usize) -> Self {
        let mark = writer.mark();
        let limit_at = mark.offset_bytes().saturating_add(budget_bytes);
        JsonReply { writer, mark, budget_bytes, limit_at, discarded_bytes: 0 }
    }

    pub(super) fn protocol(&self) -> Protocol {
        self.writer.protocol()
    }

    /// Refuses a frame of `frame_bytes` that would pass the account.
    fn charge(&self, frame_bytes: usize) -> Result<(), ReplyTooLarge> {
        match self.writer.buffered_bytes().checked_add(frame_bytes) {
            Some(end) if end <= self.limit_at => Ok(()),
            Some(_) | None => Err(ReplyTooLarge),
        }
    }

    pub(super) fn array_header(&mut self, count: usize) -> Result<(), ReplyTooLarge> {
        self.charge(line_frame_bytes(decimal_bytes(count as u64)))?;
        self.writer.array_header(count);
        Ok(())
    }

    pub(super) fn int(&mut self, value: i64) -> Result<(), ReplyTooLarge> {
        let sign = usize::from(value < 0);
        self.charge(line_frame_bytes(sign + decimal_bytes(value.unsigned_abs())))?;
        self.writer.int(value);
        Ok(())
    }

    pub(super) fn null(&mut self) -> Result<(), ReplyTooLarge> {
        self.charge(null_frame_bytes(self.writer.protocol()))?;
        self.writer.null();
        Ok(())
    }

    /// `+OK`: the one status line a charged path mutation answers.
    pub(super) fn ok(&mut self) -> Result<(), ReplyTooLarge> {
        self.charge(line_frame_bytes(2))?;
        self.writer.simple("OK");
        Ok(())
    }

    pub(super) fn bulk(&mut self, payload: &[u8]) -> Result<(), ReplyTooLarge> {
        // The two planted canaries stop charging the bulk's bytes or its
        // framing; the reply oracle must see each (ADR-0099 A1 §Oracle).
        let frame_bytes = if cfg!(inf_canary_json_reply_uncharged) {
            0
        } else if cfg!(inf_canary_json_reply_framing_uncharged) {
            payload.len()
        } else {
            bulk_frame_bytes(payload.len())
        };
        self.charge(frame_bytes)?;
        self.writer.bulk(payload);
        Ok(())
    }

    /// A number value: RESP3 native, RESP2 the JSON number text as a bulk.
    pub(super) fn number(&mut self, number: Number) -> Result<(), ReplyTooLarge> {
        match (self.writer.protocol(), number) {
            (Protocol::Resp3, Number::I64(value)) => self.int(value),
            (Protocol::Resp3, Number::F64(value)) => {
                self.charge(line_frame_bytes(display_bytes(value)))?;
                self.writer.double(value);
                Ok(())
            }
            (Protocol::Resp2, Number::I64(_) | Number::F64(_)) => {
                let mut text = [0u8; 32];
                let len = inf_doc::serialize_number_text(number, &mut text);
                self.bulk(&text[..len])
            }
        }
    }

    /// One value serialized as a bulk of JSON text.
    pub(super) fn document(
        &mut self,
        value: DocValue<'_>,
        opts: &SerializeOpts<'_>,
    ) -> Result<(), ReplyTooLarge> {
        self.patched(|out, limit| serialize_into_bounded(value, opts, out, limit))
    }

    /// A reply tree serialized as one bulk of JSON text.
    pub(super) fn reply_tree(
        &mut self,
        reply: &Reply<'_>,
        opts: &SerializeOpts<'_>,
    ) -> Result<(), ReplyTooLarge> {
        self.patched(|out, limit| serialize_reply_into_bounded(reply, opts, out, limit))
    }

    /// A patched bulk under the account. The payload is written ahead of
    /// its final header, so the serializer may run to the account plus the
    /// width the reserved header can shrink by (and one scalar token past
    /// that, ADR-0099 D3); the patched frame is then checked exactly and
    /// rolled back when it crossed.
    fn patched(
        &mut self,
        serialize: impl FnOnce(&mut Vec<u8>, usize) -> Result<(), ReplyTooLarge>,
    ) -> Result<(), ReplyTooLarge> {
        let frame = self.writer.mark();
        let frame_at = frame.offset_bytes();
        let limit = self.limit_at.saturating_add(PATCHED_HEADER_SLACK_BYTES);
        let mut built_bytes = 0;
        let written = self.writer.try_bulk_patched(|out| {
            let result = serialize(out, limit);
            if result.is_err() {
                built_bytes = out.len() - frame_at;
            }
            result
        });
        if let Err(refusal) = written {
            self.discarded_bytes += built_bytes;
            return Err(refusal);
        }
        if self.writer.buffered_bytes() > self.limit_at {
            self.discarded_bytes += self.writer.buffered_bytes() - frame_at;
            self.writer.rollback(&frame);
            return Err(ReplyTooLarge);
        }
        Ok(())
    }

    /// The reply stands as built.
    pub(super) fn finish(self) -> Settled {
        // The two bulk-charging plants disable this self-check with the
        // rule it checks, so the independent reply oracle must go red.
        let planted =
            cfg!(inf_canary_json_reply_uncharged) || cfg!(inf_canary_json_reply_framing_uncharged);
        let within = self.writer.buffered_bytes() <= self.limit_at;
        debug_assert!(planted || within, "charged writes stay in");
        Settled { verdict: ReplyVerdict::Served, refusals: 0, refused_bytes: 0 }
    }

    /// Rolls the whole reply back and answers `ERR reply too large`.
    pub(super) fn refuse(self, _refusal: ReplyTooLarge) -> Settled {
        let written = self.writer.buffered_bytes() - self.mark.offset_bytes();
        self.writer.rollback(&self.mark);
        self.writer.error(REPLY_TOO_LARGE);
        let refused_bytes = u64::try_from(written + self.discarded_bytes).unwrap_or(u64::MAX);
        Settled { verdict: ReplyVerdict::Refused, refusals: 1, refused_bytes }
    }

    /// Rolls the whole reply back and answers the command's error line.
    pub(super) fn decline(self, error: ReplyError<'_>) -> Settled {
        self.writer.rollback(&self.mark);
        error.write(self.writer);
        Settled { verdict: ReplyVerdict::Declined, refusals: 0, refused_bytes: 0 }
    }

    /// Ends a builder's run: `Built` stands, the other two consume.
    pub(super) fn settle(self, build: ReplyBuild<'_>) -> Settled {
        match build {
            Ok(()) => self.finish(),
            Err(ReplyStop::Refused(refusal)) => self.refuse(refusal),
            Err(ReplyStop::Declined(error)) => self.decline(error),
        }
    }

    /// Reserves `shape`'s maximum before the command's effect. The
    /// reservation is the command's first write, so it refuses exactly
    /// when the budget is below M(shape) (ADR-0099 A1, the fixed rows).
    pub(super) fn reserve_fixed(self, shape: FixedShape) -> FixedReservation<'w, 'b> {
        debug_assert_eq!(self.writer.buffered_bytes(), self.mark.offset_bytes(), "first write");
        match self.charge(shape.reply_bytes_max()) {
            Ok(()) => FixedReservation::Reserved(FixedReply { reply: self, shape }),
            Err(refusal) => FixedReservation::Refused(self.refuse(refusal)),
        }
    }

    /// `JSON.MGET` only: the outer header is not charged, and each element
    /// is charged against its own account of the whole budget. The reply's
    /// aggregate is ADR-0099 A1's dated deviation, not bounded here.
    pub(super) fn per_element_array(self, count: usize) -> PerElementArray<'w, 'b> {
        self.writer.array_header(count);
        PerElementArray { reply: self, refusals: 0, refused_bytes: 0 }
    }
}

/// [`JsonReply::reserve_fixed`]'s outcome.
pub(super) enum FixedReservation<'w, 'b> {
    Reserved(FixedReply<'w, 'b>),
    Refused(Settled),
}

/// A reservation of one fixed-shape reply, taken before the effect.
pub(super) struct FixedReply<'w, 'b> {
    reply: JsonReply<'w, 'b>,
    shape: FixedShape,
}

/// The replies a fixed shape can write, each owned by one shape.
#[derive(Copy, Clone, Debug)]
pub(super) enum FixedValue {
    /// `+OK` (`Status`).
    Ok,
    /// The NX/XX skip (`Status`).
    Null,
    /// A root delete: `:1` when the key was deleted (`Count`).
    Deleted(bool),
    /// The toggled boolean (`Toggle`).
    Toggled { legacy: bool, value: bool },
    /// `$` mode, no match: `*0` (`Toggle`).
    ToggleMissing,
    /// `$` mode, a non-boolean match: `*1` and a null (`Toggle`).
    ToggleSkipped,
    /// The patched number (`Number`).
    Number { legacy: bool, number: Number },
    /// `$` mode, no match: RESP3 `*0`, RESP2 the bulk `[]` (`Number`).
    NumberMissing,
    /// `$` mode, a non-number match: RESP3 `*1` and a null, RESP2 the bulk
    /// `[null]` (`Number`).
    NumberSkipped,
}

impl FixedValue {
    fn shape(self) -> FixedShape {
        match self {
            FixedValue::Ok | FixedValue::Null => FixedShape::Status,
            FixedValue::Deleted(_) => FixedShape::Count,
            FixedValue::Toggled { .. } | FixedValue::ToggleMissing | FixedValue::ToggleSkipped => {
                FixedShape::Toggle
            }
            FixedValue::Number { .. } | FixedValue::NumberMissing | FixedValue::NumberSkipped => {
                FixedShape::Number
            }
        }
    }

    fn write(self, w: &mut RespWriter<'_>) {
        let resp3 = w.protocol() == Protocol::Resp3;
        match self {
            FixedValue::Ok => w.simple("OK"),
            FixedValue::Null => w.null(),
            FixedValue::Deleted(deleted) => w.int(i64::from(deleted)),
            FixedValue::Toggled { legacy: true, value } => {
                w.bulk(if value { b"true" } else { b"false" });
            }
            FixedValue::Toggled { legacy: false, value } => {
                w.array_header(1);
                w.int(i64::from(value));
            }
            FixedValue::ToggleMissing => w.array_header(0),
            FixedValue::ToggleSkipped => {
                w.array_header(1);
                w.null();
            }
            FixedValue::NumberSkipped if resp3 => {
                w.array_header(1);
                w.null();
            }
            FixedValue::NumberSkipped => w.bulk(b"[null]"),
            FixedValue::NumberMissing if resp3 => w.array_header(0),
            FixedValue::NumberMissing => w.bulk(b"[]"),
            FixedValue::Number { legacy, number } => write_fixed_number(legacy, number, w),
        }
    }
}

/// The in-place number reply: RESP3 `*1` and the native value; RESP2 the
/// JSON number text, bare under a legacy path and `[n]` under `$`.
fn write_fixed_number(legacy: bool, number: Number, w: &mut RespWriter<'_>) {
    let mut text = [0u8; 32];
    let len = inf_doc::serialize_number_text(number, &mut text);
    match (w.protocol(), number) {
        (Protocol::Resp3, Number::I64(value)) => {
            w.array_header(1);
            w.int(value);
        }
        (Protocol::Resp3, Number::F64(value)) => {
            w.array_header(1);
            w.double(value);
        }
        (Protocol::Resp2, Number::I64(_) | Number::F64(_)) if legacy => w.bulk(&text[..len]),
        (Protocol::Resp2, Number::I64(_) | Number::F64(_)) => {
            let mut payload = [0u8; 34];
            payload[0] = b'[';
            payload[1..=len].copy_from_slice(&text[..len]);
            payload[len + 1] = b']';
            w.bulk(&payload[..len + 2]);
        }
    }
}

impl FixedReply<'_, '_> {
    /// The one write the reservation admits, after the effect.
    pub(super) fn write(self, value: FixedValue) -> Settled {
        debug_assert_eq!(value.shape(), self.shape, "a value of the reserved shape");
        let frame_at = self.reply.writer.buffered_bytes();
        value.write(self.reply.writer);
        let written = self.reply.writer.buffered_bytes() - frame_at;
        debug_assert!(
            written <= self.shape.reply_bytes_max(),
            "{written} B over M({:?})",
            self.shape
        );
        self.reply.finish()
    }

    pub(super) fn decline(self, error: ReplyError<'_>) -> Settled {
        self.reply.decline(error)
    }
}

impl<'w, 'b> FixedReply<'w, 'b> {
    /// The probe changed no byte, so the whole account comes back for the
    /// general path (the reservation wrote nothing).
    pub(super) fn reopen(self, _unapplied: Unapplied) -> JsonReply<'w, 'b> {
        self.reply
    }
}

/// `JSON.MGET`'s reply: one account of the whole budget per element.
pub(super) struct PerElementArray<'w, 'b> {
    reply: JsonReply<'w, 'b>,
    refusals: u64,
    refused_bytes: u64,
}

impl PerElementArray<'_, '_> {
    /// One element's JSON text; a crossing element answers the pinned
    /// error as that element (the `EXEC` error-in-array precedent).
    pub(super) fn document(&mut self, reply: &Reply<'_>) {
        let element = self.reply.writer.mark();
        self.reply.limit_at = element.offset_bytes().saturating_add(self.reply.budget_bytes);
        self.reply.discarded_bytes = 0;
        if let Err(ReplyTooLarge) = self.reply.reply_tree(reply, &SerializeOpts::default()) {
            self.refuse_element();
        }
    }

    /// A missing or non-document key's null element.
    pub(super) fn null(&mut self) {
        let element = self.reply.writer.mark();
        self.reply.limit_at = element.offset_bytes().saturating_add(self.reply.budget_bytes);
        self.reply.discarded_bytes = 0;
        if let Err(ReplyTooLarge) = self.reply.null() {
            self.refuse_element();
        }
    }

    fn refuse_element(&mut self) {
        self.refusals += 1;
        let discarded = u64::try_from(self.reply.discarded_bytes).unwrap_or(u64::MAX);
        self.refused_bytes = self.refused_bytes.saturating_add(discarded);
        self.reply.writer.error(REPLY_TOO_LARGE);
    }

    pub(super) fn finish(self) -> Settled {
        let (refusals, refused_bytes) = (self.refusals, self.refused_bytes);
        Settled { verdict: ReplyVerdict::Served, refusals, refused_bytes }
    }
}

/// Bytes of the decimal text of `value`.
fn decimal_bytes(mut value: u64) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

/// A one-line frame: a type byte, the text, CRLF.
fn line_frame_bytes(text_bytes: usize) -> usize {
    1 + text_bytes + 2
}

/// `$<len>\r\n<payload>\r\n`.
fn bulk_frame_bytes(payload_bytes: usize) -> usize {
    line_frame_bytes(decimal_bytes(payload_bytes as u64)) + payload_bytes + 2
}

fn null_frame_bytes(protocol: Protocol) -> usize {
    match protocol {
        Protocol::Resp2 => 5,
        Protocol::Resp3 => 3,
    }
}

/// Bytes of `value`'s `Display`, the text `RespWriter::double` writes.
/// Counted, not buffered: at most 336 B. The counting sink cannot fail;
/// were formatting ever to report an error, the count is the widest
/// double frame, which over-charges and never under-charges.
fn display_bytes(value: f64) -> usize {
    struct Count(usize);
    impl core::fmt::Write for Count {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            self.0 += text.len();
            Ok(())
        }
    }
    let mut count = Count(0);
    match core::fmt::write(&mut count, format_args!("{value}")) {
        Ok(()) => count.0,
        Err(core::fmt::Error) => inf_wire::limits::DOUBLE_REPLY_BYTES_MAX,
    }
}
