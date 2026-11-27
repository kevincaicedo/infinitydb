//! The resumable walk of a tape value: [`Emitter`], and [`OwnedEmit`], which
//! pairs one with the bytes it walks.
//!
//! An emitter's state is positions: the open containers, each a body range
//! and an offset, and what the next step owes first — a string's next
//! source byte, a separator's next byte, a value's offset. It holds no
//! reference into the document. A caller presents the same bytes at every
//! step and gets them written in slices: a step ends when the value does,
//! or when its byte budget or its work budget is spent.
//!
//! The frame being walked lies beside the stack, which holds its parents
//! only: a value one container deep opens no parent and allocates nothing,
//! and a deeper one allocates once a reply.
//!
//! | Pending | A step does | Then |
//! |---|---|---|
//! | `Open` | writes a root's opener: `{`, `[` or `"` | `Frame` or `Str` |
//! | `Atom` | writes a root `null`, boolean or number whole | `Done` |
//! | `Frame`, entries left | `,` unless first; formatted, the break | a key's `Str`, or `Value` |
//! | `Frame`, exhausted | formatted and non-empty, the break; the closer; pops | `Frame`, `Done` |
//! | `Value` | an atom whole, a string's `"`, or an opener, its parent pushed | `Frame` or `Str` |
//! | `Str` | the bytes its window takes; at its end `"`, and `:` after a key | see below |
//! | `Separator` | the bytes its window takes | its entry, closer or value |
//! | any, a budget spent | nothing more | the same state, at its position |
//! | `Done` | nothing | `Done` |
//!
//! A string never ends a step inside an escape sequence. Past a key comes
//! its value (`Value`, or the space before it when formatted); past a
//! value string, its frame's next move (`Frame`); past a root string,
//! `Done`. A value is read only inside a container: a root string or atom
//! has its own state, so no value ends the walk.
//!
//! The step that ends the value returns [`EmitStep::Done`] even when it
//! also spends its budget, so a [`EmitStep::Yield`] always means bytes
//! remain.
//!
//! What holds, and what holds it:
//!
//! - positions are applied only to the bytes they came from: the core step
//!   is crate-private; the `serialize_*` entries pair a walk with its
//!   source inside one call, and [`OwnedEmit`] owns both in private fields
//!   (type);
//! - a step writes at most its byte budget plus
//!   [`EMIT_OVERSHOOT_BYTES_MAX`], examines at most its byte budget plus
//!   one escape of string source, visits at most its work budget plus one
//!   value, and a step that yields has written (debug assertions, and the
//!   tests and the fuzz arm from the step's own output);
//! - depth is at most [`DEPTH_MAX`]: the source is a validated tape (debug
//!   assertion on push).
//!
//! A position outside its bytes is not reachable from a validated source.
//! If a validator defect made one, the slice it indexes panics.
// ADR-0144 D2/D3: a decoder scope; docs/lint-scopes.tsv names its tier per lint family.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]

use core::num::NonZeroU32;

use super::{
    Atom, CountingSink, EmitSink, Root, SeparatorPosition, SerializeOpts, TapeSource, put_atom,
    put_escaped, put_separator, put_separator_window,
};
use crate::error::DocError;
use crate::header::{self, FLAG_INTERNED, HEADER_LEN};
#[cfg(any(test, fuzzing))]
use crate::limits::ESCAPE_BYTES_MAX;
use crate::limits::{DEPTH_MAX, EMIT_OVERSHOOT_BYTES_MAX};
use crate::tape::{
    CheckBudget, CheckEnd, CheckWalk, Dict, KeyAt, ValueAt, key_at, read_value, value_at,
};

/// How one step ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EmitStep {
    /// A budget is spent and bytes remain: the next step resumes.
    Yield,
    /// The value is written whole. A later step writes nothing.
    Done,
}

/// What bounds one step. The type is each budget's switch: a step under
/// [`Unbounded`] compiles no check, one under [`Bytes`] the byte check
/// alone, one under [`StepBudget`] both.
pub(crate) trait Budget: Copy {
    const BYTES: bool;
    const WORK: bool;
    /// Bytes the step may write before it yields.
    fn bytes_max(self) -> usize;
    /// Values the step may visit before it yields.
    fn nodes_max(self) -> usize;
}

/// No budget: the step runs to the value's end.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Unbounded;

/// A byte budget and no work budget: the bounded entries', where a work
/// budget could never bind.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Bytes(pub(crate) NonZeroU32);

/// What one step of an [`OwnedEmit`] may spend: bytes, and work. Bytes
/// alone do not bound a step over values that write one byte each. The two
/// are named, so a call site cannot swap them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StepBudget {
    /// Bytes the step may write before it yields; in the check phase,
    /// bytes of string it may examine.
    pub bytes_max: NonZeroU32,
    /// Values the step may visit before it yields; in the check phase,
    /// tokens it may examine.
    pub nodes_max: NonZeroU32,
}

impl Budget for Unbounded {
    const BYTES: bool = false;
    const WORK: bool = false;

    #[inline(always)]
    fn bytes_max(self) -> usize {
        usize::MAX
    }

    #[inline(always)]
    fn nodes_max(self) -> usize {
        usize::MAX
    }
}

impl Budget for Bytes {
    const BYTES: bool = true;
    const WORK: bool = false;

    #[inline(always)]
    fn bytes_max(self) -> usize {
        wide(self.0.get())
    }

    #[inline(always)]
    fn nodes_max(self) -> usize {
        usize::MAX
    }
}

impl Budget for StepBudget {
    const BYTES: bool = true;
    const WORK: bool = true;

    #[inline(always)]
    fn bytes_max(self) -> usize {
        wide(self.bytes_max.get())
    }

    #[inline(always)]
    fn nodes_max(self) -> usize {
        wide(self.nodes_max.get())
    }
}

/// A `u32` position as an index.
#[inline(always)]
fn wide(position: u32) -> usize {
    position as usize
}

