# Benchmarks

holt ships a Criterion suite that pits it against RocksDB across
three realistic shapes of metadata workload. This page is the
project-level rollup of *what we measure*, *what we don't*, and *what
the headline numbers mean*. For the runnable scripts and per-bench
methodology see [`benches/README.md`](../benches/README.md).

## Scope

The bench targets **path-shaped metadata engines**: short
hierarchical keys, fixed-size values, dense per-prefix density, point
lookup + atomic move. That's the workload holt was built for —
filesystem inodes, S3 object metadata, multi-tenant session tables,
AI artefact catalogues.

It is **not** the right comparison for:

- High-volume analytics tables (millions of cold keys, sequential
  scans). RocksDB's block cache + bloom filters + compression sit
  outside this comparison.
- Vector search, full-text search, SQL workloads. holt exposes
  a bytes-in / bytes-out interface; the higher-level engine is up
  to the application.

## Scenarios × ops × storage modes

| Group       | Key shape                                  | Value shape                                                       |
|-------------|---------------------------------------------|-------------------------------------------------------------------|
| `kv`        | 32-byte random                              | 64-byte random                                                    |
| `objstore`  | `bucket-NN/path/sub/file-NNNN.bin`          | `{"size":...,"etag":...,"class":"STD"}` (≈60 B fixed)            |
| `fs`        | `/usr/local/share/category-N/file-NNNN`     | 32-byte packed inode (size + mtime + mode + uid + gid + nlink)    |

Each group runs three ops:

- `*_get` — random key lookup over a pre-loaded dataset.
- `*_put` — random key replacement (in-place update).
- `*_mixed` — 50% get / 50% put, key chosen at random.

And two storage modes:

- **Memory** — both engines volatile, WAL disabled (RocksDB
  `disable_wal=true, sync=false`; holt `TreeConfig::memory()`).
  Isolates the *algorithm* cost.
- **Persistent** — both engines disk-backed, WAL enabled, per-op
  fsync disabled (RocksDB `disable_wal=false, sync=false`; holt
  `TreeConfig::new(dir)` with default `wal_sync_on_commit = false`).
  Survives a process crash, not a power loss until checkpoint.

That's **2 × 3 × 3 = 18 microbenchmarks**, all single-threaded.

## Headline numbers

Apple M-series laptop, `cargo bench --bench main -- --quick`,
v0.1-dev as of 2026-05:

### Memory

| Scenario   | Op    | holt       | RocksDB       | holt / RocksDB |
|------------|-------|---------------|---------------|-------------------|
| `kv`       | get   | 9.45 Melem/s  | 1.89 Melem/s  | **5.0×**          |
| `kv`       | put   | 5.26 Melem/s  | 1.29 Melem/s  | **4.1×**          |
| `kv`       | mixed | 6.58 Melem/s  | 1.26 Melem/s  | **5.2×**          |
| `objstore` | get   | 7.05 Melem/s  | 1.75 Melem/s  | **4.0×**          |
| `objstore` | put   | 3.70 Melem/s  | 1.08 Melem/s  | **3.4×**          |
| `objstore` | mixed | 4.55 Melem/s  | 0.60 Melem/s  | **7.6×**          |
| `fs`       | get   | 6.91 Melem/s  | 1.96 Melem/s  | **3.5×**          |
| `fs`       | put   | 3.35 Melem/s  | 1.34 Melem/s  | **2.5×**          |
| `fs`       | mixed | 4.18 Melem/s  | 1.32 Melem/s  | **3.2×**          |

### Persistent

