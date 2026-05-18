# holt — architecture

## 1. The shape of the data

Every `Tree` is backed by one or more **512 KB blob frames**. Inside
each blob:

```
+------------------------ 524288 bytes ----------------------+
| BlobHeader (4096 B)                                         |
|   - blob_guid, num_slots, root_slot, space_used, gap_space  |
|   - free_list_head[8]u16  ← per-NodeType free LIFO          |
+-------------------------------------------------------------+
| Slot table (40 KB = 10240 × u32)                            |
|   Each entry is bit-packed:                                 |
|     bits 0..16  (17 bits) = byte_offset / 8                 |
|     bits 17..31 (15 bits) = NodeType (live) OR              |
|                              next free slot (freed)         |
+-------------------------------------------------------------+
| Data area (~468 KB, bump-allocated)                         |
|   Node bodies (8 to 1032 bytes each, NodeType-tagged)       |
|   Leaf key/value extents (raw bytes, ref'd by Leaf.key_off) |
+-------------------------------------------------------------+
```

A tree grows through `BlobNode` crossings — once a 512 KB blob is
full, a subtree is materialized into a new blob and a Blob-node
crossing is installed in the parent that says "the walk continues
in blob X starting at slot Y." The whole tree spans an arbitrary
number of 512 KB units; the user sees one logical Tree.

This composes recursively: each sub-blob is itself a full ART
frame, so the same walker code descends across blob boundaries
without special-casing the crossing.

## 2. NodeType variants

| ntype | Name        | Size       | Purpose                                        |
|------:|-------------|-----------:|------------------------------------------------|
|     0 | Invalid     | (panic)    | Sentinel                                       |
|     1 | Leaf        |    16 B    | (value_size, tombstone, key_offset, seq) +    |
|       |             |            | bump-allocated extent for key+value bytes      |
|     2 | Prefix      |   128 B    | Path-compressed segment (≤112 inline bytes)    |
|     3 | Blob        |   128 B    | Cross-blob crossing (target_guid + entry_slot) |
|     4 | Node4       |    24 B    | 1..4 children, linear scan                     |
|     5 | Node16      |    88 B    | 5..16 children, SIMD vpcmpeqb scan             |
|     6 | Node48      |   456 B    | 17..48 children, byte→slot index               |
|     7 | Node256     |  1032 B    | 49..256 children, direct array                 |
|     8 | EmptyRoot   |     8 B    | All-zero sentinel for an empty tree            |

Walker descends through Prefix and {Node4, Node16, Node48, Node256}
based on the next key byte; terminates at Leaf.

## 3. Concurrency — HybridLatch

Every blob frame carries a 3-mode latch (LeanStore-style):

- **Optimistic** — no real lock taken. Reader snapshots a version
  counter, walks the tree, then re-checks the counter. If a writer
  released exclusive in between, the read is retried (or escalated).
  Wait-free for readers when uncontended.
- **Shared** — multiple concurrent readers, mutually exclusive with
  writers.
- **Exclusive** — single writer, mutually exclusive with everyone.

The walker takes optimistic latches by default and only escalates on
restart-budget exhaustion. Cross-blob walks (`BlobNode` descent)
take a fresh guard on the target blob (lock coupling).

State encoding (single `AtomicU32`):
- `0` = idle
- `1..(WRITER-1)` = N shared readers
- `WRITER = u32::MAX` = exclusive

Plus an `AtomicU64` version counter, incremented on every exclusive
release. Optimistic readers snapshot version, validate it didn't
change.

## 4. Walker mechanics

Three primary operations: `insert`, `lookup`, `erase` — all share a
common descent pattern.

```
fn walk(slot, key, depth) {
    loop {
        match nodeType(slot) {
            EmptyRoot   -> "tree is empty"
            Leaf        -> compare full key, return value or not_found
            Prefix      -> match prefix bytes against key[depth..],
                           advance depth, descend to child
            Node4/16/48/256 -> use key[depth] to pick child,
                               descend
            Blob        -> swap to target blob, descend at entry slot
        }
    }
}
```

Insert adds a Leaf at the divergence point. If two leaves share a
common prefix, a Prefix node is created above their Node4. If a
Node4 fills (count=4), it promotes to Node16; then Node48, then
Node256.