/// An offset as a walk's `u32` position: one inside a region or a
/// dictionary whose length [`Emitter::root`] narrowed with `try_from`.
#[inline(always)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "bound: an offset at most the length of the region or the dictionary it lies in, \
              which `Emitter::root` narrowed with `u32::try_from` before the walk began"
)]
fn narrow(offset: usize) -> u32 {
    debug_assert!(u32::try_from(offset).is_ok(), "an offset inside a narrowed region");
    offset as u32
}

/// One open container: its body's range in the region and the next
/// entry's offset. An entry is first when `off == body_start`.
#[derive(Copy, Clone, Debug)]
struct WalkFrame {
    is_obj: bool,
    body_start: u32,
    body_end: u32,
    off: u32,
}

impl WalkFrame {
    /// The top frame's place while no container is open.
    const NONE: WalkFrame = WalkFrame { is_obj: false, body_start: 0, body_end: 0, off: 0 };
}

/// What a root handed over without its tag opens with.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Opener {
    Obj,
    Arr,
    Str,
}

/// The buffer a string's bytes lie in: a tabled key's are in the
/// dictionary, every other string's in the region.
#[derive(Copy, Clone, Debug)]
enum Span {
    Region,
    Dict,
}

#[derive(Copy, Clone, Debug)]
enum Role {
    /// An object's key: its colon and its value follow.
    Key,
    /// A value inside a container: its frame's next move follows.
    Value,
    /// The root itself: the walk ends with it.
    Root,
}

/// A string in flight: its bytes' span, `start..end`, and `at`, the next
/// source byte. `at == end` is a string whose closing quote is owed.
#[derive(Copy, Clone, Debug)]
struct StrAt {
    span: Span,
    start: u32,
    end: u32,
    at: u32,
    role: Role,
}

impl StrAt {
    /// The turn's string before it has one.
    const NONE: StrAt = StrAt { span: Span::Region, start: 0, end: 0, at: 0, role: Role::Value };

    #[inline(always)]
    fn new(span: Span, start: usize, end: usize, role: Role) -> StrAt {
        StrAt { span, start: narrow(start), end: narrow(end), at: narrow(start), role }
    }
}

/// What follows a separator.
#[derive(Copy, Clone, Debug)]
enum Then {
    /// The top frame's next entry: the break before an element.
    Entry,
    /// The top frame's closer: the break before it.
    Close,
    /// A key's value: the space after its colon.
    Value,
}

/// A separator in flight.
#[derive(Copy, Clone, Debug)]
struct SeparatorAt {
    position: SeparatorPosition,
    then: Then,
}

/// What the next step owes first.
#[derive(Copy, Clone, Debug)]
enum Pending {
    Done,
    Open(Opener),
    Atom(Atom),
    Frame,
    Value { at: u32 },
    Str(StrAt),
    Separator(SeparatorAt),
}

/// A turn's next move. It carries no position: those stay in the turn's
/// own fields, so moving from one phase to the next copies no state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Flow {
    /// The top frame's next entry or closer.
    Frame,
    /// The turn's string, from its position.
    Str,
    /// The value at the turn's value offset.
    Value,
    /// The value is written whole.
    Done,
    /// A budget ended the step inside a token: the turn says what is owed.
    Yield,
}

/// A region or a dictionary longer than a `u32` position reaches.
#[derive(Copy, Clone, Debug)]
pub(crate) struct PositionWidth;

/// What a walk begins at: an atom, or a container's body or a string's
/// payload of `body_end` bytes, handed over without its tag.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Origin {
    Atom(Atom),
    Tape { opener: Opener, body_end: u32 },
}

/// A resumable serialization of one tape value (module docs). It has no
/// lifetime: nothing in it borrows the document.
#[derive(Debug)]
pub(crate) struct Emitter {
    /// The frames under the top one, grown on push and kept across values.
    /// A value one container deep opens none, so it allocates nothing, and
    /// a reply that writes many values allocates at most once.
    parents: Vec<WalkFrame>,
    /// The frame being walked, while a container is open. A value is read
    /// only inside one: a root string or atom has its own state.
    top: WalkFrame,
    base_depth: usize,
    pending: Pending,
    /// String source bytes the last step examined.
    #[cfg(any(test, fuzzing))]
    examined: usize,
}

impl Emitter {
    /// An emitter with nothing to write.
    #[inline(always)]
    pub(crate) fn new() -> Emitter {
        Emitter {
            parents: Vec::new(),
            top: WalkFrame::NONE,
            base_depth: 0,
            pending: Pending::Done,
            #[cfg(any(test, fuzzing))]
            examined: 0,
        }
    }

    /// Roots the walk at a container's body or a string's payload, in
    /// O(1). `base_depth` is the depth of the wrapper the value nests in.
    #[inline(always)]
    pub(crate) fn root(
        &mut self,
        opener: Opener,
        source: TapeSource<'_>,
        base_depth: usize,
    ) -> Result<(), PositionWidth> {
        let body_end = u32::try_from(source.region.len()).map_err(|_| PositionWidth)?;
        u32::try_from(source.dict.bytes().len()).map_err(|_| PositionWidth)?;
        self.begin(Origin::Tape { opener, body_end }, base_depth);
        Ok(())
    }

    /// Begins a walk at `origin`, in O(1): the stack is cleared and keeps
    /// its allocation, so the same call begins a finished walk again. An
    /// atom is written whole by one step, so a measure pass counts it like
    /// any token.
    #[inline(always)]
    pub(crate) fn begin(&mut self, origin: Origin, base_depth: usize) {
        self.parents.clear();
        self.base_depth = base_depth;
        match origin {
            Origin::Atom(atom) => self.pending = Pending::Atom(atom),
            Origin::Tape { opener, body_end } => {
                let is_obj = matches!(opener, Opener::Obj);
                self.top = WalkFrame { is_obj, body_start: 0, body_end, off: 0 };
                self.pending = Pending::Open(opener);
            }
        }
    }

