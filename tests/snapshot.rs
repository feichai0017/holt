//! End-to-end tests for copy-on-write [`Tree::snapshot`].
//!
//! Exercises only the public surface. Stage 3 covers snapshot
//! creation, the scoped read path (including across blob-frame
//! boundaries), epoch advancement, and isolation from *root-local*
//! live writes — which hold without fork-on-write because the live
//! root frame is never shared (a snapshot takes a full copy of it).
//! Multi-blob isolation under mutation (the fork-on-write gate) is
//! added alongside that machinery.

use std::sync::Arc;

use holt::{BlobStore, Error, MemoryBlobStore, Tree, TreeBuilder, TreeConfig};

#[test]
fn snapshot_isolates_root_local_writes() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    for i in 0..5u32 {
        tree.put(format!("k{i}").as_bytes(), format!("v{i}").as_bytes())
            .unwrap();
    }

    let snap = tree.snapshot(b"").unwrap();

    // Mutate the live tree after the snapshot. Both writes stay inside
    // the single root frame, which the snapshot copied — so the
    // snapshot must not observe either.
    tree.put(b"k0", b"OVERWRITTEN").unwrap();
    tree.put(b"k9", b"new").unwrap();

    assert_eq!(snap.get(b"k0").unwrap().as_deref(), Some(&b"v0"[..]));
    assert_eq!(snap.get(b"k9").unwrap(), None);
    for i in 1..5u32 {
        assert_eq!(
            snap.get(format!("k{i}").as_bytes()).unwrap().as_deref(),
            Some(format!("v{i}").as_bytes()),
        );
    }

    // The live tree reflects the new writes.
    assert_eq!(
        tree.get(b"k0").unwrap().as_deref(),
        Some(&b"OVERWRITTEN"[..]),
    );
    assert_eq!(tree.get(b"k9").unwrap().as_deref(), Some(&b"new"[..]));
}

#[test]
fn snapshot_reads_across_blob_boundaries() {
    // Enough keys to force auto-spillover into child blob frames, so
    // the snapshot's copied root crosses `BlobNode`s into shared child
    // frames on read.
    let store: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
    let tree = TreeBuilder::new("ignored")
        .open_with_blob_store(store.clone())
        .unwrap();

    const N: u32 = 2000;
    let value = vec![0xAB_u8; 200];
    for i in 0..N {
        tree.put(format!("k{i:08}").as_bytes(), &value).unwrap();
    }
    assert!(
        store.list_blobs().unwrap().len() >= 2,
        "test needs a multi-blob tree to be meaningful",
    );

    let snap = tree.snapshot(b"").unwrap();
    for i in 0..N {
        assert_eq!(
            snap.get(format!("k{i:08}").as_bytes())
                .unwrap()
                .as_deref(),
            Some(&value[..]),
            "snapshot lost key {i} across a blob-frame boundary",
        );
    }
}

#[test]
fn snapshot_scope_restricts_reads() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"users/alice", b"1").unwrap();
    tree.put(b"users/bob", b"2").unwrap();
    tree.put(b"orders/x", b"9").unwrap();

    let snap = tree.snapshot(b"users/").unwrap();
    assert_eq!(snap.get(b"users/alice").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(snap.scope(), b"users/");

    let err = snap.get(b"orders/x").unwrap_err();
    assert!(
        matches!(err, Error::OutsideViewScope { .. }),
        "out-of-scope read should be rejected, got {err:?}",
    );
}

#[test]
fn snapshot_epochs_advance_and_retire() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"a", b"1").unwrap();

    let s1 = tree.snapshot(b"").unwrap();
    let e1 = s1.epoch();
    let s2 = tree.snapshot(b"").unwrap();
    let e2 = s2.epoch();
    assert!(e2 > e1, "epochs must advance: {e1} then {e2}");

    s1.retire();
    drop(s2);

    // A fresh snapshot after all prior ones retire still advances the
    // monotonic epoch.
    let s3 = tree.snapshot(b"").unwrap();
    assert!(s3.epoch() > e2, "epoch must keep advancing past {e2}");
}

