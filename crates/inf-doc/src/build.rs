//! `TapeBuilder`: canonical tape emission with an explicit scope stack
//! (ADR-0036 D3). Canonical-by-construction — byte encodings come from the
//! shared [`emit`] primitives (the S05 parser drives the same ones), so
//! `validate(build(x))` never fails and `build(decode(t)) == t` for every
//! valid tape `t` (the fuzz oracle).
//!
//! Two guard classes, deliberately different (INFINITY_STYLE §Panics):
//! data-dependent limits (size, depth) return typed errors — they are the
//! seam M3-S07's per-namespace caps thread through; *misuse* (value where
//! a key is due, `finish` mid-container) asserts — the driver is our own
//! model/mutation walker driving its own validated state, so a violation
//! is a bug.

use crate::emit;
use crate::error::DocError;
use crate::header;
use crate::limits::{DEPTH_MAX, DOC_BYTES_MAX};
use crate::tape::{FIXINT_MAX, FIXINT_MIN, TAG_ARR, TAG_OBJ};

#[derive(Debug)]
pub(crate) struct BFrame {
    /// Offset of the 3-byte length placeholder to backpatch at `end()`.
    len_at: usize,
    is_obj: bool,
    expects_key: bool,
}

/// Streaming canonical-tape builder. Output layout: 8-byte header
/// placeholder + body; `finish()` patches the header.
#[derive(Debug)]
pub struct TapeBuilder {
    out: Vec<u8>,
    stack: Vec<BFrame>,
    root_done: bool,
    /// Body byte cap. Enforced incrementally *before* copying payloads —
    /// this is the M3-S07 idoc-byte bound: a text-cap-passing document
    /// whose tape encoding explodes is rejected mid-build, never after.
    max_body: usize,
}

impl Default for TapeBuilder {
    fn default() -> TapeBuilder {
        TapeBuilder::new()
    }
}

impl TapeBuilder {
    pub fn new() -> TapeBuilder {
        TapeBuilder::with_max_body(DOC_BYTES_MAX)
    }

    /// Cap the body below the format ceiling (per-namespace config, S07).
    pub fn with_max_body(max_body: usize) -> TapeBuilder {
        debug_assert!(max_body <= DOC_BYTES_MAX);
        let mut out = Vec::with_capacity(64);
        out.resize(header::HEADER_LEN, 0);
        TapeBuilder { out, stack: Vec::new(), root_done: false, max_body }
    }

    /// Rehydrate a builder from caller-owned buffers. Arena checkpoint
    /// freeze uses this to keep both output and scope frames cell-local
    /// and allocation-free after warm-up (ADR-0043 D7).
    pub(crate) fn with_recycled(
        mut out: Vec<u8>,
        mut stack: Vec<BFrame>,
        max_body: usize,
    ) -> TapeBuilder {
        debug_assert!(max_body <= DOC_BYTES_MAX);
        out.clear();
        out.resize(header::HEADER_LEN, 0);
        stack.clear();
        TapeBuilder { out, stack, root_done: false, max_body }
    }

    /// Claim a value slot: object parity bookkeeping + single-root rule.
    #[inline]
    fn claim_value_slot(&mut self) {
        match self.stack.last_mut() {
            Some(top) if top.is_obj => {
                assert!(!top.expects_key, "builder misuse: value emitted where a key is due");
                top.expects_key = true;
            }
            Some(_) => {}
            None => {
                assert!(!self.root_done, "builder misuse: second root value");
                self.root_done = true;
            }
        }
    }

    /// Reject before copy: `extra` more body bytes must fit the cap.
    #[inline]
    fn ensure_fits(&self, extra: usize) -> Result<(), DocError> {
        let body = self.out.len() - header::HEADER_LEN;
        if body + extra > self.max_body {
            return Err(DocError::TooLarge { bytes: body + extra });
        }
        Ok(())
    }

    #[inline]
    pub fn null(&mut self) -> Result<(), DocError> {
        self.claim_value_slot();
        self.ensure_fits(1)?;
        emit::null(&mut self.out);
        Ok(())
    }

    #[inline]
    pub fn bool(&mut self, v: bool) -> Result<(), DocError> {
        self.claim_value_slot();
        self.ensure_fits(1)?;
        emit::bool(&mut self.out, v);
        Ok(())
    }

    #[inline]
    pub fn i64(&mut self, v: i64) -> Result<(), DocError> {
        self.claim_value_slot();
        // Fixints cost 1; everything else is bounded by the varint worst
        // case — exact totals are re-checked by callers' own caps.
        let worst = if (FIXINT_MIN..=FIXINT_MAX).contains(&v) { 1 } else { emit::I64_MAX_LEN };
        self.ensure_fits(worst)?;
        emit::i64(&mut self.out, v);
        Ok(())
    }

    #[inline]
    pub fn f64(&mut self, v: f64) -> Result<(), DocError> {
        self.claim_value_slot();
        if !v.is_finite() {
            // Un-claim nothing: the command layer surfaces the oracle's
            // error; an aborted build is discarded whole (§3.4 R4).
            return Err(DocError::NonFiniteNumber);
        }
        self.ensure_fits(emit::F64_LEN)?;
        emit::f64(&mut self.out, v);
        Ok(())
    }

    #[inline]
    pub fn str_value(&mut self, s: &str) -> Result<(), DocError> {
        self.claim_value_slot();
        self.emit_str(s)
    }

