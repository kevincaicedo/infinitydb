#!/usr/bin/env bash
# Planted-bug canaries (ADR-0090 D5; F-L19-03 addendum, F-L19-04): each
# `--cfg inf_canary_*` disables one load-bearing rule in the product, and
# the scenario whose oracle owns that rule must go red on the planted
# build — and green on the plain build. A canary the fleet never plants
# proves nothing; `bins/inf-sim/tests/lanes.rs` fails when a cfg in the
# workspace's `check-cfg` list has no row here.
# Three row kinds:
#   `<cfg> <scenario> <expected violation substring> [flags…]` — an
#     `inf-sim` scenario (a sweep where one seed may not reach the rule);
#   `<cfg> crate-test <package> <lib|test:NAME> <test name> [witness…]` —
#     a crate's own test, for a rule whose oracle lives below the simulator
#     (the store-tier index rows, ADR-0139). The planted build must report
#     exactly that test `FAILED`; a build that is red for any other
#     reason — a compile error, another test — is not a catch. Nor is a
#     red whose log carries `VACUOUS`: that is the test's engagement check
#     failing, not its oracle seeing the planted violation. The words after
#     the test name are the row's witness, a phrase of the oracle's own
#     assertion that the planted log must also carry, for a test with more
#     than one way to fail. A row on `inf-sim` builds with `--features
#     dst`, as every simulator recipe does (ADR-0107 D1): its tests do not
#     compile without it;
#   `<cfg> loom <package> <test name> <witness>` — a Loom model in the
#     package's library, for a memory ordering no other oracle can see
#     (ADR-0159 A1.6): built `--cfg loom --cfg <cfg>` in release, judged
#     like a crate-test row, with the plain `--cfg loom` build as the
#     control. The planted log must also carry `<witness>`, a whitespace-
#     free piece of the model's own assertion: a Loom budget panic, the
#     model's VACUOUS check or an `expect` also fail the test, and none of
#     them is the oracle seeing the stale effect.
# Usage: scripts/sim-canaries.sh [seed]
# Fixture mode (scripts/check-scripts-selftest.sh): INF_CANARY_ROWS_FILE
# replaces the row table and INF_CANARY_CARGO the cargo binary.
set -euo pipefail
cd "$(dirname "$0")/.."
seed=${1:-0xC0FFEE}
CARGO=${INF_CANARY_CARGO:-cargo}

