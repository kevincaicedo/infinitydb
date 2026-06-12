//! Pub/sub (M1-E5 · S10/S11): subscription state, the Redis frame
//! vocabulary, and the per-cell registries behind fabric fan-out.
//!
//! Architecture (the milestone §3.2 frozen boundary): a channel is owned by
//! `slot(channel)`'s cell; subscriber state lives on the *subscriber's
//! connection cell*; PUBLISH routes to the owner, which fans **one fabric
//! message per subscriber-bearing cell** — never per subscriber. Pattern
//! subscriptions are held per-cell and their index *replicates* to every
//! cell on local 0→1/1→0 transitions, so any owner can target
//! pattern-bearing cells without a global table (L1). The plane drives all
//! of this (`plane.rs` pub/sub section); this module owns the pieces that
//! are plane-agnostic.
//!
//! Two layers:
//! - **Connection-state ops + frames**: SUBSCRIBE/UNSUBSCRIBE mutate the
//!   [`ConnCx`] subscription vectors and write Redis-exact confirmation
//!   frames. Outside a plane (compat candidate, embedded), PUBLISH/PUBSUB
//!   degenerate to the single-connection view — self-delivery and
//!   own-subscription introspection — which is exactly the observable Redis
//!   behavior for one client on one server, so the oracle diff stays
//!   byte-exact through `execute` alone.
//! - **[`PubSubCell`]**: the per-cell registries — local subscriber lists,
//!   owner-side per-cell subscriber counts, the replicated pattern index —
//!   with incremental byte accounting (L5). All maps are `BTreeMap` and all
//!   lists are insertion-ordered `Vec`s: iteration order is deterministic
//!   (L7 — no hasher randomness may reach output bytes), and these paths
//!   are not the GET/SET hot path.
//!
//! Recorded decision (vs the milestone sketch "WaitList used by
//! (P)SUBSCRIBE delivery"): delivery appends complete frames directly to
//! the subscriber connection's staged output — no parked futures, no
//! per-message wakeups, no allocation per delivery beyond the output bytes.
//! The `WaitList` stays the blocking-read primitive (BLPOP/XREAD, M3/M5).

use std::collections::BTreeMap;

use inf_wire::{CommandId, Protocol, RespWriter};

use crate::exec::ConnCx;
use crate::glob::glob_match;

// ---- subscription kinds --------------------------------------------------------

/// Channel vs pattern subscription (the two Redis registries).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum SubKind {
    Channel,
    Pattern,
}

impl SubKind {
    fn confirm_verb(self) -> &'static [u8] {
        match self {
            SubKind::Channel => b"subscribe",
            SubKind::Pattern => b"psubscribe",
        }
    }

    fn unconfirm_verb(self) -> &'static [u8] {
        match self {
            SubKind::Channel => b"unsubscribe",
            SubKind::Pattern => b"punsubscribe",
        }
    }

    /// One-byte wire tag for the internal `INF.SUBD` delta op.
    pub(crate) fn wire_tag(self) -> &'static [u8] {
        match self {
            SubKind::Channel => b"c",
            SubKind::Pattern => b"p",
        }
    }

    pub(crate) fn from_wire_tag(tag: &[u8]) -> Option<SubKind> {
        match tag {
            b"c" => Some(SubKind::Channel),
            b"p" => Some(SubKind::Pattern),
            _ => None,
        }
    }
}

/// The six public pub/sub commands — the plane defers them all to the pump
/// (registries and delivery are plane state, and subscriber registration
/// must reach the owner cell before the confirmation frame is emitted).
pub(crate) fn is_plane_pubsub(id: CommandId) -> bool {
    matches!(
        id,
        CommandId::Subscribe
            | CommandId::Unsubscribe
            | CommandId::Psubscribe
            | CommandId::Punsubscribe
            | CommandId::Publish
            | CommandId::Pubsub
    )
}

// ---- RESP2 subscriber-mode restriction ------------------------------------------

/// In RESP2, a subscribed connection may only run the subscribe family,
/// PING, QUIT, and RESET (Redis `processCommand`). RESP3 lifts this.
pub(crate) fn subscriber_restricted(cx: &ConnCx) -> bool {
    cx.proto == Protocol::Resp2 && !(cx.sub_channels.is_empty() && cx.sub_patterns.is_empty())
}

