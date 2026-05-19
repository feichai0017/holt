# Benchmarks

Criterion-based microbenchmarks comparing **holt** against
**RocksDB** and **SQLite** across three shapes of metadata
workload — `kv` (anti-pattern baseline), `objstore`, and `fs`
(holt's design target).

## Scenarios

| Group | Key shape | Value shape | Models |
|---|---|---|---|
| `kv` | 32-byte random | 64-byte random | Anonymous KV baseline — **pessimal for ART** (no prefix sharing, no key locality). |
| `objstore` | `bucket-NN/path/sub/file-NNNN.bin` | `{"size":...,"etag":...,"class":"STD"}` (~60 B fixed) | S3-style object metadata. |
| `fs` | `/usr/local/share/category-N/file-NNNN` | 32-byte packed inode (size + mtime + mode + uid + gid + nlink) | POSIX filesystem metadata. |

Each scenario runs three operations:

- `*_get` — random key lookup over a pre-loaded dataset
- `*_put` — random key replacement (in-place update)
- `*_mixed` — 50% get / 50% put, key chosen at random

`N_KEYS = 20 000` — large enough that the data spreads across
**multiple holt blobs** (~6–8 × 512 KB), so the bench exercises
`BlobNode` crossings + cross-blob spillover/compact retries, not
just single-blob descent.

## Running

```sh
# Full sweep (~5 minutes — 18 bench groups × 3 engines × ~5s/measure):
cargo bench --bench main

# Quick smoke pass (~1 minute):
cargo bench --bench main -- --quick --noplot

# A single scenario:
cargo bench --bench main -- kv_get
```

HTML reports land in `target/criterion/`.

## Methodology — apples-to-apples

Two comparison modes, each with all three engines tuned to the
same durability profile:

### Memory / no-WAL mode (`*_get` / `*_put` / `*_mixed`)

Engine algorithm cost only — durability disabled across the board:

- **holt**: `TreeConfig::memory()` with `flush_on_write = false`.
  Mutations stay in the BufferManager-pinned blobs.
- **RocksDB**: temp-dir DB, `disable_wal = true`, `sync = false`,
  64 MB memtable, compression disabled.
- **SQLite**: `:memory:` DB, `journal_mode=MEMORY`,
  `synchronous=OFF`, 64 MB page cache, `WITHOUT ROWID` schema.

### Persistent mode (`*_persist_get` / `*_persist_put` / `*_persist_mixed`)

All three engines disk-backed with WAL on, per-op durability to
the OS page cache (not fsync) — the "you survive a process
crash, not a power failure" mode high-throughput services target:

- **holt**: `TreeConfig::new(tempdir)` (PersistentBackend with
  `F_NOCACHE` on macOS / `O_DIRECT` on Linux). Every `put` /
  `delete` / `rename` emits a `TxnOp` to the WAL writer;
  `wal_sync_on_commit` stays at its default `false`. Blobs only
  hit disk at spillover or `Tree::checkpoint`.
- **RocksDB**: temp-dir DB, `disable_wal = false`, `sync = false`.
  Each `put` appends to the WAL (buffered) plus the memtable.
- **SQLite**: file-backed DB, `journal_mode=WAL`,
  `synchronous=NORMAL`, 64 MB page cache.

Shared settings: 20 000 unique keys preloaded; bench iterates a
seeded permutation of that set; `cargo bench` builds with
`lto="thin"`, `codegen-units=1`, `opt-level=3`; single-threaded.

## How to read the numbers

The `objstore` + `fs` scenarios are the **right** test for what
holt is designed to do. The `kv` scenario is the **wrong** test,
included on purpose — it tells you how badly an ART degrades when
the workload violates its assumptions.

| Scenario | What it actually measures | Expected outcome |
|---|---|---|
| `kv` (random 32-byte keys) | ART without prefix sharing or locality | holt loses — every lookup chases fresh nodes across multiple blobs; B+tree (LMDB), LSM (RocksDB), and B-tree (SQLite) all win |
| `objstore` (path keys) | ART on hierarchical keys, ~30-byte shared prefix | holt wins — `O(key.len)` lookup beats `O(log N)` once the tree breaks past ~1k entries |
| `fs` (POSIX paths) | Same, with very long common prefix | holt wins biggest — deep prefix compression |

Pick the engine that matches your **key shape**. holt is for
hierarchical, prefix-rich keys; if your keys are random bytes
(hashes, UUIDs without a path prefix), reach for RocksDB / SQLite.

## Caveats

1. **Single-threaded.** Per-blob `HybridLatch` makes reads
   wait-free; concurrent-read throughput scales with cores, but
   this bench measures single-thread latency.
2. **No fsync.** Both modes set `sync=off`-equivalent — durable
   to OS page cache only. A real `fsync`-per-op workload is
   fsync-bound (~1–3 ms on consumer SSD) and overwhelms every
   engine's algorithm cost.
3. **Bench numbers are machine-dependent.** Don't take any
   absolute throughput claim from this README at face value —
   re-run on your hardware. The relative ordering (holt wins on
   path-shaped, loses on random-kv) is the load-bearing
   observation.

This bench is the right comparison for **metadata-engine
workloads** with bounded per-tree dataset and hierarchical keys —
directory listings, S3 metadata, inode tables, AI artefact
catalogs. It is not the right comparison for "100M-key analytics
datastore" workloads or "random UUID hot-path" workloads.