rows=(
  # ADR-0090 D5: the segment-blind scanner takes a foreign segment's
  # residue for this life's frames — an honest power-cut image of a
  # recycling log then refuses boot (RECYCLED RESIDUE REFUSED).
  "inf_canary_foreign_segment m2-recycle RESIDUE --sweep 64 --out target/canary-out"
  # F-L19-04: GET answers the stored value with one byte appended. The
  # shared-store replay cannot see it (both sides run the same code); the
  # shadow model (bins/inf-sim/src/harness/shadow.rs) must.
  "inf_canary_reply_lie m0-smoke divergence"
  # ADR-0129 A1 (batch 72): the durable and tiered runners keep their own
  # oracle — each writer expects the reply its own write sequence implies
  # (`REPLY VIOLATION`); the same cfg lies in the tiered read path too
  # (`plane/tiered.rs::read_value`), so both rows prove those teeth.
  "inf_canary_reply_lie m2-durable REPLY"
  "inf_canary_reply_lie m4-tiered REPLY"
  # ADR-0139 D2 rule 2 at the wire tier: the alias view is always empty,
  # so `DEL` of one alias takes the shared entry from the survivor — the
  # `m45-backfill` alias leg's from-scratch truth must see it.
  "inf_canary_alias_blind m45-backfill alias"
  # ADR-0139 — index identity under a hash alias, one plant per rule.
  # D2 rule 2: the alias view is always empty.
  "inf_canary_alias_blind crate-test inf-store test:index_alias alias_pair_equal_value_delete_one_keeps_the_survivor_indexed"
  "inf_canary_alias_blind crate-test inf-store test:index_alias_model tree_equals_the_model_over_a_collision_dense_universe"
  "inf_canary_alias_blind crate-test inf-store test:index_sidecar a_collision_history_survives_backfill_sidecar_replay_and_flush"
  # D4: coverage decided by hash — an alias's death is taken for the
  # write-set key's.
  "inf_canary_cover_by_hash crate-test inf-store test:index_alias an_evicted_alias_of_a_write_set_key_is_not_the_write_set_key"
  # D9: a 22-bit fragment match is taken as an alias.
  "inf_canary_alias_no_rehash crate-test inf-store test:index_alias a_fragment_neighbour_is_never_treated_as_an_alias"
  # D4: an evicted write-set key skips its hook (the rule before ADR-0139).
  "inf_canary_gate_covered crate-test inf-store test:index_alias gate_deaths_inside_a_bracket_converge_in_both_orders"
  "inf_canary_gate_covered crate-test inf-store test:index_alias pruned_bracket_whose_document_is_evicted_at_the_gate_leaves_no_entry"
  # D4 row 3: a covered death leaves the prune mask set.
  "inf_canary_prune_survives_death crate-test inf-store test:index_alias pruned_bracket_whose_document_the_body_reaps_leaves_no_entry"
  # D4: forgotten entries are skipped in place, not compacted.
  "inf_canary_forget_in_place crate-test inf-store test:index_alias forget_is_compacted_before_the_diff_dedups"
  # D3: the COPY mini-bracket aborts on a refused copy.
  "inf_canary_copy_abort_after_death crate-test inf-store test:index_alias copy_refused_after_reaping_its_expired_target_removes_the_targets_entries"
  "inf_canary_copy_abort_after_death crate-test inf-store test:index_alias cross_db_copy_refused_after_reaping_its_expired_target_removes_the_targets_entries"
  # D9: the held mark lands on one duplicate only.
  "inf_canary_held_on_one crate-test inf-store test:index_alias held_mark_covers_a_duplicated_write_set_key"
  "inf_canary_held_on_one crate-test inf-store test:index_alias wildcard_alias_holding_a_subset_keeps_exactly_the_shared_keys"
  # D9: an equal run is re-marked for every key a member emits — N × N
  # marks for a value repeated N times on both sides (both removal paths).
  "inf_canary_held_remark crate-test inf-store test:index_alias a_repeated_value_is_marked_held_once_by_the_bracket"
  "inf_canary_held_remark crate-test inf-store test:index_alias a_repeated_value_is_marked_held_once_by_the_death_hook"
  # D9: an excluded record's fetch is not charged to the re-hash budget.
  "inf_canary_fetch_uncharged crate-test inf-store lib an_excluded_record_is_a_charged_fetch"
  "inf_canary_fetch_uncharged crate-test inf-store test:index_alias a_write_set_of_aliases_is_charged_per_record_fetch"
  # D10: the death side shrinks without clearing (a no-op past the length).
  "inf_canary_death_scratch_kept crate-test inf-store lib death_scratch_is_released_after_a_large_death_and_a_large_backfill"
  "inf_canary_write_set_split_cap crate-test inf-store lib write_set_table_retention_is_one_combined_bound"
  # D9: every walk budget is ignored.
  "inf_canary_walk_unbounded crate-test inf-store test:index_alias alias_group_of_eight_serves_and_of_nine_degrades"
  # D9: the group budget is kept by the callback, not the traversal.
  "inf_canary_walk_callback_cap crate-test inf-store lib the_long_chain_state_loads_exactly_the_group_budget"
  # D12: the participating set is filled lazily, per evaluated index.
  "inf_canary_lazy_participation crate-test inf-store test:index_alias a_create_that_floods_one_index_degrades_every_participating_index"
  # ADR-0078 A2: the loader skips the maintenance-rules compare.
  "inf_canary_sidecar_rules_ignored crate-test inf-store test:index_sidecar a_sidecar_written_under_older_maintenance_rules_is_discarded_and_rebuilt"
  # ADR-0099 A1 — the JSON reply account, one plant per rule the types
  # cannot carry; the registry-iterating reply oracle owns all four.
  # R2: `commit_delta` runs the effect before the builder.
  "inf_canary_json_reply_after_effect crate-test inf-server test:json_commands every_json_reply_is_charged_and_refused_before_its_effect"
  # R4: a bulk reply charges nothing.
  "inf_canary_json_reply_uncharged crate-test inf-server test:json_commands every_json_reply_is_charged_and_refused_before_its_effect"
  # R4: a bulk reply charges its payload but not its header and CRLF.
  "inf_canary_json_reply_framing_uncharged crate-test inf-server test:json_commands every_json_reply_is_charged_and_refused_before_its_effect"
  # R5: root `JSON.DEL` deletes before it reserves its reply.
  "inf_canary_json_fixed_unreserved crate-test inf-server test:json_commands every_json_reply_is_charged_and_refused_before_its_effect"
  # ADR-0008 A1 — one wheel node per key hash; a refused key is swept.
  # Rule 3 (I1): every changed deadline allocates a node; the old one lingers.
  "inf_canary_wheel_arm_per_change crate-test inf-store test:expiry one_key_keeps_one_wheel_node_under_every_ttl_rewrite"
  "inf_canary_wheel_arm_per_change crate-test inf-store test:expiry wheel_matches_reference_model_under_churn"
  # Rule 4: every removal leaves a tombstone (no successor copy) — O1's
  # one-tombstone-per-list bound.
  "inf_canary_wheel_tombstone_every_removal crate-test inf-store test:expiry one_key_keeps_one_wheel_node_under_every_ttl_rewrite"
  # Rule 2: a death path skips the schedule transition — O1's stale fire.
  "inf_canary_wheel_death_skips_transition crate-test inf-store test:expiry one_key_keeps_one_wheel_node_under_every_ttl_rewrite"
  # Rule 3's crossing: a refused placement does not owe the sweep (lazy-only).
  "inf_canary_wheel_refused_lazy crate-test inf-store test:expiry refused_keys_expire_actively_at_the_node_cap"
  "inf_canary_wheel_refused_lazy crate-test inf-store test:expiry a_small_node_budget_matches_the_reference_model_under_churn"
  "inf_canary_wheel_refused_lazy m1-cache RETAINED --wheel-nodes-max 64"
  # Rule 4: removal skips the group enumeration (a hash taken for a key).
  "inf_canary_wheel_release_by_hash crate-test inf-store test:expiry a_colliding_hash_keeps_its_node_when_its_twin_dies"
  # Rule 5 and I10: a fire reaps only its first expired member and files
  # its answer unclamped — into the slot being drained.
  "inf_canary_wheel_refile_at_or_before_now crate-test inf-store test:expiry colliding_keys_with_one_deadline_both_expire_actively"
  # Rule 6 (I11): only the sweep's own refusals keep a pass from idling.
  "inf_canary_sweep_owed_by_own_refusals crate-test inf-store test:expiry a_write_refused_behind_the_sweep_cursor_is_reaped"
  # ADR-0139 D9 through the schedule: a walk that never reports `Over`
  # leaves a ninth alias unscheduled and unswept.
  "inf_canary_walk_unbounded crate-test inf-store test:expiry a_ttl_alias_group_of_nine_owes_the_sweep"
  # ADR-0159 A1.6 — the orderings of late issuance, one plant each; the
  # Loom effect witness must see an effect written before an issue go
  # missing after a request that covers it.
  # The issue is Relaxed: issues no longer form a happens-before chain.
  "inf_canary_issue_clock_relaxed loom inf-foundation loom_an_effect_before_the_issue_is_visible_after_the_request A1.6:"
  # A request word is raised with a Relaxed RMW.
  "inf_canary_request_raise_relaxed loom inf-foundation loom_an_effect_before_the_issue_is_visible_after_the_request A1.6:"
  # A request word is read with a Relaxed load.
  "inf_canary_request_read_relaxed loom inf-foundation loom_an_effect_before_the_issue_is_visible_after_the_request A1.6:"
  # ADR-0159 A1.4: `WAIT CELL k`'s confirmation skips its LASTSAVE floor
  # raise, so a LASTSAVE after the WAIT trails the checkpoint it fenced.
  "inf_canary_lastsave_floor_skipped crate-test inf-server lib lastsave_after_wait_cell_covers_the_fenced_checkpoint trails the WAIT"
  "inf_canary_lastsave_floor_skipped crate-test inf-server lib lastsave_and_the_info_gauge_answer_one_value_after_a_wait_cell is below slot"
  # The confirmation raises the floor but does not write the cell's
  # `LastSave`: both surfaces stay below the checkpoint the WAIT fenced.
  "inf_canary_lastsave_cell_stale crate-test inf-server lib lastsave_and_the_info_gauge_answer_one_value_after_a_wait_cell is below slot"
  # INFO's `rdb_last_save_time` renders the sweep's term alone: after a
  # `WAIT CELL k` that confirms ahead of the sweep it differs from LASTSAVE.
  "inf_canary_info_lastsave_from_observation crate-test inf-server lib lastsave_and_the_info_gauge_answer_one_value_after_a_wait_cell rdb_last_save_time and LASTSAVE differ on one cell"
  "inf_canary_info_lastsave_from_observation crate-test inf-sim lib lastsave_and_info_answer_one_second_when_a_wait_confirms_ahead_of_the_sweep rdb_last_save_time and LASTSAVE differ on one cell"
  # The park guard without its own-slot term (interfaces-m2.md, "Cells
  # never fold the whole board"): a cell parks on top of a `WAIT` its own
  # publication satisfied.
  "inf_canary_ckpt_park_guard_own_slot_skipped crate-test inf-sim lib an_own_publication_with_a_waiter_does_not_park_before_its_wake a parked iteration between the publication and its wake"
  # The guard without its cursor term: beyond 64 cells a cell with a waiter
  # parks while its sweep is part-way.
  "inf_canary_ckpt_park_guard_cursor_skipped crate-test inf-sim lib a_part_way_sweep_holds_a_waiting_cell_awake a waiting cell parked while its sweep was part-way"
  # Every cell's sweep watches slot 0: the term is back to nothing on every
  # other cell of a multi-cell node.
  "inf_canary_ckpt_sweep_own_slot_zero crate-test inf-sim lib an_own_publication_on_a_peer_cell_does_not_park_before_its_wake a parked iteration between an own publication on cell 1 and its wake"
  "inf_canary_ckpt_park_guard_own_slot_skipped crate-test inf-sim lib an_own_publication_on_a_peer_cell_does_not_park_before_its_wake a parked iteration between an own publication on cell 1 and its wake"
  # A completed sweep leaves `own_seen` behind: the own-slot term never
  # ends, and a cell with an unsatisfied waiter never parks.
  "inf_canary_ckpt_own_seen_not_advanced crate-test inf-sim lib a_waiter_the_own_publication_does_not_satisfy_lets_the_cell_park the cell never parks after its publication"
  "inf_canary_ckpt_own_seen_not_advanced crate-test inf-server lib an_own_publication_is_unobserved_until_a_sweep_begun_after_it_completes the own-slot term never ended"
  # The own slot loaded as the cursor passes it, not at the sweep's start:
  # beyond 64 cells the term ends over a peer the sweep read too early.
  "inf_canary_ckpt_own_loaded_at_cursor crate-test inf-server lib an_own_publication_is_unobserved_until_a_sweep_begun_after_it_completes the term ended over a slot read before its publication"
  # ADR-0178 — an offer above its class cap is issued by a counted overrun.
  # The answer before it, "not this slice" for ever: the budget scenario's
  # arm (seeds ≡ 1 mod 4; the sweep reaches 0xC0FFF1) must see an oversized
  # checkpoint block wait past T_ckpt, and R1 and R2 must fail.
  "inf_canary_unattainable_deferred m2-device-budget T_ckpt --sweep 4"
  "inf_canary_unattainable_deferred crate-test inf-runtime lib a_checkpoint_block_above_the_class_cap_is_issued_within_its_bound"
  "inf_canary_unattainable_deferred crate-test inf-server test:node_e2e a_checkpoint_holding_a_value_above_the_class_cap_completes"
  # The overrun granted from any held credit: an attainable offer beside a
  # same-class overrunner starves (the class oracle's I12).
  "inf_canary_overrun_any_held crate-test inf-runtime lib no_background_offer_waits_past_its_bound"
  # A grant ends the rest pass and its refund leaves it ended: every
  # overrun behind a zero-work sibling starves (I14).
  "inf_canary_grant_clears_rest crate-test inf-runtime lib no_background_offer_waits_past_its_bound"
  # ADR-0178 D2: the checkpoint keep-up term floored on its own before the
  # max drops up to (α − 1)/α byte a refill near the crossover.
  "inf_canary_keepup_truncates crate-test inf-runtime lib the_keepup_floor_keeps_its_remainder_at_the_crossover"
  "inf_canary_keepup_truncates crate-test inf-runtime lib no_background_offer_waits_past_its_bound"
  # ADR-0174 D2 rule 6: `room` answers `Demote(tail)` where the need lies
  # above the tail — no pad. The room property judges every demote target
  # (a page multiple above the head, at or below the tail) and the placed
  # record by the window inequality written out in the test.
  "inf_canary_replay_no_pad crate-test inf-store lib room_reaches_fits_in_four_asks_exhaustively_at_a_four_unit_page"
  "inf_canary_replay_no_pad crate-test inf-store lib room_pads_the_hostile_specs_at_the_commit_page"
  # The same plant in the DST's spec-variant class (m4-recovery): a boot
  # at a window below its ring meets a need above its tail and, unpadded,
  # asks a fifth time. Case (a)'s window is above half its ring, where no
  # page pad is placed, so this row is red on a ring-top need.
  "inf_canary_replay_no_pad m4-recovery RoomAsks --spec-variant ring-top"
  # Case (b)'s answer alone removed — a page pad's need demotes to the
  # tail, the ring-top pad kept: the page case asks a fifth time on a page
  # need.
  "inf_canary_replay_no_page_pad m4-recovery RoomAsks --spec-variant page"
  # ADR-0174 D3: `ColdKey`'s constructor checks nothing — another key's
  # record, a type tag of 0 and a length past the file each parse, and the
  # rebuild answers "distinct" (or `lookup`'s own hash check, in debug)
  # where the identity refusal is expected.
  "inf_canary_replay_settle_unchecked crate-test inf-store test:tiered_shadow a_rebuilt_slot_settles_only_on_a_verified_record_of_its_hash"
  # ADR-0174 D1: the replay entry answers a `Demote` with the refusal HEAD
  # made — a tail above the window fails the boot again.
  "inf_canary_replay_no_demote crate-test inf-store test:tiered_recovery a_tail_of_three_windows_replays_into_the_recovered_table Store(OutOfMemory)"
  # ADR-0174 D3 R7 skipped at the seal: a sealed winner leaves its
  # same-key cold twin, which the lookup serves — the key census.
  "inf_canary_replay_seal_no_settle crate-test inf-store test:tiered_replay a_shadow_pair_in_the_unit_settles_at_the_sealed_winner (a)"
  # ADR-0174 D3 R10's end settle skipped: the rebuild tickets same-key pairs in a
  # namespace that demoted — census (d).
  "inf_canary_replay_no_end_settle crate-test inf-store test:tiered_replay rewrites_still_open_at_the_end_of_replay_are_settled_before_ready (d)"
  # ADR-0174 D3 R6's reads skipped: a replayed DEL leaves the key's demoted
  # copy, which serves — census (a).
  "inf_canary_replay_del_no_verify crate-test inf-store test:tiered_replay deletes_in_the_tail_resolve_against_demoted_copies (a)"
  # The same plant in the DST: m4-tiered's class deletes aged keys whose
  # copies the reboot demoted; a resurrected one is outside the admissible
  # set (the smoke seed draws three windows, so its keys age).
  "inf_canary_replay_del_no_verify m4-tiered DURABILITY --replay-above-window"
  # The same plant at the shipped topology: the four-cell binary's replayed
  # deletes of keys written over a window ago leave the demoted copies.
  "inf_canary_replay_del_no_verify crate-test infinityd test:replay_spill a_tail_above_every_cells_window_boots_under_fsync_always DELETED KEY PRESENT"
  # ADR-0174 D3 R8: the boot's ref settle chains nothing, so the next
  # delete stages no marker for it and the key resurrects on the second
  # boot — census (a).
  "inf_canary_replay_origin_drop crate-test inf-store test:tiered_replay a_ref_settled_by_boot_one_stays_deleted_across_a_second_crash (a)"
  # The same rule where a dead tail copy of the key lies below the live
  # one: the ref must ride the live copy's origins, or it resurrects.
  "inf_canary_replay_origin_drop crate-test inf-store test:tiered_replay a_ref_settles_into_the_live_tail_copy_not_the_dead_one_below_it (a)"
  # The same plant in the DST's two-crash row (m4-recovery's class, last
  # life): the live DEL stages no marker for the settled ref, so the ref
  # outlives the second boot's tail and the deleted key returns.
  "inf_canary_replay_origin_drop m4-recovery outlived --replay-above-window"
  # ADR-0174 D3 R9 skipped: the settled ref's blob reference stays
  # with no slot — the blob census.
  "inf_canary_replay_blob_release_skip crate-test inf-store test:tiered_replay a_blob_ref_settled_during_image_load_is_released_at_the_end_of_the_checkpoint reference"
  # ADR-0174 D3 R8: the ref arm charges the record's bytes — a recovered
  # file above its true dead bytes (the dead-byte census).
  "inf_canary_replay_ref_settle_charges crate-test inf-store test:tiered_replay a_death_the_crashed_life_charged_is_not_charged_again over-counts"
  "inf_canary_replay_ref_settle_charges crate-test inf-store test:tiered_replay a_death_the_crashed_life_charged_by_a_del_of_a_blind_set_pair_is_not_charged_again over-counts"
  # ADR-0174 D3 R11 skipped: a live-set entry names a boot file and
  # overwrites its counters — a boot file that is not byte-exact.
  "inf_canary_replay_restore_unguarded crate-test inf-store test:tiered_replay a_live_set_entry_naming_a_boot_file_restores_nothing byte-exact"
  # ADR-0174 D5: the hand-over closes the boot-sealed handles instead
  # of returning them — fewer handles than sealed files.
  "inf_canary_replay_handles_dropped crate-test inf-store test:tiered_replay committed_pages_around_the_window_decide_whether_the_boot_demotes one handle per sealed file"
  # ADR-0174 D2 rule 5: the step seals the file to free the partial frame
  # — a boot file with the stall reason (the seal-reason census).
  "inf_canary_replay_stall_seal crate-test inf-store test:tiered_replay a_rewritten_key_keeps_one_slot_with_the_newest_value Stall"
  # The same plant breaks the barrier arithmetic: a seal the writer made
  # that the boot pipeline's counters do not hold.
  "inf_canary_replay_stall_seal crate-test inf-store test:tiered_replay record_lengths_and_slices_demote_with_one_barrier_per_step_and_per_seal the counter is the writer's"
  # The same plant in the DST's spec-variant class, each case: m4-recovery's
  # boot-file census reads every boot file's footer from the tier directory
  # — a boot file with the stall reason.
  "inf_canary_replay_stall_seal m4-recovery SEAL-REASON --spec-variant ring-top"
  "inf_canary_replay_stall_seal m4-recovery SEAL-REASON --spec-variant page"
  # ADR-0174 D4 skipped: a namespace no manifest section names keeps its
  # dead-life tier files, and the first flush after the boot fails on the
  # existing file (tier creation is `create_new`).
  "inf_canary_replay_no_section_gc crate-test inf-server test:recover_no_section a_namespace_without_a_manifest_section_removes_its_dead_life_files_before_the_first_flush AlreadyExists"
  # The same plant under a power cut of a demoting boot: the cut boot's
  # files outlive it and the next boot's first demote step refuses on one
  # (ADR-0174 D5: a crash during a demoting boot changes nothing the next
  # boot recovers).
  "inf_canary_replay_no_section_gc crate-test inf-server test:recover_replay_steps a_demoting_boot_cut_at_each_point_once_or_twice_recovers_the_uncut_boot the next boot refused"
  # ADR-0174 R10: the driver hands the end settle no budget, so one step
  # walks the whole open span and the last step's charge passes its
  # budget, one record's unit and the hand-over's drain.
  "inf_canary_replay_settle_unbudgeted crate-test inf-server test:recover_replay_steps every_step_yields_at_the_first_boundary_where_its_reads_and_charge_reach_the_budget SETTLE BUDGET VIOLATION"
  # The same rule at twice the budget: at 8 MiB the whole settle is the
  # last step, so the hand-over's drain bound (one barrier and the
  # rebuild's reads) is what sees it.
  "inf_canary_replay_settle_doubled crate-test inf-server test:recover_replay_steps every_step_yields_at_the_first_boundary_where_its_reads_and_charge_reach_the_budget SETTLE BUDGET VIOLATION"
  # The same oracle's other two rules: a frame's yield that leaves the
  # charge out (at 8 MiB a step spans several demote steps), and a driver
  # that charges no boot I/O (a step wrote tier bytes uncharged).
  "inf_canary_replay_yield_uncharged crate-test inf-server test:recover_replay_steps every_step_yields_at_the_first_boundary_where_its_reads_and_charge_reach_the_budget passes the budget by more than one frame's unit"
  "inf_canary_replay_charge_dropped crate-test inf-server test:recover_replay_steps every_step_yields_at_the_first_boundary_where_its_reads_and_charge_reach_the_budget tier files grew under a charge of 0"
)
if [ -n "${INF_CANARY_ROWS_FILE:-}" ]; then
  [ -f "$INF_CANARY_ROWS_FILE" ] || { echo "sim-canaries: no rows file $INF_CANARY_ROWS_FILE"; exit 2; }
  rows=()
  while IFS= read -r line; do [ -n "$line" ] && rows+=("$line"); done < "$INF_CANARY_ROWS_FILE"