    /// String source bytes the last step examined.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn examined(&self) -> usize {
        self.examined
    }

    /// One step over `source`, the bytes this walk was rooted at. A step
    /// first finishes what the last one left in flight; each turn of its
    /// loop then runs at most one string and one value, checks the budgets
    /// once, and makes the top frame's next move. No token is written
    /// twice and none is split.
    pub(crate) fn step<B: Budget, const FORMATTED: bool>(
        &mut self,
        source: TapeSource<'_>,
        opts: &SerializeOpts<'_>,
        sink: &mut impl EmitSink,
        budget: B,
    ) -> EmitStep {
        let (top, base_depth) = (self.top, self.base_depth);
        let mut turn = Turn::new(source, opts, sink, &mut self.parents, top, base_depth, budget);
        let mut flow = match self.pending {
            Pending::Done => return EmitStep::Done,
            Pending::Frame => Flow::Frame,
            Pending::Open(opener) => turn.open_root(opener),
            Pending::Atom(atom) => turn.root_atom(atom),
            Pending::Separator(separator) => turn.resume_separator(separator),
            Pending::Value { at } => {
                turn.value_at = at;
                Flow::Value
            }
            Pending::Str(string) => {
                turn.string = string;
                // Planted: a string a step resumes starts over.
                if cfg!(inf_canary_ser_drop_straddle) {
                    turn.string.at = string.start;
                }
                Flow::Str
            }
        };
        let end = loop {
            if flow == Flow::Str {
                flow = turn.string::<FORMATTED>();
            }
            if flow == Flow::Value {
                flow = turn.value();
            }
            match flow {
                Flow::Done => break EmitStep::Done,
                Flow::Yield => break EmitStep::Yield,
                Flow::Frame | Flow::Str | Flow::Value => {}
            }
            if turn.spent() {
                if flow == Flow::Str {
                    turn.owed = Pending::Str(turn.string);
                }
                break EmitStep::Yield;
            }
            if flow == Flow::Frame {
                flow = turn.frame::<FORMATTED>();
            }
        };
        turn.check(end);
        #[cfg(any(test, fuzzing))]
        {
            self.examined = turn.examined;
        }
        let (top, owed) = (turn.top, turn.owed);
        match end {
            EmitStep::Done => self.pending = Pending::Done,
            EmitStep::Yield => {
                self.top = top;
                self.pending = owed;
            }
        }
        end
    }
}

/// One step's locals: the source, the sink, what the step has spent, the
/// top frame — pushed under a container it opens, written back at a yield
/// — and the string and the value offset the turn is on. `owed` is written
/// only where a step stops: it is what the next step finds pending.
struct Turn<'a, 'e, S, B> {
    source: TapeSource<'a>,
    opts: &'e SerializeOpts<'e>,
    sink: &'e mut S,
    parents: &'e mut Vec<WalkFrame>,
    base_depth: usize,
    top: WalkFrame,
    budget: B,
    written: usize,
    visited: usize,
    string: StrAt,
    value_at: u32,
    owed: Pending,
    #[cfg(any(test, fuzzing))]
    examined: usize,
}

impl<'a, 'e, S: EmitSink, B: Budget> Turn<'a, 'e, S, B> {
    /// A step's locals at its start: nothing spent, the top frame as the
    /// last step left it.
    #[inline(always)]
    fn new(
        source: TapeSource<'a>,
        opts: &'e SerializeOpts<'e>,
        sink: &'e mut S,
        parents: &'e mut Vec<WalkFrame>,
        top: WalkFrame,
        base_depth: usize,
        budget: B,
    ) -> Self {
        Turn {
            source,
            opts,
            sink,
            parents,
            top,
            base_depth,
            budget,
            written: 0,
            visited: 0,
            string: StrAt::NONE,
            value_at: 0,
            owed: Pending::Frame,
            #[cfg(any(test, fuzzing))]
            examined: 0,
        }
    }

    /// A budget is spent: the turn's one check of both.
    #[inline(always)]
    fn spent(&self) -> bool {
        // Planted: the work budget is not read.
        let work = B::WORK && !cfg!(inf_canary_ser_ignore_nodes);
        self.bytes_spent() || (work && self.visited >= self.budget.nodes_max())
    }

