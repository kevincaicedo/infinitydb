//! Repeated-key string interning (M3-S04, **ADR-0038**): a storage-local
//! transform pair over canonical plain tapes. `intern` moves winning
//! repeated object keys into a per-document table (header flag bit0,
//! `0xA9 id:u16` refs); `unintern` restores the canonical plain form.
//! Durable emissions never carry interned bytes (ADR-0038 D3) — the
//! transforms live at the record-store seam only.
//!
//! Canonical rule (ADR-0038 D2): a key with plain per-occurrence cost `c`
//! appearing `n` times enters the table iff `n·c > (2 + len) + 3·n`
//! (strictly smaller), the table is ordered by first occurrence, tabled
//! keys MUST ref and non-tabled keys MUST stay plain — and the document
//! interns at all only when the whole encoding shrinks strictly (the
//! 2-byte table count is global overhead).

use crate::error::DocError;
use crate::header::{self, FLAG_INTERNED, HEADER_LEN};
use crate::tape::{
    self, FIXSTR_BASE, FIXSTR_MAX_LEN, STR24_MIN_LEN, TAG_ARR, TAG_KEYREF, TAG_OBJ, TAG_STR8,
    TAG_STR24, TapeDoc, read_u24,
};

/// Split a flagged document's post-header tail into (dict, body),
/// validating the table's structure (count ≥ 1, entry bounds, UTF-8).
/// Body validation is the caller's (`TapeDoc::from_bytes`).
pub(crate) fn split_dict(tail: &[u8]) -> Result<(&[u8], &[u8]), DocError> {
    if tail.len() < 2 {
        return Err(DocError::Truncated);
    }
    let count = u16::from_le_bytes([tail[0], tail[1]]) as usize;
    if count == 0 {
        return Err(DocError::NonCanonical("empty intern table"));
    }
    let mut off = 2usize;
    for _ in 0..count {
        if off + 2 > tail.len() {
            return Err(DocError::Truncated);
        }
        let len = u16::from_le_bytes([tail[off], tail[off + 1]]) as usize;
        let end = off + 2 + len;
        if end > tail.len() {
            return Err(DocError::Truncated);
        }
        if str::from_utf8(&tail[off + 2..end]).is_err() {
            return Err(DocError::BadUtf8);
        }
        off = end;
    }
    Ok(tail.split_at(off))
}

/// Resolve id → key bytes against a raw table region. O(id) walk — dicts
/// are small (a document's distinct repeated keys); the hot lookup path
/// resolves the *needle* once, then compares ids (ADR-0038 D5).
pub(crate) fn dict_region_get(dict: &[u8], id: usize) -> Option<&[u8]> {
    let count = u16::from_le_bytes([*dict.first()?, *dict.get(1)?]) as usize;
    if id >= count {
        return None;
    }
    let mut off = 2;
    for _ in 0..id {
        let len = u16::from_le_bytes([dict[off], dict[off + 1]]) as usize;
        off += 2 + len;
    }
    let len = u16::from_le_bytes([dict[off], dict[off + 1]]) as usize;
    Some(&dict[off + 2..off + 2 + len])
}

/// Find a key's id in a raw table region. O(table) — paid once per lookup
/// on interned tapes, never on plain tapes.
pub(crate) fn dict_region_find(dict: &[u8], key: &[u8]) -> Option<u16> {
    let count = u16::from_le_bytes([*dict.first()?, *dict.get(1)?]) as usize;
    let mut off = 2;
    for id in 0..count {
        let len = u16::from_le_bytes([dict[off], dict[off + 1]]) as usize;
        if &dict[off + 2..off + 2 + len] == key {
            return Some(id as u16);
        }
        off += 2 + len;
    }
    None
}

/// Plain encoded cost of a key of `len` bytes (tag/length header + bytes).
fn plain_cost(len: usize) -> usize {
    if len <= FIXSTR_MAX_LEN {
        1 + len
    } else if len < STR24_MIN_LEN {
        2 + len
    } else {
        4 + len
    }
}

