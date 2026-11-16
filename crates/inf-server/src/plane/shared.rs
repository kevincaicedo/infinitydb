//! `Shared` — the cell state every pump future holds an `Rc` to: store,
//! tier, durable cell, pools, tripwires, and the owned-command execution
//! path (`execute_owned_into`) with its durability staging and admission.

use core::convert::Infallible;

use super::*;

// ---- shared cell state (futures hold an Rc) -----------------------------------

/// One armed maintenance bracket (M4.5-S04, ADR-0076 D3): the write-set
/// keys plus the optional mutation path the prune consumes. The `NsId`
/// variant carries the resolved numbered-db namespace.
#[cfg(feature = "doc")]
type ArmedDbBracket<'a> = (NsId, Vec<&'a [u8]>, Option<inf_doc::PathProgram>);
#[cfg(feature = "doc")]
type ArmedNsBracket<'a> = (Vec<&'a [u8]>, Option<inf_doc::PathProgram>);

impl<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static> Shared<O, F> {
    /// Retires an adopt ack (ADR-0128): `true` when `token` was one of
    /// this cell's hand-offs — its credit came back with the drain and
    /// no gate waits on it.
    pub(super) fn retire_adopt_ack(&self, token: u64) -> bool {
        let mut acks = self.adopt_acks.borrow_mut();
        match acks.iter().position(|t| *t == token) {
            Some(i) => {
                acks.swap_remove(i);
                true
            }
            None => false,
        }
    }

    /// The gate an apply from `origin` runs under ([`ApplyGate::open`]): its
    /// slot opened before its first effect when its reading gates its
    /// verdict on this cell's watermark (ADR-0177 D6), `Err` at a full deque.
    pub(super) fn gate_open(
        &self,
        origin: CellId,
        read: NsApplyRead,
    ) -> Result<ApplyGate<GatedTicket>, ParkedFull> {
        ApplyGate::open(&self.gated, origin, read)
    }

    /// The divert, ADR-0083 D1's one statement: an apply from `from` in `ns`
    /// joins `from`'s FIFO pump instead of running inline when `ns` is tiered
    /// (it can suspend on a cold read, M4-S26), or durable while the pump is
    /// applying or holds `from`'s work. Nothing overtakes an apply in
    /// progress, so an origin has one open gated slot, its back (ADR-0177
    /// D6), which the gated deque's `second-open-slot` check holds.
    pub(super) fn diverts_to_pump(&self, from: CellId, ns: NsId) -> bool {
        let store = self.store.borrow();
        // The origin's pump holds its order while it is not parked.
        #[cfg(not(inf_canary_divert_queue_only))]
        let behind = !self.ns_pump_parked.borrow()[usize::from(from.0)]
            || !self.ns_applies.borrow().is_empty_for(from);
        // Canary: the queue alone diverts, so a flat durable apply runs
        // inline while its origin's pump holds an apply on a cold read.
        #[cfg(inf_canary_divert_queue_only)]
        let behind = !self.ns_applies.borrow().is_empty_for(from);
        store.is_tiered(ns) || (store.ns_fsync_class(ns).is_some() && behind)
    }

    pub(super) fn with_conn<R>(&self, key: ConnKey, f: impl FnOnce(&mut Conn) -> R) -> Option<R> {
        self.conns.borrow_mut().get_mut(key).map(f)
    }

    /// PARSE completed a whole take of the inbox: the count moves after
    /// the take's last entry, so a pump polled inside PARSE's loop reads it
    /// unmoved and waits (ADR-0196 D2). The pumps waiting on it wake; with
    /// none waiting a take pays one add, one load and one branch.
    pub(super) fn note_inbox_take(&self) {
        self.inbox_takes.set(self.inbox_takes.get().wrapping_add(1));
        if self.reader_waiting.get() > 0 {
            self.reader_waiters.wake_all(0);
        }
    }