Erase reverses: a leaf gets removed; if its parent Node4/16/48/256
drops to 1 child, the parent is collapsed and replaced with that
child in its grandparent's slot. Prefix nodes whose child
disappeared are freed. The tree contracts back to the EmptyRoot
sentinel when fully drained.

## 5. Compaction + multi-blob spillover

Three reasons trigger compaction or migration:

- `SplitTombstone` — too many tombstone leaves accumulated; rebuild
  the blob dropping them. _Not yet wired._
- `SplitGapSpace` — bump-allocator wasted space (dead bytes from
  earlier orphans) exceeds a threshold; compact in place.
  _Not yet wired_ — see "leaked extents" caveat below.
- `OutOfBlobFrame` — alloc failed in current blob; spill a subtree
  to a new blob. **Implemented (Stage 2d phase B).**

### `erase_multi` + child-blob reclaim (Stage 2d phase C)

`walker::erase_multi(backend, root_guid, root_buf, key)` mirrors
`insert_multi`: it threads `Some(backend)` through the existing
`erase_at` dispatch, and when the descent reaches a `BlobNode` it
calls `erase_at_blob_node`. That arm:

1. Reads the BlobNode body, validates `key[depth..]` against the
   inline prefix; mismatch is a no-op (key not in the subtree).
2. Loads the child blob via `backend.read_blob`.
3. Recursively runs `erase_at` inside the child frame.
4. Translates the child's `EraseSignal` back to the parent:
   - `Unchanged` → write child back, propagate `Unchanged`.
   - `Replaced(new_entry)` → patch the child blob's
     `header.root_slot` and the parent's BlobNode
     `child_entry_ptr`, write child back, propagate `Unchanged`.
   - `SubtreeGone` → the child blob is empty; free the parent's
     BlobNode slot **and** delete the orphaned child blob from the
     backend via `backend.delete_blob`. Propagate `SubtreeGone` so
     the grandparent collapses too.

`walker::lookup_multi(backend, root_buf, key)` is the symmetric
read-side helper used by `Tree::get` and `Tree::rename`'s src/dst
probes — it loops over `LookupResult::Crossing` signals, loading
each child blob as needed.

After this commit `Tree::delete` and `Tree::rename` work across
arbitrarily many blobs, completing the public API's cross-blob
symmetry with `Tree::put` (Stage 2d phase B) and `Tree::get`
(Stage 2d phase A).

### `make_blob_from_node` + `splitBlob` (Stage 2d phase B)

`make_blob_from_node` is the primitive: take a subtree, deep-copy
it into a fresh blob (recursive walk through every NodeType,
including Leaf extents), return `(new_buf, entry_slot)` ready for
the caller to write through the backend.

`splitBlob` is the in-band spillover trigger sitting inside the
walker's multi-blob insert loop:

```
walker::insert_at(slot, key, value, depth):
    descend …
    if NodeType::Blob:                         # cross-blob crossing
        load child blob via backend
        recurse into child (with its own spillover retry loop)
        on Done: patch BlobNode child_entry_ptr if it changed
        write child blob back

walker::insert_multi(root_buf):
    loop up to MAX_SPILLOVER_ATTEMPTS (= 64):
        try insert_at(root_slot)
        if Err(AllocError::OutOfSpace):
            spillover_blob(root_frame):
                pick_victim_subtree    # largest non-Blob child
                make_blob_from_node    # deep-clone to new buf
                backend.write_blob     # persist new child
                free_subtree           # release source slot entries
                alloc(BlobNode)        # reuse Prefix free list if no bump room
                rewire parent → BlobNode
            continue   # retry insert; descent now follows the new BlobNode
        if Ok: return
```

The `splitBlob` heuristic picks the **largest non-`Blob` child** of
the source root's first branching node. Skipping `Blob` children
matters: previously-migrated children would otherwise get re-
migrated into wrapper blobs without freeing any actual data.
Picking the *largest* child maximises space freed per spillover
iteration.

### Cross-type 128-byte free list

