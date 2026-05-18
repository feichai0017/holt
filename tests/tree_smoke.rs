//! End-to-end smoke tests driving the public `Tree` API.
//!
//! Exercises only the public surface so signature breakage shows
//! up here first.

use std::sync::Arc;

use artisan::{AlignedBlobBuf, Backend, MemoryBackend, Tree, TreeBuilder, TreeConfig};

#[test]
fn open_memory_get_on_empty_tree_returns_none() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    assert!(tree.get(b"anything").unwrap().is_none());
    assert!(tree.get(b"").unwrap().is_none());
}

#[test]
fn builder_memory_path() {
    let tree = TreeBuilder::new("scratch")
        .memory()
        .buffer_pool_size(32)
        .open()
        .unwrap();
    assert!(tree.get(b"x").unwrap().is_none());
}

#[test]
fn open_with_explicit_backend_round_trips_root_blob() {
    let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let _t = TreeBuilder::new("ignored")
        .open_with_backend(backend.clone())
        .unwrap();
    let blobs_after_first = backend.list_blobs().unwrap().len();
    assert!(blobs_after_first >= 1, "root blob should be present");

    let _t2 = TreeBuilder::new("ignored")
        .open_with_backend(backend.clone())
        .unwrap();
    assert_eq!(
        backend.list_blobs().unwrap().len(),
        blobs_after_first,
        "re-open must not allocate a fresh root"
    );
}

#[test]
fn checkpoint_is_idempotent_on_memory_backend() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.checkpoint().unwrap();
    tree.checkpoint().unwrap();
    assert!(tree.get(b"k").unwrap().is_none());
}

// ----------------------------------------------------------------
// Put / Get
// ----------------------------------------------------------------

#[test]
fn put_then_get_round_trip() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    assert!(tree.put(b"hello", b"world").unwrap().is_none());
    assert_eq!(tree.get(b"hello").unwrap().as_deref(), Some(&b"world"[..]));
    assert!(tree.get(b"missing").unwrap().is_none());
}

#[test]
fn put_returns_previous_value_on_update() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    assert!(tree.put(b"k", b"v1").unwrap().is_none());
    assert_eq!(tree.put(b"k", b"v2").unwrap().as_deref(), Some(&b"v1"[..]));
    assert_eq!(tree.get(b"k").unwrap().as_deref(), Some(&b"v2"[..]));
}

#[test]
fn many_keys_all_readable_via_public_api() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..100u32)
        .map(|i| (format!("img/{i:04}.jpg").into_bytes(), format!("blob#{i}").into_bytes()))
        .collect();
    for (k, v) in &pairs {
        tree.put(k, v).unwrap();
    }
    for (k, v) in &pairs {
        assert_eq!(tree.get(k).unwrap().as_deref(), Some(&v[..]));
    }
}