    /// Executes owned argv locally (queued and remote-`Apply` paths),
    /// appending the reply to `out` (callers reuse scratch buffers — the
    /// owner side of a remote `Apply` is zero-allocation, M0-E8), and
    /// reports the apply point. Returns whether the command asked for the
    /// connection to close after its reply (`QUIT`) — the pump's client
    /// sites hand it to [`close_after_reply`]; fabric callers never see
    /// `QUIT` (it addresses no key).
    #[allow(clippy::too_many_arguments)] // internal execution funnel
    pub(super) fn execute_owned_into(
        &self,
        origin: ExecOrigin,
        argv: &[&[u8]],
        proto: Protocol,
        id: u64,
        db: u16,
        ns: Option<NsId>,
        program: bool,
        out: &mut ReplyBuf,
    ) -> bool {
        let mut cx = ConnCx {
            proto,
            id,
            db,
            ns: ns.map_or(ConnNamespace::Default, ConnNamespace::Named),
            sub_channels: Vec::new(),
            sub_patterns: Vec::new(),
            node: Rc::clone(&self.node),
            close_requested: Cell::new(false),
            program,
        };
        let now = self.now.get();
        #[cfg(feature = "doc")]
        let capture_doc_log = lookup(argv[0])
            .is_some_and(|meta| crate::json::is_json_write(meta.id))
            && self.node.doc_log_admission.get().is_some();
        #[cfg(feature = "doc")]
        {
            if capture_doc_log {
                self.node.doc_log.borrow_mut().clear();
            } else {
                self.node.doc_log_admission.set(None);
            }
        }
        // Numbered-db maintenance bracket (M4.5-S04, ADR-0076 D3 row 3):
        // every numbered-db write funnels through this function, so one
        // attachment covers the mirror, the MSET legs, and the fabric
        // apply structurally. Named-ns calls (`Some(ns)`) are bracketed
        // at their two plane sites, where the commit half must follow
        // effect staging. Zero-index namespaces pay one guard branch.
        #[cfg(feature = "doc")]
        let bracket: Option<ArmedDbBracket<'_>> = if ns.is_none() {
            let target = NsId(u32::from(db));
            if self.store.borrow().ns_indexed(target) {
                match lookup(argv[0]) {
                    // COPY is store-mini-bracketed (ADR-0076 D3): its
                    // destination may live in another database.
                    Some(meta)
                        if meta.flags.contains(CmdFlags::WRITE) && meta.id != CommandId::Copy =>
                    {
                        let keys = extract_keys_slices(meta, argv);
                        if keys.is_empty() {
                            None
                        } else {
                            let path = self.json_mutation_path(meta.id, argv);
                            Some((target, keys, path))
                        }
                    }
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        #[cfg(feature = "doc")]
        if let Some((target, keys, path)) = &bracket
            && let Err(refusal) =
                self.store.borrow_mut().idx_bracket_begin(*target, keys, path.as_ref())
        {
            // Typed refusal before anything changed (ADR-0072 D7.1).
            RespWriter::new(out, proto).error(refusal.message());
            self.node.doc_log_admission.set(None);
            self.observer.borrow_mut().on_execute(
                self.cell,
                origin,
                ExecScope::of(&cx),
                argv,
                out.bytes(),
                now,
            );
            return false;
        }
        let scope = ExecScope::of(&cx);
        execute_slices(argv, &mut self.store.borrow_mut(), &mut cx, now, out);
        #[cfg(feature = "doc")]
        if let Some((target, keys, _)) = &bracket {
            // Numbered dbs never stage — the commit half attaches at the
            // same boundary with the staging call absent (ADR-0072 D3).
            self.store.borrow_mut().idx_bracket_commit(*target, keys);
        }
        #[cfg(feature = "doc")]
        self.node.doc_log_admission.set(None);
        self.observer.borrow_mut().on_execute(self.cell, origin, scope, argv, out.bytes(), now);
        cx.close_requested.get()
    }

    /// The mutation's path program for the S04 static path-overlap prune
    /// (ADR-0076 D6): only unambiguous path-carrying JSON writes yield
    /// one — everything else evaluates in full. A path that fails to
    /// compile never prunes (the command will fail with its own error).
    #[cfg(feature = "doc")]
    #[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column json_path")]
    pub(super) fn json_mutation_path(
        &self,
        id: CommandId,
        argv: &[&[u8]],
    ) -> Option<inf_doc::PathProgram> {
        let text: &[u8] = match id {
            CommandId::JsonSet if argv.len() >= 4 => argv[2],
            CommandId::JsonNumIncrBy | CommandId::JsonNumMultBy if argv.len() >= 4 => argv[2],
            CommandId::JsonDel | CommandId::JsonForget if argv.len() >= 3 => argv[2],
            CommandId::JsonToggle | CommandId::JsonClear | CommandId::JsonArrPop
                if argv.len() >= 3 =>
            {
                argv[2]
            }
            CommandId::JsonArrAppend | CommandId::JsonArrInsert | CommandId::JsonArrTrim
                if argv.len() >= 4 =>
            {
                argv[2]
            }
            CommandId::JsonMerge if argv.len() >= 4 => argv[2],
            // With three args the trailing one is the value (legacy root
            // path) — ambiguous positions never prune.
            CommandId::JsonStrAppend if argv.len() == 4 => argv[2],
            _ => return None,
        };
        let max_path_bytes = self.store.borrow().db(0)?.doc_max_path_bytes();
        let mut cache = self.node.path_cache.borrow_mut();
        cache.get_or_compile(text, max_path_bytes).ok().cloned()
    }

    /// An empty reply buffer, recycled when possible.
    pub(super) fn take_reply_buf(&self) -> Vec<u8> {
        let mut buf = self.reply_pool.borrow_mut().pop().unwrap_or_default();
        let cap = buf.capacity() as u64;
        debug_assert!(self.reply_pool_bytes.get() >= cap, "pool byte sum tracks contents");
        self.reply_pool_bytes.set(self.reply_pool_bytes.get() - cap);
        buf.clear();
        buf
    }

    /// Returns a reply buffer to the pool (bounded; oversized buffers drop).
    pub(super) fn recycle_reply_buf(&self, buf: Vec<u8>) {
        if buf.capacity() == 0 || buf.capacity() > REPLY_POOL_BUF_CAP {
            return;
        }
        let mut pool = self.reply_pool.borrow_mut();
        if pool.len() < REPLY_POOL_MAX {
            self.reply_pool_bytes.set(self.reply_pool_bytes.get() + buf.capacity() as u64);
            pool.push(buf);
        }
    }

    /// An empty `OwnedCmd` flat buffer, recycled when possible.
    pub(super) fn take_cmd_buf(&self) -> Vec<u8> {
        let buf = self.cmd_pool.borrow_mut().pop().unwrap_or_default();
        let cap = buf.capacity() as u64;
        debug_assert!(self.cmd_pool_bytes.get() >= cap, "pool byte sum tracks contents");
        self.cmd_pool_bytes.set(self.cmd_pool_bytes.get() - cap);
        buf
    }

    /// Returns an `OwnedCmd` buffer to the pool (bounded; oversized drop).
    pub(super) fn recycle_cmd_buf(&self, buf: Vec<u8>) {
        if buf.capacity() == 0 || buf.capacity() > CMD_POOL_BUF_CAP {
            return;
        }
        let mut pool = self.cmd_pool.borrow_mut();
        if pool.len() < CMD_POOL_MAX {
            self.cmd_pool_bytes.set(self.cmd_pool_bytes.get() + buf.capacity() as u64);
            pool.push(buf);
        }
    }

    /// Typed single-key DEL/UNLINK/EXISTS/TOUCH apply (local or owner side):
    /// the reply is the integer count contribution; observer sees the
    /// synthesized single-key command with its `:N` reply.
    pub(super) fn apply_counted(
        &self,
        origin: ExecOrigin,
        name: &[u8],
        key: &[u8],
        db: u16,
    ) -> i64 {
        let now = self.now.get();
        let del = name.eq_ignore_ascii_case(b"DEL") || name.eq_ignore_ascii_case(b"UNLINK");
        let hit = {
            let mut ks = self.store.borrow_mut();
            let store = ks.db_mut(usize::from(db));
            if del { store.del(key, now) } else { store.exists(key, now) }
        };
        let mut reply = OpenReply::new(Vec::new());
        RespWriter::new(reply.reply(), Protocol::Resp2).int(i64::from(hit));
        self.observer.borrow_mut().on_execute(
            self.cell,
            origin,
            ExecScope::Db(db),
            &[name, key],
            reply.bytes(),
            now,
        );
        i64::from(hit)
    }

    /// Typed DBSIZE apply (scatter contribution, M1-S02; per selected db).
    pub(super) fn apply_dbsize(&self, origin: ExecOrigin, db: u16) -> i64 {
        let now = self.now.get();
        let len = self.store.borrow_mut().db_mut(usize::from(db)).len() as i64;
        let mut reply = OpenReply::new(Vec::new());
        RespWriter::new(reply.reply(), Protocol::Resp2).int(len);
        self.observer.borrow_mut().on_execute(
            self.cell,
            origin,
            ExecScope::Db(db),
            &[b"DBSIZE"],
            reply.bytes(),
            now,
        );
        len
    }

    // ---- durable-namespace execution (M2-S08, ADR-0015 D5/D6) ----

    /// The wall anchor for `ExpireAt` record conversion (L7-injected).
    pub(super) fn wall_anchor(&self) -> WallAnchor {
        let (internal_ms, unix_ms) = self.node.wall_anchor.get();
        WallAnchor { internal_ms, unix_ms }
    }

    /// Conservative staging-bytes estimate for one command's effects:
    /// per write key, the key + the current post-image + record overhead,
    /// plus every argument byte (covers APPEND/SETRANGE growth). Checked
    /// *before* execution so a mutation is never applied unlogged.
    #[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column effect_bytes")]
    fn estimate_effect_bytes(
        &self,
        ns: NsId,
        meta: &'static inf_wire::CommandMeta,
        argv: &[&[u8]],
    ) -> usize {
        let ks = self.store.borrow();
        let now = self.now.get();
        let arg_bytes: usize = argv.iter().map(|a| a.len()).sum();
        // Canonical idoc can expand JSON text by at most the scalar-f64
        // case (9 bytes for a minimum 3-byte token); ×4 also covers the
        // fixed idoc header and container framing. Existing bytes are
        // added separately below. This is admission, so conservatism wins.
        #[cfg(feature = "doc")]
        let arg_reserve = if crate::json::is_json_write(meta.id) {
            arg_bytes.saturating_mul(4)
        } else {
            arg_bytes
        };
        #[cfg(not(feature = "doc"))]
        let arg_reserve = arg_bytes;
        let mut est = 64usize.saturating_add(arg_reserve);
        let store = ks.ns_store(ns);
        // Per-key reserve mirrors `stage_durable_effects` by effect class
        // (M4.5-S27, ADR-0083 D2). The estimate stays an upper bound —
        // `stage()` treats a post-admission refusal as an invariant
        // violation — but classes whose post-image provably excludes the
        // current image must not charge it: with the old blanket
        // `+image`, a DEL of a near-capacity value could never be
        // admitted (a park livelock), and a replace-SET was double-billed.
        for key in extract_keys_slices(meta, argv) {
            let per_key = match meta.id {
                // Delete effect only: `Delete { ns, key }` (+96 framing).
                CommandId::Del | CommandId::Unlink | CommandId::Getdel => key.len() + 96,
                // Replace class: the post-image is the new value, already
                // counted in `arg_reserve`; the second key term covers the
                // optional `ExpireAt` rider record.
                CommandId::Set
                | CommandId::Setnx
                | CommandId::Setex
                | CommandId::Psetex
                | CommandId::Getset
                | CommandId::Mset
                | CommandId::Msetnx => 2 * key.len() + 128,
                // SETRANGE builds `max(old_len, offset + payload)`: the
                // zero-padded gap is in no argument and not in the old
                // image — charge the declared offset too (a malformed
                // offset parses as 0 and fails in execution anyway).
                CommandId::Setrange => {
                    let offset: usize = argv
                        .get(2)
                        .and_then(|a| std::str::from_utf8(a).ok())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let image = store.and_then(|s| s.log_image_bytes(key, now)).unwrap_or(0);
                    key.len() + 96 + image.max(offset)
                }
                // Read-modify default (APPEND, INCR-family, COPY, expiry
                // rewrites, doc writes): post ≤ current image + arguments.
                _ => {
                    let image = store.and_then(|s| s.log_image_bytes(key, now)).unwrap_or(0);
                    key.len() + 96 + image
                }
            };
            est = est.saturating_add(per_key);
        }
        est
    }

    #[cfg(feature = "doc")]
    #[allow(clippy::too_many_arguments)]
    fn stage_doc_full(
        cell: &mut DurableCell<F>,
        ns: NsId,
        key: &[u8],
        lineage: DocLineage,
        version: u32,
        idoc: &[u8],
        expire_at_ms: Option<u64>,
        class: FsyncClass,
        anchor: WallAnchor,
    ) -> u64 {
        let mut last =
            cell.stage(&MutationEffect::DocFull { ns, key, lineage, version, idoc }, class);
        if let Some(ms) = expire_at_ms {
            let at_unix_ms = anchor.unix_from_internal(Nanos::from_millis(ms));
            last = cell.stage(&MutationEffect::ExpireAt { ns, at_unix_ms, key }, class);
        }
        last
    }

    /// Post-execution effect emission (ADR-0015 D5, amended by ADR-0098):
    /// per written key, the post-image (+ `ExpireAt` when a deadline is
    /// set) or a `Delete` — the one hook covering every string/key/expiry
    /// command, error reply or not for the commands that can partially
    /// apply. Returns the last staged seq (the `always` gate key).
    pub(super) fn stage_durable_effects(
        &self,
        ns: NsId,
        meta: &'static inf_wire::CommandMeta,
        argv: &[&[u8]],
        class: FsyncClass,
    ) -> Option<u64> {
        let mut ks = self.store.borrow_mut();
        let mut durable = self.durable.borrow_mut();
        let cell = durable.as_mut().expect("emission requires the durable plane");
        let store = ks.ns_store_mut(ns)?;
        let now = self.now.get();
        let anchor = self.wall_anchor();
        let mut last = None;

        #[cfg(feature = "doc")]
        if crate::json::is_json_write(meta.id) {
            let keys = extract_keys_slices(meta, argv);
            let key = *keys.first()?;
            let scratch = self.node.doc_log.borrow();
            match &scratch.intent {
                crate::json::DocLogIntent::None => {}
                crate::json::DocLogIntent::Delete => {
                    last = Some(cell.stage(&MutationEffect::Delete { ns, key }, class));
                }
                crate::json::DocLogIntent::Full => {
                    let decision = store
                        .json_log_full(key, now)
                        .expect("captured full key remains a live document until staging");
                    let JsonLogDecision::Full { lineage, version, idoc, expire_at_ms } = decision
                    else {
                        unreachable!("json_log_full always chooses a full image")
                    };
                    last = Some(Self::stage_doc_full(
                        cell,
                        ns,
                        key,
                        lineage,
                        version,
                        &idoc,
                        expire_at_ms,
                        class,
                        anchor,
                    ));
                }
                crate::json::DocLogIntent::Delta { program, opcode, match_count } => {
                    let candidate = MutationEffect::DocDelta {
                        ns,
                        key,
                        lineage: DocLineage::FIRST,
                        base_version: 0,
                        match_count: match_count.get(),
                        post_len: 1,
                        opcode: *opcode as u8,
                        program: program.as_bytes(),
                        operand: &scratch.operand,
                    };
                    let decision = store.json_log_delta_decision(
                        key,
                        candidate.encoded_len(),
                        scratch.operand.len(),
                        now,
                    );
                    match decision {
                        Some(JsonLogDecision::Delta { lineage, base_version, post_len }) => {
                            let effect = MutationEffect::DocDelta {
                                ns,
                                key,
                                lineage,
                                base_version,
                                match_count: match_count.get(),
                                post_len,
                                opcode: *opcode as u8,
                                program: program.as_bytes(),
                                operand: &scratch.operand,
                            };
                            last = Some(cell.stage(&effect, class));
                        }
                        Some(JsonLogDecision::Full { lineage, version, idoc, expire_at_ms }) => {
                            last = Some(Self::stage_doc_full(
                                cell,
                                ns,
                                key,
                                lineage,
                                version,
                                &idoc,
                                expire_at_ms,
                                class,
                                anchor,
                            ));
                        }
                        None => panic!("captured delta key remains a live document until staging"),
                    }
                }
            }
            return last;
        }

        let keys = extract_keys_slices(meta, argv);
        for key in &keys {
            match store.log_full_image(key, now) {
                #[cfg(feature = "doc")]
                Some(LogFullImage::JsonDoc(JsonLogDecision::Full {
                    lineage,
                    version,
                    idoc,
                    expire_at_ms,
                })) => {
                    last = Some(Self::stage_doc_full(
                        cell,
                        ns,
                        key,
                        lineage,
                        version,
                        &idoc,
                        expire_at_ms,
                        class,
                        anchor,
                    ));
                }
                #[cfg(feature = "doc")]
                Some(LogFullImage::JsonDoc(JsonLogDecision::Delta { .. })) => {
                    unreachable!("full-image probe never returns a delta")
                }
                Some(LogFullImage::String(img)) => {
                    let set = MutationEffect::StringSet { ns, key, value: img.value };
                    last = Some(cell.stage(&set, class));
                    if let Some(ms) = img.expire_at_ms {
                        let at_unix_ms = anchor.unix_from_internal(Nanos::from_millis(ms));
                        let exp = MutationEffect::ExpireAt { ns, at_unix_ms, key };
                        last = Some(cell.stage(&exp, class));
                    }
                }
                None => {
                    last = Some(cell.stage(&MutationEffect::Delete { ns, key }, class));
                }
            }
        }
        last
    }

    /// Owner-side named-namespace apply (the `ApplyNs` handler): admission,
    /// execution, emission — and for `always` writes the deferred-reply
    /// verdict (the fabric reply waits for this cell's fsync watermark).
    #[allow(clippy::too_many_arguments)] // one apply's fields, as its two callers hold them
    pub(super) fn execute_ns_owned(
        &self,
        from: CellId,
        argv: &[&[u8]],
        gate: ApplyGate<GatedTicket>,
        proto: Protocol,
        ns: NsId,
        program: bool,
        out: &mut ReplyBuf,
    ) -> NsApplyOutcome {
        let read = gate.read;
        let NsApplyRead { meta, class } = read;
        let is_write = read.is_write();
        if let (Some(meta), Some(_)) = (meta, class)
            && is_write
        {
            match self.durable_admission(ns, meta, argv) {
                DurableAdmission::Admit => {}
                // Pacing, not refusal (M4.5-S27, ADR-0083 D1): the caller
                // parks this apply on the origin's FIFO pump; nothing was
                // executed or staged, so the retry re-enters here whole.
                DurableAdmission::Park => return NsApplyOutcome::Park,
                DurableAdmission::Refuse(refusal) => {
                    RespWriter::new(out, proto).error(refusal);
                    return NsApplyOutcome::Reply;
                }
            }
        }
        // Maintenance bracket, fabric named-ns row (ADR-0072 D3 /
        // ADR-0076 D3 row 1): pre-half after admission, before execute;
        // commit-half after effect staging, before the reply queues.
        #[cfg(feature = "doc")]
        let bracket: Option<ArmedNsBracket<'_>> = match meta {
            Some(meta)
                if is_write && meta.id != CommandId::Copy && self.store.borrow().ns_indexed(ns) =>
            {
                let keys = extract_keys_slices(meta, argv);
                if keys.is_empty() {
                    None
                } else {
                    let path = self.json_mutation_path(meta.id, argv);
                    Some((keys, path))
                }
            }
            _ => None,
        };
        #[cfg(feature = "doc")]
        if let Some((keys, path)) = &bracket
            && let Err(refusal) = self.store.borrow_mut().idx_bracket_begin(ns, keys, path.as_ref())
        {
            RespWriter::new(out, proto).error(refusal.message());
            return NsApplyOutcome::Reply;
        }
        self.execute_owned_into(
            ExecOrigin::Fabric(from),
            argv,
            proto,
            0,
            0,
            Some(ns),
            program,
            out,
        );
        // Every command writes at least one frame; its outcome is the frame
        // at the reply's start (ADR-0190 D2) — a byte count, never a read
        // of the encoding.
        debug_assert!(!out.bytes().is_empty(), "every command writes at least one frame");
        let outcome = self.settle_ns_apply(gate, ns, argv, out);
        #[cfg(feature = "doc")]
        if let Some((keys, _)) = &bracket {
            self.store.borrow_mut().idx_bracket_commit(ns, keys);
        }
        outcome
    }

    /// An executed `ApplyNs`'s end (ADR-0177 D6): a write whose reply stages
    /// stages its durable effects, then its gate settles on the staged `seq`,
    /// and a gated verdict counts its ack.
    fn settle_ns_apply(
        &self,
        gate: ApplyGate<GatedTicket>,
        ns: NsId,
        argv: &[&[u8]],
        out: &ReplyBuf,
    ) -> NsApplyOutcome {
        let NsApplyRead { meta, class } = gate.read;
        let staged = if gate.read.is_write()
            && let (Some(meta), Some(class)) = (meta, class)
            && stages(out.outcome(), meta.id)
        {
            self.stage_durable_effects(ns, meta, argv, class)
        } else {
            None
        };
        // Canary: an `always` write's error waits as a verdict for the next
        // record's `seq`, not leaving at once.
        #[cfg(inf_canary_error_reply_gated)]
        let staged = staged.or_else(|| {
            use crate::durable::ack_class::AckClasses;
            let next = self.durable.borrow().as_ref().map(|durable| durable.staged_seq() + 1);
            next.filter(|_| gate.waits.is_some())
        });
        match gate.settle(staged) {
            Some((seq, ticket)) => {
                self.durable.borrow_mut().as_mut().expect("staged above").note_gated_ack();
                NsApplyOutcome::Gated(seq, ticket)
            }
            None => NsApplyOutcome::Reply,
        }
    }

    /// Durable-write admission — one typed verdict for every path
    /// (M4.5-S27, ADR-0083 D1/D2): the local pump and the fabric pump
    /// both park on [`DurableAdmission::Park`]; only conditions no drain
    /// can ever cure refuse. The pre-fix shape — local parks, fabric
    /// replies `-BUSY` — made hard refusal the node's dominant behaviour
    /// under pressure (~¾ of writes are fabric-routed at 4 cells).
    pub(super) fn durable_admission(
        &self,
        ns: NsId,
        meta: &'static inf_wire::CommandMeta,
        argv: &[&[u8]],
    ) -> DurableAdmission {
        let durable = self.durable.borrow();
        let Some(cell) = durable.as_ref() else {
            return DurableAdmission::Refuse(
                "ERR durable namespace on a cell without durable storage",
            );
        };
        if cell.failed {
            return DurableAdmission::Refuse("ERR durable plane failed (fail-stop)");
        }
        if cell.space_exhausted() {
            return DurableAdmission::Refuse(
                "ERR durable write refused: log storage exhausted (NOSPACE)",
            );
        }
        drop(durable);
        let est = self.estimate_effect_bytes(ns, meta, argv);
        let durable = self.durable.borrow();
        let cell = durable.as_ref().expect("checked above");
        let record_max = cell.staging.max_record_len() as usize;
        if !cell.would_fit(est) {
            // `would_fit(est)` can never pass when `est > record_max`:
            // parking is then a livelock, not backpressure (the M2-S08
            // up-front bound check `staging.rs` demands — ADR-0083 D2).
            if est > record_max {
                #[cfg(feature = "doc")]
                if crate::json::is_json_write(meta.id) {
                    // The doc estimate is ×4-conservative; the exact
                    // late checks (`json.rs` record-max/budget) govern
                    // with real encoded bytes — admit to execution.
                    let (budget, record_max) = cell.staging_limits();
                    self.node.doc_log_admission.set(Some(DocLogAdmission { budget, record_max }));
                    return DurableAdmission::Admit;
                }
                self.node.log_admission_oversized.set(self.node.log_admission_oversized.get() + 1);
                return DurableAdmission::Refuse(crate::durable::STAGING_OVERSIZED_ERROR);
            }
            return DurableAdmission::Park;
        }
        #[cfg(feature = "doc")]
        if crate::json::is_json_write(meta.id) {
            let (budget, record_max) = cell.staging_limits();
            self.node.doc_log_admission.set(Some(DocLogAdmission { budget, record_max }));
        }
        DurableAdmission::Admit
    }
}

/// One durable-admission verdict (M4.5-S27, ADR-0083): `Park` is
/// backpressure a drain will cure (wake on `drained`); `Refuse` is a
/// typed condition no retry can cure and goes to the client.
pub(super) enum DurableAdmission {
    Admit,
    Park,
    Refuse(&'static str),
}

/// What one `ApplyNs` is, read once before its first effect: the command's
/// registry row and the namespace's fsync class. A namespace's class is fixed
/// at its creation, so the reading holds across the apply's suspensions;
/// the placeholder and the owner's execution take this one reading.
#[derive(Clone, Copy)]
pub(super) struct NsApplyRead {
    meta: Option<&'static inf_wire::CommandMeta>,
    class: Option<FsyncClass>,
}

impl NsApplyRead {
    /// The reading of an apply of `argv` to `ns` on `shared`'s store.
    pub(super) fn of<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
        shared: &Shared<O, F>,
        ns: NsId,
        argv: &[&[u8]],
    ) -> NsApplyRead {
        let class = shared.store.borrow().ns_fsync_class(ns);
        NsApplyRead { meta: argv.first().and_then(|name| lookup(name)), class }
    }