fi
[ "${#rows[@]}" -gt 0 ] || { echo "sim-canaries: SCOPE ERROR — no canary rows"; exit 1; }

fail=0
log=$(mktemp)
[ -n "$log" ] && [ -f "$log" ] || { echo "sim-canaries: mktemp failed"; exit 2; }
trap '[ -n "$log" ] && [ -f "$log" ] && rm -f "$log"' EXIT

# crate_test <cfg> <package> <lib|test:NAME> <test name> <witness, or empty>
crate_test() {
  local cfg=$1 package=$2 target=$3 name=$4 witness=$5 target_args
  case "$target" in
    lib) target_args=(--lib) ;;
    test:*) target_args=(--test "${target#test:}") ;;
    *) echo "   SCOPE ERROR: target '$target' is neither lib nor test:NAME"; fail=1; return ;;
  esac
  if [ "$package" = inf-sim ]; then target_args+=(--features dst); fi
  local verdict="^test ([A-Za-z0-9_]+::)*${name} \\.\\.\\. "
  echo "== canary $cfg: $package $target $name on the planted build must go red"
  if RUSTFLAGS="--cfg $cfg" "$CARGO" test -p "$package" "${target_args[@]}" \
      --target-dir "target/canary-crate" -- "$name" > "$log" 2>&1; then
    echo "   NOT CAUGHT: the planted build ran green (the oracle has no teeth)"
    fail=1
  elif ! grep -Eq -- "${verdict}FAILED" "$log"; then
    echo "   red for another reason (expected 'test $name ... FAILED'):"
    tail -5 "$log"
    fail=1
  elif grep -Fq -- "VACUOUS" "$log"; then
    echo "   red for another reason (the test's engagement check, not its oracle):"
    grep -F -m1 -- "VACUOUS" "$log" | cut -c1-160
    fail=1
  elif [ -n "$witness" ] && ! grep -Fq -- "$witness" "$log"; then
    echo "   red for another reason (expected the oracle's assertion '$witness'):"
    tail -5 "$log"
    fail=1
  elif [ -n "$witness" ]; then
    echo "   caught: $(grep -F -m1 -- "$witness" "$log" | cut -c1-160)"
  else
    echo "   caught: $(grep -E -m1 -- "${verdict}FAILED" "$log")"
  fi
  echo "== canary $cfg: $package $target $name on the plain build must stay green"
  if ! "$CARGO" test -p "$package" "${target_args[@]}" -- "$name" > "$log" 2>&1 \
      || ! grep -Eq -- "${verdict}ok" "$log"; then
    echo "   the plain build does not pass '$name' (a canary needs a green control):"
    tail -5 "$log"
    fail=1
  fi
}

