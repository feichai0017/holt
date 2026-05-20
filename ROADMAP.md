# holt — roadmap

## Where things stand

holt is a single-node, Unix-only ART-over-blobs metadata engine.
The algorithm core, persistent backend, WAL + replay, sharded
buffer manager, 3-thread background checkpointer, and the curated
public API are all in place. 260+ tests (unit + property + crash +
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
wait-free optimistic reads) inside one WAL-writer critical section
per write. Slot-versioned cross-blob lock-coupling was considered
for v0.2 but deferred to v0.3 — it needs structural changes the
v0.2 scope didn't budget for, and the per-slot version array is
best designed alongside the cross-blob descent flattening rather
than retro-fitted.

Public API surface closed before crates.io publication: the
v0.1 `pub mod layout / journal / store` exposure shipped the
on-disk layout, WAL codec, and buffer-manager guards as part of
SemVer; v0.2 tightened those to `pub(crate)` so the engine is
free to evolve internally without minor-version breaks. See
[CHANGELOG.md](CHANGELOG.md) for the supported user surface.

## v0.3 — Extreme metadata-engine performance

v0.3 is now scoped as the performance ceiling milestone for an
embedded metadata engine. Feature work waits. The goal is to push
the Fractal-ART-inspired kernel as far as practical on modern
Linux/macOS: cache-resident reads, path-shaped writes, prefix/list
walks, WAL durability, checkpoint I/O, and large-tree behavior.

The target design borrows the strongest parts of Fractal ART,
LeanStore, and modern NVMe engines while keeping holt's product
boundary narrow: one embedded library, opaque byte values, no RPC
server, no replication, no distributed object-store layer.

### P0 — Remove the write-path serial choke points (done)

v0.3's concurrency cut is implemented in the codebase:

- **Cross-blob lock coupling.** The old parent-held fallback is
  gone. `insert_multi` / `erase_multi` read the parent `BlobNode`,
  pin the child, release the parent guard, and continue in the
  child. The child blob's own `header.root_slot` is the
  authoritative entry, so child-local splits, collapses, and
  compaction do not require parent rewrites.
- **No slot-version sidecar.** The earlier per-slot version plan
  would have added ~80 KB of out-of-line state per cached blob.
  The shipped shape avoids that memory tax by removing
  `BlobNode.child_entry_ptr` and making the child header the only
  cross-blob root token.
- **Scoped walker instead of unsafe state machine.** The mutation
  walker still uses recursive Rust scopes, but it no longer
  retains ancestor guards across child mutation. That gives the
  same latch lifetime as the flattened state-machine design
  without self-referential guard plumbing.
- **Maintenance is separated.** Foreground writers enter only the
  shared side of a narrow atomic `maintenance_gate` while they may
  cross `BlobNode` boundaries. `compact()` and the background
  merge pass enter the exclusive side before folding/deleting
  child blobs.
  Point reads also take the shared side, but blob-local access
  remains optimistic; ordinary readers and writers still run
  concurrently with each other.
- **Large-tree puts avoid root write-latch traffic.** Cross-blob
  puts route through the root under a shared latch, acquire the
  child write latch while the edge is stable, then mutate only
  from the child down. Child-only writes also return a precise
  `root_dirty = false`, so the caller does not mark the root dirty
  or take the dirty-map mutex for a blob it did not change.
- **Shape counters are exposed.** `Tree::stats()` now reports
  mutation walker ops, total/average/max blob hops, max
  cross-blob boundary depth, spillovers, merges, and optimistic
  read restarts. Prometheus export includes the same counters.
- **Checkpoint/eviction interlock is closed.** Dirty entries
  drained by a checkpoint round stay protected as in-flight
  `flushing` entries until their snapshotted bytes complete
  `write_through`, so eviction cannot drop the cache image in the
  gap between `snapshot_dirty()` and the planner's byte copy.

Still intentionally not in P0: per-op latch wait histograms. The
current `HybridLatch` API has no timed acquisition boundary, and
adding timing to every latch acquire should be driven by a
contention benchmark rather than added speculatively.

### P1 — Journal group commit (done, with one explicit boundary)

Durable group commit is implemented:

- Persistent trees own a dedicated journal worker instead of an
  exposed `Arc<Mutex<WalWriter>>`.
- Writers encode complete WAL records into owned buffers, submit
  them to the worker, and wait outside the commit-publish critical
  section.
- `wal_sync_on_commit=true` writers share one `sync_data` when
  they arrive inside the short group window. `Tree::stats()` and
  Prometheus export journal appends, append batches, and sync
  counts so the batching ratio is observable.
- Manual and background checkpoint rounds take the same
  `commit_lock` while draining dirty/pending sets, flushing the
  journal, and cloning dirty blob bytes. This is intentionally
  conservative: it prevents backend writes from copying a blob
  image that includes a mutation whose WAL record was not part of
  the durable snapshot.

Boundary kept on purpose: the current safe implementation still
holds `commit_lock` across walker mutation + dirty publish +
journal submission for persistent writes. Removing that final
global publish section needs per-blob publish epochs (or an
equivalent mutation-version protocol) so checkpoint byte snapshots
can reject images that raced with an uncheckpointed writer. Do not
delete the lock until that proof exists.

### P2 — NVMe-grade checkpoint I/O

The background checkpointer already has planner / I/O / eviction
threads. v0.3 makes the I/O side worth that structure:

- Submit dirty blobs as batches, not one synchronous write at a
  time.
- On Linux, upgrade the `io_uring` path from
  `submit_and_wait(1)` to a real queue: larger ring depth,
  batched SQEs, CQE polling, fixed file, registered aligned
  buffers, and optional `SQPOLL` / `IOPOLL` for direct NVMe.
- Keep macOS on `F_NOCACHE` + `pwrite`, but keep the abstraction
  shaped around batched writes so Linux is not held back.
- Stop letting manifest persistence dominate checkpoint rounds:
  add slot reuse first, then evaluate an append-only manifest log
  if full rewrite shows up in profiles.

### P3 — CPU hot-path work

- Remove per-op key padding allocation. Replace `pad_key` with a
  virtual terminator or a small-stack key view.
- Make same-key leaf comparison borrow the stored key directly;
  avoid allocating a `Vec` just to compare.
- Push SIMD beyond Node16 where profiles justify it: prefix
  compare, Node48 index scan, Node256 child scan, CRC32, and
  copy/repack loops.
- Keep node bodies packed and cache-line friendly. Avoid object
  indirection or heap nodes on the hot path.
- Use PGO/LTO as a release profile, not as a substitute for
  simpler branch structure.

### P4 — Large-tree shape control

- Replace "largest non-Blob child" spillover with an
  occupancy-aware split policy. The goal is bounded blob hops and
  healthy parent/child fill ratios, not just freeing any space.
- Implement `BlobNode` inline-prefix divergence split so a bad
  blob boundary can recover into a high-fanout structure.
- Make merge/rebalance incremental. `compact()` and background
  merge are now online with respect to foreground writers through
  the atomic `maintenance_gate`; the remaining large-tree work is policy
  quality (when to split/merge/rebalance), not basic safety.
- Add targeted benchmarks for skewed path prefixes, hot
  directories, delete-heavy churn, and working sets larger than
  the buffer pool.

### Deferred until after the performance core

These are useful features, but they do not define the metadata
engine's ceiling and should not compete with the v0.3 hot path:

- Full MVCC snapshots.
- Change feed / subscription API.
- Column families.
- Encryption-at-rest.
- Compression.
- OpenTelemetry bridge.

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