A spillover allocates a `BlobNode` (128 B body) at exactly the
moment the source blob's bump cursor is past its limit. To keep
spillover from itself OOM'ing, `BlobFrame::alloc_node` falls back
across the two 128-byte NodeTypes — `alloc_node(Blob)` reuses a
freed `Prefix` slot body when the `Blob` free list is empty, and
vice versa. Each spillover's `free_subtree` typically frees one or
more `Prefix` nodes, so the next `BlobNode` install reuses one of
those bodies for free.

### `compactBlob` — in-place extent reclaim (Stage 6 part 1)

`compact_blob(buf)` deep-clones the live tree out of `buf` into a
scratch [`AlignedBlobBuf`] (via the same [`clone_subtree`] used by
[`make_blob_from_node`]) and copies the packed image back over
`buf`. The resulting blob has:

- A contiguous packed data area: every byte in
  `DATA_AREA_START..space_used` is live
- Empty free lists
- `num_slots` equal to the live-subtree node count (+1 sentinel)
- The original `blob_guid` preserved

What this reclaims:

- **Leaf key/value extents** allocated via `alloc_extent` (no free
  list, so they accumulate on update + delete + spillover-migrate)
- Stale slot-entry / body-byte pairs whose NodeType free list has
  no live demand

What this costs: one scratch 512 KB heap allocation that lives for
the duration of the call + one full-blob memcpy at the end.
Single-digit µs on a modern machine.

### How spillover + compact pair up

The walker's multi-blob insert retry loop runs **both** on every
`AllocError::OutOfSpace`:

```
loop up to MAX_SPILLOVER_ATTEMPTS:
    try insert_at
    on OOM:
        spillover_blob(frame)   // migrate a subtree out, install BlobNode
        compact_blob(buf)       // reclaim the just-migrated extent bytes
        retry
```

`spillover_blob` is what makes slot reclaim possible (returns
nodes to per-type free lists, drops the migrated subtree). On its
own that's not enough — leaf extents stay glued to the bump area.
`compact_blob` is the second half: it does the actual byte-level
repack so the next walker pass sees a fresh bump cursor.

### `SPILLOVER_RESERVATION` (128 B bump headroom)

`alloc_node` and `alloc_extent` refuse to consume the last
`SPILLOVER_RESERVATION` (= 128 B = one `BlobNode` body) of the
data area. `alloc_node(NodeType::Blob)` is exempt and may consume
that reservation. This guarantees that **spillover always has
room to install its emergency BlobNode**, even in a blob whose
walker just hit OOM — without the reservation, a 99 %-full blob
would have no room left for the spillover code path itself.

Compact restores the reservation: the post-compact bump cursor is
the packed-image size, well below `PAGE_SIZE - SPILLOVER_RESERVATION`.

### Caveats

- `mergeBlob` (the inverse of `splitBlob`) and a true balanced
  multi-child `splitBlob` are queued — see ROADMAP.md.
- Erase-time node shrinkage (Node256 → 48 → 16 → 4) **is** wired —
  thresholds 37 / 12 / 3 give hysteresis vs the grow thresholds
  48 / 16 / 4. The terminal `Node4 → Prefix([byte])` lone-child
  collapse still applies once a node has emptied to a single
  child.

## 5b. BufferManager (Stage 6 phase 1 + 2a + 2b + 2c)

`BufferManager` sits between [`Tree`] and the underlying
[`Backend`], caching recently-accessed blobs. It **itself
implements `Backend`** — drop-in wrapper for the write path —
**and** exposes a `pin(guid)` API for zero-copy reads.

```
Tree → Arc<BufferManager> → Arc<dyn Backend>
              │                    ↑
              │           MemoryBackend or
              │           PersistentBackend
              │
              ├── read path:  bm.pin(guid).read() ─► BlobFrameRef
              └── write path: backend trait + cached write-through
```

`Tree::open_with_backend` wraps the user-supplied backend
transparently; `TreeConfig::buffer_pool_size` (default 64) drives
the cache capacity in blobs (= 32 MB resident).

### Mode: write-through

Writes go to **both** the cache **and** the inner backend in one
call. Read I/O is what gets cached; write I/O latency is
unchanged. This preserves the existing `flush_on_write`
durability semantic without forcing callers to checkpoint.