    /// The reading of a command whose row and namespace class the caller
    /// read in this step.
    pub(super) fn new(
        meta: &'static inf_wire::CommandMeta,
        class: Option<FsyncClass>,
    ) -> NsApplyRead {
        NsApplyRead { meta: Some(meta), class }
    }

    /// The command's registry row.
    pub(super) fn meta(self) -> Option<&'static inf_wire::CommandMeta> {
        self.meta
    }

    /// The namespace's fsync class: the one a staged effect is staged with.
    pub(super) fn class(self) -> Option<FsyncClass> {
        self.class
    }

    fn is_write(self) -> bool {
        self.meta.is_some_and(|meta| meta.flags.contains(CmdFlags::WRITE))
    }

    /// The one predicate of a verdict that waits for this cell's watermark
    /// (ADR-0015 D6): a write to an `always` namespace, the class read
    /// first.
    pub(super) fn gates(self) -> bool {
        self.class == Some(FsyncClass::Always) && self.is_write()
    }
}

/// One apply's gating decision (ADR-0015 D6, ADR-0177 D6), from its one
/// reading, taken before its first effect: the reading, and what the
/// apply's verdict waits behind when the reading gates it (an `always`
/// write): a peer origin's [`GatedTicket`], a client's [`ClientAck`], and on
/// a read-only leg nothing (`Infallible`). Its fields are private, so the
/// constructors below are the only ones, and a `Gated` outcome holds what it
/// waits behind.
pub(super) struct ApplyGate<G> {
    read: NsApplyRead,
    waits: Option<G>,
}

