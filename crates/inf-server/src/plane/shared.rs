//! `Shared` — the cell state every pump future holds an `Rc` to: store,
//! tier, durable cell, pools, tripwires, and the owned-command execution
//! path (`execute_owned_into`) with its durability staging and admission.

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
    pub(super) fn with_conn<R>(&self, key: ConnKey, f: impl FnOnce(&mut Conn) -> R) -> Option<R> {
        self.conns.borrow_mut().get_mut(key).map(f)
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
        out: &mut Vec<u8>,
    ) -> bool {
        let before = out.len();
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
                &out[before..],
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
        self.observer.borrow_mut().on_execute(self.cell, origin, scope, argv, &out[before..], now);
        cx.close_requested.get()
    }

    /// The mutation's path program for the S04 static path-overlap prune
    /// (ADR-0076 D6): only unambiguous path-carrying JSON writes yield
    /// one — everything else evaluates in full. A path that fails to
    /// compile never prunes (the command will fail with its own error).
    #[cfg(feature = "doc")]
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
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, Protocol::Resp2).int(i64::from(hit));
        self.observer.borrow_mut().on_execute(
            self.cell,
            origin,
            ExecScope::Db(db),
            &[name, key],
            &reply,
            now,
        );
        i64::from(hit)
    }

    /// Typed DBSIZE apply (scatter contribution, M1-S02; per selected db).
    pub(super) fn apply_dbsize(&self, origin: ExecOrigin, db: u16) -> i64 {
        let now = self.now.get();
        let len = self.store.borrow_mut().db_mut(usize::from(db)).len() as i64;
        let mut reply = Vec::new();
        RespWriter::new(&mut reply, Protocol::Resp2).int(len);
        self.observer.borrow_mut().on_execute(
            self.cell,
            origin,
            ExecScope::Db(db),
            &[b"DBSIZE"],
            &reply,
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
                        match_count: *match_count,
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
                                match_count: *match_count,
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
    pub(super) fn execute_ns_owned(
        &self,
        from: CellId,
        argv: &[&[u8]],
        proto: Protocol,
        ns: NsId,
        program: bool,
        out: &mut Vec<u8>,
    ) -> NsApplyOutcome {
        let before = out.len();
        let meta = lookup(argv[0]);
        let class = self.store.borrow().ns_fsync_class(ns);
        let is_write = meta.is_some_and(|m| m.flags.contains(CmdFlags::WRITE));
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
        let mut outcome = NsApplyOutcome::Reply;
        if is_write
            && let (Some(meta), Some(class)) = (meta, class)
            && (out.get(before) != Some(&b'-') || stages_despite_error(meta.id))
            && let Some(seq) = self.stage_durable_effects(ns, meta, argv, class)
            && class == FsyncClass::Always
        {
            self.durable.borrow_mut().as_mut().expect("staged above").note_gated_ack();
            outcome = NsApplyOutcome::Gated(seq);
        }
        #[cfg(feature = "doc")]
        if let Some((keys, _)) = &bracket {
            self.store.borrow_mut().idx_bracket_commit(ns, keys);
        }
        outcome
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

/// Owner-side verdict for one `ApplyNs`.
pub(super) enum NsApplyOutcome {
    /// The reply is staged in the scratch range — ship it now.
    Reply,
    /// `always` write: ship the reply only once seq is durable.
    Gated(u64),
    /// Staging pressure (M4.5-S27, ADR-0083 D1): nothing executed or
    /// staged — the apply parks on the origin's FIFO pump and retries
    /// when the drain wakes it, instead of refusing with `-BUSY`.
    Park,
}

/// Applies the internal `INF.NSFAN` DDL fan on a peer cell (M2-S08):
/// `CREATE name mode fsync policy maxmemory id` / `DROP name`, fields as
/// the origin serialized them (`-` = none). Returns false for non-NSFAN.
pub(super) fn handle_ns_apply<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    from: CellId,
    token: FabricToken,
    argv: &[&[u8]],
    scratch: &mut Vec<u8>,
    staged: &mut Vec<(CellId, FabricToken, StagedReply)>,
) -> bool {
    if !argv[0].eq_ignore_ascii_case(b"INF.NSFAN") {
        return false;
    }
    let start = scratch.len();
    let mut w = RespWriter::new(scratch, Protocol::Resp2);
    // Crash-matrix point (ADR-0108 D3): a peer refusing its `CREATE`
    // leg — the deterministic stand-in for the OS refusing the tier
    // ring reservation on this cell after the catalog swap.
    if argv.len() == 9
        && argv[1].eq_ignore_ascii_case(b"CREATE")
        && inf_foundation::fault::fire(crate::fault::NS_CREATE_FAN_REFUSED)
    {
        w.error("ERR fault: ns_create_fan_refused");
        staged.push((from, token, StagedReply::Bytes(start, scratch.len())));
        return true;
    }
    match apply_nsfan(shared, argv) {
        Ok(()) => w.simple("OK"),
        Err(e) => crate::admin::ns_error(e, &mut w),
    }
    staged.push((from, token, StagedReply::Bytes(start, scratch.len())));
    true
}

fn apply_nsfan<O: PlaneObserver + 'static, F: SegmentFs + Clone + 'static>(
    shared: &Shared<O, F>,
    argv: &[&[u8]],
) -> Result<(), inf_store::NsError> {
    use inf_store::NsError;
    let malformed = NsError::InvalidName; // internal vocabulary; never client-visible
    if argv.len() == 4 && argv[1].eq_ignore_ascii_case(b"DROP") {
        // ADR-0100 D4: the leg carries the persist epoch of the catalog
        // swap that drops the namespace; this cell's tier teardown holds
        // until that epoch is durable (D5).
        let epoch: u64 =
            core::str::from_utf8(argv[3]).ok().and_then(|s| s.parse().ok()).ok_or(malformed)?;
        let spec = shared.store.borrow_mut().ns_drop(argv[2])?;
        if spec.tier.is_some() {
            shared.ns_drop_releases.borrow_mut().push((spec.id, epoch));
        }
        return Ok(());
    }
    if argv.len() == 4 && argv[1].eq_ignore_ascii_case(b"SET") {
        let tier = tier_from_fan(argv[3])?.ok_or(malformed)?;
        return shared.store.borrow_mut().ns_set_tier(argv[2], tier);
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
        return shared.store.borrow_mut().ns_set_memory(argv[2], policy, maxmemory);
    }
    if argv.len() != 9 || !argv[1].eq_ignore_ascii_case(b"CREATE") {
        return Err(malformed);
    }
    fn parse_str(b: &[u8]) -> Result<&str, inf_store::NsError> {
        core::str::from_utf8(b).map_err(|_| inf_store::NsError::InvalidName)
    }
    let mode = NsMode::parse(parse_str(argv[3])?).ok_or_else(|| malformed.clone())?;
    let fsync = match argv[4] {
        b"-" => None,
        b"everysec" => Some(FsyncClass::Everysec),
        b"always" => Some(FsyncClass::Always),
        _ => return Err(malformed),
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
    shared.store.borrow_mut().ns_create(inf_store::NsSpec {
        id: NsId(id),
        name: argv[2].to_vec(),
        mode,
        fsync,
        policy,
        maxmemory,
        tier,
    })
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
