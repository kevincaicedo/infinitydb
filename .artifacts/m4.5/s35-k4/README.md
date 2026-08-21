# INVALID FOR ANY DEVICE CLAIM — tmpfs run (2026-08-21 13:13–13:23)

This `gate-run m2 --only-always --reference-box` report ran with
`--data-root /mnt/nvme/bench-data`, a path that does not exist on this
host, while the `m2` flow read only `--pressure-data-root` (default: the
system temp dir, tmpfs here). Every FUA frame was written to tmpfs, which
accepts `O_DIRECT` and `RWF_DSYNC` and does nothing with them — the
"barriers" (113–135 µs p50) are memcpys and the 1.72–1.77 M ops/s is the
engine with no device in the loop (a useful CPU-ceiling number, not a
durability number). The `always ≥ 300k w/s` PASS here binds nothing.

The tooling now refuses this shape before any row runs (`0f990be`: a
binding or FUA run on a memory filesystem is an error; the `m2` flow
honours `--data-root`). The binding S35 evidence is
`.artifacts/m4.5/s35-gate/`.