/// A client's gated reply waits on its own ack-gate waiter, made at its arm
/// (ADR-0015 D6): the gate carries no slot.
pub(super) struct ClientAck;

impl ApplyGate<GatedTicket> {
    /// A peer origin's apply: its slot at the back of `origin`'s gated deque
    /// opened now, before the apply's first effect, iff `read` gates (ADR-0177
    /// D6); `Err` at a full deque. A gate that settles to nothing drops its
    /// ticket, which releases the slot.
    pub(super) fn open(
        book: &Rc<GatedBook>,
        origin: CellId,
        read: NsApplyRead,
    ) -> Result<ApplyGate<GatedTicket>, ParkedFull> {
        let waits = if read.gates() { Some(GatedBook::open(book, origin)?) } else { None };
        Ok(ApplyGate { read, waits })
    }
}

impl ApplyGate<ClientAck> {
    /// A client's command: its reply waits on its own waiter iff `read` gates.
    pub(super) fn client(read: NsApplyRead) -> ApplyGate<ClientAck> {
        ApplyGate { read, waits: read.gates().then_some(ClientAck) }
    }
}

impl ApplyGate<Infallible> {
    /// A read-only leg (a scatter program's): `None` when `read` gates, which
    /// the leg refuses before any effect; its gate has nothing to wait behind.
    pub(super) fn read_only(read: NsApplyRead) -> Option<ApplyGate<Infallible>> {
        (!read.gates()).then_some(ApplyGate { read, waits: None })
    }
}

impl<G> ApplyGate<G> {
    /// The apply's one reading.
    pub(super) fn read(&self) -> NsApplyRead {
        self.read
    }

    /// The apply's end: `staged` is the `seq` of the effect it staged, if
    /// any. `Some` iff it staged and its gate waits, with what it waits
    /// behind; otherwise the wait drops (a ticket releases its slot). A
    /// staged `seq` under an `always` reading whose gate does not wait is a
    /// command that stages without the registry's WRITE bit: its reply would
    /// leave before its fsync, so the cell stops first.
    pub(super) fn settle(self, staged: Option<u64>) -> Option<(u64, G)> {
        let ApplyGate { read, waits } = self;
        let seq = staged?;
        if let Some(gate) = waits {
            return Some((seq, gate));
        }
        assert!(read.class != Some(FsyncClass::Always), "a staged `always` write's gate waits");
        None
    }
}

/// Owner-side verdict for one `ApplyNs`.
pub(super) enum NsApplyOutcome {
    /// The reply is staged in the scratch range — ship it now.
    Reply,
    /// `always` write: ship the reply only once seq is durable; the verdict
    /// goes into the ticket's slot.
    Gated(u64, GatedTicket),
    /// Staging pressure (M4.5-S27, ADR-0083 D1): nothing executed or
    /// staged — the apply parks on the origin's FIFO pump and retries
    /// when the drain wakes it, instead of refusing with `-BUSY`.
    Park,
}

/// Applies the internal `INF.NSFAN` DDL fan on a peer cell (M2-S08):
/// `CREATE name mode fsync policy maxmemory id tier` / `DROP name epoch id
/// mode` / `SET name …`, fields as the origin serialized them (`-` =
/// none). Returns false for non-NSFAN.
pub(super) fn handle_ns_apply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    from: CellId,
    token: FabricToken,
    argv: &[&[u8]],
    scratch: &mut ReplyStream,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
) -> bool {
    if !argv[0].eq_ignore_ascii_case(b"INF.NSFAN") {
        return false;
    }
    // Test builds only: a test holds a `CREATE` leg here, so a write the
    // origin acknowledges lands while the fan is in flight.
    if argv.len() == 9
        && argv[1].eq_ignore_ascii_case(b"CREATE")
        && inf_foundation::fault::fire(crate::fault::NS_CREATE_LEG_HELD)
    {
        inf_foundation::fault::hold(crate::fault::NS_CREATE_LEG_HELD);
    }
    // Crash-matrix point (ADR-0108 D3): a peer refusing its `CREATE`
    // leg — the deterministic stand-in for any refusal of the apply on
    // this cell after the catalog swap: the OS refusing the tier ring
    // reservation, or an admission bound lowered here during the wait.
    if argv.len() == 9
        && argv[1].eq_ignore_ascii_case(b"CREATE")
        && inf_foundation::fault::fire(crate::fault::NS_CREATE_FAN_REFUSED)
    {
        let refusal = |w: &mut RespWriter<'_>| w.error("ERR fault: ns_create_fan_refused");
        stage_leg_refusal(scratch, staged, (from, token), refusal);
        return true;
    }
    if argv.len() == 6 && argv[1].eq_ignore_ascii_case(b"DROP") {
        // Crash-matrix point: this peer cannot apply the drop (test builds
        // only); in its place, every refusal below is the same fail-stop.
        let refused = if inf_foundation::fault::fire(crate::fault::NS_DROP_LEG_REFUSED) {
            Err("the injected refusal".to_owned())
        } else {
            apply_drop_leg(shared, argv)
        };
        if let Err(why) = refused {
            let name = String::from_utf8_lossy(argv[2]);
            let detail = format!("cell {} cannot apply the drop of '{name}': {why}", shared.cell.0);
            super::ddl_fail_stop("DROP leg apply", &detail);
        }
        staged.push((from, token, StagedReply::Ok));
        return true;
    }
    match apply_nsfan(shared, argv) {
        // An applied leg answers its variant alone (ADR-0189 D5): the
        // origin reads `Ok` as applied, never a reply's bytes.
        Ok(()) => staged.push((from, token, StagedReply::Ok)),
        Err(refused) => stage_leg_refusal(scratch, staged, (from, token), |w| refused.write(w)),
    }
    true
}

/// Stages a refused DDL leg's error reply in the drain's scratch, its
/// outcome read from the reply's writer (ADR-0190 D2).
fn stage_leg_refusal(
    scratch: &mut ReplyStream,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
    (from, token): (CellId, FabricToken),
    write: impl FnOnce(&mut RespWriter<'_>),
) {
    let start = scratch.len();
    let reply = scratch.open();
    write(&mut RespWriter::new(reply, Protocol::Resp2));
    let outcome = reply.outcome();
    staged.push((from, token, StagedReply::Bytes(start, scratch.len(), outcome)));
}

fn apply_nsfan<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    argv: &[&[u8]],
) -> Result<(), CreateRefused> {
    use inf_store::NsError;
    let malformed = NsError::InvalidName; // internal vocabulary; never client-visible
    if argv.len() == 4 && argv[1].eq_ignore_ascii_case(b"SET") {
        let tier = tier_from_fan(argv[3])?.ok_or(malformed)?;
        return Ok(shared.store.borrow_mut().ns_set_tier(argv[2], tier)?);
    }
    // Memory-namespace pressure keys (M4-S27, ADR-0068 D3): the MEMCFG
    // tag disambiguates from the tier-SET arm above.
    if argv.len() == 6 && argv[1].eq_ignore_ascii_case(b"SET") && argv[3] == b"MEMCFG" {
        let policy = match argv[4] {
            b"-" => None,
            p => Some(
                core::str::from_utf8(p)
                    .ok()
                    .and_then(inf_store::EvictionPolicy::parse)
                    .ok_or_else(|| malformed.clone())?,
            ),
        };
        let maxmemory = match argv[5] {
            b"-" => None,
            b => Some(
                core::str::from_utf8(b)
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| malformed.clone())?,
            ),
        };
        return Ok(shared.store.borrow_mut().ns_set_memory(argv[2], policy, maxmemory)?);
    }
    if argv.len() != 9 || !argv[1].eq_ignore_ascii_case(b"CREATE") {
        return Err(malformed.into());
    }
    fn parse_str(b: &[u8]) -> Result<&str, inf_store::NsError> {
        core::str::from_utf8(b).map_err(|_| inf_store::NsError::InvalidName)
    }
    let mode = NsMode::parse(parse_str(argv[3])?).ok_or_else(|| malformed.clone())?;
    let fsync = match argv[4] {
        b"-" => None,
        b"everysec" => Some(FsyncClass::Everysec),
        b"always" => Some(FsyncClass::Always),
        _ => return Err(malformed.into()),
    };
    let policy = match argv[5] {
        b"-" => None,
        p => {
            Some(inf_store::EvictionPolicy::parse(parse_str(p)?).ok_or_else(|| malformed.clone())?)
        }
    };
    let maxmemory = match argv[6] {
        b"-" => None,
        b => Some(parse_str(b)?.parse::<u64>().map_err(|_| malformed.clone())?),
    };
    let id: u32 = parse_str(argv[7])?.parse().map_err(|_| malformed.clone())?;
    let tier = tier_from_fan(argv[8])?;
    let spec = inf_store::NsSpec {
        id: NsId(id),
        name: argv[2].to_vec(),
        mode,
        fsync,
        policy,
        maxmemory,
        tier,
    };
    apply_create(shared, spec)
}

