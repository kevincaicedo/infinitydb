//! The shadow-slot arms of an `m4-recovery` run (M4.5-S37, ADR-0093): the
//! harness's reconciliations and the ticket rows it drives — a ticket held
//! open across the walk and the cut (A12), a relocated twin carrying
//! origins (A11), the winners a rebuild left with several tickets (A10)
//! and the `DBSIZE`-shaped drain (A3).

use super::*;

impl Run {
    /// One reconciliation (ADR-0093 D4) played by the harness: the
    /// ticket's cold record read through the catalog, the verdict
    /// applied. A read that fails (a file the pin-analog unlink took —
    /// impossible while the slot is live) is a violation.
    fn reconcile_ticket(&mut self, life: &mut Life, ticket: inf_store::ShadowTicket, when: &str) {
        let Some(image) = read_cold_record(&self.disk, &life.flush, ticket.cold.to_raw()) else {
            self.report.violations.push(format!(
                "{when}: shadow twin at {} unreadable while its slot is live",
                ticket.cold.to_raw()
            ));
            life.table.shadow_read_failed(ticket.cold);
            return;
        };
        let same_key = self.twin_is_winner_key(life, &ticket, &image);
        match life.table.resolve_shadow(ticket.hash, ticket.cold, &image) {
            inf_store::ShadowVerdict::SameKey => {
                self.report.shadow_same_key += 1;
                if !same_key {
                    self.report
                        .violations
                        .push(format!("{when}: same-key verdict on a collision twin"));
                }
            }
            inf_store::ShadowVerdict::Collision => {
                // ADR-0093 A7: legal exactly when the twin's full key is
                // not the winner's — the crafted pairs; on equal keys it
                // is a wrong comparison.
                self.report.shadow_collision += 1;
                if same_key {
                    self.report
                        .violations
                        .push(format!("{when}: collision verdict on a same-key twin"));
                }
            }
            inf_store::ShadowVerdict::Stale | inf_store::ShadowVerdict::Deferred => {}
        }
    }

    /// The oracle's own comparison: the twin's decoded key against the
    /// ticket's current winner's key (the verdict is checked, never
    /// trusted).
    fn twin_is_winner_key(
        &self,
        life: &Life,
        ticket: &inf_store::ShadowTicket,
        image: &[u8],
    ) -> bool {
        let Some(current) = life.table.shadow_tickets().find(|t| t.cold == ticket.cold) else {
            return false;
        };
        TieredTable::decode_record(image).key == life.table.record(current.winner).key
    }

    /// The `DEL` path's verify (ADR-0093 D3): the twin read and
    /// key-compared; a same-key twin is deleted through its own marker
    /// (into the tail) and `delete` — never `resolve_shadow`, whose
    /// death attribution is deferred under a pinned walk.
    pub(super) fn verify_twin_for_delete(
        &mut self,
        life: &mut Life,
        ticket: inf_store::ShadowTicket,
        when: &str,
    ) {
        // A verified ticket needs no read (ADR-0093 A1): the exact
        // length is on the ticket — the plane's `delete_one` rule.
        if let Some(len) = ticket.verified_len {
            self.twin_origin_markers(life, &ticket);
            RecordView::ColdDisplace { ns: NS, old_addr: ticket.cold.to_raw() }
                .encode_into(&mut self.tail);
            life.table.delete(ticket.hash, ticket.cold, len as usize);
            self.report.shadow_same_key += 1;
            return;
        }
        let Some(image) = read_cold_record(&self.disk, &life.flush, ticket.cold.to_raw()) else {
            self.report.violations.push(format!(
                "{when}: shadow twin at {} unreadable while its slot is live",
                ticket.cold.to_raw()
            ));
            return;
        };
        let same_key = self.twin_is_winner_key(life, &ticket, &image);
        match life.table.verify_shadow(ticket.hash, ticket.cold, &image) {
            inf_store::ShadowVerdict::SameKey => {
                if !same_key {
                    self.report
                        .violations
                        .push(format!("{when}: same-key verdict on a collision twin"));
                }
                self.twin_origin_markers(life, &ticket);
                RecordView::ColdDisplace { ns: NS, old_addr: ticket.cold.to_raw() }
                    .encode_into(&mut self.tail);
                life.table.delete(ticket.hash, ticket.cold, image.len());
                self.report.shadow_same_key += 1;
            }
            inf_store::ShadowVerdict::Collision => {
                self.report.shadow_collision += 1;
                if same_key {
                    self.report
                        .violations
                        .push(format!("{when}: collision verdict on a same-key twin"));
                }
            }
            inf_store::ShadowVerdict::Stale | inf_store::ShadowVerdict::Deferred => {}
        }
    }

