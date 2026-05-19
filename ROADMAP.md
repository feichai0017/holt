# holt — roadmap

## Where things stand

holt is a single-node, Unix-only ART-over-blobs metadata engine.
The algorithm core, persistent backend, WAL + replay, sharded
buffer manager, 3-thread background checkpointer, and the curated
public API are all in place. 251 tests (unit + property + crash +
failpoint + multi-reader stress); zero clippy / rustdoc warnings
under `-D warnings`; ubuntu + macOS CI; `cargo deny` supply-chain
job.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design and
[CHANGELOG.md](CHANGELOG.md) for what changed when. The fine-
grained shipping log lives in `git log`; this file tracks
milestones, not individual features.

## v0.1 — Usable embedded library (shipped, 2026-05-19)

Goal: build the engine end-to-end so a path-shaped-metadata
workload can use it on a single node.

Delivered: 9-NodeType ART layout pinned at compile time, recursive
walker (insert / lookup / erase / rename), cross-blob `splitBlob`
/ `mergeBlob` / `compactBlob`, `PersistentBackend` (`O_DIRECT`
Linux + `F_NOCACHE` macOS), physiological WAL with replay,
`Tree::range` iterator with prefix + start-after + S3-style
delimiter rollup, `Tree::txn` batch transactions under one WAL
record, four examples (`basic_kv` / `filesystem_meta` /
`session_store` / `s3_metadata`), property-based tests against a
`HashMap` oracle, criterion benches vs RocksDB + SQLite.

## v0.2 — Performance + concurrency upgrades (shipped)

Goal: scope the metadata-engine core for production-shaped
workloads — no new public API surface, the bench numbers from v0.1
are the success criteria.

Delivered: `io_uring` persistent-backend fast path (Linux,
feature-gated), `crc32fast` SIMD CRC32, sharded `BufferManager`
(`DashMap` replacing the v0.1 `Mutex<HashMap>` + `VecDeque` LRU),
cached `Tree.root_pin`, range-iter fast-forward in delimiter
mode, 3-thread background checkpointer (planner + I/O worker +
eviction) under a W2D-strict protocol, adaptive tick-based
eviction, observability (`Tree::stats`, structured `tracing`,
Prometheus text-format renderer behind the `metrics` feature,
silent-pin reads so scrapes don't pollute cache counters),
diagnostics (`Tree::scan_prefix`, range-iter tombstone fix,
structured `Error::NodeCorrupt`), PGO docs, `cargo deny check`
in CI, scale-curve + p95/p99 contention benches.

Concurrency primitives: per-blob `HybridLatch` (LeanStore 3-mode,
wait-free optimistic reads) inside one `wal.lock` critical section
per write. Cross-blob lock-coupling and per-node `HybridLatch`
were considered for v0.2 but deferred to v0.3 — they need
structural changes the v0.2 scope didn't budget for, and the
per-slot version array they require is best designed alongside
the cross-blob descent flattening rather than retro-fitted.

Public API surface closed before crates.io publication: the
v0.1 `pub mod layout / journal / store` exposure shipped the
on-disk layout, WAL codec, and buffer-manager guards as part of
SemVer; v0.2 tightened those to `pub(crate)` so the engine is
free to evolve internally without minor-version breaks. See
[CHANGELOG.md](CHANGELOG.md) for the supported user surface.

## v0.3 — Concurrency + advanced features

The load-bearing v0.3 work is the concurrency primitive upgrade
that v0.2 deferred:

- **Per-node `HybridLatch`** — out-of-line
  `slot_versions: [AtomicU64; MAX_SLOTS]` on each `CachedBlob`
  (~80 KB per cached blob, in-memory only — no disk-format
  break). Walker readers capture + validate per-slot versions
  across the entire descent; walker writers bump
  `slot_versions[slot]` begin/end. Doubles as the cross-blob
  re-acquire verification token.
- **Cross-blob lock-coupling** — flatten the recursive walker
  into an iterative blob-by-blob loop so parent guards release
  before pinning the child, and only re-acquire if the parent's
  `BlobNode` needs an update. The re-acquire verifies the slot
  hasn't been freed + reused via the per-slot version.

Beyond concurrency, v0.3 collects the deferred feature work:

- **Full MVCC snapshots** — read at a specific seq; snapshot
  iteration that doesn't see writes after the snapshot point.
- **Online compaction** — `Tree::compact` that doesn't pause
  user traffic (today's implementation is documented quiescent-
  only, gated behind the v0.3 maintenance latch).
- **Change feed / subscription API** — consumers subscribe to a
  stream of `TxnOp`s; useful for index materialization,
  replication hooks, audit logs.
- **Column families** — multiple independent ARTs in one Tree,
  sharing the WAL + BM but with independent root + manifest.

Following v0.3's hard work, the "ecosystem layer" items become
fundable:

- Encryption-at-rest (per-blob AES-GCM).
- Compression (per-blob Zstd, transparent).
- OpenTelemetry bridge for the existing `tracing` events.

## v1.0 — Production-ready

- v0.3 feature set covered.
- Multi-platform stability across Linux + macOS (optional BSDs
  if anyone needs them).
- Real production deployments + case studies.
- Long-term API stability commitment — `holt::*` surface frozen,
  `#[non_exhaustive]` markers in place so additive changes stay
  non-breaking.

## Not on the roadmap

The library is **a metadata engine**, period. Single-node,
embed-in-your-process, Unix-only. Out of scope:

- **Windows support** — `O_DIRECT` (Linux) and `F_NOCACHE` (macOS)
  have no Windows analog this project wants to maintain. The
  crate `compile_error!`s on Windows targets.
- **Object-storage frontend / S3 layer** — the upstream that
  inspired holt's algorithm core wrapped its ART in an S3-style
  RPC server (PUT/GET/LIST inode handlers, multi-tenant bucket
  registry, RPC worker pool). holt does not reproduce any of
  that. The alignment is bounded to the **metadata engine**: ART
  core, blob layout, WAL, latching, range iterator. `TxnOp`
  variants holt journals share wire shape with the upstream so a
  future RPC layer could re-use the format, but holt itself ships
  no multi-root registry, no bucket namespace, no RPC dispatcher.
- **Replication / consensus** — build it above this. We expose
  hooks (change feed in v0.3) but don't implement Raft.
- **Network server** — this is a library. Wrap it in your gRPC /
  HTTP / whatever.
- **SQL** — not the right abstraction for this data shape.
- **Vector search** — combine with a dedicated vector DB.
- **Full-text search** — combine with Tantivy / Lucene-rs.

## Contributing

Early-stage project; design feedback most welcome. PRs welcome
too, but please open an issue first for non-trivial changes —
the architecture is still being shaped and we want to avoid
churn.
