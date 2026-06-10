# Cold-read fundamental fix — session handoff

Hand this to the next session. Everything below is grounded in committed code +
measurements; no re-derivation needed.

## TL;DR — start here

- **Branch:** `perf/cold-read-observability` (clean tree).
- **Goal:** stop reading a whole 512 KB blob frame to answer one cold point
  lookup. Measured amplification today: **~529 KB read per cold point read.**
- **Approach (decided):** an **in-blob routing region** — at compaction, cluster
  a blob's internal nodes into a contiguous front region and page-align the
  leaves after it, so a cold read loads the small routing region + **one leaf
  page** (~8–12 KB, or ~4 KB with the routing region resident) and **reuses the
  existing descent**. Full design: `docs/design/cold-read-oracle.md`.
- **Done:** design + page-read primitive + header fields + **stage 2 two-arena
  compaction build** (stages 0–2; stage 2 = `ed997c6`).
- **Next:** **stage 3 — the cold routed read in `src/engine/walker/lookup.rs`**
  (`cold_read_routed`), gated by the `routed_get == tree.get` data-integrity
  test (present / absent / crossing). **Land the mutation-path prerequisite
  first** (see "Stage 3 prerequisite" below): a post-routed in-place structural
  mutation must zero `routing_len`, else stage 3 reads stale geometry.
- **Validation cadence (unchanged):** correctness/compile on **mac (aarch64)**;
  real I/O + benches on **ubuntu (x86)** via rsync (see "Validation" below).

## Why (measured, don't re-measure)

Run the committed analysis any time:
```
cargo test --release -p holt cold_read_page_touch_ceiling -- --ignored --nocapture
```
objstore 300k keys / 48 B values / 225 blobs (~1333 keys/blob):
- A point-lookup descent touches **mean 4.64 distinct 4 KB pages (~18.6 KB), p95
  24 KB** — vs the 512 KB pin (~27× less cold I/O just by paging touched pages).
- **structure/value = 78% / 22%** at 48 B values. ⇒ "keep all *structure*
  resident" is NOT universal (for small values the structure *is* the data). The
  routing region keeps only the **internal nodes** resident-able (small), which
  is value-size-agnostic.

The `cold.idx` sidecar (current `b3a08ac` and below) is **not** the fundamental
fix: it caches `(key→value)` in a second, **unbounded, unaccounted** in-RAM table
(≈1 GB+ for 5 M keys) — a hit-rate play no better than enlarging the buffer pool,
useless when working set >> RAM, and it carries a class of crash/staleness bugs
(see "cold.idx review" below). The routing region is a **miss-cost** play and is
crash-safe by construction.

## What's committed (cold-read line)

