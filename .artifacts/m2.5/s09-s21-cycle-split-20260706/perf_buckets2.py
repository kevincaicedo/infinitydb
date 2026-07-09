import re, sys

BUCKETS = [
    ("parse (inf-wire)",      ["inf_wire::"]),
    ("store hash/probe/record (inf-store)", ["inf_store::"]),
    ("execute+serialize (exec/RespWriter)", ["inf_server::exec", "RespWriter"]),
    ("fabric codec+mesh (inf-fabric)", ["inf_fabric::"]),
    ("fabric plane: pump/dispatch/send/replies", ["pump", "send_apply", "handle_fabric_op", "dispatch_one", "render_outcome", "pop_or_quiesce", "OwnedCmd"]),
    ("executor/tasks/wakers (inf-runtime)", ["inf_runtime::executor", "inf_runtime::gate", "waker", "Waker", "RawTask", "poll_shim"]),
    ("reactor loop", ["reactor::", "run_iteration"]),
    ("driver/uring user side", ["inf_runtime::uring", "io_uring", "UringDriver"]),
    ("kernel", ["[k]", "kallsyms"]),
    ("libc mem/alloc", ["memmove", "memcpy", "malloc", "_int_free", "cfree", "memset", "calloc"]),
    ("plane other (parse_execute/conn/respond)", ["inf_server::plane", "infinityd"]),
]

def bucketize(path):
    tot = {}
    other = []
    total = 0.0
    for line in open(path, errors="replace"):
        m = re.match(r"\s+(\d+\.\d+)%\s+\[(.)\]\s+(.*)", line)
        if not m:
            continue
        pct, mode, sym = float(m.group(1)), m.group(2), m.group(3).strip()
        total += pct
        if mode == "k":
            tot["kernel"] = tot.get("kernel", 0.0) + pct
            continue
        placed = False
        for name, pats in BUCKETS:
            if any(p in sym for p in pats):
                tot[name] = tot.get(name, 0.0) + pct
                placed = True
                break
        if not placed:
            other.append((pct, sym))
    return tot, other, total

for leg in ["natural", "local"]:
    tot, other, total = bucketize(f"{sys.argv[1]}/{leg}-perf-agg.txt")
    print(f"== {leg} (reported {total:.1f}%)")
    for name, _ in BUCKETS:
        if name in tot:
            print(f"  {tot[name]:6.2f}%  {name}")
    osum = sum(p for p, _ in other)
    print(f"  {osum:6.2f}%  other  (top: {sorted(other, reverse=True)[:6]})")