/// A peer's `INF.NSFAN DROP name epoch id mode` leg (ADR-0189 D4, D5):
/// its receipt is built from the leg's epoch read against the catalog word,
/// which the origin's own receipt already covered; the namespace the name
/// holds here, if any, must carry the leg's id and mode; then the drop
/// applies, `Removed` or `Absent` (a peer that refused or never received
/// the `CREATE` leg), either an applied leg. `Err` says why this cell
/// cannot apply it: each is unreachable while the origin fans an accepted
/// drop's own leg, and each is the caller's fail-stop.
fn apply_drop_leg<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    argv: &[&[u8]],
) -> Result<inf_store::ddl::DropApplied, String> {
    let field = |at: usize| core::str::from_utf8(argv[at]).ok();
    let epoch: u64 = field(3).and_then(|s| s.parse().ok()).ok_or("a malformed epoch")?;
    let id: u32 = field(4).and_then(|s| s.parse().ok()).ok_or("a malformed id")?;
    let mode = field(5).and_then(NsMode::parse).ok_or("a malformed mode")?;
    let Some(control) = shared.control.borrow().clone() else {
        return Err("no catalog writer on this node".to_owned());
    };
    let receipt = inf_store::ddl::DropPersisted::from_leg(NsId(id), epoch, control.catalog_word())
        .map_err(|e| format!("epoch {} is not durable (the word is {})", e.epoch, e.word))?;
    if let Some(spec) = shared.store.borrow().ns_get(argv[2])
        && (spec.id != receipt.id() || spec.mode != mode)
    {
        let held = format!("{} {}", spec.id.0, spec.mode.name());
        return Err(format!("the leg names {id} {}, this cell holds {held}", mode.name()));
    }
    apply_drop(shared, &receipt).map_err(|e| format!("{e:?}"))
}

/// Applies an accepted `DROP` to this cell's keyspace with its receipt
/// (ADR-0189 D4, D5): the origin's, a rollback's and a peer's leg alike. A
/// removed tiered namespace's tier files wait on the receipt's epoch
/// (ADR-0189 D9), which the catalog word already covers, so they leave at
/// the next MAINTAIN's teardown slices.
pub(super) fn apply_drop<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    receipt: &inf_store::ddl::DropPersisted,
) -> Result<inf_store::ddl::DropApplied, inf_store::NsError> {
    let applied = shared.store.borrow_mut().ns_drop(receipt)?;
    if let inf_store::ddl::DropApplied::Removed(spec) = &applied
        && spec.tier.is_some()
        && let Some(tier) = shared.tier.borrow_mut().as_mut()
    {
        // The same step moves the owner to Draining (ADR-0148 D3); a cell
        // with no Live owner of it counts the conflict (F3).
        tier.drain(spec.id, receipt.fan_epoch());
    }
    Ok(applied)
}

/// Why a cell's apply of a `CREATE` refused (ADR-0148 D2): the store's
/// own refusal, or the grant's.
#[derive(Debug)]
pub(super) enum CreateRefused {
    /// The store refused the namespace: a validation it re-ran, or its
    /// reservation.
    Store(inf_store::NsError),
    /// The directory names no grant this cell can install it at.
    Grant(crate::tier_cell::InstallRefused),
}

impl From<inf_store::NsError> for CreateRefused {
    fn from(e: inf_store::NsError) -> CreateRefused {
        CreateRefused::Store(e)
    }
}

impl CreateRefused {
    /// The refusal's reply: the store's error as every `CREATE` path
    /// renders it; a grant the directory does not name as the store's
    /// unknown-namespace error; an occupied position as an internal error.
    pub(super) fn write(self, w: &mut RespWriter<'_>) {
        match self {
            CreateRefused::Store(e) => crate::admin::ns_error(e, w),
            CreateRefused::Grant(crate::tier_cell::InstallRefused::NotDeclared) => {
                crate::admin::ns_error(inf_store::NsError::Unknown, w);
            }
            CreateRefused::Grant(crate::tier_cell::InstallRefused::Occupied) => {
                w.error("ERR internal: the namespace's grant position holds an owner on this cell");
            }
        }
    }
}

/// Applies an accepted `CREATE` to this cell's store and tiered plane
/// (ADR-0148 D2, D3): the origin's and every peer's leg alike. A tiered
/// namespace's owner is installed at the position the node's directory
/// names Declared for it, in one nonsuspending step with the store's
/// `ns_create`: the grant admitted first (it moves nothing), the store's
/// fallible reservation next, the pipeline last. A grant the directory
/// does not name, or a position this cell holds, refuses before any effect.
pub(super) fn apply_create<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    spec: inf_store::NsSpec,
) -> Result<(), CreateRefused> {
    let Some(tier) = spec.tier else {
        return Ok(shared.store.borrow_mut().ns_create(spec, &shared.census)?);
    };
    let permit = match shared.tier.borrow_mut().as_mut() {
        Some(cell) => cell.admit_install(spec.id).map_err(CreateRefused::Grant)?,
        None => return Err(CreateRefused::Grant(crate::tier_cell::InstallRefused::NotDeclared)),
    };
    shared.store.borrow_mut().ns_create(spec, &shared.census)?;
    if let Some(cell) = shared.tier.borrow_mut().as_mut() {
        cell.install(permit, &tier);
    }
    Ok(())
}

/// Serializes a tier spec for the `INF.NSFAN` vector (M4-S19): `-` for
/// absent, else ten colon-joined fields in ADR-0062 D2 table order. An
/// internal wire between symmetric binaries — the decoder still runs the
/// full range gauntlet (defense against a foreign or torn fan).
pub(super) fn tier_to_fan(tier: Option<&inf_store::TierSpec>) -> Vec<u8> {
    let Some(t) = tier else { return b"-".to_vec() };
    let io = match t.tier_io_mode {
        inf_log::fs::TierIoMode::Buffered => "buffered",
        inf_log::fs::TierIoMode::Direct => "direct",
    };
    format!(
        "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
        t.mem_budget_bytes,
        t.disk_budget_bytes,
        t.mutable_permille,
        t.maintain_slice_bytes,
        t.cold_read_qd,
        t.compaction_dead_ratio_pct,
        t.compaction_slice_bytes,
        t.blob_threshold_bytes,
        io,
        t.tail_stall_timeout_ms,
    )
    .into_bytes()
}

/// Decodes [`tier_to_fan`]'s encoding; `Ok(None)` for `-`.
fn tier_from_fan(bytes: &[u8]) -> Result<Option<inf_store::TierSpec>, inf_store::NsError> {
    use inf_store::NsError;
    if bytes == b"-" {
        return Ok(None);
    }
    let malformed = NsError::InvalidName; // internal vocabulary, as above
    let text = core::str::from_utf8(bytes).map_err(|_| malformed.clone())?;
    let fields: Vec<&str> = text.split(':').collect();
    if fields.len() != 10 {
        return Err(malformed);
    }
    let int = |s: &str| s.parse::<u64>().map_err(|_| malformed.clone());
    let tier = inf_store::TierSpec {
        mem_budget_bytes: int(fields[0])?,
        disk_budget_bytes: int(fields[1])?,
        mutable_permille: u32::try_from(int(fields[2])?).map_err(|_| malformed.clone())?,
        maintain_slice_bytes: int(fields[3])?,
        cold_read_qd: u16::try_from(int(fields[4])?).map_err(|_| malformed.clone())?,
        compaction_dead_ratio_pct: u8::try_from(int(fields[5])?).map_err(|_| malformed.clone())?,
        compaction_slice_bytes: int(fields[6])?,
        blob_threshold_bytes: u32::try_from(int(fields[7])?).map_err(|_| malformed.clone())?,
        tier_io_mode: match fields[8] {
            "buffered" => inf_log::fs::TierIoMode::Buffered,
            "direct" => inf_log::fs::TierIoMode::Direct,
            _ => return Err(malformed),
        },
        tail_stall_timeout_ms: u32::try_from(int(fields[9])?).map_err(|_| malformed.clone())?,
    };
    tier.validate().map_err(NsError::InvalidTierConfig)?;
    Ok(Some(tier))
}

// ---- work parked for a peer (ADR-0177 D3, D6) ---------------------------------