/// QUIT/RESET are not in the InfinityDB surface yet (they would resolve as
/// unknown commands before this check), so the allowed set is the subscribe
/// family + PING.
pub(crate) fn allowed_in_subscriber_mode(id: CommandId) -> bool {
    matches!(
        id,
        CommandId::Subscribe
            | CommandId::Unsubscribe
            | CommandId::Psubscribe
            | CommandId::Punsubscribe
            | CommandId::Ping
    )
}

/// Redis 8 subscriber-context rejection, byte-exact (oracle-pinned):
/// container commands report their full name (`pubsub|channels`).
pub(crate) fn restricted_error(
    id: CommandId,
    name: &str,
    sub: Option<&[u8]>,
    w: &mut RespWriter<'_>,
) {
    let container = matches!(
        id,
        CommandId::Pubsub
            | CommandId::Config
            | CommandId::Client
            | CommandId::Object
            | CommandId::Command
    );
    let display = match sub {
        Some(sub) if container => format!(
            "{}|{}",
            name.to_ascii_lowercase(),
            String::from_utf8_lossy(sub).to_ascii_lowercase()
        ),
        _ => name.to_ascii_lowercase(),
    };
    w.error(&format!(
        "ERR Can't execute '{display}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context"
    ));
}

/// Subscriber-mode RESP2 PING reply: a 2-element `[pong, <arg|"">]` frame
/// (Redis `pingCommand` under `CLIENT_PUBSUB`).
pub(crate) fn subscriber_ping(arg: Option<&[u8]>, proto: Protocol, out: &mut Vec<u8>) {
    let mut w = RespWriter::new(out, proto);
    w.push_header(2);
    w.bulk(b"pong");
    w.bulk(arg.unwrap_or(b""));
}

// ---- connection-state ops + confirmation frames ---------------------------------

/// One subscription mutation: `(name, state_changed)`. `state_changed` is
/// false for re-subscribes and not-subscribed unsubscribes (Redis still
/// emits the confirmation frame; only real transitions touch registries).
pub(crate) type SubChange = (Vec<u8>, bool);

/// Applies SUBSCRIBE/PSUBSCRIBE to the connection state and writes one
/// confirmation frame per name (Redis shape: `[verb, name, count]` where
/// count is channels + patterns after the op).
pub(crate) fn apply_subscribe(
    names: &[&[u8]],
    kind: SubKind,
    cx: &mut ConnCx,
    out: &mut Vec<u8>,
) -> Vec<SubChange> {
    let mut changes = Vec::with_capacity(names.len());
    for &name in names {
        let list = match kind {
            SubKind::Channel => &mut cx.sub_channels,
            SubKind::Pattern => &mut cx.sub_patterns,
        };
        let added = if list.iter().any(|n| n == name) {
            false
        } else {
            list.push(name.to_vec());
            true
        };
        let count = (cx.sub_channels.len() + cx.sub_patterns.len()) as i64;
        let mut w = RespWriter::new(out, cx.proto);
        w.push_header(3);
        w.bulk(kind.confirm_verb());
        w.bulk(name);
        w.int(count);
        changes.push((name.to_vec(), added));
    }
    changes
}

/// Applies UNSUBSCRIBE/PUNSUBSCRIBE. `names = None` is the bare form: every
/// current subscription of that kind, in subscription order (deterministic
/// — Redis iterates a dict here; recorded deviation when orders differ).
/// With nothing to drop, Redis emits a single `[verb, nil, count]` frame.
pub(crate) fn apply_unsubscribe(
    names: Option<&[&[u8]]>,
    kind: SubKind,
    cx: &mut ConnCx,
    out: &mut Vec<u8>,
) -> Vec<SubChange> {
    let targets: Vec<Vec<u8>> = match names {
        Some(names) => names.iter().map(|n| n.to_vec()).collect(),
        None => match kind {
            SubKind::Channel => cx.sub_channels.clone(),
            SubKind::Pattern => cx.sub_patterns.clone(),
        },
    };
    if targets.is_empty() {
        let count = (cx.sub_channels.len() + cx.sub_patterns.len()) as i64;
        let mut w = RespWriter::new(out, cx.proto);
        w.push_header(3);
        w.bulk(kind.unconfirm_verb());
        w.null();
        w.int(count);
        return Vec::new();
    }
    let mut changes = Vec::with_capacity(targets.len());
    for name in targets {
        let list = match kind {
            SubKind::Channel => &mut cx.sub_channels,
            SubKind::Pattern => &mut cx.sub_patterns,
        };
        let removed = match list.iter().position(|n| *n == name) {
            Some(at) => {
                list.remove(at);
                true
            }
            None => false,
        };
        let count = (cx.sub_channels.len() + cx.sub_patterns.len()) as i64;
        let mut w = RespWriter::new(out, cx.proto);
        w.push_header(3);
        w.bulk(kind.unconfirm_verb());
        w.bulk(&name);
        w.int(count);
        changes.push((name, removed));
    }
    changes
}

