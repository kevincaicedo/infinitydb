#!/usr/bin/env bash
# Planted-bug canaries (ADR-0090 D5; F-L19-03 addendum, F-L19-04): each
# `--cfg inf_canary_*` disables one load-bearing rule in the product, and
# the scenario whose oracle owns that rule must go red on the planted
# build — and green on the plain build. A canary the fleet never plants
# proves nothing; `bins/inf-sim/tests/lanes.rs` fails when a cfg in the
# workspace's `check-cfg` list has no row here.
# Two row kinds:
#   `<cfg> <scenario> <expected violation substring> [flags…]` — an
#     `inf-sim` scenario (a sweep where one seed may not reach the rule);
#   `<cfg> crate-test <package> <lib|test:NAME> <test name>` — a crate's
#     own test, for a rule whose oracle lives below the simulator (the
#     store-tier index rows, ADR-0139). The planted build must report
#     exactly that test `FAILED`; a build that is red for any other
#     reason — a compile error, another test — is not a catch.
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

# crate_test <cfg> <package> <lib|test:NAME> <test name>
crate_test() {
  local cfg=$1 package=$2 target=$3 name=$4 target_args
  case "$target" in
    lib) target_args=(--lib) ;;
    test:*) target_args=(--test "${target#test:}") ;;
    *) echo "   SCOPE ERROR: target '$target' is neither lib nor test:NAME"; fail=1; return ;;
  esac
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

sim_built=0
for row in "${rows[@]}"; do
  # shellcheck disable=SC2206
  parts=($row)
  cfg=${parts[0]}
  if [ "${parts[1]}" = crate-test ]; then
    [ "${#parts[@]}" -eq 5 ] || { echo "sim-canaries: SCOPE ERROR — malformed row: $row"; exit 1; }
    crate_test "$cfg" "${parts[2]}" "${parts[3]}" "${parts[4]}"
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
