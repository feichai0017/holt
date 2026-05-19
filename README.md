# holt

[![Crates.io](https://img.shields.io/crates/v/holt.svg)](https://crates.io/crates/holt)
[![Docs.rs](https://docs.rs/holt/badge.svg)](https://docs.rs/holt)
[![CI](https://github.com/feichai0017/holt/actions/workflows/ci.yml/badge.svg)](https://github.com/feichai0017/holt/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.82-blue.svg)](https://github.com/feichai0017/holt/blob/main/Cargo.toml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> A carefully crafted **adaptive radix tree** for path-shaped metadata.

> ⚠️ **Pre-1.0 (v0.1.0).** The public API is still evolving —
> minor releases may break source compatibility. Pin the exact
> version in your `Cargo.toml` (`holt = "=0.1.0"`) until 1.0
> stabilises the surface.

`holt` is an embedded Rust library for storing **hierarchical
keys** — file paths, S3 object names, multi-tenant namespaces,
time-bucketed identifiers — with sub-microsecond lookups, per-blob
concurrency, and crash-safe persistence.

It targets workloads where:

- Keys are **hierarchical / path-shaped** (so prefix compression pays).
- The dominant access is **point lookup + prefix range scan**.
- Concurrency is **high** (many readers + writers across disjoint
  subtrees).
- Latency is **micro-critical** — no LSM compaction stalls, no
  single-writer locks.

It is **not** a general-purpose KV store; if you need full-text or
vector similarity, reach for the right tool. For this shape, holt
should beat LMDB / RocksDB / SQLite on its target workload.

## Why "holt"?

A **holt** (Old English *holt*) is a small grove or copse — a
self-contained collection of trees on a single piece of ground.
That maps directly to the design: each `holt::Tree` is **one** ART
made of **many** 512 KB blob frames, bounded and self-contained,
grown by repeated `splitBlob`. Short to type, distinct from other
crates, easy to say.

## When to reach for holt

| Engine        | Data structure        | Persistence       | Concurrency        | Notes                                                |
|---------------|-----------------------|-------------------|--------------------|------------------------------------------------------|
| LMDB          | B+tree                | mmap              | Single-writer MVCC | Battle-tested; page chasing for short hot keys.      |
| RocksDB       | LSM                   | SST + WAL         | MVCC               | Compaction stalls; large hot dataset is RAM-heavy.   |
| SQLite        | B-tree                | File              | Single writer      | Convenient, but writer is the bottleneck under load. |
| Sled          | Hybrid LSM            | Log-structured    | Lock-free          | Rust-native, largely unmaintained.                   |
| **holt**      | **Adaptive Radix Tree** | **512 KB blobs** | **Per-blob 3-mode latch** | **Path compression + lookup is O(key.len)** |

ART's lookup cost is `O(key.len)`, not `O(log N)`. For short hot keys
(< 64 bytes), that beats any tree-based competitor. The per-blob
HybridLatch lets N readers traverse disjoint subtrees in parallel
without coordinating.

## Project status

**v0.1 in active development.** The algorithm core (insert / lookup /
erase / rename / range / txn / compact + multi-blob crossings),
persistent backend, physiological WAL with batched transactions,
and the stateful `Tree::range` iterator (prefix anchoring +
`start_after` + S3 delimiter) are all landed. 202 tests (unit +
property-based + crash-and-replay) pass on Ubuntu + macOS CI.

See [`CHANGELOG.md`](CHANGELOG.md) for the per-feature breakdown
and [`ROADMAP.md`](ROADMAP.md) for what's queued (io_uring backend,
background checkpointer, SIMD CRC32, MVCC snapshots).

`cargo bench --bench main` runs a side-by-side comparison with
RocksDB and SQLite across three metadata workload shapes — see
[`benches/README.md`](benches/README.md) for the methodology and
headline numbers.

## Usage

Add holt to your `Cargo.toml`:

```toml
[dependencies]
holt = "0.1"
```

### Open a tree

Two storage modes; pick at open time:

```rust
use holt::{Tree, TreeBuilder, TreeConfig};

// Persistent (production), Unix-only — Linux `O_DIRECT`,
// macOS `F_NOCACHE`. The directory is created if missing.
let tree = TreeBuilder::new("/var/lib/myapp/meta.holt")
    .buffer_pool_size(128)        // pinned 512 KB blobs (default 64)
    .wal_sync_on_commit(false)    // see "Durability" below
    .open()?;

// In-memory — volatile, dies with the last handle. Good for
// tests, sidecar caches, ephemeral session stores.
let tree = Tree::open(TreeConfig::memory())?;
```

### Single-key CRUD

Bytes in, bytes out. `put` returns the previous value if any;
`delete` returns the dropped value; `rename` is atomic and
errors if `dst` exists unless `force = true`.

```rust
tree.put(b"img/01.jpg", b"rgb_data_blob_id_abc")?;

let value: Option<Vec<u8>> = tree.get(b"img/01.jpg")?;
assert_eq!(value.as_deref(), Some(&b"rgb_data_blob_id_abc"[..]));

let prev: Option<Vec<u8>> = tree.delete(b"img/01.jpg")?;
assert!(prev.is_some());

tree.put(b"old/path", b"v")?;
tree.rename(b"old/path", b"new/path", /*force=*/ false)?;
```

### Atomic batched transaction

Multiple ops under one WAL record — either all replayed on
recovery, or none. Returns `Err` mid-batch on a failing rename
(e.g. `src` missing); earlier ops in the batch are still applied
to the in-memory cache but the batch WAL record is not emitted,
so a subsequent reopen-from-WAL drops the partial work.

```rust
tree.txn(|batch| {
    batch.put(b"users/alice", b"{...}");
    batch.put(b"users/bob",   b"{...}");
    batch.delete(b"users/legacy-account");
    batch.rename(b"users/temp", b"users/permanent", true);
})?;
```

### Range scan with S3 delimiter rollup

`Tree::range` is the load-bearing API for metadata workloads —
`readdir`, `ListObjects`, AI artifact catalogs. Chain
`.prefix()` to anchor the scan, `.start_after()` for paging,
`.delimiter()` for S3-style `?delimiter=/` rollup.

```rust
use holt::RangeEntry;

// Simple prefix scan — `take(50)` for a paged "first 50" view.
let first_50: Vec<(Vec<u8>, Vec<u8>)> = tree
    .range()
    .prefix(b"users/")
    .into_iter()
    .take(50)
    .map(|r| r.map(|e| match e {
        RangeEntry::Key { key, value } => (key, value),
        _ => unreachable!("no delimiter set"),
    }))
    .collect::<Result<_, _>>()?;

// S3-style "list one level" — leaves under `/img/` get emitted
// as Key; deeper paths roll up to a single `CommonPrefix` per
// distinct subdir.
for entry in tree.range().prefix(b"img/").delimiter(b'/') {
    match entry? {
        RangeEntry::Key { key, .. }       => println!("file {key:?}"),
        RangeEntry::CommonPrefix(prefix)  => println!("dir  {prefix:?}/"),
        _ => {} // `RangeEntry` is `#[non_exhaustive]`
    }
}
```

### Durability

Per-op writes land in the WAL writer's buffer + the BufferManager
cache. Disk-truth advances at:

- **`Tree::checkpoint()`** — flush WAL (`sync_data`), write the
  BM root blob through to the backend, `fdatasync` the backend,
  truncate the WAL. Call this at your own application checkpoint
  cadence.
- **WAL auto-flush** — once the WAL writer's pending buffer
  crosses 64 KB it drains to the OS page cache (no `sync_data`).
  Bounds in-memory buffering even if `checkpoint` is rare.
- **`wal_sync_on_commit = true`** — opt in to a per-op
  `sync_data` on the WAL. Trades ~µs per write for "durable to
  disk past power loss." Default `false` matches RocksDB's
  `sync=false`.

```rust
tree.checkpoint()?;   // flush WAL + write through + truncate
```

See [`examples/`](examples/) for full programs:
[`basic_kv`](examples/basic_kv.rs),
[`filesystem_meta`](examples/filesystem_meta.rs),
[`session_store`](examples/session_store.rs),
[`s3_metadata`](examples/s3_metadata.rs).

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│ Public API: Tree, TreeBuilder, TxnBatch, RangeIter           │
├──────────────────────────────────────────────────────────────┤
│ Engine: insert / lookup / erase / range / merge / migrate    │
├──────────────────────────────────────────────────────────────┤
│ Concurrency: HybridLatch (3-mode optimistic / shared / excl) │
├──────────────────────────────────────────────────────────────┤
│ Journal: physiological WAL (11 TxnOp variants) + replay      │
├──────────────────────────────────────────────────────────────┤
│ Store: BufferManager (LRU pin/commit) + BlobFrame (512 KB)   │
├──────────────────────────────────────────────────────────────┤
│ Layout: 9 NodeType variants + bit-packed SlotEntry + Header  │
├──────────────────────────────────────────────────────────────┤
│ Backend: MemoryBackend + PersistentBackend (O_DIRECT / NOCACHE) │
└──────────────────────────────────────────────────────────────┘
```

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the deep dive.
The design draws on Leis et al.'s ART paper (ICDE 2013) for the
four-node-size scheme and LeanStore (ICDE 2018) for the HybridLatch
contract.

## Not on the roadmap

holt is **just the metadata engine** — single-node, embed-in-
your-process, Unix-only. Out of scope:

- **Windows** — `compile_error!`s the crate (Unix `O_DIRECT` /
  `F_NOCACHE` has no Windows analog worth carrying).
- **Object-storage frontend / S3 layer** — no RPC server, no
  multi-tenant bucket registry, no distributed checkpointer.
- **SQL / vector / full-text** — combine with a domain-appropriate
  engine (`+ FAISS` for vectors, `+ Tantivy` for full-text).
- **Replication / consensus** — build above; we'll expose hooks
  (change feed, snapshot transfer) but won't ship Raft.
- **Network server** — this is a library; wrap it in your own RPC.

## License

Licensed under the [MIT licence](LICENSE).
