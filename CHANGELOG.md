# Changelog

All notable changes to **holt** are documented in this file. Format
adapted from [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
versioning will follow [Semantic Versioning](https://semver.org/) once
v0.1.0 ships.

## [Unreleased] — v0.1.0-dev

The v0.1 cycle is "build the engine end-to-end." The algorithm core,
storage cache, WAL, and persistence stack are landed; the remaining
v0.1 items are the higher-level API surface (`Tree::range`,
`Tree::txn`). See [`ROADMAP.md`](ROADMAP.md) for the live list.

### Added — algorithm core

- **9-NodeType ART layout** with compile-time-asserted byte offsets
  (`Leaf` 16 B, `Prefix` 128 B, `Blob` 128 B, `Node4` 24 B, `Node16`
  88 B, `Node48` 456 B, `Node256` 1032 B, `EmptyRoot` 8 B, `Invalid`).
- **4 KB `BlobHeader`** with bit-packed `SlotEntry` (`ntype << 17 |
  offset / 8`); 10 240-slot table per 512 KB blob.
- **Recursive walker**: `insert` / `lookup` / `erase` / `rename` —
  every arm cross-blob via `BlobNode` crossings.
- **`splitBlob` auto-spillover** on `OutOfSpace`; victim heuristic
  picks the largest non-`Blob` subtree under the root's first
  branching node.
- **`compactBlob` in-place repack** via deep-clone-into-scratch +
  memcpy-back; paired with `splitBlob` on every retry so churn
  workloads (insert + delete + reinsert) stay in fewer blobs.
- **`make_blob_from_node` deep-clone primitive** + `free_subtree`
  recursive slot reclaim.
- **`mergeBlob` inverse of splitBlob** — `engine::merge_blob`
  inlines a child blob's subtree back into its parent at the
  `BlobNode` slot, preserves the BlobNode's inline prefix as a
  `Prefix` chain over the inlined root, and deletes the child
  blob. `engine::is_mergeable` guards the fold (combined data
  area + slot count fit, child has no nested crossings, no
  tombstones). `engine::try_merge_children` walks a parent's
  tree and folds every direct mergeable `BlobNode` child.
  `Tree::compact` runs it after the per-blob compact pass —
  heavy-erase workloads collapse multi-blob trees back toward a
  single root.
- **`refresh_blob_node_pointers` post-compact invariant repair**
  — `compact_blob` rebuilds a child's `header.root_slot` in
  isolation, breaking the lock-step
  `BlobNode.child_entry_ptr == child.header.root_slot`
  invariant that insert / erase keep inline.
  `Tree::compact` runs `refresh_blob_node_pointers` between the
  per-blob compact pass and the merge pass to walk every
  `BlobNode` crossing and re-point it at the child's current
  root slot.
- **`SPILLOVER_RESERVATION = 128 B`** bump-area headroom so
  `spillover_blob` always has room to allocate its emergency
  `BlobNode` placeholder.
- **Cross-type free-list fallback** (`Prefix` ↔ `Blob`, both 128 B).
- **Erase-time node shrink** (Node256 → 48 → 16 → 4) with hysteresis
  thresholds 37 / 12 / 3.
- **`Node4 → Prefix([byte])` lone-child collapse** preserves descent-
  depth invariants when an inner node empties to a single child.
- **Strict-prefix support** via a Tree-layer terminator byte.
- **In-place leaf-value update on same-size writes** — zero
  allocator activity when an update fits the existing extent.
- **SIMD Node16 byte search** (SSE2 + NEON + scalar fallback).
- **SIMD `longest_common_prefix`** (SSE2 + NEON + scalar) for
  leaf-split / prefix-split hot paths.

### Added — concurrency

- **3-mode `HybridLatch`** (LeanStore: optimistic / shared /
  exclusive) wired into `CachedBlob` over an
  `UnsafeCell<AlignedBlobBuf>`.
- **Wait-free `Tree::get`** — walks every blob under an optimistic
  snapshot, restarts from the root on a torn read. No Tree-wide
  reader lock.
- **No Tree-wide writer mutex** — `put` / `delete` serialise on the
  root blob's per-blob exclusive latch; mutations on disjoint child
  blobs proceed in parallel. `rename` keeps a small `rename_lock`
  scoped to its multi-step atomicity.

### Added — buffer manager

- **`BufferManager`** — LRU-bounded blob cache wrapping any
  `Backend`, transparent (itself implements `Backend`).
  `TreeConfig::buffer_pool_size` (default 64) sets capacity.
- **`pin` / `commit` API** with the three-guard family
  (`OptimisticGuard`, `BlobReadGuard`, `BlobWriteGuard`) — pin-and-
  operate for zero-copy reads and writes against the cached buffer.

### Added — persistence

- **`MemoryBackend`** for in-memory trees and tests.
- **`PersistentBackend`** — single packed `blobs.dat` + atomic-
  rename `manifest.bin`; `O_DIRECT` on Linux, `F_NOCACHE` on macOS.
- **`AlignedBlobBuf`** — 4 KB-aligned heap buffer required by
  `O_DIRECT`.

### Added — WAL (Stage 5a-5e)

- **`TxnOp` record codec** — 10 variants (`Insert` / `Erase` /
  `Split` / `Merge` / `Compact` / `RenameObject` / `Rename` /
  `NewTree` / `RmTree` / `MemMarker`) encoded as
  `MAGIC | LEN | SEQ | TY | BODY | CRC32`.
- **CRC32 (table-driven, IEEE 802.3)** with a 256-entry compile-
  time table — ≈1.5 GB/s, ~110 ns per typical 175-byte record.
- **`WalWriter`** — append-only file with `sync_data`-on-flush
  durability boundary + 64 KB group-commit auto-flush.
- **`replay()` forward scanner** — torn-tail-tolerant; real mid-
  file corruption surfaces as `Error::ReplaySanityFailed` with the
  bad record's byte offset.
- **Tree ↔ WAL integration** — `put` / `delete` / `rename` emit
  ops; `Tree::open` replays onto the BM-cached blob; `Tree::checkpoint`
  flushes WAL + commits BM + atomically truncates the WAL.
- **Reference-based `WalWriter::append_insert` / `append_erase` /
  `append_rename_object`** fast paths — skip the three `Vec` clones
  the `TxnOp` enum's owned-data shape would force.

### Added — public API

- **`Tree::open(TreeConfig)`** — single entry point;
  `TreeConfig::new(dir)` opens persistent (default),
  `TreeConfig::memory()` is volatile.
- **`Tree::put / get / delete / rename`** — bytes-in, bytes-out.
- **`Tree::checkpoint`** — flush WAL + commit BM + truncate WAL.
- **`TreeConfig::wal_sync_on_commit`** — opt-in per-op WAL fsync
  (default `false`, matching RocksDB's `sync=false` baseline).
- **`TreeBuilder`** — chainable config (`memory()`,
  `buffer_pool_size(n)`, `wal_sync_on_commit(bool)`,
  `checkpoint_byte_interval(b)`).
- **Typed `Error`** — `BackendIo` / `Alloc` / `Free` /
  `KeyTooLong` / `ValueTooLong` / `NotYetImplemented` /
  `NodeCorrupt` / `ReplaySanityFailed` / `NotFound` / `DstExists`.

### Added — tests / benches / examples

- **176 tests**: 114 unit + 30 tree_smoke + 15 wal_round_trip + 10
  wal_tree_integration + 2 property-based + 5 layout-invariants.
- **Property-based tests** (`proptest`) — random put / delete /
  rename traces cross-checked against a `HashMap` oracle in memory
  mode + crash-and-replay in persistent mode.
- **Criterion benchmarks** vs RocksDB across three workload shapes
  (`kv` / `objstore` / `fs`) × three ops (get / put / mixed) ×
  two storage modes (memory / persistent) = 18 microbenchmarks.
  See [`benches/README.md`](benches/README.md) for headline
  numbers.
- **Four examples**: `basic_kv`, `filesystem_meta`, `session_store`,
  `s3_metadata`. Each `cargo run --example` prints golden output.

### Added — tooling / project polish

- **GitHub Actions CI** — matrix of ubuntu + macOS × build / test /
  doctest + lint (`cargo fmt --check`, `cargo clippy -D warnings`)
  + docs (`cargo doc -D warnings`) + MSRV (1.82) build.
- **Platform scope locked**: holt is Unix-only by design. Building
  on Windows fires a top-of-crate `compile_error!`; the persistent
  backend's `O_DIRECT` (Linux) / `F_NOCACHE` (macOS) fast path has
  no Windows analog worth carrying.
- **Zero clippy / rustdoc warnings** under `-D warnings`. The
  curated `#![allow]` block in `src/lib.rs` lists the
  `clippy::pedantic` lints we've reviewed and judged either
  intentional or noise.
- **`CONTRIBUTING.md`** / **`CODE_OF_CONDUCT.md`** / this changelog.

### Notes

- The crate is pinned to MSRV **1.82**.
- License: MIT.
- v0.2 will add the 3-thread async checkpointer, `io_uring` backend
  (Linux), SIMD CRC32, and the buffer-pool tuning knobs.