    #[inline(always)]
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: `written` counts bytes of one step's output, at most 6 a source byte \
                  of a region in memory"
    )]
    fn bytes_spent(&self) -> bool {
        // Planted: the check reads one byte early, so a step of one byte
        // yields before it writes.
        let written =
            if cfg!(inf_canary_ser_yield_early) { self.written + 1 } else { self.written };
        B::BYTES && written >= self.budget.bytes_max()
    }

    /// What the byte budget has left: the window of a string or a separator.
    #[inline(always)]
    fn window(&self) -> usize {
        if B::BYTES { self.budget.bytes_max().saturating_sub(self.written) } else { usize::MAX }
    }

    #[inline(always)]
    fn put_byte(&mut self, byte: u8) {
        self.sink.put_byte(byte);
        self.wrote(1);
    }

    /// Counts bytes the step wrote.
    #[inline(always)]
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: `written` counts bytes of one step's output, at most 6 a source byte \
                  of a region in memory"
    )]
    fn wrote(&mut self, bytes: usize) {
        self.written += bytes;
    }

    /// Counts a value the step visited.
    #[inline(always)]
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: `visited` counts values of one region in memory, each at least a byte"
    )]
    fn visit(&mut self) {
        self.visited += 1;
    }

    /// Stops the step inside a token: `owed` is what the next step resumes.
    #[inline(always)]
    fn suspend(&mut self, owed: Pending) -> Flow {
        self.owed = owed;
        Flow::Yield
    }

    /// The top frame's next move: an entry, or its closer.
    #[inline(always)]
    fn frame<const FORMATTED: bool>(&mut self) -> Flow {
        let top = self.top;
        if top.off == top.body_end {
            if FORMATTED && top.body_start != top.body_end {
                let position = SeparatorPosition::BREAK;
                return self.separator(SeparatorAt { position, then: Then::Close });
            }
            return self.close();
        }
        if top.off != top.body_start {
            self.put_byte(b',');
        }
        if FORMATTED {
            let position = SeparatorPosition::BREAK;
            return self.separator(SeparatorAt { position, then: Then::Entry });
        }
        self.entry()
    }

    /// The depth a separator's break indents to.
    #[inline(always)]
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: a depth is the open frames, a vector's length and one, plus a \
                  wrapper's stack length"
    )]
    fn separator_depth(&self, then: Then) -> usize {
        // Planted: a value's indentation ignores the wrapper it nests in.
        let base = if cfg!(inf_canary_ser_base_ignored) { 0 } else { self.base_depth };
        match then {
            Then::Entry => base + self.parents.len() + 1,
            Then::Close => base + self.parents.len(),
            Then::Value => 0,
        }
    }

    /// A separator from its start, then what it precedes: whole when the
    /// window takes it, which is the walk's usual turn.
    #[inline(always)]
    fn separator(&mut self, mut separator: SeparatorAt) -> Flow {
        let depth = self.separator_depth(separator.then);
        let window = self.window();
        let (written, ended) =
            put_separator(self.sink, self.opts, depth, &mut separator.position, window);
        self.wrote(written);
        if !ended || self.bytes_spent() {
            return self.suspend(Pending::Separator(separator));
        }
        self.after_separator(separator.then)
    }

    /// A separator a step finds pending, from its position: the windowed
    /// writer alone, so the turn that starts a separator keeps its own
    /// shape whatever a resumed one needs.
    #[inline(always)]
    fn resume_separator(&mut self, mut separator: SeparatorAt) -> Flow {
        let depth = self.separator_depth(separator.then);
        let window = self.window();
        let position = &mut separator.position;
        let (written, ended) = put_separator_window(self.sink, self.opts, depth, position, window);
        self.wrote(written);
        if !ended || self.bytes_spent() {
            return self.suspend(Pending::Separator(separator));
        }
        self.after_separator(separator.then)
    }

    /// What a separator precedes: an entry, a closer, or a key's value.
    #[inline(always)]
    fn after_separator(&mut self, then: Then) -> Flow {
        match then {
            Then::Entry => self.entry(),
            Then::Close => self.close(),
            Then::Value => {
                self.value_at = self.top.off;
                Flow::Value
            }
        }
    }

    /// The top frame's next entry: an object's key, or an array's element.
    #[inline(always)]
    fn entry(&mut self) -> Flow {
        if self.top.is_obj {
            self.key();
            return Flow::Str;
        }
        self.value_at = self.top.off;
        Flow::Value
    }

    /// A key's opening quote, and where its bytes lie. The frame moves to
    /// the key's value.
    #[inline(always)]
    fn key(&mut self) {
        let off = wide(self.top.off);
        let (key, value_at) = key_at(self.source.region, self.source.dict, off);
        self.top.off = narrow(value_at);
        self.put_byte(b'"');
        self.string = match key {
            KeyAt::Body { start, end } => StrAt::new(Span::Region, start, end, Role::Key),
            // Planted: a tabled key is read from the region, where only
            // its three-byte reference lies.
            KeyAt::Dict { .. } if cfg!(inf_canary_ser_key_in_region) => {
                StrAt::new(Span::Region, off, value_at, Role::Key)
            }
            KeyAt::Dict { start, end } => StrAt::new(Span::Dict, start, end, Role::Key),
        };
    }

    /// The top frame's closer. The walk moves to the parent, or ends.
    #[inline(always)]
    fn close(&mut self) -> Flow {
        self.put_byte(if self.top.is_obj { b'}' } else { b']' });
        match self.parents.pop() {
            Some(parent) => {
                self.top = parent;
                Flow::Frame
            }
            None => Flow::Done,
        }
    }

    /// The turn's string from its position, as far as the window takes
    /// it; at its end the closing quote and, after a key, the colon and
    /// the space before its value.
    #[inline(always)]
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: `at + consumed <= end`, the string's end in its buffer; `examined` \
                  counts source bytes of buffers in memory"
    )]
    fn string<const FORMATTED: bool>(&mut self) -> Flow {
        let string = self.string;
        let mut bytes = self.source.region;
        if matches!(string.span, Span::Dict) {
            bytes = self.source.dict.bytes();
        }
        let window = self.window();
        let did = put_escaped(self.sink, &bytes[..wide(string.end)], wide(string.at), window);
        self.wrote(did.written);
        self.string.at = narrow(wide(string.at) + did.consumed);
        #[cfg(any(test, fuzzing))]
        {
            self.examined += did.examined;
        }
        if self.string.at < string.end || self.bytes_spent() {
            return self.suspend(Pending::Str(self.string));
        }
        self.put_byte(b'"');
        match string.role {
            Role::Root => Flow::Done,
            Role::Value => Flow::Frame,
            Role::Key => {
                self.put_byte(b':');
                if FORMATTED {
                    let position = SeparatorPosition::SPACE;
                    return self.separator(SeparatorAt { position, then: Then::Value });
                }
                if self.bytes_spent() {
                    return self.suspend(Pending::Value { at: self.top.off });
                }
                self.value_at = self.top.off;
                Flow::Value
            }
        }
    }

    /// One value: an atom whole, a string's opening quote, or a
    /// container's opener, its frame pushed.
    #[inline(always)]
    fn value(&mut self) -> Flow {
        self.visit();
        let (value, next) = value_at(self.source.region, wide(self.value_at));
        match value {
            ValueAt::Null => self.atom(Atom::Null, next),
            ValueAt::Bool(b) => self.atom(Atom::Bool(b), next),
            ValueAt::I64(i) => self.atom(Atom::I64(i), next),
            ValueAt::F64(f) => self.atom(Atom::F64(f), next),
            ValueAt::Str { start } => {
                self.top.off = narrow(next);
                self.put_byte(b'"');
                self.string = StrAt::new(Span::Region, start, next, Role::Value);
                Flow::Str
            }
            ValueAt::Obj { start } => self.open(true, start, next),
            ValueAt::Arr { start } => self.open(false, start, next),
        }
    }

    #[inline(always)]
    fn atom(&mut self, atom: Atom, next: usize) -> Flow {
        let written = put_atom(self.sink, atom);
        self.wrote(written);
        self.top.off = narrow(next);
        Flow::Frame
    }

    /// A container's opener. Its parent, pushed, resumes past it; its own
    /// frame is the new top.
    #[inline(always)]
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: the open frames are a vector's length and one"
    )]
    fn open(&mut self, is_obj: bool, body_start: usize, body_end: usize) -> Flow {
        self.put_byte(if is_obj { b'{' } else { b'[' });
        debug_assert!(self.parents.len() + 1 < DEPTH_MAX, "a validated tape's depth");
        self.top.off = narrow(body_end);
        self.parents.push(self.top);
        let body_start = narrow(body_start);
        self.top = WalkFrame { is_obj, body_start, body_end: narrow(body_end), off: body_start };
        Flow::Frame
    }

    /// The opener of a root handed over without its tag.
    #[inline(always)]
    fn open_root(&mut self, opener: Opener) -> Flow {
        self.visit();
        match opener {
            Opener::Obj => {
                self.put_byte(b'{');
                Flow::Frame
            }
            Opener::Arr => {
                self.put_byte(b'[');
                Flow::Frame
            }
            Opener::Str => {
                self.put_byte(b'"');
                let end = self.source.region.len();
                if cfg!(inf_canary_ser_root_str_atomic) {
                    // Planted: a root string is one token, whatever it holds.
                    let did = put_escaped(self.sink, self.source.region, 0, usize::MAX);
                    self.wrote(did.written);
                    self.put_byte(b'"');
                    return Flow::Done;
                }
                self.string = StrAt::new(Span::Region, 0, end, Role::Root);
                Flow::Str
            }
        }
    }

    #[inline(always)]
    fn root_atom(&mut self, atom: Atom) -> Flow {
        self.visit();
        let written = put_atom(self.sink, atom);
        self.wrote(written);
        Flow::Done
    }

    /// What a budgeted step holds at its end. Each plant turns off the
    /// self-check of the rule it breaks, so the independent oracle is what
    /// reads it red.
    #[inline(always)]
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bound: a budget that is read came from a `u32`, and each addend is a const \
                  of at most 32"
    )]
    fn check(&self, end: EmitStep) {
        // A budget that is read came from a `u32`, so each sum below fits.
        if B::BYTES {
            let bytes_max = self.budget.bytes_max();
            let atomic = cfg!(inf_canary_ser_root_str_atomic);
            let most = bytes_max + EMIT_OVERSHOOT_BYTES_MAX;
            debug_assert!(atomic || self.written <= most, "one unsplit token over");
            let early = cfg!(inf_canary_ser_yield_early);
            debug_assert!(early || end == EmitStep::Done || self.written > 0, "a yield wrote");
            #[cfg(any(test, fuzzing))]
            {
                let ahead = atomic || cfg!(inf_canary_ser_scan_to_escape);
                let window = bytes_max + ESCAPE_BYTES_MAX;
                debug_assert!(ahead || self.examined <= window, "a scan stays in its window");
            }
        }
        if B::WORK {
            let most = self.budget.nodes_max() + 1;
            let unread = cfg!(inf_canary_ser_ignore_nodes);
            debug_assert!(unread || self.visited <= most, "one value over");
        }
    }
}

