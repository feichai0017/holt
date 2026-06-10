# Design: whole-IO-stack optimization (mac + ubuntu)

Status: **plan (design)**. No on-disk format change for the P1/P2 work; the
per-blob bloom (stage 6) is RAM-first and defers any format commitment. Builds on
the in-blob routing region (`docs/design/cold-read-oracle.md`, stages 1–5 + the
B/C lazy-routing engagement).

Source: an adversarially-verified audit of the whole I/O stack (read, write,
checkpoint, WAL, buffer alloc) — 42 opportunities, 23 confirmed-safe, 8 flagged
as correctness-breaking, 11 rejected. This doc is the distilled, ranked plan plus
the traps that bound it.

## Problem (measured)

holt bypasses the OS page cache (`O_DIRECT` on Linux, `F_NOCACHE` on macOS), so
every read/write is a real device round-trip. After the cold routed read shipped
(header + routing region + one leaf page ≈ 12 KB instead of a 512 KB frame pin),
benchmarks at 2 M and 20 M keys showed:

| scale | cold-read full-frame bytes (legacy → routed) | full-frame pins | `get` latency |
|---|---|---|---|
| 2 M (depth 2) | 244.8 GB → 69.2 GB (**3.54×**) | 464,662 → 129,936 | 492 → 492 µs |
| 20 M (depth 3) | 368 GB → 116.9 GB (**3.15×**) | 670,633 → 191,651 | 567 → 577 µs |

**The byte reduction is real and scale-stable (~3.1–3.5×); the latency win did not
materialise on mac.** Root cause: the cold read issues **2–3 *separate* positional
`pread` syscalls** (`routed_read_cached`: header → routing region → leaf page), and
`read_blob_range` **explicitly bypasses the io_uring ring** (plain `pread`). Under
`O_DIRECT`/`F_NOCACHE`, latency is dominated by syscall/round-trip **count**, not
transfer size — so cutting bytes 512 KB → 12 KB barely moves latency until the
round-trips themselves are cut.

**The lever is round-trip count, not bytes (the bytes are already won).**

## Ranked plan

No P0 emerged (no zero-risk, huge-win item). The headline P1 is the convergent
finding of three independent audit lenses.

### P1 — batched cold read + kill the 512 KB scratch (headline)

The cold read (`src/engine/walker/lookup.rs::routed_read_cached`) does, per
lookup: `AlignedBlobBuf::zeroed()` (a **512 KB alloc + memset**, of which ~12 KB
is used), then up to three serial `read_blob_range` calls (header @0, routing
@`routing_off`, leaf @ data-dependent offset).

Fix — one change closes both the syscall-count and the alloc/zero waste:

- Add `pread_many_at(&[(offset, &mut [u8])])` to the io_uring backend
  (`src/store/blob_store/file/uring.rs`), mirroring the existing
  `pwrite_many_at` / `submit_write_batch` pattern: push one Read SQE per range,
  one `submit_and_wait(n)`, drain completions in order, validate each. Expose it
  through a new `BlobStore::read_blob_ranges` (plural) and a `BufferManager`
  wrapper.
- On Linux this batches the cold read's 2–3 disjoint reads into **one submission**
  (3 → 1 syscall). On macOS / the portable path it falls back to serial `pread`
  into the same per-range buffers (no regression; the stage-4 routing cache
  already removes the routing read in the common case, so mac is usually 2 reads).
- Take **per-range small buffers** (header page, routing region, leaf page) instead
  of one 512 KB `AlignedBlobBuf` — eliminating the **512 KB alloc + zeroing per
  cold read** on both platforms. Each buffer is independently 4 KB-aligned, so
  every range stays `O_DIRECT`-legal.

Eager-batching subtlety: the routing-region length comes from the header, and the
routing cache is validated by `compact_times` which also comes from the header.
So either (a) batch `[header]` first, then conditionally `[routing, leaf]` after
the header decodes (2 submissions worst case), or (b) batch `[header, routing,
leaf]` eagerly and discard the routing read if the cache turns out to hold a newer
version. (a) is simpler and keeps the cache-hit path at 2 reads.