#[test]
fn concurrent_writers_serialised_by_internal_lock() {
    use std::thread;

    let tree = Arc::new(Tree::open(TreeConfig::memory()).unwrap());
    let handles: Vec<_> = (0..8u8)
        .map(|t| {
            let tree = tree.clone();
            thread::spawn(move || {
                for i in 0..25u32 {
                    let k = format!("t{t}/k{i:03}").into_bytes();
                    let v = format!("v{t}-{i}").into_bytes();
                    tree.put(&k, &v).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    for t in 0..8u8 {
        for i in 0..25u32 {
            let k = format!("t{t}/k{i:03}").into_bytes();
            let v = format!("v{t}-{i}").into_bytes();
            assert_eq!(tree.get(&k).unwrap().as_deref(), Some(&v[..]));
        }
    }
}

#[test]
fn strict_prefix_key_pair_now_works() {
    // "abc" and "abcdef" — one is a strict prefix of the other.
    // Resolved at the Tree layer via the terminator-byte trick.
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"abc", b"v1").unwrap();
    tree.put(b"abcdef", b"v2").unwrap();
    assert_eq!(tree.get(b"abc").unwrap().as_deref(), Some(&b"v1"[..]));
    assert_eq!(tree.get(b"abcdef").unwrap().as_deref(), Some(&b"v2"[..]));
    // Other length within the chain stays NotFound.
    assert!(tree.get(b"abcd").unwrap().is_none());
}

#[test]
fn deeply_nested_strict_prefix_chain() {
    // The classic "filesystem path" workload: each level of the
    // path is a key.
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    let paths: &[&[u8]] = &[
        b"/", b"/a", b"/a/b", b"/a/b/c", b"/a/b/c/d", b"/a/b/c/d/e",
    ];
    for (i, p) in paths.iter().enumerate() {
        tree.put(p, format!("level{i}").as_bytes()).unwrap();
    }
    for (i, p) in paths.iter().enumerate() {
        assert_eq!(
            tree.get(p).unwrap().as_deref(),
            Some(format!("level{i}").as_bytes()),
        );
    }
    // Holes in the chain stay NotFound.
    assert!(tree.get(b"/a/b/c/d/e/f").unwrap().is_none());
}

#[test]
fn empty_key_round_trips() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"", b"empty-key-value").unwrap();
    assert_eq!(tree.get(b"").unwrap().as_deref(), Some(&b"empty-key-value"[..]));
    tree.put(b"a", b"other").unwrap();
    assert_eq!(tree.get(b"").unwrap().as_deref(), Some(&b"empty-key-value"[..]));
    assert_eq!(tree.get(b"a").unwrap().as_deref(), Some(&b"other"[..]));
}

// ----------------------------------------------------------------
// Delete (Stage 2c)
// ----------------------------------------------------------------

#[test]
fn delete_existing_key_returns_value_and_removes_it() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"k", b"v").unwrap();
    assert_eq!(tree.delete(b"k").unwrap().as_deref(), Some(&b"v"[..]));
    assert!(tree.get(b"k").unwrap().is_none());
}

#[test]
fn delete_missing_key_is_noop() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    assert!(tree.delete(b"missing").unwrap().is_none());
}

#[test]
fn delete_then_reinsert_round_trips() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"k", b"v1").unwrap();
    assert_eq!(tree.delete(b"k").unwrap().as_deref(), Some(&b"v1"[..]));
    tree.put(b"k", b"v2").unwrap();
    assert_eq!(tree.get(b"k").unwrap().as_deref(), Some(&b"v2"[..]));
}

#[test]
fn delete_all_keys_then_reinsert_works() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..50u32)
        .map(|i| (format!("img/{i:03}").into_bytes(), format!("v{i}").into_bytes()))
        .collect();
    for (k, v) in &pairs {
        tree.put(k, v).unwrap();
    }
    for (k, v) in &pairs {
        assert_eq!(tree.delete(k).unwrap().as_deref(), Some(&v[..]));
    }
    for (k, _) in &pairs {
        assert!(tree.get(k).unwrap().is_none());
    }
    tree.put(b"fresh", b"V").unwrap();
    assert_eq!(tree.get(b"fresh").unwrap().as_deref(), Some(&b"V"[..]));
}

#[test]
fn delete_keeps_siblings_under_shared_prefix() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"img/01.jpg", b"a").unwrap();
    tree.put(b"img/02.jpg", b"b").unwrap();
    tree.put(b"img/03.jpg", b"c").unwrap();
    assert_eq!(tree.delete(b"img/02.jpg").unwrap().as_deref(), Some(&b"b"[..]));
    assert_eq!(tree.get(b"img/01.jpg").unwrap().as_deref(), Some(&b"a"[..]));
    assert!(tree.get(b"img/02.jpg").unwrap().is_none());
    assert_eq!(tree.get(b"img/03.jpg").unwrap().as_deref(), Some(&b"c"[..]));
}

// ----------------------------------------------------------------
// Rename (Stage 2c)
// ----------------------------------------------------------------

#[test]
fn rename_moves_value_to_new_key() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"old", b"v").unwrap();
    tree.rename(b"old", b"new", false).unwrap();
    assert!(tree.get(b"old").unwrap().is_none());
    assert_eq!(tree.get(b"new").unwrap().as_deref(), Some(&b"v"[..]));
}

#[test]
fn rename_missing_src_errors_not_found() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    let r = tree.rename(b"nope", b"new", false);
    assert!(matches!(r, Err(artisan::Error::NotFound)));
}