/// A parked entry: the request or reply bytes behind it (lengths, not
/// capacities, as `fabric_parked_bytes` sums them), and whether it is work
/// a task owes, where a placeholder is its own apply's, still running.
pub(super) trait Parked {
    fn parked_bytes(&self) -> u64;
    fn waits(&self) -> bool;
}

/// A full per-origin deque: the op is answered `Refused` before any effect
/// and `fabric_parked_refused` counts it. Derived never: every entry holds an
/// un-replied data op of its origin, which has at most `d`.
#[derive(Debug)]
pub(super) struct ParkedFull;

/// One `CappedDeque` per peer, each over the peer's data credits `d`
/// (ADR-0177 D6), indexed by origin; a cell parks nothing for itself. Every
/// entry enters through [`PeerDeques::publish_with`] and leaves through a
/// pop, each of which moves the cell's `fabric_parked_bytes` by the entry's
/// bytes, so the gauge is the deques' sum by construction; `waiting` counts
/// the entries of every origin that wait for a task, so whether any does
/// is one load.
pub(super) struct PeerDeques<T> {
    cell: CellId,
    deques: Vec<CappedDeque<T, Reserve>>,
    waiting: usize,
    /// The origin `pop_rotating` reads first.
    rotation: usize,
}

impl<T: Parked> PeerDeques<T> {
    /// A deque of `cap` for each of the `cells − 1` peers of `cell`, each
    /// on the cell's census (one row, `cap`'s name).
    pub(super) fn new(
        cell: CellId,
        cells: u16,
        cap: Cap,
        census: &Rc<CapCensus>,
    ) -> Result<PeerDeques<T>, CapError> {
        let peers = usize::from(cells.saturating_sub(1));
        let mut deques = Vec::new();
        deques.try_reserve_exact(peers).map_err(|_| CapError::Alloc)?;
        for _ in 0..peers {
            deques.push(CappedDeque::new(cap, census)?);
        }
        Ok(PeerDeques { cell, deques, waiting: 0, rotation: 0 })
    }

    /// The deque of `origin`: cells below this one keep their id, cells
    /// above it shift down by one.
    fn index(&self, origin: CellId) -> usize {
        debug_assert_ne!(origin, self.cell, "a cell parks no work for itself");
        let id = usize::from(origin.0);
        if origin.0 > self.cell.0 { id - 1 } else { id }
    }

    fn origin(&self, index: usize) -> CellId {
        let id = u16::try_from(index).unwrap_or(u16::MAX);
        if id >= self.cell.0 { CellId(id + 1) } else { CellId(id) }
    }

    /// Whether an entry of some origin waits for a task.
    pub(super) fn waits(&self) -> bool {
        self.waiting > 0
    }

    /// Whether `origin` has no entry parked.
    pub(super) fn is_empty_for(&self, origin: CellId) -> bool {
        self.deques[self.index(origin)].is_empty()
    }

    /// Entries parked for `origin`.
    #[cfg(test)]
    pub(super) fn len_for(&self, origin: CellId) -> usize {
        self.deques[self.index(origin)].len()
    }

    /// Origins with an entry parked, in id order: at most `cells − 1`.
    pub(super) fn origins_waiting(&self) -> impl Iterator<Item = CellId> + '_ {
        (0..self.deques.len()).filter(|&i| !self.deques[i].is_empty()).map(|i| self.origin(i))
    }

    /// Reserves `origin`'s next entry, then builds it with `make`, so
    /// nothing is copied for an op a full deque refuses. `Full` changes
    /// nothing but `fabric_parked_refused` and the row's crossings.
    pub(super) fn publish_with(
        &mut self,
        origin: CellId,
        node: &NodeState,
        make: impl FnOnce() -> T,
    ) -> Result<(), ParkedFull> {
        let index = self.index(origin);
        match self.deques[index].reserve() {
            Reserved::Slot(slot) => {
                let entry = make();
                parked_added(node, entry.parked_bytes());
                self.waiting += usize::from(entry.waits());
                slot.publish(entry);
                Ok(())
            }
            Reserved::Full => {
                node.fabric_parked_refused.set(node.fabric_parked_refused.get() + 1);
                Err(ParkedFull)
            }
        }
    }

    /// `origin`'s oldest entry, its bytes subtracted from the gauge.
    pub(super) fn pop_front(&mut self, origin: CellId, node: &NodeState) -> Option<T> {
        let index = self.index(origin);
        let entry = self.deques[index].pop_front()?;
        self.popped(node, &entry);
        Some(entry)
    }

    /// `origin`'s newest entry, its bytes subtracted from the gauge.
    pub(super) fn pop_back(&mut self, origin: CellId, node: &NodeState) -> Option<T> {
        let index = self.index(origin);
        let entry = self.deques[index].pop_back()?;
        self.popped(node, &entry);
        Some(entry)
    }

    /// The oldest entry of the first origin with one, from the rotation's
    /// cursor: at most `cells − 1` origins read, and the cursor moves past
    /// the one served, so every origin is served in turn.
    pub(super) fn pop_rotating(&mut self, node: &NodeState) -> Option<(CellId, T)> {
        let peers = self.deques.len();
        for step in 0..peers {
            let index = (self.rotation + step) % peers;
            if let Some(entry) = self.deques[index].pop_front() {
                self.rotation = (index + 1) % peers;
                self.popped(node, &entry);
                return Some((self.origin(index), entry));
            }
        }
        None
    }

    fn popped(&mut self, node: &NodeState, entry: &T) {
        if entry.waits() {
            debug_assert!(self.waiting > 0, "a waiting entry was counted");
            self.waiting = self.waiting.saturating_sub(1);
        }
        #[cfg(not(inf_canary_parked_bytes_kept))]
        parked_removed(node, entry.parked_bytes());
        // Canary: a pop subtracts nothing, so the gauge keeps every byte.
        #[cfg(inf_canary_parked_bytes_kept)]
        let _ = (node, entry);
    }
}

/// `bytes` more parked on the cell: the gauge and its high water.
fn parked_added(node: &NodeState, bytes: u64) {
    let now = node.fabric_parked_bytes.get() + bytes;
    node.fabric_parked_bytes.set(now);
    if now > node.fabric_parked_bytes_max.get() {
        node.fabric_parked_bytes_max.set(now);
    }
}

/// `bytes` fewer parked on the cell.
fn parked_removed(node: &NodeState, bytes: u64) {
    let now = node.fabric_parked_bytes.get();
    debug_assert!(now >= bytes, "the gauge holds every parked entry's bytes");
    node.fabric_parked_bytes.set(now.saturating_sub(bytes));
}

/// The three per-origin deques a plane parks its peers' work in (ADR-0177
/// D6), built with the plane, before anything serves, on its census.
pub(super) struct ParkedDeques {
    pub(super) ns_applies: PeerDeques<NsApply>,
    pub(super) owner_pubs: PeerDeques<OwnerPub>,
    pub(super) gated: PeerDeques<Gated>,
}

impl ParkedDeques {
    /// Each deque of each peer over the mesh's `data_credits`.
    pub(super) fn build(
        cell: CellId,
        cells: u16,
        data_credits: u32,
        census: &Rc<CapCensus>,
    ) -> Result<ParkedDeques, CapError> {
        let cap = |ceiling: &Cap| crate::limits::per_origin(ceiling, data_credits);
        Ok(ParkedDeques {
            ns_applies: PeerDeques::new(cell, cells, cap(&crate::limits::NS_APPLIES)?, census)?,
            owner_pubs: PeerDeques::new(cell, cells, cap(&crate::limits::OWNER_PUBS)?, census)?,
            gated: PeerDeques::new(cell, cells, cap(&crate::limits::GATED_VERDICTS)?, census)?,
        })
    }
}

/// The gated deques, the next ticket of each origin and the node's gauge
/// (ADR-0177 D6): the one `Rc` each [`GatedTicket`] holds, so a ticket's drop
/// releases its slot with nothing else borrowed.
pub(super) struct GatedBook {
    deques: RefCell<PeerDeques<Gated>>,
    /// Per origin, in the deques' order, the number its next slot takes.
    next: Box<[Cell<u32>]>,
    node: Rc<NodeInfo>,
}

impl GatedBook {
    pub(super) fn new(deques: PeerDeques<Gated>, node: Rc<NodeInfo>) -> GatedBook {
        let next = (0..deques.deques.len()).map(|_| Cell::new(0)).collect();
        GatedBook { deques: RefCell::new(deques), next, node }
    }

    /// Opens a slot at the back of `origin`'s deque and returns its ticket.
    /// The divert keeps one apply of an origin in progress, so its back is
    /// not open here; past that a second apply overtook the first, and the
    /// cell stops before the second's first effect.
    pub(super) fn open(book: &Rc<GatedBook>, origin: CellId) -> Result<GatedTicket, ParkedFull> {
        let mut deques = book.deques.borrow_mut();
        let index = deques.index(origin);
        assert!(
            !matches!(deques.deques[index].back(), Some(Gated::Open { .. })),
            "second-open-slot: a second apply of one origin in progress"
        );
        let ticket = book.next[index].get();
        deques.publish_with(origin, &book.node, || Gated::Open { ticket })?;
        book.next[index].set(ticket.wrapping_add(1));
        Ok(GatedTicket { book: Rc::clone(book), origin, ticket, filled: false })
    }

    /// Whether a verdict waits for the `gated` task.
    pub(super) fn waits(&self) -> bool {
        self.deques.borrow().waits()
    }

    /// [`PeerDeques::take_covered`] on the gated deques.
    pub(super) fn take_covered(
        &self,
        watermark: u64,
        budget: usize,
        publish: impl FnMut(CellId, GatedVerdict),
    ) -> (usize, Option<u64>) {
        self.deques.borrow_mut().take_covered(watermark, budget, &self.node, publish)
    }
}