A later revision (Stage 6 phase 3) will add **write-back** mode
with dirty tracking + a background checkpointer thread.

### Per-blob locking — `HybridLatch + UnsafeCell<AlignedBlobBuf>`

Each cached blob lives behind a LeanStore-style `HybridLatch`
(3-mode latch) wrapping the 512 KB buffer in `UnsafeCell`:

| Mode       | Cost                | Used by                          |
|------------|---------------------|----------------------------------|
| Optimistic | atomic load + check | `Tree::get` walker (wait-free)   |
| Shared    | brief CAS spin loop | `BufferManager::commit`          |
| Exclusive  | brief CAS spin loop | `Tree::put` / `delete` / spillover |

On **different** blobs, ops never contend. On the **same** blob:
- N optimistic readers don't block writers (each takes a version
  snapshot + revalidates after the walk; on a torn read the
  walker restarts from the root).
- N shared readers run in parallel; writers wait.
- A single writer runs alone and bumps the version on release so
  in-flight optimistic readers detect the change.

The cache's `HashMap`/LRU mutex is held only for very short
windows (insertions, eviction, LRU touches).

### Pin-and-operate (Stage 6 phase 2a + 2b + 2c)

Every blob operated on by the walker — root **and** every
cross-blob hop, for both reads and writes — is pinned in the BM
via `BufferManager::pin(guid)`. The returned `Arc<CachedBlob>`
keeps the entry alive (`strong_count >= 2` skips LRU eviction);
the walker borrows into the underlying buffer via a typed guard:

| Operation                    | Guard               | Wrap as                       |
|------------------------------|---------------------|-------------------------------|
| Wait-free read (Tree::get)   | `read_optimistic()` → `OptimisticGuard` | `BlobFrameRef::wrap(g.as_slice())`, then `g.validate()` |
| Shared read (commit)         | `read()` → `BlobReadGuard` | derefs to `&AlignedBlobBuf`     |
| Exclusive write              | `write()` → `BlobWriteGuard` | derefs to `&mut AlignedBlobBuf` |

After an exclusive write the walker calls
`BufferManager::commit(guid)` to durably write the cached buffer
through to the inner backend. No second 512 KB memcpy: `commit`
takes a shared read-guard on the pinned cache entry and writes
its bytes directly.

### Optimistic-read restart loop

`Tree::get` (via `engine::lookup_multi`) walks each blob under an
`OptimisticGuard` and validates AFTER consuming any borrowed
data. If `validate()` returns false (an exclusive writer lapped
the snapshot mid-walk), the lookup restarts from the root — the
parent BlobNode that pointed at the just-torn child may also
have moved, so the only safe re-entry point is the tree root.

### No Tree-wide writer mutex

`Tree::put` / `Tree::delete` no longer take any Tree-wide lock.
Per-blob `HybridLatch` exclusive on the root serialises
concurrent mutators (every write hits root); two writes on
disjoint child subtrees can proceed in parallel after their root
windows finish. `Tree::rename` keeps a `Mutex<()>` `rename_lock`
because its `lookup_probe + erase + insert` sequence must appear
atomic to other renames.

`Tree` no longer keeps its own `state.root_buf` — the canonical
in-memory image of the root blob lives in the BM cache, and both
readers and writers reach it the same way.

### LRU eviction

When the cache exceeds `capacity` blobs the oldest *evictable*
entry is dropped. "Evictable" = `Arc::strong_count(entry) == 1`
(no outstanding pin outside the cache). Pinned blobs are skipped
until the pinning walker drops its handle.

## 6. Persistence + crash safety

WAL (write-ahead log) with 10 physiological TxnOp variants:

```
Insert, Erase, Split, Merge, Compact, RenameObject, Rename,
NewTree, RmTree, + mem-only twins for post-replay-ack
reconciliation
```

Every mutation emits a TxnOp before commit; the journal is flushed
at configurable batch boundaries. On startup, replay applies the
journal up to the last checkpoint.

Checkpointer periodically:
1. Picks dirty blobs (any blob with `dirty=true` flag).
2. Flushes them via the storage backend (file write / mmap msync /
   etc.).
3. Advances the journal `trim_id` past the flushed records (so they
   can be reclaimed).
