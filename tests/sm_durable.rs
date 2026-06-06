//! StateMachine durable recovery — `DB::commit_durable` + reopen.
//!
//! In StateMachine mode there is no WAL: the durable on-disk point is a
//! copy-on-write snapshot committed atomically with the manifest. Reopen
//! rehydrates from it; writes past the last `commit_durable` are lost
//! (an external log would replay them).

use std::path::Path;

use holt::{Durability, Error, Tree, TreeConfig, DB};
use tempfile::tempdir;

fn sm_file_db(dir: &Path) -> DB {
    let mut cfg = TreeConfig::new(dir);
    cfg.durability = Durability::StateMachine;
    DB::open(cfg).expect("open state-machine file DB")
}

/// A metadata-store-shaped fixture: many named families opened up front,
/// commands applied as multi-family `DB::atomic` conditional batches
/// (`put_if_absent` to a "current" family + a "dedupe" family, like a
/// replicated metadata service), each pinned with `commit_durable`.
#[test]
fn durable_recovers_metadata_store_shaped_workload() {
    // 14 families, opened at startup like a real metadata store.
    const FAMILIES: &[&str] = &[
        "system_current",
        "mount_current",
        "inode_current",
        "dentry_current",
        "parent_current",
        "xattr_current",
        "chunk_current",
        "session_current",
        "path_index_current",
        "watch_current",
        "snapshot_current",
        "gc_current",
        "command_dedupe_current",
        "history",
    ];
    let dir = tempdir().unwrap();
    {
        // memory_flush_on_write=false: frames stay dirty until checkpoint,
        // so commit_durable's flush is the only thing persisting them.
        let mut cfg = TreeConfig::new(dir.path());
        cfg.durability = Durability::StateMachine;
        cfg.memory_flush_on_write = false;
        let db = DB::open(cfg).unwrap();
        for f in FAMILIES {
            db.create_tree(f).unwrap();
        }
        for n in 1..=40u64 {
            db.atomic(|b| {
                let key = format!("dir/{n:05}");
                // current record (NotExists predicate -> put_if_absent)
                b.put_if_absent("dentry_current", key.as_bytes(), &n.to_le_bytes());
                // dedupe record (put_if_absent by request id)
                b.put_if_absent(
                    "command_dedupe_current",
                    format!("req-{n}").as_bytes(),
                    b"ok",
                );
                // a history record for the prior version
                if n > 1 {
                    b.put(
                        "history",
                        format!("h/dentry/dir/{:05}/{}", n - 1, n - 1).as_bytes(),
                        &n.to_le_bytes(),
                    );
                }
            })
            .unwrap();
            db.commit_durable(n).unwrap();
        }
    }
    // Reopen with no WAL — recovery is the durable manifest alone.
    let db = sm_file_db(dir.path());
    assert_eq!(db.durable_applied_index().unwrap(), 40);
    assert_eq!(db.list_trees().unwrap().len(), FAMILIES.len());
    let dentry = db.open_tree("dentry_current").unwrap();
    let dedupe = db.open_tree("command_dedupe_current").unwrap();
    for n in 1..=40u64 {
        assert_eq!(
            dentry.get(format!("dir/{n:05}").as_bytes()).unwrap(),
            Some(n.to_le_bytes().to_vec()),
            "dentry dir/{n:05}",
        );
        assert!(
            dedupe.get(format!("req-{n}").as_bytes()).unwrap().is_some(),
            "dedupe req-{n}",
        );
    }
}

fn sm_file_tree(dir: &Path) -> Tree {
    let mut cfg = TreeConfig::new(dir);
    cfg.durability = Durability::StateMachine;
    Tree::open(cfg).expect("open state-machine file tree")
}

#[test]
fn commit_durable_recovers_state_and_index() {
    let dir = tempdir().unwrap();
    {
        let db = sm_file_db(dir.path());
        let inodes = db.create_tree("inodes").unwrap();
        let dentries = db.create_tree("dentries").unwrap();
        for i in 0..200u32 {
            inodes
                .put(format!("ino/{i:05}").as_bytes(), format!("v{i}").as_bytes())
                .unwrap();
            dentries
                .put(format!("d/{i:05}").as_bytes(), format!("e{i}").as_bytes())
                .unwrap();
        }
        db.commit_durable(42).unwrap();
    }

    // Reopen with no WAL: state + applied_index come from the durable manifest.
    let db = sm_file_db(dir.path());
    assert_eq!(db.durable_applied_index().unwrap(), 42);
    assert_eq!(db.list_trees().unwrap(), vec!["dentries", "inodes"]);
    let inodes = db.open_tree("inodes").unwrap();
    let dentries = db.open_tree("dentries").unwrap();
    for i in 0..200u32 {
        assert_eq!(
            inodes.get(format!("ino/{i:05}").as_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes()),
            "inode {i}",
        );
        assert_eq!(
            dentries.get(format!("d/{i:05}").as_bytes()).unwrap(),
            Some(format!("e{i}").into_bytes()),
            "dentry {i}",
        );
    }
}