#[test]
fn rename_to_existing_dst_without_force_errors_dst_exists() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"a", b"v_a").unwrap();
    tree.put(b"b", b"v_b").unwrap();
    let r = tree.rename(b"a", b"b", false);
    assert!(matches!(r, Err(artisan::Error::DstExists)));
    // Both keys still present, values unchanged.
    assert_eq!(tree.get(b"a").unwrap().as_deref(), Some(&b"v_a"[..]));
    assert_eq!(tree.get(b"b").unwrap().as_deref(), Some(&b"v_b"[..]));
}

#[test]
fn rename_force_overwrites_existing_dst() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"a", b"v_a").unwrap();
    tree.put(b"b", b"v_b").unwrap();
    tree.rename(b"a", b"b", true).unwrap();
    assert!(tree.get(b"a").unwrap().is_none());
    assert_eq!(tree.get(b"b").unwrap().as_deref(), Some(&b"v_a"[..]));
}

#[test]
fn rename_same_key_is_noop() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"k", b"v").unwrap();
    tree.rename(b"k", b"k", false).unwrap();
    assert_eq!(tree.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
}

#[test]
fn rename_through_shared_prefix() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"img/01.jpg", b"a").unwrap();
    tree.put(b"img/02.jpg", b"b").unwrap();
    tree.put(b"img/03.jpg", b"c").unwrap();
    tree.rename(b"img/02.jpg", b"img/02-renamed.jpg", false).unwrap();
    assert_eq!(tree.get(b"img/01.jpg").unwrap().as_deref(), Some(&b"a"[..]));
    assert!(tree.get(b"img/02.jpg").unwrap().is_none());
    assert_eq!(
        tree.get(b"img/02-renamed.jpg").unwrap().as_deref(),
        Some(&b"b"[..]),
    );
    assert_eq!(tree.get(b"img/03.jpg").unwrap().as_deref(), Some(&b"c"[..]));
}

// ----------------------------------------------------------------
// Stage 2d phase A — multi-blob lookup
//
// The spillover trigger that creates multi-blob state automatically
// lands in phase B. For now we hand-construct a 2-blob layout via
// the engine's `make_blob_from_node` primitive + a directly-installed
// `BlobNode`, then verify `Tree::get` follows the crossing.
// ----------------------------------------------------------------

// ----------------------------------------------------------------
// Stage 2d phase B — automatic multi-blob spillover
// ----------------------------------------------------------------

#[test]
fn auto_spillover_creates_child_blob_when_root_blob_fills() {
    // Insert enough data to overflow the root blob (~448 KB usable
    // data area). Walker `insert_multi` auto-triggers `splitBlob`
    // when any alloc hits `OutOfSpace`, migrating the largest non-
    // BlobNode subtree of the current frame to a fresh child blob,
    // then retries.
    //
    // **Workload note:** until Stage 6's `compactBlob` lands, leaf
    // extents leak after every same-size update; the bump cursor
    // is monotonic. So spillover only buys "slot table" room, not
    // bump-area room — once the root blob has many subtrees
    // migrated out, *every* subsequent insert routes through a
    // BlobNode into a child blob and the root blob stays at its
    // high-water-mark bump cursor.
    //
    // We pick a workload size that triggers at least one spillover
    // but doesn't push past the `MAX_SPILLOVER_ATTEMPTS` per-call
    // budget. ~2000 keys × ~250 B per leaf = ~500 KB → ~50 KB has
    // to spill out via splitBlob.
    let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let tree = TreeBuilder::new("ignored")
        .open_with_backend(backend.clone())
        .unwrap();

    const N: u32 = 2000;
    let value = vec![0xAB; 200];
    for i in 0..N {
        let k = format!("k{i:08}").into_bytes();
        tree.put(&k, &value).unwrap();
    }

    // All keys readable through the multi-blob tree.
    for i in 0..N {
        let k = format!("k{i:08}").into_bytes();
        assert_eq!(
            tree.get(&k).unwrap().as_deref(),
            Some(&value[..]),
            "post-spillover lookup failed at key {k:?}; backend has {} blob(s)",
            backend.list_blobs().unwrap().len(),
        );
    }

    // Spillover should have created at least one child blob.
    let blobs = backend.list_blobs().unwrap();
    assert!(
        blobs.len() >= 2,
        "expected auto-spillover to allocate at least 1 child blob, got {} total blob(s)",
        blobs.len(),
    );
}

