# Holt performance findings

A record of the read/write optimization pass on the `perf/u16-children`
branch: what landed, how holt measures against RocksDB/SQLite on a *fair*
benchmark, and — most importantly — the precisely-diagnosed remaining
bottleneck and the honest architectural tradeoffs. Read this before
chasing further perf work so you don't re-derive it.

## What landed (10 commits)

```
c6062da  bench: drop the holt Tree before its TempDir in persistent groups
9261965  perf(addressing)!: child body offsets — drop slot-table read indirection (2 loads→1/hop; manifest v4)
2845bec  perf(layout)!:   flatten leaf → one variable-size self-describing node (manifest v3; subsumes LeafInline)
4ffe973  perf(walker):    one-byte leaf key fingerprint (skip extent read on key mismatch)
10bfa0c  perf(range):     scan-ahead prefetch of the next sibling subtree
a1852d2  perf(walker):    software-prefetch the next node body during descent (+ aarch64 PRFM)
83b92df  perf(buffer-manager): cheap GUID hasher for the blob cache map (pin −37%)
945b9f9  ③ LeafInline   2ffe29a ① NEON 16-lane scan   abd4e69 ② spillover footprint memo
```

Two breaking on-disk format changes (R3 leaf flatten = v3, R1 offset
addressing = v4). Every step validated on **aarch64 (NEON)** and
**x86 (AVX2 + io_uring)** through the corruption gates: `proptest`
(randomized ops vs a BTreeMap/WAL-replay oracle) and
`checkpoint_failpoint` (crash injection). `④ pointer swizzling` was
**measured and rejected** (≤9–16% ceiling, large concurrency surface)
in favor of the GUID hasher.

## Fair benchmark vs RocksDB / SQLite

Methodology (benches/main.rs, holt-bench crate): **N = 20 000**
object-store metadata keys (~30 B path keys, ~60 B JSON values), spread
across ~7 × 512 KB blobs. Persistent groups run holt
`Durability::Wal { sync: false }` with the **journal + background
checkpointer threads running**, vs RocksDB **WAL on, sync off** — a fair
"hot service" durability profile (WAL to page cache, no per-op fsync).
Numbers from the x86 box (perf_event_paranoid=4, no perf sampling).

| operation (persistent, threads running) | holt | RocksDB | result |
|---|---|---|---|
| point read  (objstore_persist_get) | **210 ns** | 499 ns | **holt 2.4× faster** |
| point read  (memory)               | 219 ns | 487 ns | holt 2.2× |
| write       (objstore_persist_put, 1 thread) | 2.6 µs | 2.58 µs | **parity** |
| write       (memory, no WAL)        | 418 ns | 1397 ns | holt 3.3× (no durability) |
| prefix scan (objstore_list, 100 entries) | 16.35 µs | 15.74 µs | **~parity (within 4%)** |

R1 (offset addressing) was the highest-leverage single change: point
read −10.6%, **prefix scan −24.2%** (closed a 30%→4% gap), writes
unchanged.

### Concurrent write (1M keys, persistent WAL + checkpoint, 16-core x86)

| threads | holt | RocksDB |
|---|---|---|
| 1  | 8938 ns/op (0.11 Mops/s) | 3716 ns/op (0.27) |
| 4  | 2211 ns/op (0.45)        | 2044 ns/op (0.49) |
| 8  | 3089 ns/op (0.32)        | 1435 ns/op (0.70) |
| 16 | 3437 ns/op (0.29)        | 1532 ns/op (0.65) |

holt **peaks at 4 threads then negatively scales**; p99 tail at 16
threads ≈ 296 µs vs RocksDB 56 µs. RocksDB scales to ~0.65–0.70 Mops/s.

## Honest conclusions

**holt is a read engine.** It crushes RocksDB on reads (2.4×, durable or
not) because the ART + 512 KB self-describing blobs give one-load node
hops (post-R1) and subtree locality. That is the product story.

**Writes have two separate problems:**

1. **Architectural (hard to beat): in-place tree vs append-only LSM.**
   A holt `put` costs O(tree depth + route-cache miss + possible
   spillover); at 1M keys that's ~513 blobs, depth 2, ~78% route-cache
   miss, 512 spillovers — so single-thread put is ~2.4× RocksDB at scale.
   RocksDB's LSM append is ~O(1) (append memtable + WAL, defer
   reorganization to background compaction). Chasing LSM on raw write
   throughput fights the architecture; don't.