Gain: Linux 3 → 1 (or 2) syscalls per cold read (~12–40 µs on real NVMe, queue-depth
dependent) + a 512 KB alloc/memset removed per cold read on both platforms.
Risk: low — read-only, mirrors an existing batch pattern, any short/failed read →
`Unknown` → authoritative full pin (the pure-accelerator contract is preserved).

### P1 — other confirmed, independent

- **`io_queue_capacity` default (16) is conservative for io_uring** (ubuntu): a
  deeper ring keeps the SQE batch fuller during checkpoint bursts. Small, low risk.
- **`needs_flush()` is polled every idle checkpoint round** — make it
  event-driven so idle rounds don't pay a `fdatasync`-class probe. Small.
- **WAL truncate `fsync` does not coalesce with the preceding checkpoint
  data-sync** — one saved `fsync` per checkpoint. Medium; must not reorder W2D.

### P2 — refinements (after P1 lands + is measured on ubuntu)

- Skip the header re-read when the routing region is resident *and* a cheaper
  freshness signal exists (today the header read is the freshness check — needs a
  separate validation token to drop it; do not drop it naively).
- Short-circuit known-un-routable blobs to skip the routing probe.
- Coalesce leaf-straddle double reads into the same batched submission (Linux).
- macOS: where two needed ranges are file-contiguous, one `pread` covers both
  (note: header @0 and routing @`routing_off` are **not** contiguous — the 40 KB
  slot table sits between them — so a single `[0, leaf_region_start)` pread would
  waste 40 KB; only coalesce genuinely adjacent ranges).
- `snapshot_bytes` / checkpoint per-blob buffer pooling (the 512 KB memcpy
  dominates, not the alloc — modest).

### P3 — marginal (do only if a profile demands it)

- WAL replay streaming/read-ahead (only matters for >50 MB WALs).
- `pin()` cache-miss buffer pre-sizing for the registered-buffer fast path.

### (d) WAL/checkpoint io_uring rewrite — large, ubuntu-focused, do last

On Linux the WAL writer uses plain `write_all()` + `sync_data()`, and truncate is
a separate `ftruncate` + `sync_data`. Routing WAL writes + the checkpoint write
batch through io_uring with linked submission could pipeline `write → fsync →
truncate → fsync` into fewer round-trips. **High risk** (durability hot path);
gate behind adversarial verification and do it after the P1/P2 wins are measured.

## ⚠️ Traps — confirmed to break correctness; do NOT do these

The audit's verifiers flagged 8 proposals as W2D-breaking or misdiagnosed. The
load-bearing ones:

1. **Do not remove `F_NOCACHE` / add a no-cache-bypassing second fd.** It is the
   basis of buffer-manager-exclusive caching and W2D: a concurrent write could be
   kernel-cached while appearing flushed, breaking crash recovery. It is per-fd,
   not per-op. And it wouldn't help anyway: the routing region is already cached
   in-process, and each cold lookup reads a *different* leaf (no kernel-cache
   reuse). The bottleneck is syscall count, which `F_NOCACHE` does not cause.
2. **Do not run multiple checkpoint epochs in flight / batch-then-flush-once.**
   Per-epoch durability is load-bearing: epoch N+1's data must not become durable
   before epoch N's pending deletes apply, or recovery leaves orphan blobs. Batched
   *writes* with strict per-epoch *flush+delete* ordering is the only safe form,
   and that is a careful refactor, not "flush once".
3. **Do not skip `flush` on idle/stale rounds.** A `Stale` write-through still
   wrote data to the store; skipping the flush persists data without its manifest
   entry → duplicate-blob-at-different-epoch on retry. (Event-driven `needs_flush`
   is fine; *skipping a required flush* is not.)