/// Why a step of an [`OwnedEmit`] wrote or counted nothing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EmitError {
    /// The three option lengths differ from the first measure step's. The
    /// phase and its positions are as before the call.
    OptionsChanged,
    /// The measured length passed `u64::MAX`: the measure cannot finish.
    LengthOverflow,
    /// The emit pass wrote past the measured length, or ended short of it:
    /// a defect. The length is already on the wire, so the caller closes
    /// the connection.
    LengthMismatch,
}

impl core::fmt::Display for EmitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            EmitError::OptionsChanged => "serialize options changed between steps",
            EmitError::LengthOverflow => "measured length passes u64",
            EmitError::LengthMismatch => "emitted length differs from the measured length",
        })
    }
}

impl core::error::Error for EmitError {}

/// The lengths of the three options, which one [`OwnedEmit`] holds fixed:
/// a separator's position is an offset into them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct OptsLen {
    indent: usize,
    newline: usize,
    space: usize,
}

impl OptsLen {
    fn of(opts: &SerializeOpts<'_>) -> OptsLen {
        OptsLen { indent: opts.indent.len(), newline: opts.newline.len(), space: opts.space.len() }
    }
}

/// A checked document's root: what its walk begins at, and where the
/// root's region begins in the document's bytes. A plain document has no
/// dictionary, and its root's region ends where the document does.
#[derive(Copy, Clone, Debug)]
struct DocRoot {
    origin: Origin,
    region_at: u32,
}

impl DocRoot {
    /// The root of a checked document, read from its first token in O(1).
    fn of(doc: &[u8]) -> Result<DocRoot, DocError> {
        let wide_doc = DocError::TooLarge { bytes: doc.len() };
        let (value, _) = read_value(&doc[HEADER_LEN..], Dict::empty(), 0);
        match Root::of(value.into()) {
            Root::Atom(atom) => {
                let region_at = u32::try_from(doc.len()).map_err(|_| wide_doc)?;
                Ok(DocRoot { origin: Origin::Atom(atom), region_at })
            }
            Root::Tape { source, opener } => {
                let body_end = u32::try_from(source.region.len()).map_err(|_| wide_doc)?;
                let region_at = doc.len().checked_sub(source.region.len()).ok_or(wide_doc)?;
                let region_at = u32::try_from(region_at).map_err(|_| wide_doc)?;
                Ok(DocRoot { origin: Origin::Tape { opener, body_end }, region_at })
            }
            // A document's bytes are tape form; a tree is refused, typed.
            Root::Tree(_) => Err(DocError::NonCanonical("tree form")),
        }
    }