// ---- message frames --------------------------------------------------------------

/// `[message, channel, payload]` delivery frame (push in RESP3).
pub(crate) fn write_message(out: &mut Vec<u8>, proto: Protocol, channel: &[u8], payload: &[u8]) {
    let mut w = RespWriter::new(out, proto);
    w.push_header(3);
    w.bulk(b"message");
    w.bulk(channel);
    w.bulk(payload);
}

/// `[pmessage, pattern, channel, payload]` delivery frame (push in RESP3).
pub(crate) fn write_pmessage(
    out: &mut Vec<u8>,
    proto: Protocol,
    pattern: &[u8],
    channel: &[u8],
    payload: &[u8],
) {
    let mut w = RespWriter::new(out, proto);
    w.push_header(4);
    w.bulk(b"pmessage");
    w.bulk(pattern);
    w.bulk(channel);
    w.bulk(payload);
}

// ---- exec-layer fallbacks (no plane: compat candidate, embedded) -----------------

/// PUBLISH without a plane: the only reachable subscriber is the publishing
/// connection itself (RESP3 — RESP2 subscribers cannot PUBLISH). The
/// receiver-count reply precedes the push frames, and channel delivery
/// precedes pattern delivery — the Redis order, oracle-pinned (Redis
/// delivers to the publishing client after its command reply).
pub(crate) fn publish_fallback(channel: &[u8], payload: &[u8], cx: &mut ConnCx, out: &mut Vec<u8>) {
    let on_channel = cx.sub_channels.iter().any(|c| c == channel);
    let patterns: Vec<&Vec<u8>> =
        cx.sub_patterns.iter().filter(|p| glob_match(p, channel, false)).collect();
    let receivers = i64::from(on_channel) + patterns.len() as i64;
    RespWriter::new(out, cx.proto).int(receivers);
    if on_channel {
        write_message(out, cx.proto, channel, payload);
    }
    for pattern in patterns {
        write_pmessage(out, cx.proto, pattern, channel, payload);
    }
}

/// PUBSUB without a plane: the single-connection view (CHANNELS = own
/// channels, NUMSUB = 0/1, NUMPAT = own pattern count) — byte-equal to a
/// one-client Redis. The plane path answers from the cell registries.
pub(crate) fn pubsub_fallback(args: &[&[u8]], cx: &mut ConnCx, out: &mut Vec<u8>) {
    let mut w = RespWriter::new(out, cx.proto);
    let sub = args[0];
    if sub.eq_ignore_ascii_case(b"CHANNELS") && args.len() <= 2 {
        let pattern = args.get(1).copied();
        let hits: Vec<&Vec<u8>> = cx
            .sub_channels
            .iter()
            .filter(|c| pattern.is_none_or(|p| glob_match(p, c, false)))
            .collect();
        w.array_header(hits.len());
        for c in hits {
            w.bulk(c);
        }
    } else if sub.eq_ignore_ascii_case(b"NUMSUB") {
        w.array_header((args.len() - 1) * 2);
        for name in &args[1..] {
            w.bulk(name);
            w.int(i64::from(cx.sub_channels.iter().any(|c| c == name)));
        }
    } else if sub.eq_ignore_ascii_case(b"NUMPAT") && args.len() == 1 {
        w.int(cx.sub_patterns.len() as i64);
    } else {
        pubsub_subcommand_error(sub, &mut w);
    }
}

/// Redis 8 PUBSUB unknown-subcommand format (oracle-pinned).
pub(crate) fn pubsub_subcommand_error(sub: &[u8], w: &mut RespWriter<'_>) {
    w.error(&format!(
        "ERR Unknown PUBSUB subcommand or wrong number of arguments for '{}'",
        String::from_utf8_lossy(sub)
    ));
}

// ---- per-cell registries (plane state) --------------------------------------------

/// Estimated heap cost of one registry entry beyond its key bytes (BTree
/// node share + Vec header). The pub/sub attribution is an *estimate* —
/// maintained incrementally at mutation sites, never recomputed by walking
/// the maps on the hot MAINTAIN path.
const ENTRY_OVERHEAD: usize = 64;
/// Per-subscriber / per-cell-counter slot cost inside an entry's vectors.
const SLOT_OVERHEAD: usize = 8;