    /// ADR-0093 A11 (batch 23): a same-key twin's own relocation origins
    /// ride the `DEL`'s markers ahead of its address — the origins a
    /// checkpoint that began before the twin's relocation may still ref.
    /// Pre-fix the plane (and this mirror) dropped them: the deleted key
    /// resurfaced at the next boot as an orphan cold slot (the seven red
    /// seeds of `pre-fix-dst-recovery-seeds64.log`).
    fn twin_origin_markers(&mut self, life: &mut Life, ticket: &inf_store::ShadowTicket) {
        for (origin, _) in life.table.take_displacement_origins(ticket.hash, ticket.cold) {
            RecordView::ColdDisplace { ns: NS, old_addr: origin }.encode_into(&mut self.tail);
            self.report.shadow_twin_origins_covered += 1;
        }
    }

    /// F-L07-01 (batch 23): `DEL` of every winner the boot rebuild left
    /// carrying two or more tickets, with them open.
    pub(super) fn delete_multi_ticket_winners(&mut self, life: &mut Life) {
        let mut by_winner: BTreeMap<u64, usize> = BTreeMap::new();
        for ticket in life.table.shadow_tickets() {
            *by_winner.entry(ticket.winner.to_raw()).or_default() += 1;
        }
        let winners: Vec<Vec<u8>> = by_winner
            .iter()
            .filter(|(_, n)| **n >= 2)
            .map(|(w, _)| {
                life.table.record(LogicalAddr::from_raw(*w).expect("48-bit")).key.to_vec()
            })
            .collect();
        for key in winners {
            self.report.shadow_multi_ticket_winners += 1;
            self.apply_op(life, &key, Op::Del);
            self.report.shadow_multi_ticket_dels += 1;
        }
    }

    /// F-L07-01 / ADR-0093 A11 (batch 23): one relocated cold slot (a
    /// record carrying displacement origins) is overwritten through the
    /// shadow path and its winner deleted with the ticket open. Crafted
    /// colliding keys are skipped — the row is about origins, not hashes.
    pub(super) fn directed_twin_with_origins(&mut self, life: &mut Life, rng: &mut SplitMix64) {
        let keys: Vec<Vec<u8>> = self
            .model
            .keys()
            .filter(|k| !k.starts_with(inf_store::COLLISION_KEY_PREFIX))
            .cloned()
            .collect();
        if keys.is_empty() {
            return;
        }
        let start = (rng.next_u64() % keys.len() as u64) as usize;
        for i in 0..keys.len() {
            let key = &keys[(start + i) % keys.len()];
            let hash = life.table.hash_key(key);
            let TieredLookup::Cold(addr) = life.table.lookup(key, hash, &[]) else { continue };
            if life.table.displacement_origins_len(hash, addr) == 0
                || !matches!(life.table.shadow_probe(key, hash), inf_store::ShadowProbe::One(_))
            {
                continue;
            }
            let value = vec![(rng.next_u64() % 251) as u8; 40];
            self.apply_op(life, key, Op::SetShadow(value));
            if life.table.shadow_pending() == 0 {
                // Admission refused (the shadow write fell back to a
                // plain SET, which took the origins itself) — not a row.
                return;
            }
            self.report.shadow_twin_origin_rows += 1;
            self.apply_op(life, key, Op::Del);
            return;
        }
    }

    /// The `DBSIZE` drain played by the harness (ADR-0093 A3): verify
    /// every unverified ticket (reads only — no settle, so a pinned walk
    /// is no obstacle), then `len()` must equal the model with the
    /// verified tickets still open. An unverified collision ticket that
    /// survived verification, or a count off by one, is the review's
    /// finding reconstructed.
    pub(super) fn audit_len_after_drain(&mut self, life: &mut Life, when: &str) {
        for ticket in life.table.shadow_unverified_tickets() {
            let Some(image) = read_cold_record(&self.disk, &life.flush, ticket.cold.to_raw())
            else {
                self.report.violations.push(format!(
                    "{when}: drain — shadow twin at {} unreadable",
                    ticket.cold.to_raw()
                ));
                return;
            };
            let same_key = self.twin_is_winner_key(life, &ticket, &image);
            match life.table.verify_shadow(ticket.hash, ticket.cold, &image) {
                inf_store::ShadowVerdict::SameKey if !same_key => self
                    .report
                    .violations
                    .push(format!("{when}: drain — same-key verdict on a collision twin")),
                inf_store::ShadowVerdict::Collision if same_key => self
                    .report
                    .violations
                    .push(format!("{when}: drain — collision verdict on a same-key twin")),
                inf_store::ShadowVerdict::Collision => self.report.shadow_collision += 1,
                inf_store::ShadowVerdict::SameKey
                | inf_store::ShadowVerdict::Stale
                | inf_store::ShadowVerdict::Deferred => {}
            }
        }
        self.report.shadow_drain_checks += 1;
        if life.table.shadow_unverified() != 0 {
            self.report
                .violations
                .push(format!("{when}: drain left {} unverified", life.table.shadow_unverified()));
        }
        if life.table.len() != self.model.len() {
            self.report.violations.push(format!(
                "{when}: DBSIZE EXACTNESS VIOLATION — len {} vs model {} with {} verified \
                 tickets open",
                life.table.len(),
                self.model.len(),
                life.table.shadow_pending()
            ));
        }
    }