4. Evicts cold blobs from the buffer pool.

v0.1 ships with a **synchronous** checkpointer (caller decides
when to call `tree.checkpoint()`). v0.2+ adds an asynchronous
3-thread checkpointer.

## 7. Iterators

A stateful iterator that supports:

- `start_after` marker for pagination.
- `prefix` filter (only emit keys starting with prefix).
- `delimiter` for S3 hierarchy rollup (`?delimiter=/` returns one
  entry per immediate child folder).
- Resume from a saved path — the iterator's stack is serializable.

Cross-blob traversal is transparent (the same path stack used for
in-blob descent also crosses `BlobNode` boundaries).

## 8. Backend abstraction

```rust
pub trait Backend: Send + Sync {
    fn read_blob(&self, guid: BlobGuid, into: &mut [u8]) -> Result<()>;
    fn write_blob(&self, guid: BlobGuid, from: &[u8]) -> Result<()>;
    fn list_blobs(&self) -> Result<Vec<BlobGuid>>;
    fn delete_blob(&self, guid: BlobGuid) -> Result<()>;
}
```

Implementations:
- `MemoryBackend` — `HashMap<BlobGuid, Vec<u8>>`, for tests +
  ephemeral use cases.
- `FileBackend` — one file per blob; uses POSIX `pread` / `pwrite`.
- `MmapBackend` — mmap-backed, faster reads, more complex flush
  semantics. Optional feature.
- (Future) `IoUringBackend` — Linux-only, behind `tokio-uring`
  or `glommio`. v0.2+.
- (Future) `RemoteBackend` — talks to a remote blob store via gRPC.
  For distributed deployments.

## 9. Threading model

`Tree` itself is `Send + Sync` once opened. Concurrency is
**per-blob**:

- Two operations targeting different subtrees in different blobs
  run in true parallel.
- Two operations on the same blob serialize at that blob's
  HybridLatch.
- The walker takes optimistic latches first; only escalates to
  shared / exclusive when needed.

The library does NOT manage a thread pool. The caller supplies
threads (via `std::thread`, `tokio`, `rayon`, whatever). The
checkpointer is one background thread by default; this is
configurable.

## 10. Memory budget

For a tree with N keys averaging K bytes key + V bytes value:

- Total disk footprint ≈ `ceil(N × (16 + K + V + 12) / 512KB)`
  blob frames, plus 4 KB header + 40 KB slot table overhead per
  blob.
- In-memory footprint ≈ `(buffer_pool_size × 512KB)` plus latches
  and metadata structures.
- Buffer pool is configurable; default 64 blobs (= 32 MB).

For path-shaped workloads with heavy prefix sharing, the
on-disk-per-key cost drops significantly because shared prefixes
are stored ONCE per Prefix node.

## 11. Failure modes + safety

- **Crash mid-write**: WAL replay restores the tree to the last
  committed TxnOp boundary. Uncommitted partial writes are
  discarded.
- **Partial flush**: blobs are written atomically (write to temp,
  fsync, rename); a flush that crashes mid-stream leaves the old
  blob intact.
- **WAL corruption**: each TxnOp carries a `sanity_info` field;
  replay validates and bails out cleanly on mismatch rather than
  corrupting the tree.
- **Out of disk space**: backend write errors propagate as
  `Error::BackendIo`; the tree remains in a consistent state.
- **OOM in buffer pool**: an LRU-ish eviction policy reclaims cold
  blobs. The pool can be sized at `Tree::open` time.

## 12. What this is NOT

To avoid surprise:

- **Not a SQL database.** No joins, no aggregates, no query planner.
- **Not a vector DB.** No kNN, no embeddings, no similarity.
- **Not a full-text index.** No tokenization, no inverted index.
- **Not a replication / consensus layer.** The library is
  single-node + persistent. Replication is a layer above this.
- **Not a network server.** This is a library you embed; bring your
  own RPC if you want to expose it remotely.

For these, combine holt with a domain-appropriate engine:
- holt + FAISS / Qdrant / pgvector → AI workspace metadata + vectors
- holt + Tantivy → FS metadata + full-text
- holt + custom Raft → distributed holt