/// One cell's pub/sub registries, generic over the plane's connection key.
///
/// - `chan_conns` / `pat_conns`: this cell's local subscribers (the
///   subscriber-side state — delivery targets).
/// - `owned`: owner-side per-cell subscriber counts for channels this cell
///   owns (PUBLISH fan-out targeting + receiver counting).
/// - `patterns`: the replicated pattern index — every cell holds every live
///   pattern's per-cell counts, so any owner targets pattern-bearing cells
///   locally. Pattern transitions are rare; PUBLISH is the hot direction.
#[derive(Debug)]
pub(crate) struct PubSubCell<K: Copy + Eq> {
    cells: usize,
    chan_conns: BTreeMap<Vec<u8>, Vec<K>>,
    pat_conns: BTreeMap<Vec<u8>, Vec<K>>,
    owned: BTreeMap<Vec<u8>, Vec<u32>>,
    patterns: BTreeMap<Vec<u8>, Vec<u32>>,
    bytes: usize,
}

impl<K: Copy + Eq> PubSubCell<K> {
    pub(crate) fn new(cells: u16) -> PubSubCell<K> {
        PubSubCell {
            cells: usize::from(cells.max(1)),
            chan_conns: BTreeMap::new(),
            pat_conns: BTreeMap::new(),
            owned: BTreeMap::new(),
            patterns: BTreeMap::new(),
            bytes: 0,
        }
    }

    fn local_map(&mut self, kind: SubKind) -> &mut BTreeMap<Vec<u8>, Vec<K>> {
        match kind {
            SubKind::Channel => &mut self.chan_conns,
            SubKind::Pattern => &mut self.pat_conns,
        }
    }

    /// Registers a local subscriber. Returns true on this cell's 0→1
    /// transition for `name` (the owner/scatter notification trigger).
    pub(crate) fn local_add(&mut self, kind: SubKind, name: &[u8], conn: K) -> bool {
        let mut added_entry = false;
        let list = self.local_map(kind).entry(name.to_vec()).or_insert_with(|| {
            added_entry = true;
            Vec::new()
        });
        let first = list.is_empty();
        debug_assert!(!list.contains(&conn), "caller dedups via ConnCx state");
        list.push(conn);
        self.bytes += SLOT_OVERHEAD + if added_entry { name.len() + ENTRY_OVERHEAD } else { 0 };
        first
    }

    /// Removes a local subscriber. Returns true on the 1→0 transition.
    pub(crate) fn local_remove(&mut self, kind: SubKind, name: &[u8], conn: K) -> bool {
        let mut freed = 0usize;
        let mut emptied = false;
        {
            let map = self.local_map(kind);
            let Some(list) = map.get_mut(name) else { return false };
            let Some(at) = list.iter().position(|k| *k == conn) else { return false };
            list.remove(at);
            freed += SLOT_OVERHEAD;
            if list.is_empty() {
                map.remove(name);
                freed += name.len() + ENTRY_OVERHEAD;
                emptied = true;
            }
        }
        self.bytes = self.bytes.saturating_sub(freed);
        emptied
    }

    /// Local subscribers of one channel (delivery order = subscription
    /// order). Cloned out so delivery can borrow connections freely.
    pub(crate) fn channel_conns(&self, channel: &[u8]) -> Vec<K> {
        self.chan_conns.get(channel).cloned().unwrap_or_default()
    }

    /// Local pattern subscriptions matching `channel`, in pattern
    /// (BTreeMap) order — deterministic delivery order (L7).
    pub(crate) fn matching_pattern_conns(&self, channel: &[u8]) -> Vec<(Vec<u8>, Vec<K>)> {
        self.pat_conns
            .iter()
            .filter(|(pat, _)| glob_match(pat, channel, false))
            .map(|(pat, conns)| (pat.clone(), conns.clone()))
            .collect()
    }

    /// Applies an `INF.SUBD` count delta from `cell` (or a local
    /// transition when this cell is the owner / for its own pattern slot).
    pub(crate) fn apply_delta(&mut self, kind: SubKind, name: &[u8], cell: u16, delta: i32) {
        let cells = self.cells;
        let map = match kind {
            SubKind::Channel => &mut self.owned,
            SubKind::Pattern => &mut self.patterns,
        };
        let mut added_entry = false;
        let counts = map.entry(name.to_vec()).or_insert_with(|| {
            added_entry = true;
            vec![0u32; cells]
        });
        if added_entry {
            self.bytes += name.len() + ENTRY_OVERHEAD + 4 * cells;
        }
        let slot = &mut counts[usize::from(cell) % cells];
        *slot = slot.saturating_add_signed(delta);
        if counts.iter().all(|&c| c == 0) {
            map.remove(name);
            self.bytes = self.bytes.saturating_sub(name.len() + ENTRY_OVERHEAD + 4 * cells);
        }
    }