    /// Reconciles up to `max` tickets (the MAINTAIN pump played by the
    /// harness) — oldest winner first, the store's own work list.
    pub(super) fn reconcile(&mut self, life: &mut Life, max: usize, when: &str) {
        for read in life.table.shadow_work(max) {
            if Some(read.ticket.cold.to_raw()) == self.held_twin {
                // The reconciler's device error (ADR-0093 D4.3): the
                // ticket stays for the next round — held on purpose.
                life.table.shadow_read_failed(read.ticket.cold);
                continue;
            }
            self.reconcile_ticket(life, read.ticket, when);
        }
    }

    /// ADR-0093 A12 (batch 23): opens one ticket the reconciler will not
    /// resolve this life. Placed mid-phase so the ops that follow seal
    /// and flush the winner past the mutable window before the walk.
    pub(super) fn hold_a_ticket(&mut self, life: &mut Life, rng: &mut SplitMix64) {
        if self.held_twin.is_some() {
            return;
        }
        let keys: Vec<Vec<u8>> = self
            .model
            .keys()
            .filter(|k| !k.starts_with(inf_store::COLLISION_KEY_PREFIX))
            .cloned()
            .collect();
        if keys.is_empty() {
            return;
        }
        let start = (rng.next_u64() % keys.len() as u64) as usize;
        for i in 0..keys.len() {
            let key = &keys[(start + i) % keys.len()];
            let hash = life.table.hash_key(key);
            let inf_store::ShadowProbe::One(cold) = life.table.shadow_probe(key, hash) else {
                continue;
            };
            let before = life.table.shadow_pending();
            let value = vec![(rng.next_u64() % 251) as u8; 48];
            self.apply_op(life, key, Op::SetShadow(value));
            if life.table.shadow_pending() > before {
                self.held_twin = Some(cold.to_raw());
                self.held_key = Some((key.clone(), hash));
                self.report.shadow_held_rows += 1;
                // The row's premise: the winner is sealed and flushed
                // **before** the walk. Twice the mutable window of plain
                // writes on fresh keys — and at least the mutable window
                // and two tier frames, since a live flush claims full
                // frames only (ADR-0056 D5) — pushes it out, then a
                // maintain round seals and flushes it (release stops at
                // the pin); a page more of them per round until it does,
                // where a variant's long records hold the frame back.
                // Asserted — a winner still above `flushed` would be
                // imaged by watermark alone and the row would prove
                // nothing.
                let winner = life
                    .table
                    .shadow_tickets()
                    .find(|t| t.cold == cold)
                    .map(|t| t.winner)
                    .expect("the ticket just opened");
                let mutable = self.spec.live.mutable_target_bytes();
                let mut want = (2 * mutable).max(mutable + 2 * TIER_FRAME_BYTES as u64);
                // Bound: a live budget of filler past the first `want`,
                // a page more per round until the winner is flushed.
                let cap = want + self.spec.live.mem_budget_bytes;
                let mut written = 0u64;
                let mut i = 0u64;
                loop {
                    while written < want {
                        let filler =
                            format!("held:{}:{i}", self.report.shadow_held_rows).into_bytes();
                        // Inline-sized (below `BLOB_THRESHOLD`): a plain SET.
                        let value = vec![(rng.next_u64() % 251) as u8; 200];
                        self.apply_op(life, &filler, Op::Set(value));
                        written += 232;
                        i += 1;
                    }
                    self.maintain(life);
                    if life.table.space().flushed() > winner || want >= cap {
                        break;
                    }
                    want += PAGE;
                }
                if life.table.space().flushed() <= winner {
                    self.report.violations.push(format!(
                        "HELD ROW VACUOUS: the winner at {} is still above the flushed watermark \
                         {} after {i} filler writes and a maintain round",
                        winner.to_raw(),
                        life.table.space().flushed().to_raw()
                    ));
                }
            }
            return;
        }
    }

    /// Reconciles every open ticket; a round that resolves nothing is a
    /// wedged reconciler — a violation, never a spin.
    pub(super) fn reconcile_all(&mut self, life: &mut Life, when: &str) {
        while life.table.shadow_pending() > 0 {
            let before = life.table.shadow_pending();
            self.reconcile(life, 16, when);
            if life.table.shadow_pending() >= before {
                self.report.violations.push(format!(
                    "{when}: reconciliation made no progress with {before} tickets open"
                ));
                return;
            }
        }
    }
}
