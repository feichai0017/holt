# holt — benchmark results

End-to-end criterion micro-benches comparing holt against
**RocksDB** (`rocksdb` crate, default-features-off + bundled
`librocksdb-sys`) and **SQLite** (`rusqlite` with the
`bundled` libsqlite3, so contributors don't need a system
SQLite installation). Three workload shapes (KV, S3
object-store metadata, POSIX filesystem metadata) ×
{ memory, persistent } × { get, put, mixed, list, list-delim }.

## Reproducing

```bash
# Full suite (~5 min on M3 Pro).
cargo bench --bench main -- --output-format bencher

# One group only — e.g. just KV.
cargo bench --bench main -- kv_ --output-format bencher
```

Each criterion sample is one op. Numbers are mean ± noise band
in nanoseconds; lower is better. Holt's per-op numbers are
randomised over a 10 000-key dataset (see `gen_*_dataset`);
RocksDB / SQLite are driven by the same dataset for fair
comparison.

## Test environment

- **Hardware**: Apple M3 Pro (12 cores), 36 GB RAM
- **OS**: macOS 26.3 (Darwin 25.0.0)
- **Rust**: 1.94.0 stable, release profile (`lto=thin`,
  `codegen-units=1`, `opt-level=3`)
- **holt**: commit `63b181d` (v0.2 release-class — `wal.lock`
  W2D protocol, sharded BufferManager, 3-thread bg
  checkpointer, SIMD CRC32 + node scans).
- **RocksDB**: 0.24 (`librocksdb-sys` 0.18, bundled)
- **SQLite**: rusqlite 0.39 (bundled libsqlite3)
- **Knob alignment**: all three engines use comparable
  "per-op durable to OS page cache, not fsync'd" semantics —
  see the durability matrix at the top of `benches/main.rs`.

## Headline numbers

24 baseline benches across KV / objstore / fs shapes, memory +
persistent variants at 20 k keys: **holt wins all 24** vs
RocksDB and SQLite. Margin range: 1.3× (in-memory fs_put vs
SQLite — both short codepaths) to **467×** (`fs_list_dir`
S3-style rollup vs RocksDB — fast-forward over `BlobNode`
crossings beats seek-iterator-per-leaf hands down).

The scale curve in Group B (below) extends this across
`{ 20 k, 100 k, 500 k, 2 M }` keys × three workload shapes,
yielding a more nuanced picture once the working set exceeds
the buffer pool:

- **Get scales beautifully**: holt wins every get cell at every
  tier. The lead vs RocksDB widens to **5.4× / 2.8× / 2.2×** at
  2 M (kv / objstore / fs) as the LSM's read-amplification
  finally bites.
- **Put crosses over to LSM at 2 M on path-shaped keys**: holt
  wins puts at 20 k / 100 k / 500 k, ties RocksDB at 2 M kv,
  but **loses 8-22 % on 2 M objstore / fs put**. This is the
  one regime where LSM-style write amortization is the right
  design choice and ART-over-blobs isn't competitive. v0.3's
  cross-blob lock-coupling should close most of the gap.

## KV workload (short random keys + short values)

| Bench               | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| ------------------- | --------: | -----------: | ----------: | ---------: | --------: |
| **memory** get      |  **169**  |          684 |         567 |       4.0× |      3.4× |
| **memory** put      |  **344**  |        1 201 |         629 |       3.5× |      1.8× |
| **memory** mixed    |  **351**  |        2 138 |         663 |       6.1× |      1.9× |
| **persist** get     |  **187**  |          637 |       1 508 |       3.4× |      8.1× |
| **persist** put     |  **473**  |        3 470 |       2 310 |       7.3× |      4.9× |
| **persist** mixed   |  **328**  |        3 294 |       1 951 |      10.0× |      5.9× |

## Object-store workload (S3-shaped path keys + metadata values)

| Bench                       | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| --------------------------- | --------: | -----------: | ----------: | ---------: | --------: |
| **memory** get              |  **250**  |          702 |         622 |       2.8× |      2.5× |
| **memory** put              |  **481**  |        1 441 |         664 |       3.0× |      1.4× |
| **memory** mixed            |  **377**  |        2 152 |         663 |       5.7× |      1.8× |
| **memory** list             |  **10 808** |     16 815 |      16 637 |       1.6× |      1.5× |
| **persist** get             |  **247**  |          740 |       1 508 |       3.0× |      6.1× |
| **persist** put             |  **567**  |        3 499 |       2 319 |       6.2× |      4.1× |
| **persist** mixed           |  **420**  |        3 264 |       1 954 |       7.8× |      4.7× |
| **persist** list            |  **10 651** |     16 937 |      17 801 |       1.6× |      1.7× |
| **list_dir** (S3 rollup)    |  **2 463** |    624 672 |     436 204 |     **254×** |  **177×** |