# loom_test <cfg> <package> <test name> <witness>: the model under
# `--cfg loom`, planted and plain, each in its own target dir (RUSTFLAGS
# differ).
loom_test() {
  local cfg=$1 package=$2 name=$3 witness=$4
  local verdict="^test ([A-Za-z0-9_]+::)*${name} \\.\\.\\. "
  echo "== canary $cfg: $package loom $name on the planted build must go red"
  if RUSTFLAGS="--cfg loom --cfg $cfg" LOOM_MAX_PREEMPTIONS=3 "$CARGO" test -p "$package" \
      --release --lib --target-dir "target/canary-loom" -- "$name" > "$log" 2>&1; then
    echo "   NOT CAUGHT: the planted build ran green (the model has no teeth)"
    fail=1
  elif ! grep -Eq -- "${verdict}FAILED" "$log"; then
    echo "   red for another reason (expected 'test $name ... FAILED'):"
    tail -5 "$log"
    fail=1
  elif ! grep -Fq -- "$witness" "$log"; then
    echo "   red for another reason (expected the model's assertion '$witness'):"
    tail -5 "$log"
    fail=1
  else
    echo "   caught: $(grep -F -m1 -- "$witness" "$log" | cut -c1-160)"
  fi
  echo "== canary $cfg: $package loom $name on the plain loom build must stay green"
  if ! RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 "$CARGO" test -p "$package" --release \
      --lib --target-dir "target/loom" -- "$name" > "$log" 2>&1 \
      || ! grep -Eq -- "${verdict}ok" "$log"; then
    echo "   the plain loom build does not pass '$name' (a canary needs a green control):"
    tail -5 "$log"
    fail=1
  fi
}

