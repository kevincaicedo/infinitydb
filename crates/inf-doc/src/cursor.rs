//! Form-agnostic read cursors (ADR-0036 D1): one value/object/array view
//! over both physical forms, so the S09 path evaluator and the M4.5
//! predicate VM never know which form a document is in. Enum dispatch,
//! not `dyn` — two inlined arms, no hot-path vtables (INFINITY_STYLE
//! §Performance).

use crate::arena;
use crate::tape::{self, DocStr, ValueRef};

/// A document value behind either physical form.
#[derive(Copy, Clone, Debug)]
pub enum DocValue<'a> {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(DocStr<'a>),
    Obj(ObjCursor<'a>),
    Arr(ArrCursor<'a>),
}

impl<'a> From<ValueRef<'a>> for DocValue<'a> {
    fn from(v: ValueRef<'a>) -> DocValue<'a> {
        match v {
            ValueRef::Null => DocValue::Null,
            ValueRef::Bool(b) => DocValue::Bool(b),
            ValueRef::I64(i) => DocValue::I64(i),
            ValueRef::F64(f) => DocValue::F64(f),
            ValueRef::Str(s) => DocValue::Str(s),
            ValueRef::Obj(o) => DocValue::Obj(ObjCursor::Tape(o)),
            ValueRef::Arr(a) => DocValue::Arr(ArrCursor::Tape(a)),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum ObjCursor<'a> {
    Tape(tape::ObjRef<'a>),
    Arena(arena::ObjRef<'a>),
}

impl<'a> ObjCursor<'a> {
    /// Entry count. Tape walks (no stored count, D3); arena reads it.
    pub fn len(&self) -> usize {
        match self {
            ObjCursor::Tape(o) => o.len(),
            ObjCursor::Arena(o) => o.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            ObjCursor::Tape(o) => o.is_empty(),
            ObjCursor::Arena(o) => o.is_empty(),
        }
    }

    /// First entry matching `key` (memcmp; the pinned D5 rule).
    #[inline]
    pub fn get(&self, key: &[u8]) -> Option<DocValue<'a>> {
        match self {
            ObjCursor::Tape(o) => o.get(key).map(DocValue::from),
            ObjCursor::Arena(o) => o.get(key),
        }
    }

    pub fn iter(&self) -> ObjEntries<'a> {
        match self {
            ObjCursor::Tape(o) => ObjEntries::Tape(o.iter()),
            ObjCursor::Arena(o) => ObjEntries::Arena(o.iter()),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ObjEntries<'a> {
    Tape(tape::ObjIter<'a>),
    Arena(arena::ObjIter<'a>),
}

impl<'a> Iterator for ObjEntries<'a> {
    type Item = (DocStr<'a>, DocValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ObjEntries::Tape(it) => it.next().map(|(k, v)| (k, DocValue::from(v))),
            ObjEntries::Arena(it) => it.next(),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum ArrCursor<'a> {
    Tape(tape::ArrRef<'a>),
    Arena(arena::ArrRef<'a>),
}

impl<'a> ArrCursor<'a> {
    /// Element count. Tape walks; arena reads it.
    pub fn len(&self) -> usize {
        match self {
            ArrCursor::Tape(a) => a.len(),
            ArrCursor::Arena(a) => a.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            ArrCursor::Tape(a) => a.is_empty(),
            ArrCursor::Arena(a) => a.is_empty(),
        }
    }

    /// Element at `index`. Tape: `index` O(1) skips; arena: O(1).
    /// Negative-index commands resolve against `len()` upstream.
    pub fn index(&self, index: usize) -> Option<DocValue<'a>> {
        match self {
            ArrCursor::Tape(a) => a.index(index).map(DocValue::from),
            ArrCursor::Arena(a) => a.index(index),
        }
    }

    pub fn iter(&self) -> ArrEntries<'a> {
        match self {
            ArrCursor::Tape(a) => ArrEntries::Tape(a.iter()),
            ArrCursor::Arena(a) => ArrEntries::Arena(a.iter()),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ArrEntries<'a> {
    Tape(tape::ArrIter<'a>),
    Arena(arena::ArrIter<'a>),
}

impl<'a> Iterator for ArrEntries<'a> {
    type Item = DocValue<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ArrEntries::Tape(it) => it.next().map(DocValue::from),
            ArrEntries::Arena(it) => it.next(),
        }
    }
}
