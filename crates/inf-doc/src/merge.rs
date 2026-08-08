//! RFC 7386 merge-patch over validated canonical tapes (M3-S14;
//! ADR-0042 D6): the [`crate::apply::ApplyOp::Merge`] backend. Pure
//! function of (target bytes, patch fragment) — replay re-executes it
//! from a `DocDelta` whose operand is the patch, so iteration order and
//! output bytes are part of the durable contract (L7).
//!
//! The RFC's recursive definition is realized with an explicit frame
//! stack (the L9/STYLE no-recursion rule); depth is bounded by the
//! validated inputs' 128-level cap. Two frame kinds:
//!
//! - `Merge` — both sides are objects: phase 1 walks the target's
//!   entries in order (kept verbatim, replaced, recursed into, or
//!   deleted by a `null` patch member); phase 2 appends patch-only keys
//!   in patch order (insertion-order rule, ADR-0036). Existing keys keep
//!   their positions; canonical key uniqueness is preserved by
//!   construction (both inputs are canonical, and phase 2 skips keys the
//!   target already had).
//! - `Strip` — an object patch lands on a non-object (or absent) target:
//!   `MergePatch({}, patch)` — null-valued members drop, recursively
//!   through object chains only; arrays and scalars copy verbatim (the
//!   RFC's rule — nulls *inside* arrays survive).
//!
//! A non-object patch is always a literal replacement at the selected
//! value. Member deletion occurs only while merging a null-valued member
//! from an object patch, exactly where the RFC defines it.

use crate::tape::{Dict, TAG_NULL, TAG_OBJ, ValueRef, read_value, skip_value};

/// One in-progress container merge. Offsets index the frame's own input
/// slices (`t` ranges → target body, `p` ranges → patch fragment);
/// `hdr_at` is the emitted-but-unpatched `TAG_OBJ` header in `out`.
#[derive(Copy, Clone)]
enum Frame {
    Merge {
        t_start: usize,
        t_off: usize,
        t_end: usize,
        p_start: usize,
        p_off: usize,
        p_end: usize,
        hdr_at: usize,
        in_patch_phase: bool,
    },
    Strip {
        p_off: usize,
        p_end: usize,
        hdr_at: usize,
    },
}

/// Append `MergePatch(target value at t_at, patch)` to `out` as canonical
/// body bytes. `patch` is a whole canonical fragment (§3.4 R6 — plain
/// form, never interned); non-object patches replace literally.
pub(crate) fn merge_value(t_body: &[u8], t_at: usize, patch: &[u8], out: &mut Vec<u8>) {
    if patch[0] != TAG_OBJ {
        out.extend_from_slice(patch);
        return;
    }
    let mut stack: Vec<Frame> = Vec::new();
    let hdr_at = open_object(out);
    let p_end = skip_value(patch, 0);
    if t_body[t_at] == TAG_OBJ {
        let t_end = skip_value(t_body, t_at);
        stack.push(Frame::Merge {
            t_start: t_at + 4,
            t_off: t_at + 4,
            t_end,
            p_start: 4,
            p_off: 4,
            p_end,
            hdr_at,
            in_patch_phase: false,
        });
    } else {
        stack.push(Frame::Strip { p_off: 4, p_end, hdr_at });
    }
    run(t_body, patch, out, &mut stack);
}

/// `MergePatch` against an **absent** target (ADR-0042 D6): object
/// patches merge into `{}` (nulls stripped through object chains);
/// everything else is literal. Appends canonical body bytes to `out`.
pub(crate) fn merge_absent(patch: &[u8], out: &mut Vec<u8>) {
    if patch[0] != TAG_OBJ {
        out.extend_from_slice(patch);
        return;
    }
    let hdr_at = open_object(out);
    let p_end = skip_value(patch, 0);
    let mut stack = vec![Frame::Strip { p_off: 4, p_end, hdr_at }];
    run(patch, patch, out, &mut stack);
}