/// Read a plain-form object key at `off`: (key bytes, next offset).
fn read_plain_key(body: &[u8], off: usize) -> (&[u8], usize) {
    let tag = body[off];
    let (data_at, len) = if (FIXSTR_BASE..=0x9F).contains(&tag) {
        (off + 1, (tag - FIXSTR_BASE) as usize)
    } else if tag == TAG_STR8 {
        (off + 2, body[off + 1] as usize)
    } else {
        debug_assert_eq!(tag, TAG_STR24, "validated key positions hold strings");
        (off + 4, read_u24(body, off + 1))
    };
    (&body[data_at..data_at + len], data_at + len)
}

/// Emit a string in its canonical width (mirrors the builder's rule).
fn emit_plain_str(out: &mut Vec<u8>, s: &[u8]) {
    let len = s.len();
    if len <= FIXSTR_MAX_LEN {
        out.push(FIXSTR_BASE + len as u8);
    } else if len < STR24_MIN_LEN {
        out.push(TAG_STR8);
        out.push(len as u8);
    } else {
        out.push(TAG_STR24);
        let bytes = (len as u32).to_le_bytes();
        out.extend_from_slice(&bytes[..3]);
    }
    out.extend_from_slice(s);
}

struct Scope {
    /// Input offset where this container's region ends.
    end: usize,
    is_obj: bool,
    expects_key: bool,
}

/// Walk a validated body, calling `f` for every object key (plain form —
/// used on plain tapes by the counting pass). Iterative, stack-bounded by
/// the already-enforced depth cap.
fn for_each_key<'a>(body: &'a [u8], mut f: impl FnMut(&'a [u8])) {
    let mut stack: Vec<Scope> = Vec::new();
    let mut off = 0usize;
    loop {
        while let Some(top) = stack.last() {
            if off == top.end {
                stack.pop();
            } else {
                break;
            }
        }
        if off == body.len() {
            debug_assert!(stack.is_empty(), "validated scopes close within the body");
            return;
        }
        if stack.last().is_some_and(|s| s.is_obj && s.expects_key) {
            let (key, next) = read_plain_key(body, off);
            f(key);
            stack.last_mut().expect("key implies open object").expects_key = false;
            off = next;
            continue;
        }
        if let Some(top) = stack.last_mut()
            && top.is_obj
        {
            top.expects_key = true;
        }
        match body[off] {
            tag @ (TAG_OBJ | TAG_ARR) => {
                let len = read_u24(body, off + 1);
                let is_obj = tag == TAG_OBJ;
                stack.push(Scope { end: off + 4 + len, is_obj, expects_key: is_obj });
                off += 4;
            }
            _ => off = tape::skip_value(body, off),
        }
    }
}

/// An object key as found in the input body.
enum KeyIn<'a> {
    /// Plain string form; `encoding` is the full original encoding
    /// (tag/length header + bytes) for verbatim copies.
    Plain { bytes: &'a [u8], encoding: &'a [u8] },
    /// An interned ref (only on interned inputs).
    Ref { id: u16 },
}