/// An open gated slot at the back of its origin's deque (ADR-0177 D6): made
/// only by [`GatedBook::open`], not `Clone`, filled once by [`GatedTicket::fill`]
/// or released by its drop, which leaves the deque as it was before the
/// apply. It holds no borrow, so it crosses the apply's suspension; the back
/// stays its own because an origin has one apply in progress at a time
/// ([`Shared::diverts_to_pump`]). Its `u32` wraps: it is compared with its
/// origin's one open slot only.
#[must_use = "a ticket is filled with its verdict, or its drop releases the slot"]
pub(super) struct GatedTicket {
    book: Rc<GatedBook>,
    origin: CellId,
    ticket: u32,
    /// Set first by `fill`: the drop of a filled ticket releases nothing.
    filled: bool,
}

impl GatedTicket {
    /// The apply was gated: its slot becomes `verdict`, which the `gated`
    /// task publishes once the watermark covers its `seq`. The slot is the
    /// back of its origin's deque (`open`'s check); past that the verdict
    /// would wedge its origin behind a slot no fill reaches: the cell stops.
    /// The ticket is disarmed before the compare, so a failed compare
    /// unwinds through a drop that releases nothing: one panic.
    pub(super) fn fill(mut self, verdict: GatedVerdict) {
        self.filled = true;
        let book = &self.book;
        let mut deques = book.deques.borrow_mut();
        let index = deques.index(self.origin);
        let bytes = verdict.parked_bytes();
        let back = deques.deques[index].back_mut();
        assert!(
            matches!(back, Some(Gated::Open { ticket }) if *ticket == self.ticket),
            "a filled ticket's slot is the back of its origin's deque"
        );
        if let Some(back) = back {
            *back = Gated::Verdict(verdict);
            parked_added(&book.node, bytes);
            deques.waiting += 1;
        }
    }
}

impl Drop for GatedTicket {
    /// The apply was not gated (a reply, a park, a refusal, an arm that
    /// forgot, teardown): its slot goes, and the deque is as it was before
    /// the apply. Past the compare a pop would drop another apply's verdict,
    /// so the cell stops first.
    fn drop(&mut self) {
        if self.filled {
            return;
        }
        // Canary: the drop releases nothing, and the slot stays open.
        if cfg!(inf_canary_gated_drop_kept) {
            return;
        }
        let book = &self.book;
        let mut deques = book.deques.borrow_mut();
        let index = deques.index(self.origin);
        let back = deques.deques[index].back();
        assert!(
            matches!(back, Some(Gated::Open { ticket }) if *ticket == self.ticket),
            "a released ticket's slot is the back of its origin's deque"
        );
        deques.pop_back(self.origin, &book.node);
    }
}

impl PeerDeques<Gated> {
    /// Pops every verdict at a front that `watermark` covers, at most
    /// `budget`, from the rotation's cursor, handing each to `publish` with
    /// its origin; returns how many it published and the smallest `seq` of
    /// a verdict left at a front. A placeholder holds its origin's queue:
    /// nothing behind it is reached (it is always the back).
    pub(super) fn take_covered(
        &mut self,
        watermark: u64,
        budget: usize,
        node: &NodeState,
        mut publish: impl FnMut(CellId, GatedVerdict),
    ) -> (usize, Option<u64>) {
        let peers = self.deques.len();
        let mut published = 0;
        let mut pending: Option<u64> = None;
        for step in 0..peers {
            let index = (self.rotation + step) % peers;
            while let Some(Gated::Verdict(front)) = self.deques[index].front() {
                #[cfg(not(inf_canary_gated_ignores_watermark))]
                let covered = front.seq <= watermark;
                // Canary: every filled front is published, durable or not.
                #[cfg(inf_canary_gated_ignores_watermark)]
                let covered = {
                    let _ = watermark;
                    true
                };
                if !covered {
                    pending = Some(pending.map_or(front.seq, |seq| seq.min(front.seq)));
                    break;
                }
                if published == budget {
                    self.rotation = index;
                    return (published, pending);
                }
                let origin = self.origin(index);
                if let Some(Gated::Verdict(verdict)) = self.pop_front(origin, node) {
                    publish(origin, verdict);
                    published += 1;
                }
            }
        }
        (published, pending)
    }
}

#[allow(
    clippy::disallowed_types,
    reason = "test-only: the std model the per-origin deques are checked against"
)]
#[cfg(test)]
mod parked_tests {
    use std::collections::VecDeque;

    use inf_foundation::rng::{Entropy, SplitMix64};

    use super::*;

    /// A parked entry of `self.0` bytes, waiting.
    #[derive(Debug, PartialEq, Eq)]
    struct Bytes(u64);

    impl Parked for Bytes {
        fn parked_bytes(&self) -> u64 {
            self.0
        }

        fn waits(&self) -> bool {
            true
        }
    }

    fn deques<T: Parked>(cell: u16, cells: u16, entries: u32) -> PeerDeques<T> {
        let (census, _serve_mark) = CapCensus::new(CellId(cell));
        let cap = crate::limits::per_origin(&crate::limits::NS_APPLIES, entries).expect("cap");
        PeerDeques::new(CellId(cell), cells, cap, &census).expect("built")
    }

    fn verdict(seq: u64, bytes: usize) -> GatedVerdict {
        GatedVerdict {
            token: FabricToken(seq),
            seq,
            reply: vec![b'+'; bytes].into_boxed_slice(),
            outcome: ReplyOutcome::Success,
        }
    }

    /// Every peer has its own deque and the cell none; the rotation serves
    /// each origin in turn; a full deque refuses before it builds the entry
    /// and counts the refusal; the gauge moves by each entry's bytes.
    #[test]
    fn each_peer_has_a_deque_served_in_rotation_and_a_full_one_builds_nothing() {
        let node = NodeState::default();
        let mut parked = deques::<Bytes>(1, 4, 2);
        assert!(!parked.waits());
        for origin in [0u16, 2, 3, 0] {
            parked
                .publish_with(CellId(origin), &node, || Bytes(u64::from(origin) + 10))
                .expect("room");
        }
        assert_eq!(node.fabric_parked_bytes.get(), 10 + 12 + 13 + 10);
        assert_eq!(parked.origins_waiting().collect::<Vec<_>>(), [CellId(0), CellId(2), CellId(3)]);
        let full = parked.publish_with(CellId(0), &node, || panic!("a full deque builds nothing"));
        assert!(full.is_err());
        assert_eq!(node.fabric_parked_refused.get(), 1);
        let order: Vec<u16> = core::iter::from_fn(|| parked.pop_rotating(&node))
            .map(|(origin, _)| origin.0)
            .collect();
        assert_eq!(order, [0, 2, 3, 0], "origins in turn, each in arrival order");
        assert!(!parked.waits());
        assert_eq!(node.fabric_parked_bytes.get(), 0, "every pop subtracted its bytes");
        assert_eq!(node.fabric_parked_bytes_max.get(), 45, "the high water stays");
    }

    /// A seeded history of publishes and pops at both ends against one
    /// `VecDeque` per origin: after every op the entries, the gauge (the
    /// models' byte sum) and its high water agree.
    #[test]
    fn a_seeded_history_keeps_the_gauge_at_the_models_byte_sum() {
        for seed in 0..32 {
            let node = NodeState::default();
            let mut parked = deques::<Bytes>(2, 5, 3);
            let mut model: Vec<VecDeque<u64>> = vec![VecDeque::new(); 5];
            let (mut high, mut refused) = (0u64, 0u64);
            let mut rng = SplitMix64::new(seed);
            for _ in 0..400 {
                let origin = [0u16, 1, 3, 4][rng.next_below(4) as usize];
                let queue = &mut model[usize::from(origin)];
                match rng.next_below(4) {
                    0 | 1 => {
                        let bytes = rng.next_below(64);
                        let took =
                            parked.publish_with(CellId(origin), &node, || Bytes(bytes)).is_ok();
                        assert_eq!(took, queue.len() < 3, "seed {seed}");
                        if took {
                            queue.push_back(bytes);
                        } else {
                            refused += 1;
                        }
                    }
                    2 => {
                        let got = parked.pop_front(CellId(origin), &node).map(|b| b.0);
                        assert_eq!(got, queue.pop_front(), "seed {seed}");
                    }
                    _ => {
                        let got = parked.pop_back(CellId(origin), &node).map(|b| b.0);
                        assert_eq!(got, queue.pop_back(), "seed {seed}");
                    }
                }
                let sum: u64 = model.iter().flatten().sum();
                high = high.max(sum);
                assert_eq!(node.fabric_parked_bytes.get(), sum, "seed {seed}");
                assert_eq!(node.fabric_parked_bytes_max.get(), high, "seed {seed}");
                assert_eq!(parked.waits(), model.iter().any(|q| !q.is_empty()), "seed {seed}");
            }
            assert_eq!(node.fabric_parked_refused.get(), refused, "seed {seed}");
        }
    }

    /// A gated book on cell 1 of `cells`, each origin's deque `entries` long.
    fn book(cells: u16, entries: u32) -> Rc<GatedBook> {
        let (node, _) = NodeInfo::try_default().expect("fixture cache allocation");
        let node = Rc::new(node);
        Rc::new(GatedBook::new(deques::<Gated>(1, cells, entries), node))
    }

    /// A second apply of one origin opening its slot while the first holds
    /// its own, which the divert rules out: the cell stops before the
    /// second's first effect.
    #[test]
    #[should_panic(expected = "second-open-slot: a second apply of one origin in progress")]
    fn a_second_open_slot_of_one_origin_stops_the_cell() {
        let book = book(3, 4);
        let _earlier = GatedBook::open(&book, CellId(0)).expect("room");
        let _later = GatedBook::open(&book, CellId(0));
    }