| Scenario   | Op    | holt       | RocksDB       | holt / RocksDB |
|------------|-------|---------------|---------------|-------------------|
| `kv`       | get   | 10.0 Melem/s  | 2.09 Melem/s  | **4.8×**          |
| `kv`       | put   | 1.36 Melem/s  | 0.29 Melem/s  | **4.7×**          |
| `kv`       | mixed | 2.35 Melem/s  | 0.54 Melem/s  | **4.3×**          |
| `objstore` | get   | 7.04 Melem/s  | 1.94 Melem/s  | **3.6×**          |
| `objstore` | put   | 1.35 Melem/s  | 0.38 Melem/s  | **3.5×**          |
| `objstore` | mixed | 2.15 Melem/s  | 0.58 Melem/s  | **3.7×**          |
| `fs`       | get   | 6.90 Melem/s  | 1.90 Melem/s  | **3.6×**          |
| `fs`       | put   | 1.49 Melem/s  | 0.37 Melem/s  | **4.0×**          |
| `fs`       | mixed | 2.35 Melem/s  | 0.55 Melem/s  | **4.3×**          |

## What the gap is made of

### holt reads (100–145 ns)

- Walk depth = `O(key.len)`, not `O(log N)`. For < 64 B keys this is
  3-5 SIMD `pcmpeqb` (Node16) / single-byte indexed loads (Node48 /
  Node256), no pointer chasing across cache lines.
- The whole working set fits in one 512 KB blob inside L2. The
  cached buffer is borrowed in place under a `HybridLatch`
  optimistic snapshot — no `memcpy` per get.
- Path compression via the `Prefix` node folds long shared paths
  into a single hop.

### holt persistent writes (670–745 ns)

Breakdown for a 64 B-value `put`:

| Cost                                                | Time   |
|-----------------------------------------------------|--------|
| Walker mutation (in-place leaf update common case)  | ~180 ns |
| CRC32 over the ≈175 B record (table-driven)         | ~110 ns |
| `Vec::extend_from_slice` + length backpatch         | ~100 ns |
| `Mutex<WalWriter>` lock + auto-flush threshold check | ~40 ns  |
| Amortised syscall for the auto-flush `write_all`    | ~10 ns  |
| Rust runtime / allocator / `pad_key` / misc         | ~230 ns |

The CRC32 + the reference-based `WalWriter::append_insert` fast path
(skip the `TxnOp` enum's three `Vec` clones) together cut the
persistent put cost in half — from ≈1.74 µs in Stage 5c down to
≈735 ns in Stage 5e.

### RocksDB writes (2.5-3.5 µs)

- WAL append + memtable insert per `put`. The WAL append is a
  buffered `pwrite` syscall (no `fsync`), but the memtable insert
  walks a skiplist whose tail isn't always cache-resident.
- Block cache + bloom-filter machinery doesn't help here (small
  dataset; everything is in memory). It's pure constant-factor
  overhead for this workload.

## Caveats

1. **No fsync.** Both persistent benches set `sync = false` —
   "durable to OS page cache" only. A real `fsync`-per-op workload
   (banking-grade durability) is fsync-bound (~1-3 ms on consumer
   SSDs) and overwhelms both engines' algorithm costs. To benchmark
   that profile in holt, set `TreeConfig::wal_sync_on_commit =
   true`.
2. **Small dataset (2000 keys).** Intentionally inside L2 so the
   benchmark isolates engine throughput from cache misses. Metadata-
   engine workloads routinely fit this profile; 100 M-key analytics
   workloads are RocksDB's home turf.
3. **Single-threaded.** Stage 6 phase 2b's `HybridLatch` makes reads
   wait-free — concurrent-read throughput scales linearly with
   cores, but this bench measures single-thread latency.
4. **CRC32 table is the limiting WAL cost.** SIMD-accelerated
   variants (`PCLMULQDQ` on x86, the AArch64 CRC32 intrinsic) would
   drop the WAL emit by another ~80 ns per record. Queued for v0.2.

## Running the suite yourself

```bash
cargo bench --bench main                       # ~3 min full sweep
cargo bench --bench main -- --quick --noplot   # ~1 min smoke
cargo bench --bench main -- kv_get             # single scenario
```

HTML reports land in `target/criterion/`. The `--quick` mode uses a
shorter sample count but is still statistically stable for the
shapes here.