    /// The root's region, re-sliced from the document in O(1).
    fn source(self, doc: &[u8]) -> TapeSource<'_> {
        TapeSource { region: &doc[wide(self.region_at)..], dict: Dict::empty() }
    }
}

/// The first phase: the canonical check, stepped. Nothing is written.
#[derive(Debug)]
pub struct Check {
    walk: CheckWalk,
}

/// The second phase: the walk over a counting sink, which learns the
/// length the emit pass will write.
#[derive(Debug)]
pub struct Measure {
    emitter: Emitter,
    root: DocRoot,
    counted: CountingSink,
    /// The option lengths of the first step, which every later step keeps.
    opts_len: Option<OptsLen>,
    /// What the check's last step examined.
    #[cfg(any(test, fuzzing))]
    checked: crate::tape::CheckTally,
}

/// The third phase: the same walk into the caller's sink, counted against
/// the measured length.
#[derive(Debug)]
pub struct Emit {
    emitter: Emitter,
    root: DocRoot,
    /// The measure's option lengths.
    opts_len: OptsLen,
    length: u64,
    emitted: u64,
}

/// A document's bytes and the positions of its serialization in one
/// value: a plain canonical document a caller steps through a check, a
/// measure and an emit, each under a [`StepBudget`], across suspensions.
/// The bytes are owned, private and never changed, so a position kept from
/// one step is valid at the next with no check. A phase that ends moves the
/// value into the next, so nothing is measured before the check has passed
/// and nothing emitted before the measure has ended.
pub struct OwnedEmit<P> {
    doc: Vec<u8>,
    phase: P,
}

/// The document by its length: its bytes are a document's worth.
impl<P: core::fmt::Debug> core::fmt::Debug for OwnedEmit<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OwnedEmit")
            .field("doc_bytes", &self.doc.len())
            .field("phase", &self.phase)
            .finish()
    }
}

/// One check step's outcome.
#[derive(Debug)]
pub enum CheckStep {
    /// A budget is spent; step again.
    Yield(OwnedEmit<Check>),
    /// The document is canonical: the measure may begin.
    Checked(OwnedEmit<Measure>),
    /// The bytes are not a canonical document. They are dropped.
    Refused(DocError),
}

/// One measure step's outcome.
#[derive(Debug)]
pub enum MeasureStep {
    /// A budget is spent; step again.
    Yield(OwnedEmit<Measure>),
    /// The length is known: the emit may begin, at the root again.
    Measured(OwnedEmit<Emit>),
    /// Nothing was counted; the measure is handed back as it was.
    Refused(OwnedEmit<Measure>, EmitError),
}

impl OwnedEmit<Check> {
    /// Takes a document's bytes, header and body, in O(1): the header is
    /// decoded and the interned form refused, and nothing else is read.
    /// The body is checked by [`Self::step`].
    pub fn new(bytes: Vec<u8>) -> Result<OwnedEmit<Check>, DocError> {
        let decoded = header::decode(&bytes)?;
        if decoded.flags & FLAG_INTERNED != 0 {
            return Err(DocError::NonCanonical("interned form"));
        }
        Ok(OwnedEmit { doc: bytes, phase: Check { walk: CheckWalk::new() } })
    }

    /// One step of the canonical check: at most `budget.nodes_max` tokens
    /// and `budget.bytes_max` bytes of string examined, the bytes passed
    /// by at most
    /// [`CHECK_OVERSHOOT_BYTES_MAX`](crate::limits::CHECK_OVERSHOOT_BYTES_MAX);
    /// at least one of either, and nothing written.
    pub fn step(mut self, budget: StepBudget) -> CheckStep {
        let budget = CheckBudget {
            bytes_max: wide(budget.bytes_max.get()),
            tokens_max: wide(budget.nodes_max.get()),
        };
        match self.phase.walk.step(&self.doc[HEADER_LEN..], budget) {
            Ok(CheckEnd::Yield) => CheckStep::Yield(self),
            Ok(CheckEnd::Done) => match DocRoot::of(&self.doc) {
                Ok(root) => {
                    let mut emitter = Emitter::new();
                    emitter.begin(root.origin, 0);
                    CheckStep::Checked(OwnedEmit {
                        phase: Measure {
                            emitter,
                            root,
                            counted: CountingSink::default(),
                            opts_len: None,
                            #[cfg(any(test, fuzzing))]
                            checked: self.phase.walk.tally(),
                        },
                        doc: self.doc,
                    })
                }
                Err(refusal) => CheckStep::Refused(refusal),
            },
            Err(refusal) => CheckStep::Refused(refusal),
        }
    }

    /// What the last check step examined.
    #[cfg(any(test, fuzzing))]
    #[doc(hidden)]
    pub fn checked(&self) -> crate::tape::CheckTally {
        self.phase.walk.tally()
    }
}

impl OwnedEmit<Measure> {
    /// One step of the measure: the walk over a counting sink. The first
    /// step records the three option lengths; a later step with others is
    /// refused.
    pub fn step(mut self, opts: &SerializeOpts<'_>, budget: StepBudget) -> MeasureStep {
        let lengths = OptsLen::of(opts);
        // Planted: a changed option length is accepted.
        let unchecked = cfg!(inf_canary_ser_options_unchecked);
        if !unchecked && self.phase.opts_len.is_some_and(|recorded| recorded != lengths) {
            return MeasureStep::Refused(self, EmitError::OptionsChanged);
        }
        if self.phase.counted.total().is_none() {
            return MeasureStep::Refused(self, EmitError::LengthOverflow);
        }
        self.phase.opts_len = Some(lengths);
        let Measure { emitter, root, counted, .. } = &mut self.phase;
        let end = measure_step(emitter, root.source(&self.doc), opts, counted, budget);
        let Some(length) = self.phase.counted.total() else {
            return MeasureStep::Refused(self, EmitError::LengthOverflow);
        };
        if end == EmitStep::Yield {
            return MeasureStep::Yield(self);
        }
        let Measure { mut emitter, root, .. } = self.phase;
        emitter.begin(root.origin, 0);
        MeasureStep::Measured(OwnedEmit {
            phase: Emit { emitter, root, opts_len: lengths, length, emitted: 0 },
            doc: self.doc,
        })
    }