| commit | what it gives you |
|---|---|
| `137d5ba` | **Design doc** `docs/design/cold-read-oracle.md` — routing region layout, build, read path, crash/compat, 6-stage plan. |
| `808a5fa` | **Page-read primitive** `BlobStore::read_blob_range(guid, byte_offset, dst)`. FileBlobStore = positional O_DIRECT/F_NOCACHE `pread` (4 KB-aligned, bypasses the 512 KB io_uring ring); Memory = RAM copy; default = read-whole-and-copy. **Dual-arch validated** (`range_read_test::page_reads_reconstruct_each_blob`: page-reads reconstruct every real blob byte-for-byte; x86 O_DIRECT no EINVAL). Also the `cold_read_page_touch_ceiling` analysis in `cold.rs`. |
| `12ce05a` | **Stage 1 — header fields (transparent).** `BlobHeader` gains `routing_off/routing_len/leaf_region_start` (u32, at 0xb0/b4/b8, carved from pad; size still 4096; offset asserts extended). `BlobHeader::routing_region() -> Option<RoutingRegion>` (None ⇒ legacy whole-frame). **Safety:** `BlobFrameMut::init` zeroes the whole frame ⇒ every old/not-yet-recompacted blob reads `routing_len==0` ⇒ full-pin fallback; **no manifest bump needed.** Pinned by `header::tests::zeroed_header_is_legacy_layout`. The reader is `#[allow(dead_code)]` until stage 3. |
| `ed997c6` | **Stage 2 — two-arena compaction build.** `compact_blob` now: pass-0 `routing_budget` sizes the live subtree EXACTLY (via `packed_inner_size`, which mirrors `pack_inner_node`'s tier collapse — a 1-survivor node packs to a 128 B `Prefix`, *larger* than a source `Node4`/`Node16`, so source size would under-count); fixes a page-aligned `leaf_region_start` up front; clones with internals bumping `space_used` (routing arena) and `clone_leaf` drawing from `alloc_leaf_at` (leaf cursor); stamps the header fields. **Back-patch unchanged** (post-order `encode_child_off`). Release-safe overrun guard → legacy rebuild (+`debug_assert!`). Spillover/merge stay legacy; merge demotes a routed parent (`routing_len=0`). Blobs with `< ROUTE_MIN_LEAF_BYTES` (8 KB leaves) stay legacy so the ≤4 KB page-align gap can't dominate `space_used`. Gated by `routing_equals_full_descend_and_oracle` + `routed_compaction_matches_oracle` proptest + degenerate/fallback cases + `packed_inner_size_matches_pack_inner_node`. Full suite + clippy green on aarch64. |

**Stage 2 decisions (beyond the original four):** routing is gated on
`ROUTE_MIN_LEAF_BYTES = 2 * PAGE_4K` (8 KB) — a tunable. Routed `space_used`
= legacy + the ≤4 KB page-align gap, so `is_mergeable` over-counts a routed
child by the gap (conservative/safe) and a tiny blob's compaction would *look*
like growth; the gate keeps small blobs legacy. `routing_len` is emitted
honestly (no upper cap — stage 3 owns any full-pin-on-large-routing policy).

(The WAL ring work — the other big effort — is on a **separate branch**
(`perf/u16-children`) and is unrelated to this cold-read line; don't conflate.)

## Stage 3 prerequisite — mutation-path policy (land before stage 3 trusts the fields)

Stage 2's build is correct in isolation, but a **post-routed in-place
structural mutation** (an insert that grows an internal node, or appends a
leaf via `alloc_node`/`alloc_leaf` from `space_used` = top of the leaf arena)
places a NEW internal node *above* `leaf_region_start`, silently violating the
`off < leaf_region_start ⟺ internal` invariant. `merge_blob` already demotes
(zeros `routing_len`); the general writer path in `src/engine/walker/writers.rs`
(insert / CoW after fork) must do the same: **zero `routing_len` on the first
structural mutation of a routed frame** (simplest, safe). Until that lands,
stage 3's reader cannot trust `routing_region()` on a frame that may have been
mutated since its last compaction. `fork_frame` / snapshot copy bytes verbatim,
so a forked routed frame inherits a `leaf_region_start` that a later in-place
mutation would invalidate too — same fix covers it.

(The WAL ring work — the other big effort — is on a **separate branch**
(`perf/u16-children`) and is unrelated to this cold-read line; don't conflate.)

## Remaining plan (stages 2–6) with concrete entry points

### Stage 2 — two-arena compaction build  ✅ **DONE (`ed997c6`)**

*Implemented as "measure-then-route" (pass-0 `routing_budget` fixes a
page-aligned `leaf_region_start` before the clone, so back-patch is unchanged).
The `install_new_blob` reference below was wrong — fresh-blob creation is
`make_blob_from_node*` in `migrate.rs` (gated to legacy) and
`BufferManager::install_new_blob` only installs pre-built bytes. Original plan
retained below for context.*
**Files:** `src/engine/walker/migrate.rs` (`clone_subtree`, `clone_leaf`,
`compact_blob`), `src/engine/walker/spillover.rs` (`install_new_blob`).
- `clone_subtree` already DFS-walks the source in key order. Make it write into
  **two cursors**: internal nodes (root, `Prefix`, `Node4/16/48/256`, `BlobNode`)
  → routing arena starting at `DATA_AREA_START`; leaves (`[16B hdr][key][value]`)
  → leaf arena, **page-aligned**, after the routing arena. Child offsets are
  back-patched exactly as today (R1 offset_div8 addressing unchanged; offsets
  just land in two zones).
- Set `header.routing_off = DATA_AREA_START`, `routing_len = <internal bytes>`,
  `leaf_region_start = <page-aligned start of leaf arena>`.
- **Invariant the build must guarantee:** every offset `< leaf_region_start` is
  an internal node; every offset `>= leaf_region_start` is a leaf. (This is what
  lets the cold descent tell "internal vs leaf" from the offset without reading
  the node.)
- **Gate (write it first):** a `routing == full` test — build a blob, then assert
  the key set + values obtained by a routing-aware descent equal a full-frame
  descent (and a BTreeMap oracle). Add to proptest.
- Watch: routing region must fit (≤ ~2–3 pages typ.); if a blob's internals
  exceed a budget, leave `routing_len=0` (full-pin fallback) for that blob.
- Spillover (`install_new_blob`) writes fresh blobs too — apply the same layout
  there, or leave spillover blobs legacy and let the next compaction route them.

### Stage 3 — cold routed read  ← **START HERE** (land the mutation-path prerequisite above first)
**File:** `src/engine/walker/lookup.rs` — `cold_lookup_or_pin` (currently ~line
356; the `ColdBlobLookup::Unknown` arm at the non-resident fallback does
`bm.pin(child_guid)` = the 512 KB read). Add `cold_read_routed`:
1. `header.routing_region()` is `None` ⇒ keep the full pin (legacy).
2. Else `read_blob_range(guid, routing_off, …)` the routing region (1–2 pages),
   wrap `[header ++ routing region]`, run the **existing descent**.
3. When the descent reaches a child offset `>= leaf_region_start`:
   `read_blob_range` that one leaf page (two if the leaf straddles / value > 4 KB
   — `value_len` is known), read `[hdr][key][value]`, compare the full key (with
   terminator), return `Found{value,seq}` / `NotFound`. `BlobNode` ⇒ recurse the
   crossing loop.
- **DATA-INTEGRITY GATE:** `routed_get(key) == tree.get(key)` for ≥100k random
  keys incl. **absent** and **crossing** keys. A wrong `NotFound` = silent data
  loss. Dual-arch + cold `bm_read_bytes` drop bench (target ~512 KB → ~8–12 KB).

### Stage 4 — bounded resident routing cache
Keep routing regions hot in a **bounded, accounted** cache (~15–30 MB for 5 M
keys, vs cold.idx's 1 GB+). Cold read → 1 leaf pread. Account it in/alongside the
BM pool budget (do NOT repeat cold.idx's unbounded sin).

### Stage 5 — remove `cold.idx`
The routing region subsumes the sidecar. Delete `src/store/blob_store/file/
cold_index.rs` + the `cold_lookup_blob` sidecar path + `summarize_blob_for_cold_
index` + the manifest generation field if only the sidecar used it. **This
deletes the entire cold.idx bug class** (below). Gate: full suite + the SIGKILL
crash-soak (`cargo run --release --example wal_crash_soak -- 40`).

### Stage 6 — per-blob bloom (later)
A bloom in the header for free *within-prefix* negatives. Orthogonal/additive.

### Stage 3.5/4 addendum — push io_uring to the extreme (cold-read I/O backend)

**Today's io_uring is NOT optimized** (`src/store/blob_store/file/uring.rs`): one
ring behind a **global Mutex**, **synchronous `submit_and_wait(1)` per read** (no
read batching — only checkpoint writes batch via `pwrite_many_at`), **no SQPOLL,
no IOPOLL**. It captures only the *static* registration wins (`register_files` +
`register_buffers` → `ReadFixed`/`WriteFixed`, O_DIRECT). So the cold-read path
runs at **device queue depth 1** — exactly wrong for random reads over a working
set >> RAM. And `read_blob_range` (the page-granular primitive) currently
**bypasses the ring** (plain `read_exact_at`).

This is the right place to fix io_uring, because the only paths that touch disk
are cold misses + checkpoint writes; the warm read path (holt's 2.1–2.4× win)
has **no I/O**, so optimizing io_uring only pays on the cold path — the one this
work redesigns. Fold this into stages 3–4:

1. **Route page-granular cold reads through a batched-async read interface**, not
   plain `pread`. The BM cold path issues a *wave* of leaf-page reads (across
   concurrent queries and/or a single query's pages) as one batch.
2. **Linux backend — io_uring to the extreme:**
   - **Multi-SQE submit**: push N page-read SQEs, `submit_and_wait(N)` → device
     QD = N (the big lever for random cold reads). (Today reads are QD 1.)
   - **Per-core / small pool of rings** to drop the single global Mutex →
     concurrent submission from multiple threads.
   - Optional **IOPOLL** (NVMe busy-poll completions, lowest latency, burns a
     poller core) and **SQPOLL** (kernel-side submit, cuts the `io_uring_enter`
     syscall) behind config flags.
   - Keep the existing fixed files + fixed buffers + ordered batched writes.
3. **Cross-platform backend (macOS + Linux-without-io_uring): a small thread pool
   of blocking `pread`s** → the same device parallelism without io_uring. Do
   **NOT** use Darwin POSIX aio (`aio_read`/`lio_listio`) — it is libc
   thread-pool-emulated and weak. Keep `F_NOCACHE`.
4. **Interface**: extend `BlobStore` with a batched read (e.g.
   `read_pages_batch(&[(guid, page, dst)])` or a submit/poll pair), backed by
   io_uring (multi-SQE) on Linux and the thread pool elsewhere — one interface,
   two backends. macOS is dev/test (prod = Linux NVMe), so it needs *correct +
   parallel*, not *extreme*.
5. **Measure on a REAL cold bench** (ubuntu/x86): dataset >> RAM, **drop the OS
   page cache** (the current 137× is page-cache-warm and not representative),
   sweep **QD = 1 vs 8 / 32 / 64**, report cold p50/p99 + throughput; compare to
   RocksDB at matched block-cache bytes.

Sequencing: do this *after* stage 3 lands the routed read (so there is a real
page-read load to batch), as part of or right after stage 4 (resident routing
cache). Until then, single-op reads are fine.

## cold.idx safety review (why stage 5 deletes a bug class)

A multi-agent review of the cold.idx stack (`ae0c524..b3a08ac`) found (steady
state is sound — residency mutex + manifest-v5 generation are the load-bearing
guards — but the crash boundary + resource discipline have real holes). If
cold.idx is kept as an interim, these need fixing; the routing region avoids
them by construction:

1. **Crash-window generation aliasing (data-integrity):** cold.idx append isn't
   fsync'd and is fsync'd *after* manifest.log; a generation bump lost in a crash
   can be re-issued for different content, so a stale cold record can match the
   manifest generation → resurrected deleted keys / stale values after recovery.
   Cheap fix if kept: **truncate/delete cold.idx whenever reopen replays ≥1 WAL
   record.**
2. **Spurious `Err(NotFound)` on a live key:** `931e055` dropped the parent
   shared guard before resolving the child; a concurrent merge/erase unlinks the
   child between edge-validate and probe → `cold_lookup_or_pin`'s uncaught `?`
   surfaces `Err(BlobStoreIo NotFound)` from `get()`. Fix: hold the parent guard
   across `cold_lookup_or_pin`, or treat `is_blob_store_not_found` as
   restart-from-root.
3. **Unbounded table cache** (no eviction/accounting) — the 137× is "unbounded
   RAM vs 8 MB pool", holt-vs-holt, page-cache-warm (not real cold). Don't quote
   137× as structural/competitive.
4. **Torn-tail `cold.idx` replay** corrupts future opens (valid_len includes the
   orphan header). **Sidecar I/O errors fail authoritative ops / user gets**
   (violates "rebuildable, not source of truth"). `entry_of` miss → `Err` not
   `Unknown`.

## Key layout facts / gotchas

- Blob frame = **512 KB** (`PAGE_SIZE = 0x80000`, confusingly named). Pages = 4 KB.
  Layout: `[0,4KB)` BlobHeader (page 0); `[4KB,44KB)` slot table (40 KB, pages
  1–10, **off the read path since R1**); `[44KB,512KB)` data area (`DATA_AREA_
  START=0xB000`, pages 11–127).
- R1: children store `offset_div8` inline (`decode_child_off`/`child_offset`),
  not slot indices. R3 leaf = `[16B hdr: key_fp@0, node_type@1, value_len@2,
  key_len@4, tombstone@6, seq@8][key][value]`, inline in the blob. `cold.rs`'s
  `summarize_*` is the canonical node-walk template (reuse it).
- `BlobFrameMut::init` **zeroes the whole 512 KB** — the reason new header fields
  default safe.
- New header fields at 0xb0/b4/b8; `blob_guid` ends at 0xb0; size assert pins 4096.
- O_DIRECT (Linux) needs 4 KB-aligned offset+len+buffer; whole-page reads into a
  page-aligned slice of an `AlignedBlobBuf` satisfy it (proven on x86).

## Validation

- **mac (aarch64), local:** `cargo test --lib`, `cargo clippy --all-targets`,
  the on-disk suites (`wal_tree_integration`, `checkpoint`, `tree_smoke`).
- **ubuntu (x86), real I/O + O_DIRECT + io_uring + benches:**
  `export LIBCLANG_PATH=$HOME/libclang-shim` (rocksdb comparator needs a
  libclang shim: `ln -sf /usr/lib/llvm-18/lib/libclang.so.1
  ~/libclang-shim/libclang.so`), then
  `rsync -az --exclude target/ --exclude .git/ --exclude benches/target/ ./
  ubuntu:~/holt/` and run there.
- **Cold-read bench:** the stress bench supports `--no-default-features` Holt-only
  runs and `HOLT_STRESS_DROP_COLD_INDEX_AFTER_PRELOAD=1`. For a *true* cold
  number, also drop the OS page cache (the current 137× is page-cache-warm).
- **Gates:** stage 2 = `routing==full` invariant; stage 3 = `routed_get==tree.get`
  for present/absent/crossing (data-integrity); stage 5 = SIGKILL crash-soak.

## Tasks (mirror of the tracker)

- **#18 (in progress):** Cold-read in-blob routing region. Design (`137d5ba`);
  primitive (`808a5fa`); stage 1 header fields (`12ce05a`); **stage 2 two-arena
  compaction build + `routing==full` gate (`ed997c6`) done.** **Next: the
  mutation-path prerequisite (zero `routing_len` on first structural mutation in
  `writers.rs`), then stage 3 `cold_read_routed`** (+ `routed_get==tree.get`
  data-integrity gate + cold bench) → **stage 3.5/4 io_uring-to-the-extreme
  cold-read backend** (batched multi-SQE async reads / per-core rings /
  IOPOLL+SQPOLL on Linux; thread-pool backend for macOS+fallback; QD-sweep cold
  bench) → stage 4 resident routing cache → stage 5 remove cold.idx (+ crash-soak)
  → stage 6 bloom.
- **#10 (pending):** R2 BlobNode prefix bloom — folds into stage 6.
- **#12 (pending):** hot-scan residual ~4% (separate, low priority).