#[test]
fn snapshot_isolates_cross_blob_writes() {
    // The fork-on-write correctness gate: live writes that descend into
    // frames the snapshot still references must fork those frames, not
    // overwrite them. Uses a multi-blob tree so the writes cross into
    // shared child frames.
    let store: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
    let tree = TreeBuilder::new("ignored")
        .open_with_blob_store(store.clone())
        .unwrap();

    const N: u32 = 2000;
    let orig = vec![0xAB_u8; 200];
    for i in 0..N {
        tree.put(format!("k{i:08}").as_bytes(), &orig).unwrap();
    }
    assert!(
        store.list_blobs().unwrap().len() >= 2,
        "test needs a multi-blob tree",
    );

    let snap = tree.snapshot(b"").unwrap();

    // Mutations that must fork shared child frames: a spread of
    // different-size overwrites (forces leaf realloc), fresh inserts,
    // and a spread of deletes.
    for i in (0..N).step_by(4) {
        tree.put(format!("k{i:08}").as_bytes(), b"UPDATED").unwrap();
    }
    for i in N..N + 100 {
        tree.put(format!("k{i:08}").as_bytes(), b"brand-new").unwrap();
    }
    for i in (2..N).step_by(7) {
        tree.delete(format!("k{i:08}").as_bytes()).unwrap();
    }

    // Snapshot unchanged: every original key still maps to the original
    // value, and no post-snapshot insert is visible.
    for i in 0..N {
        assert_eq!(
            snap.get(format!("k{i:08}").as_bytes())
                .unwrap()
                .as_deref(),
            Some(&orig[..]),
            "snapshot key {i} changed under a live cross-blob write",
        );
    }
    for i in N..N + 100 {
        assert_eq!(snap.get(format!("k{i:08}").as_bytes()).unwrap(), None);
    }

    // Live tree reflects every mutation. Delete ran last, so a key that
    // was both updated and deleted ends up absent.
    for i in 0..N {
        let k = format!("k{i:08}");
        let live = tree.get(k.as_bytes()).unwrap();
        if i >= 2 && (i - 2) % 7 == 0 {
            assert_eq!(live, None, "live key {i} should be deleted");
        } else if i % 4 == 0 {
            assert_eq!(
                live.as_deref(),
                Some(&b"UPDATED"[..]),
                "live key {i} should be updated",
            );
        } else {
            assert_eq!(live.as_deref(), Some(&orig[..]), "live key {i} unchanged");
        }
    }
    for i in N..N + 100 {
        assert_eq!(
            tree.get(format!("k{i:08}").as_bytes())
                .unwrap()
                .as_deref(),
            Some(&b"brand-new"[..]),
        );
    }
}

#[test]
fn nested_cross_blob_snapshots_each_isolated() {
    // Two overlapping snapshots over a multi-blob tree: each must see
    // its own generation while the live tree advances. Exercises the
    // multi-epoch fork barrier (a frame forked for snapshot 1 becomes a
    // shared frame that snapshot 2 in turn freezes).
    let store: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
    let tree = TreeBuilder::new("ignored")
        .open_with_blob_store(store.clone())
        .unwrap();

    const N: u32 = 2000;
    let v1 = vec![0x01_u8; 200];
    let v2 = vec![0x02_u8; 200];
    let v3 = vec![0x03_u8; 200];

    for i in 0..N {
        tree.put(format!("k{i:08}").as_bytes(), &v1).unwrap();
    }
    assert!(store.list_blobs().unwrap().len() >= 2);

    let s1 = tree.snapshot(b"").unwrap();
    for i in 0..N {
        tree.put(format!("k{i:08}").as_bytes(), &v2).unwrap();
    }
    let s2 = tree.snapshot(b"").unwrap();
    for i in 0..N {
        tree.put(format!("k{i:08}").as_bytes(), &v3).unwrap();
    }

    for i in 0..N {
        let k = format!("k{i:08}");
        assert_eq!(
            s1.get(k.as_bytes()).unwrap().as_deref(),
            Some(&v1[..]),
            "s1 key {i}",
        );
        assert_eq!(
            s2.get(k.as_bytes()).unwrap().as_deref(),
            Some(&v2[..]),
            "s2 key {i}",
        );
        assert_eq!(
            tree.get(k.as_bytes()).unwrap().as_deref(),
            Some(&v3[..]),
            "live key {i}",
        );
    }
}

#[test]
fn snapshot_stable_under_randomized_churn() {
    use std::collections::HashMap;

    let store: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
    let tree = TreeBuilder::new("ignored")
        .open_with_blob_store(store)
        .unwrap();

    // Deterministic LCG so the interleaving is reproducible.
    let mut lcg: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        lcg = lcg
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (lcg >> 33) as u32
    };

    // Seed a multi-blob tree and mirror it in a model map.
    let mut live: HashMap<String, Vec<u8>> = HashMap::new();
    for i in 0..1500u32 {
        let k = format!("key{i:06}");
        let v = vec![(i & 0xFF) as u8; 180];
        tree.put(k.as_bytes(), &v).unwrap();
        live.insert(k, v);
    }

    // Freeze the expected snapshot state, then churn the live tree.
    let snap = tree.snapshot(b"").unwrap();
    let frozen = live.clone();

    for _ in 0..6000 {
        // Keys 1500..1800 are never seeded ⇒ post-snapshot inserts.
        let k = format!("key{:06}", next() % 1800);
        if next() % 4 == 0 {
            tree.delete(k.as_bytes()).unwrap();
            live.remove(&k);
        } else {
            let vlen = 1 + (next() % 200) as usize;
            let v = vec![(next() & 0xFF) as u8; vlen];
            tree.put(k.as_bytes(), &v).unwrap();
            live.insert(k, v);
        }
    }

    // The snapshot is frozen at capture time regardless of the churn.
    for (k, v) in &frozen {
        assert_eq!(
            snap.get(k.as_bytes()).unwrap().as_deref(),
            Some(&v[..]),
            "snapshot drifted at {k}",
        );
    }
    for i in 1500..1800u32 {
        let k = format!("key{i:06}");
        assert_eq!(
            snap.get(k.as_bytes()).unwrap(),
            None,
            "snapshot saw post-snapshot key {k}",
        );
    }

    // The live tree matches the model after all churn.
    for (k, v) in &live {
        assert_eq!(
            tree.get(k.as_bytes()).unwrap().as_deref(),
            Some(&v[..]),
            "live tree drifted at {k}",
        );
    }
}
