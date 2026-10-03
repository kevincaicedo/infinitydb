# Deploying InfinityDB

> [!WARNING]
> InfinityDB is **alpha** software: **single-node, with no authentication or
> TLS**, and its on-disk formats may still change between releases. It
> listens on all interfaces (there is no bind option yet): run it only on
> localhost or on a trusted private network, and do not use it as a source of
> truth.

- [Requirements](#requirements)
- [Running with Docker](#running-with-docker)
- [The io_uring / seccomp requirement](#the-io_uring--seccomp-requirement)
- [Running a prebuilt binary (after the first release)](#running-a-prebuilt-binary-after-the-first-release)
- [Server options](#server-options)
- [Configuration](#configuration)
- [Connecting](#connecting)
- [Limitations](#limitations)

## Requirements

- **Linux** with `io_uring` support — **kernel 5.15+** (6.1 or newer
  recommended). InfinityDB probes the kernel at boot and uses the best
  available `io_uring` features (multishot accept/recv, provided buffers).
- For Docker: a runtime that lets you set a seccomp profile (Docker, Podman,
  containerd) — see [below](#the-io_uring--seccomp-requirement).
- macOS builds for development and correctness testing (via `kqueue`) but is
  not a performance target and is not recommended for deployment.

## Running with Docker

The release image is a `scratch`-based image containing only the static
`infinityd` binary (no shell, no libc, minimal CVE surface). Replace
`<version>` with a published release tag; until the first release is tagged,
build the image from this repository's `Dockerfile` (`docker build -t
infinitydb:dev .`) or build the binary from source.

```bash
docker run --rm -p 127.0.0.1:6379:6379 \
  --security-opt seccomp=deploy/seccomp/infinitydb-seccomp.json \
  ghcr.io/kevincaicedo/infinitydb:<version>
```

`-p 127.0.0.1:6379:6379` publishes the port on the host's loopback
interface only; publish it more widely only on a trusted network.

The `deploy/seccomp/infinitydb-seccomp.json` profile ships in this repository.
If you are running the image elsewhere, download that file alongside it, or
use one of the alternatives in the next section.

## The io_uring / seccomp requirement

InfinityDB's networking is built on Linux `io_uring`. **Docker's default
seccomp profile blocks the `io_uring` syscalls** (`io_uring_setup`,
`io_uring_enter`, `io_uring_register`). Under the default profile,
InfinityDB cannot create its reactor and exits immediately with:

```
infinityd: cell failed: Operation not permitted (os error 1)
```

You have three options, from most to least hardened:

**1. The bundled hardened profile (recommended).**
`deploy/seccomp/infinitydb-seccomp.json` allows `io_uring` while still denying
the high-risk container-escape syscalls (`mount`, `pivot_root`, kernel-module
loading, `kexec_load`, `bpf`, `ptrace`, `perf_event_open`, keyring and
clock-setting calls, and more):

```bash
docker run --security-opt seccomp=deploy/seccomp/infinitydb-seccomp.json ...
```

**2. Start from your runtime's full default profile and add io_uring.** For the
strictest allow-list posture, take Docker's upstream `default.json` and append
the three `io_uring` syscalls to an `SCMP_ACT_ALLOW` entry. This keeps the full
default deny-list and only adds what InfinityDB needs.

**3. Unconfined seccomp (development only).** Quick but removes *all* seccomp
filtering — only for local development on a trusted machine:

```bash
docker run --security-opt seccomp=unconfined ...
```

> [!NOTE]
> Running `infinityd` **directly on a host** (not in a container) needs none of
> this — host processes are not subject to Docker's seccomp profile. The
> requirement is specific to containerized runs.

Hosts/orchestrators differ: some Kubernetes and CI environments already permit
`io_uring`, others apply the Docker default. If the container exits with the
error above, seccomp is the cause.

## Running a prebuilt binary (after the first release)

No release has been published yet; until one is, build `infinityd` from
source (the [project README](../README.md) shows how). Once releases exist,
static musl binaries for `linux/x86_64` and `linux/aarch64` are attached to
each [release](https://github.com/kevincaicedo/infinitydb/releases).
`--version` prints the version, the git SHA and the build target:

```bash
tar xzf infinitydb-<version>-linux-x86_64.tar.gz
./infinityd --version
./infinityd --port 6379
```

Each tarball also includes the generated `compat-matrix.md`. Verify downloads
against the published `SHA256SUMS`.

## Server options

```
infinityd [--port 6379] [--cells 4] [--pin-start CORE] [--pin-stride 2]
          [--data-dir PATH] [--device-probe auto|off]
          [--buffers 4096] [--buf-size 4096] [--route-local-only]
          [--version] [--help]
```

The table covers the flags an operator sets. `infinityd --help` lists the
others, except `--park-us N`: the longest an idle cell with no timer due
parks before it checks again, in microseconds (500 with more than one cell,
5000 with one). The others tune the write path or the idle loop, or select an
experiment arm, and their defaults are the ones to run with.

| Flag | Meaning |
|---|---|
| `--port` | TCP port to listen on (default 6379). A port another process already listens on is refused at startup (`port N is already owned by another process`, exit 1): the cells share the port through `SO_REUSEPORT`, which would otherwise let a second node silently join the first's listener group and split its clients between two keyspaces. |
| `--cells` | Number of cells, one thread each (default 4). A data directory records the count at its first boot and refuses a boot with another count (`topology.toml`, below). |
| `--pin-start` / `--pin-stride` | Pin each cell's thread to a core: cell *i* pins to `pin-start + i × pin-stride`. The stride defaults to 2 (every other logical core). Without `--pin-start` nothing is pinned. |
| `--data-dir` | The durable root: the namespace catalog, each cell's log and checkpoints, and the files below. Without it the node is memory-only: the numbered databases serve, and namespace DDL (`INF.NS CREATE`, for example) and checkpoints are refused. |
| `--device-probe` | `auto` (the default): the first boot of a data directory measures the device for 10 to 15 seconds, before it accepts connections, and records the result in `io-properties.toml`. The `control: cell N not ready` lines printed meanwhile are the probe at work, not a hang; the node serves once it prints `control: recovery complete`. Every boot of a data directory answers `-LOADING` on every cell until recovery completes; clients retry. `off`: no probe, conservative write settings. |
| `--buffers` / `--buf-size` | `io_uring` provided-buffer pool: buffer count and per-buffer bytes (default 4096 each). |
| `--route-local-only` | Treat every key as local to the accepting cell (benchmark/diagnostic mode). |
| `--version` | Print version + git SHA + target and exit. |

### Data-directory files a first boot writes

With `--data-dir`, the first boot of a directory writes three files before
any cell creates a log, and every later boot reads them:

| File | What it is |
|---|---|
| `topology.toml` | The **cell topology** (ADR-0095): the `--cells` count the directory was written at. The keyspace's slot ranges are partitioned by it, so a boot whose `--cells` disagrees is refused with a typed error naming both counts — reopening at another count would silently lose access to acked durable data. Resizing a node is an explicit re-shard, never a flag edit. A directory written by an older build, before this file existed, gets one at its first boot on a current build: the count is derived from its `shard-*` directories, and a boot whose `--cells` disagrees is refused the same way. |
| `io-properties.toml` | The device model and barrier class the probe measured (ADR-0091); identity-bound to the filesystem + device. |
| `key-hash.toml` | The **key-hash secret** (ADR-0094): the index hashes every key with SipHash-1-3 under a 128-bit secret drawn from the OS at this first boot. Every checkpoint ref and index sidecar under the directory is placed by it — never edit it, never copy it between directories, and back it up with the directory. A directory that holds data without it (one written by an older build, before this file existed) is refused at boot with a typed message: reload from a dump into a new directory. A node without `--data-dir` draws a fresh secret per boot. |

A tiered namespace's cold directory (`shard-N/ns-N/cold/`) may also hold
`blob-NNNNNN.iblob.quarantine` files (ADR-0096): a boot that finds a
well-formed blob extent no durable artifact references **quarantines** it
by rename instead of deleting it — the bytes stay recoverable for one
full life, a later boot revives the file if the replayed state references
it after all, and only a second still-unreferenced verdict deletes it.
Leave these files alone; `INFO tiering` discloses them
(`tiering_blob_quarantined` / `tiering_blob_quarantine_revived` — the
revived counter going nonzero means a wrong orphan verdict healed and is
worth reporting).

A boot after a crash replays each tiered namespace's records since its last
checkpoint began. When they re-append more than the namespace's RAM window
(`MEM-BUDGET` + `MAINTAIN-SLICE`) on a cell, the boot demotes the excess to
tier files as the running node would, instead of refusing to start
(ADR-0174). Such a boot needs device space for the bytes it demotes — a full
device refuses the boot with a typed message naming the namespace — and
takes longer by about one sequential write of those bytes; a crash during it
leaves files the next boot removes before it replays again. `INFO
persistence` reports what the last boot did, summed over the cells, in the
`recover_node_tier_` lines: the demote steps, the tier bytes written, the
barriers, the files sealed, the settle reads and the largest step charge
(the largest over the cells). The demote steps, pads, tier bytes, barriers,
files sealed, settle reads, settles, verified deletes and blob releases stay
zero on a boot whose replay fits every window; the largest step charge, the
markers skipped and the dead-life files removed are outside that set and can
move on any boot.

## Configuration

Configuration uses the Redis `CONFIG` command surface. The most relevant keys
for the cache core:

```bash
redis-cli config set maxmemory 256mb
redis-cli config set maxmemory-policy allkeys-lfu
redis-cli config set client-output-buffer-limit "pubsub 33554432 8388608 60"
```

`maxmemory` is divided across cells. Supported eviction policies: `noeviction`,
`allkeys-lru`, `volatile-lru`, `allkeys-lfu`, `volatile-lfu`, `allkeys-random`,
`volatile-random`, `volatile-ttl`.

### Per-namespace budgets (named memory namespaces)

Named **memory** namespaces created with `INF.NS` carry their own enforced
pressure config (ADR-0068). Namespace DDL needs a node started with
`--data-dir`. The commands below create a namespace with a 512 MB budget,
raise the budget to 1 GB (hot-reloadable), return the namespace to the node's
eviction policy, and remove the budget:

```bash
redis-cli INF.NS CREATE sessions EVICTION allkeys-lru MAXMEMORY 512mb
redis-cli INF.NS SET sessions MAXMEMORY 1gb
redis-cli INF.NS SET sessions EVICTION inherit
redis-cli INF.NS SET sessions MAXMEMORY 0
```

- **With its own `MAXMEMORY`**, a namespace evicts toward its budget in
  isolation: it never displaces other namespaces' keys, and other
  namespaces never reclaim from it. OOM refusals at its budget are scoped
  to connections using that namespace — the error is the Redis-exact
  `OOM` string, but the scope is the namespace, not the node. Its bytes
  are **outside** the node `maxmemory` comparison (ADR-0068 A1): the
  node budget bounds the *pool* — the numbered databases plus every
  namespace without a budget of its own — so total memory is bounded by
  `maxmemory + Σ per-namespace MAXMEMORY`, and `INFO memory`
  `used_memory` (which counts every namespace) may sit above
  `maxmemory` in steady state. `INFO memory` `used_memory_pool` is the
  figure `maxmemory` compares against (ADR-0068 A2): `maxmemory −
  used_memory_pool` is the headroom before eviction runs.
- **Without one**, the namespace inherits the node `maxmemory`/policy and
  participates in node-wide eviction like the numbered databases.
- `EVICTION` unset (or `inherit`) follows `maxmemory-policy`; an explicit
  per-namespace policy wins over later `CONFIG SET maxmemory-policy`.
- **Durable namespaces never evict** (replayed data must not silently
  disappear); their `EVICTION`/`MAXMEMORY` refuse typed. **Tiered
  namespaces** budget memory with `MEM-BUDGET` (demotion to disk, not key
  death) and refuse these keys too — one budget authority per namespace.
- Like `maxmemory`, a per-namespace budget divides across cells.

## Connecting

Any Redis client works. For example, an interactive `redis-cli` session and
the `INFO server` section:

```bash
redis-cli -p 6379
redis-cli -p 6379 INFO server
```

```python
import redis
r = redis.Redis(host="127.0.0.1", port=6379, decode_responses=True)
r.set("k", "v"); print(r.get("k"))
```

See the [compatibility matrix](compat-matrix.md) for exactly which commands are
supported and any documented deviations.

## Limitations

InfinityDB is alpha software. Known limitations:

- **No authentication or TLS.** The server listens on all interfaces: run
  it only on localhost or on a trusted network. `AUTH`/`requirepass` and
  bind-address control come before the first alpha release; TLS and ACLs
  later (see the [roadmap](roadmap.md)).
- **Single node.** No replication or clustering yet.
- **No collection types** (hashes, lists, sets, sorted sets), **no
  transactions or Lua** (`MULTI`/`EXEC`/`WATCH`, `EVAL`), **no streams** and
  **no RDB import/export** yet.
- **On-disk formats are not yet stable.** A data directory written by one
  alpha build is not guaranteed to open under the next.

The [compatibility matrix](compat-matrix.md) lists every absent command
family with the milestone that brings it; the [roadmap](roadmap.md) says what
lands when.

Tiered namespaces (datasets larger than RAM) report their address space,
watermarks and memory budgets in `INFO tiering`, and `INF.NS` configures them
per namespace.