    /// Object key. `&str` makes UTF-8 a type-level fact (D6: validated at
    /// the boundary, trusted after).
    #[inline]
    pub fn key(&mut self, k: &str) -> Result<(), DocError> {
        let top = self.stack.last_mut().expect("builder misuse: key outside an object");
        assert!(top.is_obj, "builder misuse: key inside an array");
        assert!(top.expects_key, "builder misuse: key emitted where a value is due");
        top.expects_key = false;
        self.emit_str(k)
    }

    #[inline]
    fn emit_str(&mut self, s: &str) -> Result<(), DocError> {
        self.ensure_fits(emit::str_header_len(s.len()) + s.len())?;
        emit::str(&mut self.out, s.as_bytes());
        Ok(())
    }

    pub fn begin_obj(&mut self) -> Result<(), DocError> {
        self.begin_container(TAG_OBJ)
    }

    pub fn begin_arr(&mut self) -> Result<(), DocError> {
        self.begin_container(TAG_ARR)
    }

    #[inline]
    fn begin_container(&mut self, tag: u8) -> Result<(), DocError> {
        // The container claims its parent's value slot at push time; its
        // own scope opens after (matches the validator's rule).
        self.claim_value_slot();
        if self.stack.len() == DEPTH_MAX {
            return Err(DocError::DepthExceeded);
        }
        self.ensure_fits(emit::CONTAINER_OPEN_LEN)?;
        let len_at = emit::begin(&mut self.out, tag);
        let is_obj = tag == TAG_OBJ;
        self.stack.push(BFrame { len_at, is_obj, expects_key: is_obj });
        Ok(())
    }

    /// Close the innermost container and backpatch its u24 length. Fixed
    /// width means children never move (the D3 backpatch argument).
    #[inline]
    pub fn end(&mut self) {
        let frame = self.stack.pop().expect("builder misuse: end() with no open container");
        assert!(
            !frame.is_obj || frame.expects_key,
            "builder misuse: object closed between key and value"
        );
        emit::end(&mut self.out, frame.len_at);
    }

    /// Seal the document: header patched over the placeholder. Consumes
    /// the builder — a finished tape is immutable by type.
    pub fn finish(mut self) -> Result<Vec<u8>, DocError> {
        assert!(self.stack.is_empty(), "builder misuse: finish() with open containers");
        assert!(self.root_done, "builder misuse: finish() without a root value");
        let body_len = (self.out.len() - header::HEADER_LEN) as u32;
        header::patch(&mut self.out, 0, body_len);
        Ok(self.out)
    }

    /// Finish while returning the scope buffer to the caller for reuse.
    pub(crate) fn finish_recycled(mut self) -> (Vec<u8>, Vec<BFrame>) {
        assert!(self.stack.is_empty(), "builder misuse: finish() with open containers");
        assert!(self.root_done, "builder misuse: finish() without a root value");
        let body_len = (self.out.len() - header::HEADER_LEN) as u32;
        header::patch(&mut self.out, 0, body_len);
        (self.out, self.stack)
    }

    /// Recover buffers from an aborted typed build.
    pub(crate) fn into_recycled(self) -> (Vec<u8>, Vec<BFrame>) {
        (self.out, self.stack)
    }

    /// Seal as a bare canonical fragment (no header) — the `DocDelta`
    /// operand encoding (ADR-0036 D8; milestone §3.4 R6). Versioning is
    /// the containing record's job.
    pub fn finish_fragment(mut self) -> Result<Vec<u8>, DocError> {
        assert!(self.stack.is_empty(), "builder misuse: finish_fragment() with open containers");
        assert!(self.root_done, "builder misuse: finish_fragment() without a root value");
        self.out.drain(..header::HEADER_LEN);
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::TapeDoc;

    #[test]
    fn builds_validating_documents() {
        let mut b = TapeBuilder::new();
        b.begin_obj().expect("obj");
        b.key("a").expect("key");
        b.i64(1).expect("value");
        b.key("b").expect("key");
        b.begin_arr().expect("arr");
        b.str_value("x").expect("elem");
        b.end();
        b.end();
        let bytes = b.finish().expect("finishes");
        TapeDoc::from_bytes(&bytes).expect("builder output always validates");
    }

    #[test]
    fn size_cap_rejects_before_copy() {
        let mut b = TapeBuilder::with_max_body(8);
        b.begin_arr().expect("arr fits");
        let err = b.str_value("0123456789");
        assert_eq!(err, Err(DocError::TooLarge { bytes: 15 }));
    }

    #[test]
    fn depth_cap_is_typed() {
        let mut b = TapeBuilder::new();
        for _ in 0..DEPTH_MAX {
            b.begin_arr().expect("within depth");
        }
        assert_eq!(b.begin_arr(), Err(DocError::DepthExceeded));
    }

    #[test]
    fn non_finite_is_typed() {
        let mut b = TapeBuilder::new();
        assert_eq!(b.f64(f64::NAN), Err(DocError::NonFiniteNumber));
        assert_eq!(TapeBuilder::new().f64(f64::INFINITY), Err(DocError::NonFiniteNumber));
    }

    #[test]
    fn fragment_has_no_header() {
        let mut b = TapeBuilder::new();
        b.i64(5).expect("value");
        assert_eq!(b.finish_fragment().expect("fragment"), vec![5u8]);
    }
}