4. **Do not co-schedule cold reads with checkpoint writes on the ring** as a
   "contention" fix — misdiagnosed: routed reads bypass the ring entirely; the
   single I/O worker keeps the write ring uncontended on the hot path.

These bound the optimization space: `F_NOCACHE` and per-epoch flush ordering are
fixed invariants, not tunables.

## Per-blob bloom (stage 6 of cold-read-oracle / ROADMAP v0.4 P1)

Metadata workloads are negative-heavy (`open`/`stat`/`head` of missing keys). A
per-blob bloom makes within-blob negatives free without changing `get()`
semantics.

- **Placement / RAM-first.** Reserve header bytes at `0xc4` (`filter_off: u32`,
  `filter_len_pages: u16`, `filter_bits_per_key: u16`, carved from `_pad_c0`) but
  **do not persist in v0.4**: the bloom lives in a BM-resident sidecar keyed +
  validated by `compact_times`, rebuilt on cache fill. On-disk encoding is a
  later (stage 6.x) decision, gated on measured benefit.
- **Never a false negative (contract).** The bloom only filters the *leaf-read*
  decision after the routed descent reaches a child `>= leaf_region_start`. On
  any uncertainty (epoch mismatch, stale flag, no filter, corrupt) → read the
  leaf as today. `compact_blob` rebuilds it from the final leaf set; `alloc_node`
  de-route marks it stale. Enforced by a `bloom_never_false_negative` test over
  100 k random present/absent keys across mutation/compaction/eviction.
- **Build.** At `compact_blob` / spillover, after the final leaf set is placed,
  one DFS pass feeds leaf keys to a `BloomBuilder` (size adaptive, like
  `routing_len`). Populated into the BM sidecar on cache fill.
- **Lookup.** In `descend_routed`, before `read_blob_range(leaf_off)`: query the
  resident bloom → `No` ⇒ return `NotFound` with zero leaf reads; `Maybe`/no-filter
  ⇒ read the leaf (existing flow). Orthogonal to descent + cross-blob routing.
- **Size.** 8 bits/key (~1 % FPR); ~50 KB total resident even at 20 M keys (scales
  with leaves, not key count). Counted in the BM small-metadata budget; surfaced
  as `bm_bloom_bytes` / `bm_bloom_queries` / `bm_bloom_negatives` in `Tree::stats`.
- **Crash safety.** Zero risk while RAM-only (rebuilt like leaves are read). If
  later persisted: written atomically with the blob, CRC + rebuild-on-corrupt, no
  WAL coupling (always derivable from leaves).
- **Staged plan.** 6.0 `BloomFilter`/`BloomBuilder` (isolated, property-tested) →
  6.1 build at compaction (stamp header, no reads yet) → 6.2 BM resident + wire
  into `descend_routed` leaf decision → 6.3 `alloc_node` stale-marking → 6.4 stats
  → (later) on-disk encoding only if ubuntu negative-heavy bench shows >20 % win.

## Validation cadence (unchanged)

Correctness/compile on **mac (aarch64)** (lib + clippy + integration +
`wal_crash_soak` SIGKILL). Real I/O + latency on **ubuntu (x86)** — the io_uring
batched-read and WAL/checkpoint wins only manifest there; the mac runs prove the
byte reduction and correctness, not the latency.

## Implementation order

1. **io-optimization design doc** (this file).
2. **(a)** batched cold read + kill 512 KB zeroing — headline P1, mac-verifiable.
3. **(b)** zero-risk cheap wins — `io_queue_capacity`, event-driven `needs_flush`,
   WAL-truncate-fsync coalesce.
4. **(c)** per-blob bloom 6.0–6.2.
5. **(d)** WAL/checkpoint io_uring rewrite — largest, most durability-sensitive;
   adversarially verified before commit.

Each step: full gate (lib + clippy + integration + crash-soak) and a commit;
durability-sensitive changes get an adversarial review first.