sim_built=0
for row in "${rows[@]}"; do
  # shellcheck disable=SC2206
  parts=($row)
  cfg=${parts[0]}
  if [ "${parts[1]}" = crate-test ]; then
    [ "${#parts[@]}" -ge 5 ] || { echo "sim-canaries: SCOPE ERROR — malformed row: $row"; exit 1; }
    crate_test "$cfg" "${parts[2]}" "${parts[3]}" "${parts[4]}" "${parts[*]:5}"
    continue
  fi
  if [ "${parts[1]}" = loom ]; then
    [ "${#parts[@]}" -eq 5 ] || { echo "sim-canaries: SCOPE ERROR — malformed row: $row"; exit 1; }
    loom_test "$cfg" "${parts[2]}" "${parts[3]}" "${parts[4]}"
    continue
  fi
  name=${parts[1]}; expect=${parts[2]}; flags=("${parts[@]:3}")
  if [ "$sim_built" -eq 0 ]; then
    "$CARGO" build --release -p inf-sim --features dst --bin inf-sim
    sim_built=1
  fi
  plain=target/release/inf-sim
  target="target/canary-$cfg"
  echo "== canary $cfg: building inf-sim with --cfg $cfg into $target"
  RUSTFLAGS="--cfg $cfg" "$CARGO" build --release -p inf-sim --features dst --bin inf-sim \
    --target-dir "$target"
  planted="$target/release/inf-sim"
  echo "== canary $cfg: $name (seed $seed) on the planted build must go red"
  if "$planted" --scenario "$name" --seed "$seed" "${flags[@]}" > "$log" 2>&1; then
    echo "   NOT CAUGHT: the planted build ran green (the oracle has no teeth)"
    fail=1
  elif ! grep -q -- "$expect" "$log"; then
    echo "   red for another reason (expected a violation mentioning '$expect'):"
    tail -5 "$log"
    fail=1
  else
    echo "   caught: $(grep -m1 -- "$expect" "$log" | cut -c1-160)"
  fi
  echo "== canary $cfg: $name on the plain build must stay green"
  "$plain" --scenario "$name" --seed "$seed" "${flags[@]}" > /dev/null
done
if [ "$fail" -ne 0 ]; then echo "sim-canaries: FAILED"; exit 1; fi
echo "sim-canaries: ${#rows[@]} canaries caught, plain builds green (seed $seed)"