#[test]
fn writes_after_commit_durable_roll_back_on_reopen() {
    let dir = tempdir().unwrap();
    {
        let db = sm_file_db(dir.path());
        let t = db.create_tree("t").unwrap();
        t.put(b"a", b"1").unwrap();
        db.commit_durable(10).unwrap();
        // Past the durable point, not committed — must not survive.
        t.put(b"a", b"2").unwrap();
        t.put(b"b", b"never").unwrap();
    }
    let db = sm_file_db(dir.path());
    assert_eq!(db.durable_applied_index().unwrap(), 10);
    let t = db.open_tree("t").unwrap();
    assert_eq!(t.get(b"a").unwrap(), Some(b"1".to_vec())); // rolled back
    assert_eq!(t.get(b"b").unwrap(), None); // never durable
}

#[test]
fn reopen_then_write_without_commit_still_rolls_back() {
    // The reopen-time held durable snapshot (Risk R1): first post-reopen
    // writes must fork past the durable image, not overwrite it in place.
    let dir = tempdir().unwrap();
    {
        let db = sm_file_db(dir.path());
        let t = db.create_tree("t").unwrap();
        t.put(b"a", b"1").unwrap();
        db.commit_durable(10).unwrap();
    }
    {
        let db = sm_file_db(dir.path());
        assert_eq!(
            db.open_tree("t").unwrap().get(b"a").unwrap(),
            Some(b"1".to_vec())
        );
        db.open_tree("t").unwrap().put(b"a", b"99").unwrap();
        db.open_tree("t").unwrap().put(b"c", b"x").unwrap();
    }
    let db = sm_file_db(dir.path());
    assert_eq!(db.durable_applied_index().unwrap(), 10);
    let t = db.open_tree("t").unwrap();
    assert_eq!(t.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(t.get(b"c").unwrap(), None);
}

#[test]
fn second_commit_durable_advances_index() {
    let dir = tempdir().unwrap();
    {
        let db = sm_file_db(dir.path());
        let t = db.create_tree("t").unwrap();
        t.put(b"k", b"v1").unwrap();
        db.commit_durable(5).unwrap();
        t.put(b"k", b"v2").unwrap();
        db.commit_durable(9).unwrap();
    }
    let db = sm_file_db(dir.path());
    assert_eq!(db.durable_applied_index().unwrap(), 9);
    assert_eq!(
        db.open_tree("t").unwrap().get(b"k").unwrap(),
        Some(b"v2".to_vec())
    );
}

#[test]
fn commit_durable_requires_state_machine() {
    let dir = tempdir().unwrap();
    let mut cfg = TreeConfig::new(dir.path());
    cfg.durability = Durability::Wal { sync: false };
    let db = DB::open(cfg).unwrap();
    assert!(matches!(
        db.commit_durable(1),
        Err(Error::CommitDurableRequiresStateMachine),
    ));
}

#[test]
fn fresh_state_machine_db_has_no_durable_index() {
    let dir = tempdir().unwrap();
    let db = sm_file_db(dir.path());
    assert_eq!(db.durable_applied_index().unwrap(), 0);
}

#[test]
fn standalone_tree_commit_durable_recovers_and_rolls_back() {
    let dir = tempdir().unwrap();
    {
        let tree = sm_file_tree(dir.path());
        for i in 0..150u32 {
            tree.put(format!("k{i:04}").as_bytes(), format!("v{i}").as_bytes())
                .unwrap();
        }
        tree.commit_durable(17).unwrap();
        tree.put(b"uncommitted", b"x").unwrap(); // past the durable point
    }
    let tree = sm_file_tree(dir.path());
    assert_eq!(tree.durable_applied_index().unwrap(), 17);
    for i in 0..150u32 {
        assert_eq!(
            tree.get(format!("k{i:04}").as_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes()),
            "key {i}",
        );
    }
    assert_eq!(tree.get(b"uncommitted").unwrap(), None);
}

#[test]
fn standalone_tree_commit_durable_requires_state_machine() {
    let dir = tempdir().unwrap();
    let mut cfg = TreeConfig::new(dir.path());
    cfg.durability = Durability::Wal { sync: false };
    let tree = Tree::open(cfg).unwrap();
    assert!(matches!(
        tree.commit_durable(1),
        Err(Error::CommitDurableRequiresStateMachine),
    ));
}