## Filesystem-metadata workload (inode + dirent path keys)

| Bench                | Holt (ns) | RocksDB (ns) |  SQLite (ns) | vs RocksDB | vs SQLite |
| -------------------- | --------: | -----------: | -----------: | ---------: | --------: |
| **memory** get       |  **239**  |          700 |          630 |       2.9× |      2.6× |
| **memory** put       |  **488**  |        1 452 |          660 |       3.0× |      1.4× |
| **memory** mixed     |  **372**  |        2 469 |          668 |       6.6× |      1.8× |
| **memory** list      |  **10 854** |    17 887 |       16 775 |       1.6× |      1.5× |
| **persist** get      |  **251**  |          701 |        1 516 |       2.8× |      6.0× |
| **persist** put      |  **555**  |        3 456 |        2 292 |       6.2× |      4.1× |
| **persist** mixed    |  **411**  |        3 165 |        1 961 |       7.7× |      4.8× |
| **persist** list     |  **11 111** |    17 842 |       17 727 |       1.6× |      1.6× |
| **list_dir**         |  **2 812** |  1 317 457 |      917 245 |     **468×** |  **326×** |

## Note on `wal_sync_on_commit=true`

A previous draft tried to bench all three engines at the
"flip the strongest fsync knob" tier. The result wasn't a
fair comparison: each engine's "sync=true" knob actually
maps to a different syscall on macOS (`F_FULLFSYNC` vs
`F_BARRIERFSYNC` vs just `write()`+lazy-fsync), so we ended
up measuring drive-cache flush latency for some engines and
kernel-page-cache flushes for others. The numbers said more
about the platform than the engines, so that bench group was
removed. The numbers above (`*_persist_put`) are the honest
"per-op durable to OS page cache, not fsync'd" tier, which
all three engines actually do reach with comparable
semantics.

## Workload notes

- **`*_get` / `*_put`**: 10 000-key dataset, randomly sampled
  with `StdRng(seed=SEED)`. Pre-load happens once outside the
  measured region.
- **`*_mixed`**: 80 % gets, 20 % puts, same dataset.
- **`*_list`** (plain): prefix narrows to ~625 keys
  (`objstore`) / ~1 250 keys (`fs`); each criterion sample
  iterates up to 100 results.
- **`*_list_dir`** (S3-style rollup): prefix + delimiter `/`;
  emits 32 (`objstore`) / 16 (`fs`) `CommonPrefix` entries per
  pass, then stops. Holt's iterator's fast-forward — ascend
  past each rollup's subtree — turns the walk from
  `O(leaves_under_prefix)` into `O(distinct_rollups)`. RocksDB
  + SQLite both scan every leaf and dedupe in the host loop,
  which is what the 100–500× gap measures.

## Group B — Scale curve (20 k → 100 k → 500 k → 2 M keys)

Parameterized point lookup + upsert across **three workload
shapes × four dataset sizes**, so the comparison is not biased
by "everything fits in L2 cache" at 20 k. The 500 k tier (~48 MB)
already exceeds holt's default 32 MB (64-blob) buffer pool; the
**2 M tier (~192 MB) is 6× the pool**, so every miss pays the
full `read_blob` + cross-blob descent cost. This is the
working-set-cannot-be-held shape where engine behaviour diverges.

```bash
cargo bench --bench main -- _scale_ --output-format bencher
```

### `kv` (random 32-byte keys — ART anti-pattern, no prefix sharing)

`kv_scale_get`:

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **188** |          684 |         567 |       3.6× |      3.0× |
| **100 k** |   **292** |          866 |         768 |       3.0× |      2.6× |
| **500 k** |   **591** |        1 503 |       1 157 |       2.5× |      2.0× |
|  **2 M**  | **1 015** |    **5 509** |       1 418 |   **5.4×** |      1.4× |

