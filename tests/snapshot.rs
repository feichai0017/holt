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