/// Rewrite a validated body into `out`, re-deriving every container's u24
/// length (key encodings change width) and delegating key emission to
/// `emit_key`. Values copy verbatim.
fn rewrite_body(body: &[u8], out: &mut Vec<u8>, mut emit_key: impl FnMut(&mut Vec<u8>, KeyIn<'_>)) {
    #[derive(Copy, Clone)]
    struct OutScope {
        end_in: usize,
        /// Offset of the 3-byte length placeholder in `out`.
        len_at: usize,
        is_obj: bool,
        expects_key: bool,
    }
    const EMPTY: OutScope = OutScope { end_in: 0, len_at: 0, is_obj: false, expects_key: false };
    let mut stack = [EMPTY; crate::limits::DEPTH_MAX + 1];
    let mut depth = 0usize;
    let mut off = 0usize;
    loop {
        while depth > 0 {
            let top = &stack[depth - 1];
            if off != top.end_in {
                break;
            }
            let body_len = out.len() - (top.len_at + 3);
            let bytes = (body_len as u32).to_le_bytes();
            out[top.len_at..top.len_at + 3].copy_from_slice(&bytes[..3]);
            depth -= 1;
        }
        if off == body.len() {
            debug_assert_eq!(depth, 0, "validated scopes close within the body");
            return;
        }
        if depth > 0 && stack[depth - 1].is_obj && stack[depth - 1].expects_key {
            if body[off] == TAG_KEYREF {
                let id = u16::from_le_bytes([body[off + 1], body[off + 2]]);
                emit_key(out, KeyIn::Ref { id });
                off += 3;
            } else {
                let (bytes, next) = read_plain_key(body, off);
                emit_key(out, KeyIn::Plain { bytes, encoding: &body[off..next] });
                off = next;
            }
            stack[depth - 1].expects_key = false;
            continue;
        }
        if depth > 0 && stack[depth - 1].is_obj {
            stack[depth - 1].expects_key = true;
        }
        match body[off] {
            tag @ (TAG_OBJ | TAG_ARR) => {
                let len = read_u24(body, off + 1);
                out.push(tag);
                let len_at = out.len();
                out.extend_from_slice(&[0, 0, 0]);
                let is_obj = tag == TAG_OBJ;
                stack[depth] =
                    OutScope { end_in: off + 4 + len, len_at, is_obj, expects_key: is_obj };
                depth += 1;
                off += 4;
            }
            _ => {
                let next = tape::skip_value(body, off);
                out.extend_from_slice(&body[off..next]);
                off = next;
            }
        }
    }
}

/// Exact canonical plain length of a validated interned tape, without
/// allocating or materializing it. Durable admission consumes this value.
pub fn uninterned_len(bytes: &[u8]) -> usize {
    debug_assert!(TapeDoc::from_bytes(bytes).is_ok(), "length takes validated tapes");
    debug_assert!(bytes[3] & FLAG_INTERNED != 0, "length takes interned tapes");
    let (dict, body) = split_dict(&bytes[HEADER_LEN..]).expect("validated dict splits");
    #[derive(Copy, Clone)]
    struct LenScope {
        end: usize,
        is_obj: bool,
        expects_key: bool,
    }
    const EMPTY: LenScope = LenScope { end: 0, is_obj: false, expects_key: false };
    let mut stack = [EMPTY; crate::limits::DEPTH_MAX + 1];
    let mut depth = 0usize;
    let mut off = 0usize;
    let mut plain_body = 0usize;
    loop {
        while depth > 0 && off == stack[depth - 1].end {
            depth -= 1;
        }
        if off == body.len() {
            debug_assert_eq!(depth, 0, "validated scopes close within the body");
            return HEADER_LEN + plain_body;
        }
        if depth > 0 && stack[depth - 1].is_obj && stack[depth - 1].expects_key {
            if body[off] == TAG_KEYREF {
                let id = u16::from_le_bytes([body[off + 1], body[off + 2]]);
                let key = dict_region_get(dict, id as usize).expect("validated keyref resolves");
                plain_body += plain_cost(key.len());
                off += 3;
            } else {
                let (_, next) = read_plain_key(body, off);
                plain_body += next - off;
                off = next;
            }
            stack[depth - 1].expects_key = false;
            continue;
        }
        if depth > 0 && stack[depth - 1].is_obj {
            stack[depth - 1].expects_key = true;
        }
        match body[off] {
            tag @ (TAG_OBJ | TAG_ARR) => {
                let len = read_u24(body, off + 1);
                let is_obj = tag == TAG_OBJ;
                plain_body += 4;
                stack[depth] = LenScope { end: off + 4 + len, is_obj, expects_key: is_obj };
                depth += 1;
                off += 4;
            }
            _ => {
                let next = tape::skip_value(body, off);
                plain_body += next - off;
                off = next;
            }
        }
    }
}

/// Intern `plain` (a validated canonical **plain** tape). `None` when no
/// key wins the D2 rule or the interned encoding would not strictly
/// shrink — interning can never cost bytes at rest.
pub fn intern(plain: &[u8]) -> Option<Vec<u8>> {
    debug_assert!(
        TapeDoc::from_bytes(plain).is_ok_and(|d| d.dict().is_empty()),
        "intern takes validated plain tapes"
    );
    let body = &plain[HEADER_LEN..];
    let mut keys: Vec<&[u8]> = Vec::new();
    for_each_key(body, |k| keys.push(k));
    if keys.is_empty() {
        return None;
    }
    // Occurrence counts per distinct key, then the D2 strict-win rule.
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    let mut winners: Vec<&[u8]> = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let key = sorted[i];
        let mut j = i + 1;
        while j < sorted.len() && sorted[j] == key {
            j += 1;
        }
        let n = j - i;
        if key.len() <= u16::MAX as usize && n * plain_cost(key.len()) > 2 + key.len() + 3 * n {
            winners.push(key);
        }
        i = j;
    }
    if winners.is_empty() {
        return None;
    }
    // Ids in first-occurrence document order; membership via sorted probe.
    let mut table: Vec<&[u8]> = Vec::new();
    let mut id_of: Vec<(&[u8], u16)> = Vec::new(); // sorted by key bytes
    for key in &keys {
        if winners.binary_search(key).is_ok() {
            let probe = id_of.binary_search_by(|(k, _)| k.cmp(key));
            if let Err(pos) = probe {
                id_of.insert(pos, (key, table.len() as u16));
                table.push(key);
            }
        }
    }
    let mut tail = Vec::with_capacity(plain.len());
    tail.extend_from_slice(&(table.len() as u16).to_le_bytes());
    for key in &table {
        tail.extend_from_slice(&(key.len() as u16).to_le_bytes());
        tail.extend_from_slice(key);
    }
    rewrite_body(body, &mut tail, |out, kin| match kin {
        KeyIn::Plain { bytes, encoding } => match id_of.binary_search_by(|(k, _)| k.cmp(&bytes)) {
            Ok(pos) => {
                out.push(TAG_KEYREF);
                out.extend_from_slice(&id_of[pos].1.to_le_bytes());
            }
            Err(_) => out.extend_from_slice(encoding),
        },
        KeyIn::Ref { .. } => unreachable!("plain input carries no refs"),
    });
    if HEADER_LEN + tail.len() >= plain.len() {
        return None; // the 2-byte count ate the margin — stay plain
    }
    let mut out = Vec::with_capacity(HEADER_LEN + tail.len());
    header::encode(FLAG_INTERNED, tail.len() as u32, &mut out);
    out.extend_from_slice(&tail);
    debug_assert!(TapeDoc::from_bytes(&out).is_ok(), "interned output validates");
    Some(out)
}