    /// PUBLISH fan-out targets at the owner: cells (excluding `this_cell`)
    /// with channel subscribers or a pattern matching `channel`.
    pub(crate) fn fan_targets(&self, channel: &[u8], this_cell: u16) -> Vec<u16> {
        let mut bearing = vec![false; self.cells];
        if let Some(counts) = self.owned.get(channel) {
            for (cell, &count) in counts.iter().enumerate() {
                bearing[cell] |= count > 0;
            }
        }
        for (pattern, counts) in &self.patterns {
            if glob_match(pattern, channel, false) {
                for (cell, &count) in counts.iter().enumerate() {
                    bearing[cell] |= count > 0;
                }
            }
        }
        bearing
            .iter()
            .enumerate()
            .filter(|&(cell, &b)| b && cell != usize::from(this_cell))
            .map(|(cell, _)| cell as u16)
            .collect()
    }

    /// Owner-side total subscribers of one channel (PUBSUB NUMSUB).
    pub(crate) fn owned_count(&self, channel: &[u8]) -> i64 {
        self.owned.get(channel).map_or(0, |counts| counts.iter().map(|&c| i64::from(c)).sum())
    }

    /// Channels this cell owns that have ≥ 1 subscriber anywhere,
    /// optionally pattern-filtered (PUBSUB CHANNELS), in sorted order.
    pub(crate) fn live_owned_channels(&self, pattern: Option<&[u8]>) -> Vec<Vec<u8>> {
        self.owned
            .iter()
            .filter(|(name, counts)| {
                counts.iter().any(|&c| c > 0) && pattern.is_none_or(|p| glob_match(p, name, false))
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Distinct live patterns node-wide (PUBSUB NUMPAT — the index is
    /// replicated, so every cell answers locally).
    pub(crate) fn live_pattern_count(&self) -> u64 {
        self.patterns.values().filter(|counts| counts.iter().any(|&c| c > 0)).count() as u64
    }

    /// Channels this cell owns with ≥ 1 subscriber (INFO `pubsub_channels`,
    /// per-cell scope like every INFO gauge).
    pub(crate) fn live_owned_channel_count(&self) -> u64 {
        self.owned.values().filter(|counts| counts.iter().any(|&c| c > 0)).count() as u64
    }

    /// Estimated registry bytes (L5 attribution — see the constants note).
    pub(crate) fn state_bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx(proto: Protocol) -> ConnCx {
        ConnCx { proto, ..ConnCx::default() }
    }

    #[test]
    fn subscribe_frames_and_counts_are_redis_shaped() {
        let mut cx = cx(Protocol::Resp2);
        let mut out = Vec::new();
        let changes = apply_subscribe(&[b"a", b"b"], SubKind::Channel, &mut cx, &mut out);
        assert_eq!(
            out,
            b"*3\r\n$9\r\nsubscribe\r\n$1\r\na\r\n:1\r\n*3\r\n$9\r\nsubscribe\r\n$1\r\nb\r\n:2\r\n"
        );
        assert_eq!(changes, vec![(b"a".to_vec(), true), (b"b".to_vec(), true)]);
        // Re-subscribe: frame still emitted, no state change.
        let mut out = Vec::new();
        let changes = apply_subscribe(&[b"a"], SubKind::Channel, &mut cx, &mut out);
        assert_eq!(out, b"*3\r\n$9\r\nsubscribe\r\n$1\r\na\r\n:2\r\n");
        assert_eq!(changes, vec![(b"a".to_vec(), false)]);
        // Patterns join the same count.
        let mut out = Vec::new();
        apply_subscribe(&[b"n.*"], SubKind::Pattern, &mut cx, &mut out);
        assert_eq!(out, b"*3\r\n$10\r\npsubscribe\r\n$3\r\nn.*\r\n:3\r\n");
    }

    #[test]
    fn resp3_confirmations_are_push_frames() {
        let mut cx = cx(Protocol::Resp3);
        let mut out = Vec::new();
        apply_subscribe(&[b"a"], SubKind::Channel, &mut cx, &mut out);
        assert_eq!(out, b">3\r\n$9\r\nsubscribe\r\n$1\r\na\r\n:1\r\n");
    }

    #[test]
    fn unsubscribe_bare_with_nothing_emits_one_nil_frame() {
        let mut cx = cx(Protocol::Resp2);
        let mut out = Vec::new();
        let changes = apply_unsubscribe(None, SubKind::Channel, &mut cx, &mut out);
        assert!(changes.is_empty());
        assert_eq!(out, b"*3\r\n$11\r\nunsubscribe\r\n$-1\r\n:0\r\n");
    }

    #[test]
    fn unsubscribe_bare_drops_in_subscription_order() {
        let mut cx = cx(Protocol::Resp2);
        let mut out = Vec::new();
        apply_subscribe(&[b"z", b"a"], SubKind::Channel, &mut cx, &mut out);
        let mut out = Vec::new();
        let changes = apply_unsubscribe(None, SubKind::Channel, &mut cx, &mut out);
        assert_eq!(changes, vec![(b"z".to_vec(), true), (b"a".to_vec(), true)]);
        assert!(cx.sub_channels.is_empty());
    }

    #[test]
    fn publish_fallback_replies_then_self_delivers_channel_before_pattern() {
        let mut cx = cx(Protocol::Resp3);
        let mut out = Vec::new();
        apply_subscribe(&[b"news.tech"], SubKind::Channel, &mut cx, &mut out);
        apply_subscribe(&[b"news.*"], SubKind::Pattern, &mut cx, &mut out);
        let mut out = Vec::new();
        publish_fallback(b"news.tech", b"hi", &mut cx, &mut out);
        // Oracle-pinned order: the receiver count precedes the push frames.
        let want = b":2\r\n>3\r\n$7\r\nmessage\r\n$9\r\nnews.tech\r\n$2\r\nhi\r\n\
                     >4\r\n$8\r\npmessage\r\n$6\r\nnews.*\r\n$9\r\nnews.tech\r\n$2\r\nhi\r\n";
        assert_eq!(out, &want[..]);
    }

    #[test]
    fn registry_transitions_and_targets() {
        let mut ps: PubSubCell<u32> = PubSubCell::new(4);
        // Two local subscribers: only the first flips the cell transition.
        assert!(ps.local_add(SubKind::Channel, b"ch", 1));
        assert!(!ps.local_add(SubKind::Channel, b"ch", 2));
        assert!(!ps.local_remove(SubKind::Channel, b"ch", 1));
        assert!(ps.local_remove(SubKind::Channel, b"ch", 2));
        assert_eq!(ps.state_bytes(), 0, "exact unwind");
        // Owner counts drive fan targets; the owner cell itself is excluded.
        ps.apply_delta(SubKind::Channel, b"ch", 1, 1);
        ps.apply_delta(SubKind::Channel, b"ch", 3, 1);
        assert_eq!(ps.fan_targets(b"ch", 3), vec![1]);
        assert_eq!(ps.owned_count(b"ch"), 2);
        // Pattern-bearing cells join the target set for matching channels.
        ps.apply_delta(SubKind::Pattern, b"c*", 2, 1);
        assert_eq!(ps.fan_targets(b"ch", 3), vec![1, 2]);
        assert_eq!(ps.fan_targets(b"other", 3), vec![]);
        assert_eq!(ps.live_pattern_count(), 1);
        // Deltas unwind exactly.
        ps.apply_delta(SubKind::Channel, b"ch", 1, -1);
        ps.apply_delta(SubKind::Channel, b"ch", 3, -1);
        ps.apply_delta(SubKind::Pattern, b"c*", 2, -1);
        assert_eq!(ps.owned_count(b"ch"), 0);
        assert_eq!(ps.live_pattern_count(), 0);
        assert_eq!(ps.state_bytes(), 0);
    }

    #[test]
    fn live_owned_channels_filter_and_sort() {
        let mut ps: PubSubCell<u32> = PubSubCell::new(2);
        ps.apply_delta(SubKind::Channel, b"beta", 0, 1);
        ps.apply_delta(SubKind::Channel, b"alpha", 1, 1);
        ps.apply_delta(SubKind::Channel, b"news.x", 0, 1);
        assert_eq!(
            ps.live_owned_channels(None),
            vec![b"alpha".to_vec(), b"beta".to_vec(), b"news.x".to_vec()]
        );
        assert_eq!(ps.live_owned_channels(Some(b"news.*")), vec![b"news.x".to_vec()]);
    }
}