fn run(t_body: &[u8], patch: &[u8], out: &mut Vec<u8>, stack: &mut Vec<Frame>) {
    while let Some(&frame) = stack.last() {
        debug_assert!(stack.len() <= 128, "validated inputs bound merge depth");
        let top = stack.len() - 1;
        match frame {
            Frame::Strip { p_off, p_end, hdr_at } => {
                if p_off >= p_end {
                    close_object(out, hdr_at);
                    stack.pop();
                    continue;
                }
                let (_, val_at) = entry_key(patch, p_off);
                let val_end = skip_value(patch, val_at);
                stack[top] = Frame::Strip { p_off: val_end, p_end, hdr_at };
                match patch[val_at] {
                    TAG_NULL => {}
                    TAG_OBJ => {
                        out.extend_from_slice(&patch[p_off..val_at]);
                        let child = open_object(out);
                        stack.push(Frame::Strip {
                            p_off: val_at + 4,
                            p_end: val_end,
                            hdr_at: child,
                        });
                    }
                    _ => out.extend_from_slice(&patch[p_off..val_end]),
                }
            }
            Frame::Merge {
                t_start,
                t_off,
                t_end,
                p_start,
                p_off,
                p_end,
                hdr_at,
                in_patch_phase,
            } => {
                if !in_patch_phase {
                    if t_off >= t_end {
                        stack[top] = Frame::Merge {
                            t_start,
                            t_off,
                            t_end,
                            p_start,
                            p_off,
                            p_end,
                            hdr_at,
                            in_patch_phase: true,
                        };
                        continue;
                    }
                    let (key, val_at) = entry_key(t_body, t_off);
                    let val_end = skip_value(t_body, val_at);
                    stack[top] = Frame::Merge {
                        t_start,
                        t_off: val_end,
                        t_end,
                        p_start,
                        p_off,
                        p_end,
                        hdr_at,
                        in_patch_phase,
                    };
                    let Some((pv_at, pv_end)) = find_member(patch, p_start, p_end, key) else {
                        out.extend_from_slice(&t_body[t_off..val_end]);
                        continue;
                    };
                    match patch[pv_at] {
                        // A null patch member deletes the target member.
                        TAG_NULL => {}
                        TAG_OBJ if t_body[val_at] == TAG_OBJ => {
                            out.extend_from_slice(&t_body[t_off..val_at]);
                            let child = open_object(out);
                            stack.push(Frame::Merge {
                                t_start: val_at + 4,
                                t_off: val_at + 4,
                                t_end: val_end,
                                p_start: pv_at + 4,
                                p_off: pv_at + 4,
                                p_end: pv_end,
                                hdr_at: child,
                                in_patch_phase: false,
                            });
                        }
                        TAG_OBJ => {
                            out.extend_from_slice(&t_body[t_off..val_at]);
                            let child = open_object(out);
                            stack.push(Frame::Strip {
                                p_off: pv_at + 4,
                                p_end: pv_end,
                                hdr_at: child,
                            });
                        }
                        _ => {
                            out.extend_from_slice(&t_body[t_off..val_at]);
                            out.extend_from_slice(&patch[pv_at..pv_end]);
                        }
                    }
                } else {
                    if p_off >= p_end {
                        close_object(out, hdr_at);
                        stack.pop();
                        continue;
                    }
                    let (key, val_at) = entry_key(patch, p_off);
                    let val_end = skip_value(patch, val_at);
                    stack[top] = Frame::Merge {
                        t_start,
                        t_off,
                        t_end,
                        p_start,
                        p_off: val_end,
                        p_end,
                        hdr_at,
                        in_patch_phase,
                    };
                    // Keys the target held were handled (or deleted) in
                    // phase 1; nulls introduce nothing on absent keys.
                    if find_member(t_body, t_start, t_end, key).is_some() {
                        continue;
                    }
                    match patch[val_at] {
                        TAG_NULL => {}
                        TAG_OBJ => {
                            out.extend_from_slice(&patch[p_off..val_at]);
                            let child = open_object(out);
                            stack.push(Frame::Strip {
                                p_off: val_at + 4,
                                p_end: val_end,
                                hdr_at: child,
                            });
                        }
                        _ => out.extend_from_slice(&patch[p_off..val_end]),
                    }
                }
            }
        }
    }
}

/// Emit an object header with a placeholder length; [`close_object`]
/// back-patches it when the frame completes.
fn open_object(out: &mut Vec<u8>) -> usize {
    let hdr_at = out.len();
    out.extend_from_slice(&[TAG_OBJ, 0, 0, 0]);
    hdr_at
}

fn close_object(out: &mut [u8], hdr_at: usize) {
    let body_len = out.len() - hdr_at - 4;
    debug_assert!(body_len < 1 << 24, "the document byte cap bounds every u24");
    let bytes = (body_len as u32).to_le_bytes();
    out[hdr_at + 1..hdr_at + 4].copy_from_slice(&bytes[..3]);
}

/// Key bytes + value offset of the object entry at `off` (validated
/// object bodies hold a string key at every entry position).
fn entry_key(body: &[u8], off: usize) -> (&[u8], usize) {
    match read_value(body, Dict::empty(), off) {
        (ValueRef::Str(s), val_at) => (s.as_bytes(), val_at),
        _ => unreachable!("validated object key positions hold strings"),
    }
}

/// First entry of the object body `[start, end)` whose key equals `key`
/// → its value's byte extents (canonical bodies hold each key once).
fn find_member(body: &[u8], start: usize, end: usize, key: &[u8]) -> Option<(usize, usize)> {
    let mut off = start;
    while off < end {
        let (entry, val_at) = entry_key(body, off);
        let val_end = skip_value(body, val_at);
        if entry == key {
            return Some((val_at, val_end));
        }
        off = val_end;
    }
    None
}
