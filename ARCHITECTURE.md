# artisan — architecture

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

This is the "fractal" property: each sub-blob is itself an ART, and
they compose recursively.

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

### Caveat: leaked extents

Until `compactBlob` ships (Stage 6 reclaim), Leaf key/value
**extents leak after every same-size update** (in-place value
overwrite reuses the extent; new keys allocate a fresh extent
bump). `spillover_blob` reclaims *slot table* entries but **not**
the bump-area bytes those extents occupied. The bump cursor is
monotonic until compaction.

In practice the live integration tests insert past one blob's
capacity (~2000 keys × 200 B values overflows the 448 KB usable
data area) and verify all keys round-trip across multiple blobs.
Significantly larger workloads currently require multi-spillover
sequences whose per-call budget would need `compactBlob` to
release. Stage 6 closes this loop.

`mergeBlob` (the inverse of `splitBlob`) and a true balanced
multi-child `splitBlob` are both queued — see ROADMAP.md.

## 6. Persistence + crash safety

WAL (write-ahead log) with 13+ physiological TxnOp variants:

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

For these, combine artisan with a domain-appropriate engine:
- artisan + FAISS / Qdrant / pgvector → AI workspace metadata + vectors
- artisan + Tantivy → FS metadata + full-text
- artisan + custom Raft → distributed artisan