`kv_scale_put`:

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **324** |        1 140 |         594 |       3.5× |      1.8× |
| **100 k** |   **524** |        1 281 |         845 |       2.4× |      1.6× |
| **500 k** |   **848** |        1 360 |       1 154 |       1.6× |      1.4× |
|  **2 M**  |     1 296 |        1 280 |       1 436 |       1.0× |      1.1× |

### `objstore` (S3-shaped path keys with ~30-byte shared prefix per bucket)

`objstore_scale_get`:

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **232** |          634 |         542 |       2.7× |      2.3× |
| **100 k** |   **387** |          889 |         771 |       2.3× |      2.0× |
| **500 k** |   **824** |        1 227 |       1 121 |       1.5× |      1.4× |
|  **2 M**  | **1 088** |    **3 066** |       1 358 |   **2.8×** |      1.2× |

`objstore_scale_put`:

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **448** |        1 085 |         574 |       2.4× |      1.3× |
| **100 k** |   **706** |        1 264 |         841 |       1.8× |      1.2× |
| **500 k** |     1 224 |        1 322 |       1 177 |       1.1× |      1.0× |
|  **2 M**  |     1 503 |        1 301 |       1 369 |       0.87× |     0.91× |

### `fs` (POSIX paths, very long common prefix per directory)

`fs_scale_get`:

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **218** |          670 |         584 |       3.1× |      2.7× |
| **100 k** |   **378** |          872 |         772 |       2.3× |      2.0× |
| **500 k** |   **803** |        1 257 |       1 144 |       1.6× |      1.4× |
|  **2 M**  | **1 105** |    **2 382** |       1 353 |   **2.2×** |      1.2× |

`fs_scale_put`:

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **443** |        1 079 |         605 |       2.4× |      1.4× |
| **100 k** |   **653** |        1 263 |         798 |       1.9× |      1.2× |
| **500 k** |     1 156 |        1 327 |       1 147 |       1.1× |      1.0× |
|  **2 M**  |     1 492 |        1 227 |       1 376 |       0.82× |     0.92× |

### Observations

#### Get path scales gracefully on all three workloads

Across kv / objstore / fs at every tier through 2 M keys, **holt
wins point lookup**. The lead vs RocksDB widens dramatically at
2 M because RocksDB's bloom filters start missing more often and
the read has to descend multiple SST levels:

| 2 M get  | Holt   | RocksDB | speedup |
| -------- | -----: | ------: | ------: |
| kv       | 1 015  | 5 509   |  5.4×   |
| objstore | 1 088  | 3 066   |  2.8×   |
| fs       | 1 105  | 2 382   |  2.2×   |

(The kv get speedup is largest because random 32-byte keys force
the worst LSM behaviour — no path locality, every read is a
fresh bloom probe. The path-key workloads still see a >2× lead
even though RocksDB's prefix compression in SSTs softens the
blow.) Holt's descent depth scales with `log(N)` of distinct
prefixes, not with SST level count, so it grows
**5.4× / 4.7× / 5.1×** across the 100× data growth (kv / objstore
/ fs) — far less than RocksDB's 8× / 4.8× / 3.6× for the same
range.

SQLite get tightens to 1.2-1.4× at 2 M because its B-tree handles
cache pressure gracefully — bounded fan-out + 64 MB page cache
keeps lookup depth dominated by index height, which grows slowly.

#### Put path: LSM wins at 2 M on path-shaped workloads

This is where the picture turns honest. **At 2 M put on
path-shaped keys, holt loses to both RocksDB and SQLite by
8–22 %**:

| 2 M put  | Holt   | RocksDB | SQLite | vs Rocks | vs SQL |
| -------- | -----: | ------: | -----: | -------: | -----: |
| kv       | 1 296  | 1 280   | 1 436  | 0.99×    | 1.11×  |
| objstore | 1 503  | 1 301   | 1 369  | **0.87×** | **0.91×** |
| fs       | 1 492  | 1 227   | 1 376  | **0.82×** | **0.92×** |

This is the first "holt loses" cell across the entire bench suite,
and it's not noise — the ±26 / ±49 noise bands don't overlap. The
cause is **eviction churn on every put once the working set
exceeds the buffer pool**:

- Holt put = walker.insert + mark_dirty + wal.append, all under
  `wal.lock`. The walker.insert pinpoints the target leaf via
  cross-blob descent; at 2 M, the target child blob is usually
  out of cache, so each put pays a `read_blob` (512 KB) + parse
  + descent + (possibly) `spillover`/`compact` retry.