    /// A verdict published behind an open slot, past `open`'s check: the
    /// ticket's fill finds the verdict at the back, and the cell stops rather
    /// than wedge the origin behind a slot no fill reaches.
    #[test]
    #[should_panic(expected = "a filled ticket's slot is the back of its origin's deque")]
    fn a_fill_behind_another_verdict_stops_the_cell() {
        let book = book(3, 4);
        let earlier = GatedBook::open(&book, CellId(0)).expect("room");
        let other = || Gated::Verdict(verdict(2, 1));
        book.deques.borrow_mut().publish_with(CellId(0), &book.node, other).expect("room");
        earlier.fill(verdict(1, 1));
    }

    /// The same, at a release: the pop would drop the other verdict, so the
    /// cell stops first.
    #[test]
    #[should_panic(expected = "a released ticket's slot is the back of its origin's deque")]
    fn a_release_behind_another_verdict_stops_the_cell() {
        let book = book(3, 4);
        let earlier = GatedBook::open(&book, CellId(0)).expect("room");
        let other = || Gated::Verdict(verdict(2, 1));
        book.deques.borrow_mut().publish_with(CellId(0), &book.node, other).expect("room");
        drop(earlier);
    }

    /// R5 (ADR-0015 D6, ADR-0177 D6): over class × read or write × staged or
    /// not, a gate settles to its wait iff its reading is an `always` write
    /// and the apply staged; an unstaged waiting gate releases its slot, so
    /// the origin's deque is its before-state. A client's gate reads the
    /// same predicate.
    #[test]
    fn a_gate_settles_only_a_staged_write_of_a_gating_reading() {
        let book = book(3, 4);
        let classes = [None, Some(FsyncClass::Everysec), Some(FsyncClass::Always)];
        for class in classes {
            for meta in [lookup(b"GET"), lookup(b"SET")] {
                for staged in [None, Some(7)] {
                    let read = NsApplyRead { meta, class };
                    let case = format!("{class:?}, write {}, staged {staged:?}", read.is_write());
                    if class == Some(FsyncClass::Always) && !read.is_write() && staged.is_some() {
                        continue; // the fail-stop, its own test below
                    }
                    let waits = class == Some(FsyncClass::Always) && read.is_write();
                    let gate = ApplyGate::open(&book, CellId(0), read).expect("room");
                    match gate.settle(staged) {
                        Some((seq, ticket)) => {
                            assert!(waits && staged == Some(seq), "{case}: settled to a wait");
                            drop(ticket);
                        }
                        None => assert!(!(waits && staged.is_some()), "{case}: no wait"),
                    }
                    let deques = book.deques.borrow();
                    let index = deques.index(CellId(0));
                    assert!(deques.deques[index].is_empty(), "{case}: the before-state");
                    drop(deques);
                    let client = ApplyGate::client(read).settle(staged);
                    assert_eq!(client.is_some(), waits && staged.is_some(), "{case}: a client");
                }
            }
        }
    }

    /// R5: a staged `always` reading that is not a write (a command that
    /// stages without the registry's WRITE bit) stops the cell before its
    /// reply leaves.
    #[test]
    #[should_panic(expected = "a staged `always` write's gate waits")]
    fn a_staged_always_reading_that_is_not_a_write_stops_the_cell() {
        let read = NsApplyRead { meta: lookup(b"GET"), class: Some(FsyncClass::Always) };
        let _ = ApplyGate::client(read).settle(Some(1));
    }

    /// The gated deque (ADR-0177 D3, D6): an open slot waits for nothing,
    /// fills with its verdict or is released by its ticket's drop; the
    /// watermark publishes each origin's covered fronts in `seq` order and
    /// stops at an open slot; the smallest uncovered front is the one
    /// awaited; a budget ends a pass with the rest left.
    #[test]
    fn gated_verdicts_publish_under_the_watermark_in_seq_order() {
        let book = book(3, 4);
        let first = GatedBook::open(&book, CellId(0)).expect("room");
        assert!(!book.waits(), "an open slot is its apply's, still running");
        first.fill(verdict(5, 10));
        GatedBook::open(&book, CellId(2)).expect("room").fill(verdict(3, 7));
        let running = GatedBook::open(&book, CellId(0)).expect("room");
        assert_eq!(book.node.fabric_parked_bytes.get(), 17, "an open slot is no bytes");
        let mut sent = Vec::new();
        let (published, pending) = book.take_covered(4, 1_024, |to, v| sent.push((to.0, v.seq)));
        assert_eq!((published, pending, sent.as_slice()), (1, Some(5), &[(2u16, 3u64)][..]));
        let (published, pending) = book.take_covered(9, 1_024, |to, v| sent.push((to.0, v.seq)));
        assert_eq!((published, pending), (1, None), "the open slot holds origin 0's queue");
        assert_eq!(sent, [(2, 3), (0, 5)]);
        drop(running);
        assert!(!book.waits());
        assert_eq!(book.node.fabric_parked_bytes.get(), 0);
        for seq in [10, 11, 12] {
            GatedBook::open(&book, CellId(2)).expect("room").fill(verdict(seq, 1));
        }
        let (published, _) = book.take_covered(u64::MAX, 2, |_, _| {});
        assert_eq!(published, 2, "the budget ends the pass");
        let (published, pending) = book.take_covered(u64::MAX, 2, |_, v| assert_eq!(v.seq, 12));
        assert_eq!((published, pending), (1, None));
        assert_eq!(book.node.fabric_parked_refused.get(), 0);
    }

    /// One origin's slot in the model: open under its ticket's number, or a
    /// verdict's `seq` and bytes.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Slot {
        Open(u32),
        Verdict(u64, u64),
    }

    /// A seeded history of tickets opened, filled and dropped over three
    /// origins at `d` = 8 (the plain build's cap: the credits), against one
    /// `VecDeque` per origin: after every step each deque equals its model,
    /// the gauge is the model's verdict bytes, and each pass publishes the
    /// model's covered fronts. A ticket dropped at any step releases its
    /// slot, so the origin's next open is not refused for it.
    #[test]
    fn a_seeded_history_of_tickets_keeps_each_origin_at_its_model() {
        let origins = [CellId(0), CellId(2), CellId(3)];
        for seed in 0..32 {
            let book = book(4, 8);
            let mut model: [VecDeque<Slot>; 3] = Default::default();
            let mut tickets: [Option<GatedTicket>; 3] = Default::default();
            let mut numbers = [0u32; 3];
            let (mut seq, mut watermark, mut refused) = (0u64, 0u64, 0u64);
            let mut rng = SplitMix64::new(seed);
            for _ in 0..600 {
                let at = rng.next_below(3) as usize;
                match rng.next_below(5) {
                    0 | 1 if tickets[at].is_none() => {
                        let opened = GatedBook::open(&book, origins[at]);
                        assert_eq!(opened.is_ok(), model[at].len() < 8, "seed {seed}");
                        match opened {
                            Ok(ticket) => {
                                model[at].push_back(Slot::Open(numbers[at]));
                                numbers[at] = numbers[at].wrapping_add(1);
                                tickets[at] = Some(ticket);
                            }
                            Err(ParkedFull) => refused += 1,
                        }
                    }
                    2 => {
                        if let Some(ticket) = tickets[at].take() {
                            seq += 1;
                            let bytes = 1 + rng.next_below(64);
                            ticket.fill(verdict(seq, bytes as usize));
                            *model[at].back_mut().expect("its slot") = Slot::Verdict(seq, bytes);
                        }
                    }
                    3 => {
                        if let Some(ticket) = tickets[at].take() {
                            drop(ticket);
                            assert!(matches!(model[at].pop_back(), Some(Slot::Open(_))));
                        }
                    }
                    _ => {
                        watermark += rng.next_below(4);
                        let mut sent = Vec::new();
                        let _ = book.take_covered(watermark, usize::MAX, |to, v| {
                            sent.push((to, v.seq));
                        });
                        let mut expected = Vec::new();
                        for (index, queue) in model.iter_mut().enumerate() {
                            while let Some(&Slot::Verdict(front, _)) = queue.front()
                                && front <= watermark
                            {
                                expected.push((origins[index], front));
                                queue.pop_front();
                            }
                        }
                        sent.sort_unstable();
                        expected.sort_unstable();
                        assert_eq!(sent, expected, "seed {seed}: the pass's published set");
                    }
                }
                let deques = book.deques.borrow();
                for (index, queue) in model.iter().enumerate() {
                    let held: Vec<Slot> = deques.deques[deques.index(origins[index])]
                        .iter()
                        .map(|slot| match slot {
                            Gated::Open { ticket } => Slot::Open(*ticket),
                            Gated::Verdict(v) => Slot::Verdict(v.seq, v.parked_bytes()),
                        })
                        .collect();
                    assert_eq!(held, Vec::from(queue.clone()), "seed {seed}: origin {index}");
                }
                let bytes: u64 = model
                    .iter()
                    .flatten()
                    .map(|slot| if let Slot::Verdict(_, b) = slot { *b } else { 0 })
                    .sum();
                assert_eq!(book.node.fabric_parked_bytes.get(), bytes, "seed {seed}: the gauge");
                let waits = model.iter().flatten().any(|slot| matches!(slot, Slot::Verdict(..)));
                assert_eq!(book.waits(), waits, "seed {seed}");
            }
            drop(tickets);
            let _ = book.take_covered(u64::MAX, usize::MAX, |_, _| {});
            assert!(!book.waits(), "seed {seed}");
            assert_eq!(book.node.fabric_parked_bytes.get(), 0, "seed {seed}");
            assert_eq!(book.node.fabric_parked_refused.get(), refused, "seed {seed}");
        }
    }
}
