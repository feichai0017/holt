# Benchmarks

Criterion-based microbenchmarks comparing **artisan** against
**RocksDB** across three realistic shapes of metadata workload.

## Scenarios

| Group | Key shape | Value shape | Models |
|---|---|---|---|
| `kv` | 32-byte random | 64-byte random | Anonymous KV baseline |
| `objstore` | `bucket-NN/path/sub/file-NNNN.bin` | `{"size":...,"etag":...,"class":"STD"}` (~60 B fixed) | S3-style object metadata |
| `fs` | `/usr/local/share/category-N/file-NNNN` | 32-byte packed inode (size + mtime + mode + uid + gid + nlink) | POSIX filesystem metadata |

Each scenario runs three workloads:

- `*_get` — random key lookup over a pre-loaded dataset
- `*_put` — random key replacement (in-place update)
- `*_mixed` — 50% get / 50% put, key chosen at random

## Running

```sh
# Full sweep (~3 minutes — criterion's default 5s/benchmark × 18 benches):
cargo bench --bench main

# Quick smoke pass (~1 minute):
cargo bench --bench main -- --quick --noplot

# A single scenario:
cargo bench --bench main -- kv_get
```

HTML reports land in `target/criterion/`.

## Methodology — apples-to-apples

Two parallel comparisons, each in a fair-rules subgroup:

### Memory / no-WAL group (`*_get` / `*_put` / `*_mixed`)

Engine algorithm cost only — durability disabled on both sides:

- **artisan**: `TreeConfig::memory()` with `flush_on_write = false`.
  Mutations stay in the BufferManager-pinned root blob.
- **RocksDB**: temp-dir DB, `disable_wal = true`, `sync = false`,
  64 MB memtable, compression disabled.

### Persistent group (`*_persist_get` / `*_persist_put` / `*_persist_mixed`)

Both engines disk-backed with WAL enabled, per-op durability to
the OS page cache (not fsync) — the "you survive a process
crash, not a power failure" mode that high-throughput services
target:

- **artisan**: `TreeConfig::new(tempdir)` (PersistentBackend with
  `F_NOCACHE` on macOS / `O_DIRECT` on Linux). Every `put` /
  `delete` / `rename` emits a `TxnOp` to the WAL writer.
  `wal_sync_on_commit` stays at its default `false`, so the
  records sit in the writer's pending buffer / OS page cache (no
  per-op fsync). The blob image only hits disk at
  `Tree::checkpoint`.
- **RocksDB**: temp-dir DB, `disable_wal = false`, `sync = false`.
  Each `put` appends to the WAL (buffered) plus the memtable.

> **Why artisan persist put is slower than artisan memory put
> (≈1.7 µs vs ≈180 ns)**: the extra ≈1.5 µs is pure WAL emit
> cost. Approximate breakdown per record:
>
> - 3 `Vec::to_vec` clones (key + value + prev_value) for the
>   `TxnOp` enum: ≈240 ns
> - CRC32 (bitwise IEEE 802.3) over ~175 record bytes: ≈300 ns
> - `Vec::extend_from_slice` writes + length backpatch: ≈100 ns
> - `Mutex<WalWriter>` lock + the auto-flush threshold check:
>   ≈40 ns
> - Amortised syscall for the auto-flush write_all (one per
>   ~800 records at the 64 KB threshold): ≈10 ns
>
> Group-commit auto-flush (Stage 5d) keeps the user-space buffer
> bounded to 64 KB, so this cost is constant per record across
> long-running processes; without it the buffer would balloon
> unboundedly between `checkpoint()` calls. Queued perf wins:
> a 256-entry CRC table (≈2.5× CRC speedup) and a
> reference-based `append_insert(&[u8], &[u8], …)` fast path
> that skips the `TxnOp` enum's clones.
>
> The `*_persist_get` numbers remain apples-to-apples: neither
> engine touches disk on the get path.

Other shared settings:

- 2000 unique keys preloaded; bench iterates over a random
  permutation of that set
- Seeded RNG → reproducible across runs
- `cargo bench` builds with `lto="thin"`, `codegen-units=1`,
  `opt-level=3`
- Single-threaded

## Sample results

Apple M-series laptop, `cargo bench --bench main -- --quick`,
post-`Stage 6 phase 2b` (HybridLatch / optimistic reads):

### Memory / no-WAL