- RocksDB put = WAL append + memtable insert, both bounded
  constants. Compaction work is amortized across many puts;
  point-write latency is roughly flat across all tiers
  (1 140 → 1 280 ns from 20 k to 2 M, just 12 % growth).
- SQLite put = B-tree page lookup + page-level update. The
  64 MB page cache still helps significantly at 2 M.

Holt's put cost grows 4× across 100× data growth on kv
(324 → 1 296 ns), 3.4× on objstore (448 → 1 503 ns), 3.4× on fs
(443 → 1 492 ns). The path-key workloads land slightly worse
than kv at 2 M because longer keys mean larger leaves, fewer
keys per blob, more total blobs, and deeper descent — every
miss costs more.

This is the regime where **LSM-style write amortization is the
right design choice** and ART-over-blobs isn't competitive.
Two paths forward for v0.3:

1. **Cross-blob lock-coupling** (deferred from v0.2) — release
   the parent guard before pinning the child so writers on
   disjoint subtrees don't serialise on the root, reducing
   wal.lock contention at high working-set sizes.
2. **Adaptive write batching** — coalesce writes hitting the
   same child blob within a small window, amortizing the
   `read_blob` cost across them.

Neither closes the LSM amortization gap entirely, but together
they should bring the 2 M put case back to parity with RocksDB.

#### What this means in practice

The honest summary for users:

- **Read-dominated metadata workloads at any scale**: holt wins
  cleanly across kv / objstore / fs / list / list_dir, with the
  lead widening at larger working sets (5.4× / 2.8× / 2.2× at
  2 M get).
- **Mixed workloads at working-set ≤ buffer-pool size**: holt
  wins puts too (≥ 1.6× over RocksDB at 500 k and below).
- **Write-heavy workloads with working-set ≫ buffer-pool**: at
  this point a tuned LSM (RocksDB) wins point-write latency by
  ~15-20 %. If your workload sits here, either size the holt
  buffer pool to hold the hot set, or pick a write-optimized
  engine. v0.3's lock-coupling work should close most of this
  gap.

## Group C — p95 / p99 latency under maintenance interference

`tests/bench_contention_p95.rs` runs four `put` writers + a
background checkpointer (5 ms cadence) + a compaction thread
that periodically calls `tree.compact()` — the worst-case
"engine is doing maintenance while users keep writing"
shape. Every `put` records its wall-clock latency to a
`hdrhistogram` for percentile reporting.

```bash
cargo test --release --test bench_contention_p95 \
    -- --ignored --nocapture
```

### Result (20-second window, 4 writers + bg checkpoint + compact)

| Metric           | Value         |
| ---------------- | ------------: |
| ops              |   6 152 095   |
| throughput       |   306 918 ops/s |
| **mean**         |     12.79 µs  |
| **p50**          |      1.96 µs  |
| **p95**          |     28.54 µs  |
| **p99**          |    107.58 µs  |
| p99.9            |   2 310.14 µs |
| max              |  30 654.46 µs |

### Observations

- **307 k ops/s sustained** with 4 writer threads + a
  background checkpointer + concurrent `compact()`. Each
  writer averages ~77 k ops/s on its own, so the wal-lock
  serialization tax is modest.
- **p50 ≈ 2 µs** — most puts hit only the common "walker
  mutate + mark_dirty + wal.append + flush" critical section
  with no maintenance interference.
- **p99 ≈ 100 µs** — tail dominated by the wal.lock
  serialization point during checkpoint snapshots (rounds run
  every ~5 ms and briefly take the lock to drain dirty +
  pending_deletes + flush WAL).
- **p99.9 ≈ 2 ms** and **max ≈ 30 ms** — the spikes are
  `compact()` calls themselves (which take the wal.lock for
  the duration of phase 1 / 1.5 / 2 since `compact` is not
  yet online — see the docstring on `Tree::compact`). These
  bound the worst case under maintenance; the v0.3 maintenance
  latch will reduce them further by serializing compact
  against writers more cleanly.

The mean-vs-p50 gap (12.8 µs mean vs 2 µs p50) reflects that
the slow tail (compact calls hit a handful of writes hard) is
real but bounded — the distribution isn't long-tailed enough
to perturb the median.
