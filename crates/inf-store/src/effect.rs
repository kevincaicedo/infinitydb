//! Mutation effects emitted by the store for the M2 log spine.
//!
//! The store owns key/value semantics, so it is the right layer to say what
//! changed. It deliberately does not know log record tags, frame layouts, or
//! file durability. The cell owner adapts these effects into `inf-log`.

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MutationEffect<'a> {
    StringPostImage { key: &'a [u8], value: &'a [u8], expire_at_ms: Option<u64>, raw: bool },
    Delete { key: &'a [u8] },
    ExpireAt { key: &'a [u8], expire_at_ms: Option<u64> },
}

pub trait MutationSink {
    fn push(&mut self, effect: MutationEffect<'_>);
}

#[derive(Copy, Clone, Default, Debug)]
pub struct NoMutationSink;

impl MutationSink for NoMutationSink {
    #[inline]
    fn push(&mut self, _effect: MutationEffect<'_>) {}
}
