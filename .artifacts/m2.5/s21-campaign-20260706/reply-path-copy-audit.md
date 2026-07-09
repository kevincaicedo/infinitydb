# M2.5-S21 reply-path copy audit (2026-07-06, code audit — file:line verified)

**Rule audited (L1 single-copy):** a value answering a remote op crosses the
fabric exactly once, and is copied a bounded, known number of times end to end.

## Verdict: the fabric-crossing rule HOLDS; origin side pays 3 bounded copies

Owner side (value -> wire):
1. store value -> `reply_scratch` staging (`plane.rs` `handle_fabric_op`
   Op::Read/`execute_owned_into` — replies staged as byte ranges because the
   drain mutably borrows the fabric).
2. `reply_scratch` -> per-destination pack (`CellFabric::reply` -> `encode`).
3. pack -> ring slot at seal (`FabricMsg::from_frame`; frames > 62 B spill to
   one heap box, counted by `spilled_frames`).
The ring crossing itself happens once: pack-coalesced slots, published with
one Release store per destination per flush.

Origin side (ring slot -> client):
1. ring slot -> pooled reply buf (`plane.rs:1771` `buf.extend_from_slice`) —
   forced by borrow lifetime: `Outcome::Bytes` borrows the slot the drain
   releases; the pooled buf is the only owned holding spot until the pump
   resumes. Buffer is pooled (`take_reply_buf`/`recycle_reply_buf`) — no
   allocation steady-state.
2. pooled buf -> `conn.out` (`plane.rs:2057`) at pump resume — pipelined
   replies must concatenate in order per connection.
3. `conn.out` -> leased send buffer (`plane.rs:1694`) at RESPOND — the wire
   write custody rule (send buffers are pool leases; `conn.out` is not).

## Cost accounting at the campaign shape (64 B values)

3 origin copies x 64 B ~= 20-40 ns combined vs a measured per-op remote
residual of ~2-3 us: copies are < 2% of the hop cost at this value size.
Eliminating copy 1 requires deferring ring-slot release past EXECUTE
(shrinks effective ring capacity under load); copy 2 requires vectored
conn output (iovec chains through RESPOND custody rules). Both are recorded
as candidates for a large-value workload, NOT taken for the 64 B campaign
shape — the decomposition shows the budget lives elsewhere (scheduling
latency of the hop across loop iterations, not memcpy).

Disposition: **audit passed, no code change** (documented candidates for
M5-era large-value work).