    /// What the check's last step examined.
    #[cfg(any(test, fuzzing))]
    #[doc(hidden)]
    pub fn checked(&self) -> crate::tape::CheckTally {
        self.phase.checked
    }

    /// String source bytes the last measure step examined.
    #[cfg(any(test, fuzzing))]
    #[doc(hidden)]
    pub fn examined(&self) -> usize {
        self.phase.emitter.examined()
    }

    /// A measure whose count stands at `counted`: the overflow row's start.
    #[cfg(test)]
    pub(crate) fn counted_from(mut self, counted: u64) -> Self {
        self.phase.counted = CountingSink::starting_at(counted);
        self
    }
}

impl OwnedEmit<Emit> {
    /// The length the emit pass writes: a RESP bulk's length.
    pub fn length(&self) -> u64 {
        self.phase.length
    }

    /// One step of the emit: into `sink` at most `budget.bytes_max` bytes
    /// and [`EMIT_OVERSHOOT_BYTES_MAX`] more, over at most
    /// `budget.nodes_max` values and one more. `opts` are the measure's:
    /// other lengths are refused, and nothing is written.
    pub fn step(
        &mut self,
        opts: &SerializeOpts<'_>,
        sink: &mut impl EmitSink,
        budget: StepBudget,
    ) -> Result<EmitStep, EmitError> {
        // Planted: a changed option length is accepted.
        let unchecked = cfg!(inf_canary_ser_options_unchecked);
        if !unchecked && self.phase.opts_len != OptsLen::of(opts) {
            return Err(EmitError::OptionsChanged);
        }
        let source = self.phase.root.source(&self.doc);
        let mut counted = Counted { sink, bytes: CountingSink::starting_at(self.phase.emitted) };
        let end = if opts.is_compact() {
            self.phase.emitter.step::<_, false>(source, opts, &mut counted, budget)
        } else {
            self.phase.emitter.step::<_, true>(source, opts, &mut counted, budget)
        };
        let emitted = counted.bytes.total().ok_or(EmitError::LengthMismatch)?;
        self.phase.emitted = emitted;
        let length = self.phase.length;
        if emitted > length || (end == EmitStep::Done && emitted != length) {
            return Err(EmitError::LengthMismatch);
        }
        Ok(end)
    }

    /// String source bytes the last emit step examined.
    #[cfg(any(test, fuzzing))]
    #[doc(hidden)]
    pub fn examined(&self) -> usize {
        self.phase.emitter.examined()
    }
}

/// A sink that also counts what it passes on.
struct Counted<'s, S> {
    sink: &'s mut S,
    bytes: CountingSink,
}

impl<S: EmitSink> EmitSink for Counted<'_, S> {
    #[inline]
    fn put(&mut self, bytes: &[u8]) {
        self.sink.put(bytes);
        self.bytes.put(bytes);
    }

    #[inline]
    fn put_byte(&mut self, byte: u8) {
        self.sink.put_byte(byte);
        self.bytes.put(&[byte]);
    }
}

/// One step of a measure pass: the emitter over a counting sink.
fn measure_step(
    emitter: &mut Emitter,
    source: TapeSource<'_>,
    opts: &SerializeOpts<'_>,
    counted: &mut CountingSink,
    budget: StepBudget,
) -> EmitStep {
    // Planted: the measure skips a break's indentation.
    let skipped = SerializeOpts { indent: &[], ..*opts };
    let counted_opts = if cfg!(inf_canary_ser_measure_skips_separators) { &skipped } else { opts };
    if opts.is_compact() {
        emitter.step::<_, false>(source, counted_opts, counted, budget)
    } else {
        emitter.step::<_, true>(source, counted_opts, counted, budget)
    }
}

/// One step of [`serialize_sliced`], as its caller reads it.
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
#[derive(Debug)]
pub struct SlicedStep<'a> {
    /// The bytes this step wrote.
    pub bytes: &'a [u8],
    /// String source bytes this step examined.
    pub examined: usize,
    pub end: EmitStep,
}

/// A value's root for a sliced run; `None` for a tree-form value, which
/// no step walks.
#[cfg(any(test, fuzzing))]
fn sliced_root<'a>(
    value: crate::DocValue<'a>,
    base_depth: usize,
) -> Option<(Emitter, TapeSource<'a>)> {
    let mut emitter = Emitter::new();
    match Root::of(value) {
        Root::Atom(atom) => {
            emitter.begin(Origin::Atom(atom), 0);
            Some((emitter, TapeSource { region: &[], dict: Dict::empty() }))
        }
        Root::Tape { source, opener } => {
            emitter.root(opener, source, base_depth).ok()?;
            Some((emitter, source))
        }
        Root::Tree(_) => None,
    }
}