/// Restore the canonical plain form of a validated **interned** tape —
/// what every durable emission and comparator consumes (ADR-0038 D3).
pub fn unintern(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    unintern_into(bytes, &mut out);
    out
}

/// Restore canonical plain form into a caller-recycled buffer.
pub fn unintern_into(bytes: &[u8], out: &mut Vec<u8>) {
    debug_assert!(TapeDoc::from_bytes(bytes).is_ok(), "unintern takes validated tapes");
    debug_assert!(bytes[3] & FLAG_INTERNED != 0, "unintern takes interned tapes");
    let (dict, body) = split_dict(&bytes[HEADER_LEN..]).expect("validated dict splits");
    let plain_len = uninterned_len(bytes);
    out.clear();
    out.reserve(plain_len);
    out.resize(HEADER_LEN, 0);
    rewrite_body(body, out, |out, kin| match kin {
        KeyIn::Ref { id } => emit_plain_str(
            out,
            dict_region_get(dict, id as usize).expect("validated keyref resolves"),
        ),
        KeyIn::Plain { encoding, .. } => out.extend_from_slice(encoding),
    });
    let body_len = out.len() - HEADER_LEN;
    header::patch(out, 0, body_len as u32);
    debug_assert!(
        TapeDoc::from_bytes(out).is_ok_and(|d| d.dict().is_empty()),
        "unintern output is a valid plain tape"
    );
    debug_assert_eq!(out.len(), plain_len);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{self, Value};

    fn wide(count: usize) -> Vec<u8> {
        let element = |i: i64| {
            Value::Obj(vec![
                ("identifier".into(), Value::I64(i)),
                ("display_name".into(), Value::Str(format!("row{i}"))),
            ])
        };
        model::encode(&Value::Arr((0..count as i64).map(element).collect())).expect("encodes")
    }

    #[test]
    fn intern_round_trips_and_shrinks_wide_shapes() {
        let plain = wide(64);
        let interned = intern(&plain).expect("repeated keys win");
        assert!(interned.len() < plain.len());
        assert_eq!(uninterned_len(&interned), plain.len());
        assert_eq!(unintern(&interned), plain, "unintern ∘ intern == id");
        let mut scratch = Vec::new();
        unintern_into(&interned, &mut scratch);
        assert_eq!(scratch, plain);
        let capacity = scratch.capacity();
        unintern_into(&interned, &mut scratch);
        assert_eq!(scratch, plain);
        assert_eq!(scratch.capacity(), capacity, "checkpoint scratch is reused");
        let doc = TapeDoc::from_bytes(&interned).expect("interned form validates");
        assert_eq!(
            model::from_tape(&doc),
            model::from_tape(&TapeDoc::from_bytes(&plain).expect("plain validates")),
            "interned decode equals plain decode"
        );
    }

    #[test]
    fn single_use_keys_never_intern() {
        let plain = model::encode(&Value::Obj(vec![
            ("alpha_key_one".into(), Value::I64(1)),
            ("alpha_key_two".into(), Value::I64(2)),
        ]))
        .expect("encodes");
        assert_eq!(intern(&plain), None, "n = 1 cannot win the strict rule");
    }

    #[test]
    fn marginal_wins_that_lose_to_the_count_field_stay_plain() {
        // Two occurrences of a 6-byte key: per-key win is
        // 2·7 = 14 > (2+6) + 3·2 = 14 — false (strict), so no winner.
        let plain = model::encode(&Value::Arr(vec![
            Value::Obj(vec![("kkkkkk".into(), Value::I64(1))]),
            Value::Obj(vec![("kkkkkk".into(), Value::I64(2))]),
        ]))
        .expect("encodes");
        assert_eq!(intern(&plain), None);
    }

    #[test]
    fn interned_lookup_and_iteration_resolve_keys() {
        let plain = wide(8);
        let interned = intern(&plain).expect("interns");
        let doc = TapeDoc::from_bytes(&interned).expect("validates");
        let crate::tape::ValueRef::Arr(arr) = doc.root() else { panic!("array root") };
        let crate::tape::ValueRef::Obj(first) = arr.index(0).expect("element") else {
            panic!("object element")
        };
        assert!(matches!(first.get(b"identifier"), Some(crate::tape::ValueRef::I64(0))));
        let keys: Vec<_> = first.iter().map(|(k, _)| k.as_bytes().to_vec()).collect();
        assert_eq!(keys, vec![b"identifier".to_vec(), b"display_name".to_vec()]);
        assert!(first.get(b"missing").is_none());
    }

    #[test]
    fn validator_rejects_non_canonical_interned_shapes() {
        use crate::DocError;
        let plain = wide(8);
        let interned = intern(&plain).expect("interns");
        // Out-of-bounds ref id: patch the first 0xA9's id to table size.
        let mut bad = interned.clone();
        let pos = bad.iter().position(|&b| b == TAG_KEYREF).expect("has refs");
        bad[pos + 1] = 0xFF;
        bad[pos + 2] = 0xFF;
        assert_eq!(TapeDoc::from_bytes(&bad).unwrap_err(), DocError::BadKey);
        // Empty table is non-canonical.
        let mut hdr = Vec::new();
        header::encode(FLAG_INTERNED, 3, &mut hdr);
        hdr.extend_from_slice(&[0, 0, 0xA0]);
        assert_eq!(
            TapeDoc::from_bytes(&hdr).unwrap_err(),
            DocError::NonCanonical("empty intern table")
        );
    }

    #[test]
    fn under_used_table_entries_reject() {
        // Hand-build: table ["identifier"] but only ONE ref in the body.
        let mut tail = Vec::new();
        tail.extend_from_slice(&1u16.to_le_bytes());
        tail.extend_from_slice(&10u16.to_le_bytes());
        tail.extend_from_slice(b"identifier");
        // body: {ref0: 1}
        tail.push(TAG_OBJ);
        tail.extend_from_slice(&[4, 0, 0]);
        tail.push(TAG_KEYREF);
        tail.extend_from_slice(&0u16.to_le_bytes());
        tail.push(0x01);
        let mut doc = Vec::new();
        header::encode(FLAG_INTERNED, tail.len() as u32, &mut doc);
        doc.extend_from_slice(&tail);
        assert_eq!(
            TapeDoc::from_bytes(&doc).unwrap_err(),
            crate::DocError::NonCanonical("intern entry referenced fewer than twice")
        );
    }

    #[test]
    fn plain_encoding_of_a_tabled_key_rejects() {
        let plain = wide(8);
        let interned = intern(&plain).expect("interns");
        let doc = TapeDoc::from_bytes(&interned).expect("validates");
        let dict_len = doc.dict().len();
        // Replace the LAST keyref with the plain encoding of table[0]
        // ("identifier", 10 bytes = fixstr 0x8A + bytes = 11 B vs 3 B ref);
        // splice keeps every container length consistent by rebuilding via
        // rewrite: simpler here — hand-splice and fix the two enclosing
        // lengths is brittle, so instead build a fresh doc that pairs one
        // ref with one plain occurrence of the same tabled key.
        let _ = (doc, dict_len);
        let mut tail = Vec::new();
        tail.extend_from_slice(&1u16.to_le_bytes());
        tail.extend_from_slice(&10u16.to_le_bytes());
        tail.extend_from_slice(b"identifier");
        // body: [{ref0:1},{ref0:2},{"identifier":3}]
        let mut body = Vec::new();
        body.push(TAG_ARR);
        let arr_len_at = body.len();
        body.extend_from_slice(&[0, 0, 0]);
        for v in [1u8, 2] {
            body.push(TAG_OBJ);
            body.extend_from_slice(&[4, 0, 0]);
            body.push(TAG_KEYREF);
            body.extend_from_slice(&0u16.to_le_bytes());
            body.push(v);
        }
        body.push(TAG_OBJ);
        body.extend_from_slice(&[12, 0, 0]);
        body.push(FIXSTR_BASE + 10);
        body.extend_from_slice(b"identifier");
        body.push(0x03);
        let arr_body_len = body.len() - (arr_len_at + 3);
        let len_bytes = (arr_body_len as u32).to_le_bytes();
        body[arr_len_at..arr_len_at + 3].copy_from_slice(&len_bytes[..3]);
        tail.extend_from_slice(&body);
        let mut doc = Vec::new();
        header::encode(FLAG_INTERNED, tail.len() as u32, &mut doc);
        doc.extend_from_slice(&tail);
        assert_eq!(
            TapeDoc::from_bytes(&doc).unwrap_err(),
            crate::DocError::NonCanonical("plain encoding of an interned key")
        );
    }
}