#[test]
fn auto_spillover_preserves_data_across_reopen() {
    // After auto-spillover the tree is multi-blob. Closing and
    // reopening the same backend must surface the same key/value
    // mapping (root blob + all spilled child blobs are persisted).
    let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    {
        let tree = TreeBuilder::new("ignored")
            .open_with_backend(backend.clone())
            .unwrap();
        for i in 0..2000u32 {
            tree.put(format!("k{i:08}").as_bytes(), &vec![0xCD; 192]).unwrap();
        }
        tree.checkpoint().unwrap();
    }

    let tree = TreeBuilder::new("ignored")
        .open_with_backend(backend.clone())
        .unwrap();
    for i in 0..2000u32 {
        let k = format!("k{i:08}").into_bytes();
        let v = tree.get(&k).unwrap();
        assert!(
            v.is_some(),
            "post-reopen lookup failed at key {k:?}; backend has {} blob(s)",
            backend.list_blobs().unwrap().len(),
        );
    }
}

#[test]
fn tree_get_follows_blob_node_crossing_across_two_blobs() {
    use artisan::engine::make_blob_from_node;
    use artisan::layout::{BlobNode, NodeType};
    use artisan::store::BlobFrame;

    // Step 1: build a normal tree with some keys.
    let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    {
        let tree = TreeBuilder::new("ignored")
            .open_with_backend(backend.clone())
            .unwrap();
        for i in 0..10u32 {
            let k = format!("k{i:02}").into_bytes();
            let v = format!("v{i}").into_bytes();
            tree.put(&k, &v).unwrap();
        }
    }

    // Step 2: read root blob; deep-clone its subtree into a fresh
    // child blob.
    let root_guid = [0u8; 16];
    let child_guid = [0xAA; 16];

    let mut root_buf = AlignedBlobBuf::zeroed();
    backend.read_blob(root_guid, &mut root_buf).unwrap();

    let (saved_root_slot, child_outcome) = {
        let root_frame = BlobFrame::wrap(root_buf.as_mut_slice());
        let saved_root = root_frame.header().root_slot;
        let outcome = make_blob_from_node(&root_frame, saved_root, child_guid).unwrap();
        (saved_root, outcome)
    };

    // Step 3: write the child blob through the backend.
    backend.write_blob(child_guid, &child_outcome.buf).unwrap();

    // Step 4: rewrite root blob: allocate a BlobNode at a fresh
    // slot pointing at (child_guid, entry_slot), and re-point
    // header.root_slot there. The old saved_root subtree leaks
    // (its slot entries stay tagged live but unreachable) — fine
    // for this test; production spillover (phase B) will free
    // them via free_node walks.
    {
        let mut root_frame = BlobFrame::wrap(root_buf.as_mut_slice());
        let bn_out = root_frame.alloc_node(NodeType::Blob).unwrap();
        let bn = BlobNode::new(b"", child_guid, u32::from(child_outcome.entry_slot));
        // SAFETY: layout types are #[repr(C)] POD; body has the
        // right size; BlobFrame's bump allocator gives 8-byte
        // alignment.
        let body = root_frame.body_of_slot_mut(bn_out.slot).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping(
                &bn as *const BlobNode as *const u8,
                body.as_mut_ptr(),
                std::mem::size_of::<BlobNode>(),
            );
        }
        root_frame.header_mut().root_slot = bn_out.slot;
        let _ = saved_root_slot; // intentionally orphaned in this test
    }
    backend.write_blob(root_guid, &root_buf).unwrap();

    // Step 5: open a fresh Tree against the same backend; verify
    // every original key is reachable through the BlobNode crossing.
    let tree = TreeBuilder::new("ignored")
        .open_with_backend(backend.clone())
        .unwrap();
    for i in 0..10u32 {
        let k = format!("k{i:02}").into_bytes();
        let v = format!("v{i}").into_bytes();
        assert_eq!(
            tree.get(&k).unwrap().as_deref(),
            Some(&v[..]),
            "post-crossing lookup failed for key {k:?}",
        );
    }
    // Missing keys still NotFound.
    assert!(tree.get(b"k99").unwrap().is_none());
}