/// The harnesses' entry to the core step: one tape value written by steps
/// under the budgets `budgets` hands out, each step shown to `each` by the
/// bytes it wrote. The walk and its source are paired by this one call.
/// `false`, and nothing written, for a value no step walks.
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
pub fn serialize_sliced(
    value: crate::DocValue<'_>,
    opts: &SerializeOpts<'_>,
    base_depth: usize,
    out: &mut Vec<u8>,
    budgets: &mut dyn FnMut() -> StepBudget,
    each: &mut dyn FnMut(&SlicedStep<'_>),
) -> bool {
    let Some((mut emitter, source)) = sliced_root(value, base_depth) else {
        return false;
    };
    loop {
        let budget = budgets();
        let before = out.len();
        let end = if opts.is_compact() {
            emitter.step::<_, false>(source, opts, out, budget)
        } else {
            emitter.step::<_, true>(source, opts, out, budget)
        };
        each(&SlicedStep { bytes: &out[before..], examined: emitter.examined(), end });
        if end == EmitStep::Done {
            return true;
        }
    }
}

/// [`serialize_sliced`] over the counting sink: the length a measure pass
/// learns for one tape value, or `None` for a value no step walks. The
/// run ends once its count passes `length_max`, so a measure that repeats
/// itself answers a wrong length and does not run on.
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
pub fn measure_sliced(
    value: crate::DocValue<'_>,
    opts: &SerializeOpts<'_>,
    base_depth: usize,
    budgets: &mut dyn FnMut() -> StepBudget,
    length_max: u64,
) -> Option<Result<u64, EmitError>> {
    let (mut emitter, source) = sliced_root(value, base_depth)?;
    let mut counted = CountingSink::default();
    loop {
        let end = measure_step(&mut emitter, source, opts, &mut counted, budgets());
        let Some(length) = counted.total() else {
            return Some(Err(EmitError::LengthOverflow));
        };
        if end == EmitStep::Done || length > length_max {
            return Some(Ok(length));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ser_frozen as frozen;
    use crate::{DocValue, JsonParser, TapeDoc};

    const COMPACT: SerializeOpts<'static> = SerializeOpts { indent: b"", newline: b"", space: b"" };
    /// Separators of several bytes, one multi-byte: a step edge falls inside each.
    const FORMATTED: SerializeOpts<'static> =
        SerializeOpts { indent: "→ ".as_bytes(), newline: b"\r\n", space: b"  " };

    fn tape(json: &[u8]) -> Vec<u8> {
        JsonParser::new().parse(json).expect("the fixture parses")
    }

    fn budget(bytes_max: u32, nodes_max: u32) -> StepBudget {
        let nz = |n| NonZeroU32::new(n).expect("a budget is at least 1");
        StepBudget { bytes_max: nz(bytes_max), nodes_max: nz(nodes_max) }
    }

    /// A document's root container, as a walk is rooted at it.
    fn container(doc: &[u8]) -> (TapeSource<'_>, Opener) {
        let root = TapeDoc::from_bytes(doc).expect("the fixture validates").root();
        let Root::Tape { source, opener } = Root::of(DocValue::from(root)) else {
            panic!("the fixture is a tape container");
        };
        (source, opener)
    }

    /// The serializer as it stood, which shares no code with the walk.
    fn frozen_bytes(doc: &[u8], opts: &SerializeOpts<'_>) -> Vec<u8> {
        let root = TapeDoc::from_bytes(doc).expect("the fixture validates").root();
        let mut out = Vec::new();
        frozen::serialize_into(DocValue::from(root), opts, &mut out);
        out
    }

    fn step(
        emitter: &mut Emitter,
        source: TapeSource<'_>,
        opts: &SerializeOpts<'_>,
        out: &mut Vec<u8>,
        budget: StepBudget,
    ) -> EmitStep {
        if opts.is_compact() {
            emitter.step::<_, false>(source, opts, out, budget)
        } else {
            emitter.step::<_, true>(source, opts, out, budget)
        }
    }

    /// The walk to its end, in small steps. `want_len` bounds the run:
    /// every yield writes, so a walk that runs past its value fails here
    /// and never loops.
    fn finish(
        emitter: &mut Emitter,
        source: TapeSource<'_>,
        opts: &SerializeOpts<'_>,
        want_len: usize,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let before = out.len();
            let end = step(emitter, source, opts, &mut out, budget(7, 3));
            assert!(out.len() <= want_len, "the re-rooted walk runs past its value's length");
            if end == EmitStep::Done {
                return out;
            }
            assert!(out.len() > before, "a yield wrote nothing");
        }
    }

    /// `begin` roots a walk again wherever the last one stopped. One step
    /// leaves a walk unfinished at every byte and every value of a
    /// document nested seven deep, under a wrapper two deep; the same
    /// emitter, rooted at another container, must write it as the frozen
    /// serializer does. A parent the old walk left on the stack would be
    /// popped when the new root closes, and the walk would write past it
    /// or read outside its region.
    #[test]
    fn a_walk_left_unfinished_is_rooted_again_at_another_value() {
        let first = br#"{"a":[[1,2,{"b":[3,4,5]}],"x"],"c":{"d":[6,{"e":{"f":[[["deep"]]]}}]}}"#;
        let first = tape(first);
        let targets =
            [tape(br#"[{"k":[7,8]},"y",[[9]]]"#), tape(br#"{"m":{"n":[true,null]},"o":"p"}"#)];
        let (first_source, first_opener) = container(&first);
        let (mut unfinished, mut deepest) = (0usize, 0usize);
        for opts in [COMPACT, FORMATTED] {
            let first_len = frozen_bytes(&first, &opts).len();
            let first_len = u32::try_from(first_len).expect("a short fixture");
            let by_bytes = (1..first_len).map(|bytes_max| (bytes_max, u32::MAX));
            let by_nodes = (1..=32).map(|nodes_max| (u32::MAX, nodes_max));
            for (bytes_max, nodes_max) in by_bytes.chain(by_nodes) {
                for target in &targets {
                    let mut emitter = Emitter::new();
                    emitter.root(first_opener, first_source, 2).expect("a short region");
                    let mut scratch = Vec::new();
                    let first_budget = budget(bytes_max, nodes_max);
                    if step(&mut emitter, first_source, &opts, &mut scratch, first_budget)
                        == EmitStep::Done
                    {
                        continue;
                    }
                    unfinished += 1;
                    deepest = deepest.max(emitter.parents.len());
                    let want = frozen_bytes(target, &opts);
                    let (source, opener) = container(target);
                    emitter.root(opener, source, 0).expect("a short region");
                    let got = finish(&mut emitter, source, &opts, want.len());
                    assert!(got == want, "re-rooted after a step of ({bytes_max}, {nodes_max})");
                }
            }
        }
        assert!(unfinished >= 400, "VACUOUS: {unfinished} walks left unfinished");
        assert!(deepest >= 7, "VACUOUS: parents at most {deepest} deep");
    }
}