2. **Fixable: concurrent writes serialize on the root blob's latch.**
   The write path is **lock-coupled with exclusive latches**:
   `cross_and_insert` (src/engine/walker/insert.rs) takes the parent
   blob's `BlobWriteGuard` (exclusive), pins the child, takes the
   child's `BlobWriteGuard`, then drops the parent. So **every write to
   any child blob first exclusively latches the root blob** to traverse
   it → all writers serialize on the root's exclusive latch (classic
   lock-coupled-tree root bottleneck). This is why concurrency scales
   negatively and the tail explodes — and it is *not* architectural; it
   is fixable.

   **Attempted fix (REJECTED — measured regression):** optimistic write
   descent (LeanStore-style optimistic lock coupling) — traverse the
   upper blobs optimistically (snapshot `content_version`, read wait-free,
   validate) exactly like the read path, and take the **exclusive latch
   only on the target blob** where the mutation lands (revalidate the
   parent chain by version, restart on a miss, escalate to pessimistic
   after a 4-restart budget; CoW-fork escalates too). Fully implemented
   and **validated for correctness** (lib +3 tests, concurrent_stress 5,
   proptest 5, checkpoint_failpoint 8, restart counts bounded; aarch64
   20/20 + x86 12/12 hardened-stress loops clean, both arches).

   But a **clean same-machine A/B** (x86, `objstore put`, 50k ops/thread,
   RocksDB reproduced identically both runs → clean attribution) showed
   it is a **large regression at every thread count**, not a win:

   | threads | baseline Mops/s | with descent | Δ | baseline p50 | descent p50 |
   |--------:|----------------:|-------------:|---:|------------:|------------:|
   | 1  | 0.105 | 0.087 | **−17%** | 1.9µs | 4.8µs (2.5×) |
   | 4  | 0.453 | 0.138 | **−70%** | 2.9µs | 28.8µs (10×) |
   | 8  | 0.316 | 0.127 | **−60%** | 6.6µs | 62µs (9×) |
   | 16 | 0.287 | 0.134 | **−53%** | 7.3µs | 92µs (13×) |

   **Why it lost:** the workload *grows* the tree, so node splits and blob
   spillover are frequent. When a target mutation needs to split a node
   across a blob boundary (`TargetMutation::Crossing`) or hits a
   snapshot-shared child (`Escalate`), the optimistic attempt is wasted —
   it descends, takes the target latch, discovers it can't complete,
   restarts, burns the budget, and falls back to the full pessimistic
   path, **paying for both descents**. The −17% / 2.5× single-thread p50
   blowup (zero contention) is the proof: the fast path is mostly wasted
   work here, not a parallelism win. Optimistic descent only pays off when
   target blobs are disjoint *and* the mutation lands in-place (no split);
   for a path-shaped, growing object-store keyspace neither holds often
   enough. Code preserved at `docs/experiments/optimistic-write-descent.patch`;
   do not re-attempt without first making the bail path cheap (skip the
   optimistic attempt when the target node is full / a split is likely)
   or solving the real bottleneck below.

   **The real bottleneck is broader than intermediate-blob latching.**
   The baseline *already* scales negatively (0.453 → 0.287 from 4→16
   threads) with an exploding tail (p99 55µs → 286µs). Removing
   intermediate-blob exclusive latches did not help because the
   serialization survivors are (a) the **root/upper-blob exclusive latch**
   — a growing tree mutates upper levels constantly as splits propagate,
   and the root is on every descent — and (b) the **serialized WAL
   group-commit**. Beating RocksDB on *concurrent* write would require
   fine-grained or latch-free upper-level concurrency (ROWEX-style), which
   is a large, high-risk project — not a focused session. Until then,
   holt's write story is honest: **single-thread durable write is at
   parity with RocksDB; concurrent write loses** (RocksDB's LSM absorbs
   concurrent writes into a per-thread memtable/WAL far better). holt's
   durable edge is **reads (2.2–2.44×), scans (~parity), and cold-miss**,
   not write concurrency.

## Suggested next work (each its own focused session)

- ~~**Optimistic write descent**~~ — **DONE & REJECTED** (see diagnosis
  above): implemented, proven correct on both arches, but a clean A/B
  showed a 53–70% concurrent-write *regression* because the growing-tree
  workload bails to pessimistic too often. Patch parked in
  `docs/experiments/`. The remaining concurrent-write bottleneck is the
  root/upper-blob exclusive latch + serialized WAL group-commit, which
  needs latch-free upper-level concurrency (ROWEX-style) — a large
  project, not a focused session.
- **R2 — BlobNode prefix Bloom** — a per-edge Bloom (a Bloom *extent* in
  the parent, sized ~10 bits/key, not inline) so a negative lookup whose
  key matches a crossing's path prefix is answered without pinning +
  reading the 512 KB child. Targets cold-miss / existence checks; the
  crossing's inline path prefix already filters cross-prefix misses, so
  the marginal win is within-prefix existence misses. Write maintenance
  (update the parent edge on insert) is the cost; Bloom = no false
  negatives, so it is correctness-safe to skip on a miss.
- **Key-ordered leaf layout for cold scans** — compaction's `clone_subtree`
  DFS already lays leaves out in key order, so post-compaction/cold scans
  are already sequential; the remaining hot-scan ~4% is the optimistic
  restart-safe cursor's per-entry copy (diminishing returns).

## Benchmark reproduction notes

- RocksDB/SQLite comparators need libclang; the x86 box has only
  `libclang.so.1`, so symlink a shim and point clang-sys at it:
  `mkdir -p ~/libclang-shim && ln -sf /usr/lib/llvm-18/lib/libclang.so.1 ~/libclang-shim/libclang.so`
  then `export LIBCLANG_PATH=$HOME/libclang-shim`.
- Single-thread latency: `cargo bench --manifest-path benches/Cargo.toml --bench main -- --quick --noplot "objstore_persist_(get|put)/(holt|rocksdb)"`
- Concurrency: `HOLT_CONCURRENT_THREADS=1,4,8,16 HOLT_CONCURRENT_OPS_PER_THREAD=50000 HOLT_CONCURRENT_OPS=put cargo bench --manifest-path benches/Cargo.toml --bench concurrent -- objstore`