| Scenario | Op | artisan | RocksDB | artisan / RocksDB |
|---|---|---|---|---|
| `kv` | get | **9.45 Melem/s** | 1.89 Melem/s | **5.0×** |
| `kv` | put | **5.26 Melem/s** | 1.29 Melem/s | **4.1×** |
| `kv` | mixed | **6.58 Melem/s** | 1.26 Melem/s | **5.2×** |
| `objstore` | get | **7.05 Melem/s** | 1.75 Melem/s | **4.0×** |
| `objstore` | put | **3.70 Melem/s** | 1.08 Melem/s | **3.4×** |
| `objstore` | mixed | **4.55 Melem/s** | 0.60 Melem/s | **7.6×** |
| `fs` | get | **6.91 Melem/s** | 1.96 Melem/s | **3.5×** |
| `fs` | put | **3.35 Melem/s** | 1.34 Melem/s | **2.5×** |
| `fs` | mixed | **4.18 Melem/s** | 1.32 Melem/s | **3.2×** |

### Persistent

| Scenario | Op | artisan | RocksDB | artisan / RocksDB |
|---|---|---|---|---|
| `kv` | get | **9.56 Melem/s** | 1.98 Melem/s | **4.8×** |
| `kv` | put | **4.71 Melem/s** | 0.39 Melem/s | **12.0×** |
| `kv` | mixed | **6.79 Melem/s** | 0.54 Melem/s | **12.5×** |
| `objstore` | get | **6.85 Melem/s** | 1.90 Melem/s | **3.6×** |
| `objstore` | put | **3.56 Melem/s** | 0.41 Melem/s | **8.7×** |
| `objstore` | mixed | **4.45 Melem/s** | 0.63 Melem/s | **7.1×** |
| `fs` | get | **6.75 Melem/s** | 1.91 Melem/s | **3.5×** |
| `fs` | put | **3.23 Melem/s** | 0.40 Melem/s | **8.2×** |
| `fs` | mixed | **4.38 Melem/s** | 0.59 Melem/s | **7.4×** |

Per-op latency, memory mode: artisan get ≈ 105–145 ns, put ≈
190–300 ns. Per-op latency, persistent mode: artisan get
≈ 105–148 ns (unchanged — BM cache hit), put ≈ 212–309 ns
(slightly higher than memory due to extra atomic-counter increment
on spillover-attempt path; RocksDB persistent put ≈ 2.4–2.5 µs
dominated by the WAL buffered write).

### Why artisan wins on this shape

- **The whole tree fits in L2.** 200–250 KB of leaves + internal
  nodes for 2000 keys; the cached root blob is a single 512 KB
  buffer and stays hot. RocksDB's memtable adds skiplist
  pointer-chasing overhead.
- **SIMD Node16 lookup.** SSE2 / NEON `pcmpeqb`+`movemask` reduces
  the medium-fan-out byte-search to ~3 instructions.
- **In-place update on same-size values.** When the new value fits
  inside the existing leaf extent (very common — `objstore` /
  `fs` workloads pin value length, `kv` uses 64 B everywhere),
  artisan rewrites the bytes in place. Zero allocator activity,
  zero extent leak.

## Caveats — honest read

artisan's current implementation has constraints that matter once
you go bigger:

1. **No WAL yet.** The `*_persist_put` numbers favour artisan
   because it has no write-ahead log; RocksDB has its WAL turned
   on. Once Stage 5 (WAL) lands artisan's persistent `put`
   numbers will close the gap. The `*_persist_get` numbers are
   the apples-to-apples read comparison.
2. **No fsync.** Both persistent benches set `sync = false` —
   "durable to OS page cache" only. A real `fsync`-per-op
   workload (banking-grade durability) is fsync-bound (~1–3 ms
   on consumer SSD) and overwhelms both engines' algorithm costs.
3. **Small dataset (2000 keys).** Intentionally inside L2 so the
   benchmark isolates engine throughput from cache misses. The
   metadata-engine workloads artisan targets (directory listings,
   S3 metadata, AI artefact catalogs) routinely fit this profile;
   100M-key analytics datastore workloads are RocksDB's home turf.
4. **Single-threaded.** Stage 6 phase 2b's HybridLatch makes
   reads wait-free — concurrent-read throughput scales with
   cores, but this bench measures single-thread latency.

This bench is the right comparison for **metadata-engine
workloads** where the per-tree dataset is bounded — directory
listings, S3 metadata, inode tables, AI artefact catalogs. It is
not the right comparison for "100M-key analytics datastore"
workloads; that's RocksDB's home turf.
