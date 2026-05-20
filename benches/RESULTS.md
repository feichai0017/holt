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

# Put-only scale curve refreshed for the current v0.3 branch.
cargo bench --bench main -- _scale_put --noplot --output-format bencher
```

Each criterion sample is one op. Numbers are Criterion bencher
`ns/iter` point estimates in nanoseconds, with noise bands printed
by the command; lower is better. Holt's per-op numbers are
randomised over a 10 000-key dataset (see `gen_*_dataset`);
RocksDB / SQLite are driven by the same dataset for fair
comparison.

## Test environment

- **Hardware**: Apple M3 Pro (12 cores), 36 GB RAM
- **OS**: macOS 26.3 (Darwin 25.0.0)
- **Rust**: 1.94.0 stable, release profile (`lto=thin`,
  `codegen-units=1`, `opt-level=3`)
- **holt**: current v0.3 working tree for the refreshed scale-put
  tables (atomic maintenance gate, root-dirty precision, and
  root shared-routing for cross-blob puts). Older get/list tables
  are from the same M3 Pro benchmark track and should be
  refreshed before a release tag.
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
`{ 20 k, 100 k, 500 k, 2 M }` keys × three workload shapes
(scale-put tables refreshed on the current v0.3 branch; get
tables are from the earlier v0.3 M3 Pro run):

- **Get scales beautifully**: holt wins every get cell at every
  tier. The lead vs RocksDB widens to **5.4× / 2.8× / 2.2×** at
  2 M (kv / objstore / fs) as the LSM's read-amplification
  finally bites.
- **Put wins every point estimate in the current scale-put run**:
  holt wins puts at every tier through 2 M. At 2 M: **1.38×**
  ahead of RocksDB on kv, **1.13×** ahead on objstore, and
  **1.05×** on fs; vs SQLite the same cells are **1.32× /
  1.22× / 1.09×**. Treat fs-vs-RocksDB as the tight remaining
  write cell, not as a large win.

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

`kv_scale_put` (v0.3 current — blind `put` + cross-blob latch coupling):

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **294** |        1 608 |         644 |       5.5× |      2.2× |
| **100 k** |   **531** |        1 545 |       1 008 |       2.9× |      1.9× |
| **500 k** |   **818** |        1 716 |       1 346 |       2.1× |      1.6× |
|  **2 M**  | **1 313** |        1 806 |       1 729 |   **1.38×** |     1.32× |

### `objstore` (S3-shaped path keys with ~30-byte shared prefix per bucket)

`objstore_scale_get`:

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **232** |          634 |         542 |       2.7× |      2.3× |
| **100 k** |   **387** |          889 |         771 |       2.3× |      2.0× |
| **500 k** |   **824** |        1 227 |       1 121 |       1.5× |      1.4× |
|  **2 M**  | **1 088** |    **3 066** |       1 358 |   **2.8×** |      1.2× |

`objstore_scale_put` (v0.3 current):

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **338** |        1 507 |         636 |       4.5× |      1.9× |
| **100 k** |   **659** |        1 724 |       1 014 |       2.6× |      1.5× |
| **500 k** | **1 161** |        1 671 |       1 390 |       1.4× |      1.2× |
|  **2 M**  | **1 511** |        1 713 |       1 842 |   **1.13×** |     1.22× |

### `fs` (POSIX paths, very long common prefix per directory)

`fs_scale_get`:

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **218** |          670 |         584 |       3.1× |      2.7× |
| **100 k** |   **378** |          872 |         772 |       2.3× |      2.0× |
| **500 k** |   **803** |        1 257 |       1 144 |       1.6× |      1.4× |
|  **2 M**  | **1 105** |    **2 382** |       1 353 |   **2.2×** |      1.2× |

`fs_scale_put` (v0.3 current):

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **326** |        1 542 |         700 |       4.7× |      2.1× |
| **100 k** |   **695** |        1 608 |         979 |       2.3× |      1.4× |
| **500 k** | **1 116** |        1 787 |       1 365 |       1.6× |      1.2× |
|  **2 M**  | **1 482** |        1 556 |       1 617 |   **1.05×** |     1.09× |

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

#### Put path: v0.3 close-out

v0.2.1 had an honest gap on 2 M path-shaped put: -13 % vs
RocksDB on objstore, -18 % vs RocksDB on fs. **The current v0.3
line closes that comparative gap in this run**: all three 2 M
put cells are ahead of both RocksDB and SQLite, including the
path-shaped metadata workloads. Do not over-read absolute deltas
against the older v0.2.1 numbers; the table below is useful for
direction, while the speedup columns come from the current
same-run baselines.

| 2 M put  | v0.2.1 | current v0.3 | Δ      | current v0.3 vs Rocks | vs SQLite |
| -------- | -----: | -----------: | -----: | --------------------: | --------: |
| kv       | 1 296  |        1 313 | +1 %   | **1.38×** ahead | 1.32× ahead |
| objstore | 1 503  |        1 511 | +1 %   | **1.13×** ahead | 1.22× ahead |
| fs       | 1 492  |        1 482 | -1 %   | **1.05×** ahead | 1.09× ahead |

The root cause of the v0.2.1 gap was **API + walker constant-
factor overhead**, not the cross-blob descent cost we initially
attributed it to:

1. `Tree::put`'s `Result<Option<Vec<u8>>>` signature forced a
   per-op leaf-extent value read + clone on every same-key
   update, even though the bench never consumed the returned
   `Option`. RocksDB / SQLite's blind overwrite paid no
   equivalent cost.
2. `insert_into_prefix` allocated a `Vec` per Prefix descent to
   work around a borrow it didn't actually need (`Prefix` is
   `Copy`). Hot on path-shaped keys where Prefix chains are
   deep.
3. WAL `Insert.prev_value` was encoded as `Some(prev)` on every
   put even though replay never reads it; pure wire-format
   overhead.

v0.3.0 split `put` (blind, `Result<()>`) from `insert` (returning,
`Result<Option<Vec<u8>>>`); same for `delete` / `remove`. Blind
walker path skips the leaf-extent value read, drops the prefix
`.to_vec()`, and writes `Option::None` into the WAL `prev_value`
slot. The later cross-blob latch-coupling + `BlobNode` format
break removes the parent-held fallback path and the child-entry
repair work.

The current v0.3 line adds three more hot-path fixes on top:

1. Cross-blob updates return a precise `root_dirty` bit, so a
   child-only update no longer takes the dirty-map mutex again to
   mark the root dirty.
2. Cross-blob puts route through the root under a shared latch,
   acquire the child write latch while the edge is still stable,
   then mutate only from the child down. This removes the root
   exclusive latch from the large-tree steady state.
3. Padded-key construction keeps the 256-byte inline fast path
   but writes only the live key bytes + terminator, avoiding a
   full stack-buffer clear per op.

**Adjacent v0.3 wins.** These scale-put benches exercise blind
`put` on a single op at a time. Several v0.3 changes matter more
for adjacent surfaces than for this exact table:

- **WAL format v3** (`Insert.prev_value` / `Erase.value` slots
  removed entirely). Bench writes are blind, so the v0.3.0 path
  already wrote `Option::None` for those slots — saving one
  presence byte per record. The full win lands on the *returning*
  `Tree::insert` / `Tree::remove` paths, which v0.3.0 still
  wrote `Some(prev)` for; the current v0.3 line doesn't write the
  prev value to the WAL at all (the walker hands it straight to
  the caller).
- **Public lookup surface stays owned.** The draft
  `Tree::get_with` callback API was removed before v0.3 ships;
  this bench continues to measure the stable `Tree::get` path
  that returns owned `Vec<u8>` values.
- **`Tree::txn` batch encoder bypass.** Single-op writes are
  unaffected; multi-op batches skip per-op intermediate `Vec`
  clones that the v0.3.0 `wal_ops: Vec<TxnOp>` aggregator forced.
  Bench doesn't exercise `txn`.

The weakest remaining cell is **`fs_scale_put` at 2 M**. It now
wins by point estimate (1.05× over RocksDB, 1.09× over SQLite),
but the RocksDB comparison is still tight. This is the regime
where LSM and B-tree write paths are most competitive: WAL append
+ memtable/page update stay cheap regardless of working-set size,
while ART-over-blobs pays cross-blob descent plus deeper Prefix
chains on long path keys. Treat this as "ahead, but not a large
write win"; the larger claims are still on get/list/list_dir.

#### What this means in practice

- **Read-dominated metadata workloads at any scale**: holt wins
  cleanly across kv / objstore / fs / list / list_dir, with the
  lead widening at larger working sets (5.4× / 2.8× / 2.2× at
  2 M get).
- **Mixed workloads**: holt wins puts too at every tier in the
  current scale-put run. The caveat is 2 M fs put: it is a small
  win, not a large one. If your workload sits there with heavy
  write skew, size the holt buffer pool to hold the hot set and
  benchmark the exact key shape before making a durability choice.

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
| ops              |   6 391 929   |
| throughput       |   266 957 ops/s |
| **mean**         |     12.31 µs  |
| **p50**          |      1.71 µs  |
| **p95**          |     22.88 µs  |
| **p99**          |    108.09 µs  |
| p99.9            |     688.13 µs |
| max              |  48 365.57 µs |

### Observations

- **267 k ops/s sustained** with 4 writer threads + a
  background checkpointer + concurrent `compact()`. Each
  writer averages ~67 k ops/s on its own. This run predates the
  journal-worker group-commit cut; rerun it before quoting current
  tail-latency numbers.
- **p50 ≈ 1.7 µs** — most puts hit only the common walker mutate
  + dirty publish + WAL append path with no maintenance
  interference.
- **p95 ≈ 23 µs / p99 ≈ 108 µs** — in this pre-group-commit run,
  tail was dominated by the WAL/checkpoint publish point during
  checkpoint snapshots. Current code uses a journal worker plus
  `commit_lock`; rerun the bench for updated numbers.
- **p99.9 ≈ 0.69 ms** with one max outlier near 48 ms — online
  compact no longer dominates the steady tail, but rare scheduler
  / checkpoint interference still shows up at the extreme max.
- This refreshed run also exercises the dirty-snapshot / eviction
  interlock: checkpoint-owned `flushing` entries remain protected
  until `write_through` completes, so the run finishes without the
  previous `dirty entry lost cache image` invariant failure.

The mean-vs-p50 gap (12.3 µs mean vs 1.7 µs p50) reflects that
the slow tail is real but bounded — the distribution is not
long-tailed enough to perturb the median.
