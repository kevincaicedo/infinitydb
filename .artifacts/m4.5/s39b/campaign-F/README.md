# S39b campaign F — corrected first-boot recovery and one-slot ratification

Rules written before the run. Campaigns D/E timed the same image twice; the
second, previously binding measurement was already recovered and cleaned. This
campaign runs the product arm (`--segment-recycle-slots 1`) against
`--no-segment-recycle`, three interleaved repetitions, with the existing S39b
shape. After each workload server is killed, its fresh crashed image remains
untouched for 40 seconds and is then booted exactly once. The timer spans that
first launch through `loading:0`; there is no immediate boot and no second boot.

Binding decision: use paired per-replicate arm/baseline recovery ratios and
their median. If the median is `<= 1.05`, every first boot succeeds, and the
correctness suite is green on the campaign commit, explicitly ratify the
one-slot default in ADR-0090, the plan, review ledger and C41. If it is `> 1.05`
or any boot refuses, return the product default to zero; write-reduction evidence
remains recorded but does not override recovery correctness/performance.

The warmed zero-fill `<= 0.1` gate is still expected red at one slot (campaign D
measured 0.38); it remains `Revised` onto ADR-0090 D9 and is not a condition for
this narrowly scoped default-ratification decision. All other S39b columns stay
visible as controls. The block-device counter is host sectors written, not NAND
wear. No recovery number from campaigns D/E is reused.
