//! DB checkpoint export/install — whole-database image transfer.

use holt::{CheckpointImage, TreeConfig, DB};

fn memory_db() -> DB {
    DB::open(TreeConfig::memory()).expect("open memory DB")
}

#[test]
fn checkpoint_round_trips_all_families() {
    let image = {
        let db = memory_db();
        let inodes = db.create_tree("inodes").unwrap();
        let dentries = db.create_tree("dentries").unwrap();
        for i in 0..500u32 {
            inodes
                .put(format!("ino/{i:06}").as_bytes(), format!("v{i}").as_bytes())
                .unwrap();
            dentries
                .put(format!("d/{i:06}").as_bytes(), format!("e{i}").as_bytes())
                .unwrap();
        }
        db.export_checkpoint().unwrap()
    };
    image.validate().unwrap();

    // Install into a fresh DB.
    let db = memory_db();
    db.install_checkpoint(&image).unwrap();
    assert_eq!(db.list_trees().unwrap(), vec!["dentries", "inodes"]);

    let inodes = db.open_tree("inodes").unwrap();
    let dentries = db.open_tree("dentries").unwrap();
    for i in 0..500u32 {
        assert_eq!(
            inodes.get(format!("ino/{i:06}").as_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes()),
            "inodes key {i}",
        );
        assert_eq!(
            dentries.get(format!("d/{i:06}").as_bytes()).unwrap(),
            Some(format!("e{i}").into_bytes()),
            "dentries key {i}",
        );
    }
}

#[test]
fn checkpoint_survives_serialized_bytes() {
    // Archive/transfer flow: export -> bytes -> from_bytes -> install
    // on a fresh node. Multi-blob families exercise the cross-frame scan.
    const N: u32 = 2000;
    let value = vec![0xAB_u8; 200];

    let raw: Vec<u8> = {
        let db = memory_db();
        let meta = db.create_tree("meta").unwrap();
        for i in 0..N {
            meta.put(format!("k{i:08}").as_bytes(), &value).unwrap();
        }
        db.export_checkpoint().unwrap().into_bytes()
    };

    let db = memory_db();
    let image = CheckpointImage::from_bytes(raw);
    db.install_checkpoint(&image).unwrap();
    let meta = db.open_tree("meta").unwrap();
    for i in 0..N {
        assert_eq!(
            meta.get(format!("k{i:08}").as_bytes()).unwrap().as_deref(),
            Some(&value[..]),
            "key {i}",
        );
    }
}

#[test]
fn checkpoint_is_a_consistent_snapshot() {
    let db = memory_db();
    let m = db.create_tree("m").unwrap();
    for i in 0..1000u32 {
        m.put(format!("k{i:06}").as_bytes(), b"old").unwrap();
    }
    let image = db.export_checkpoint().unwrap();
    // Mutate after the export — the image is frozen at export time.
    for i in 0..1000u32 {
        m.put(format!("k{i:06}").as_bytes(), b"new").unwrap();
    }

    let db2 = memory_db();
    db2.install_checkpoint(&image).unwrap();
    let m2 = db2.open_tree("m").unwrap();
    for i in 0..1000u32 {
        assert_eq!(
            m2.get(format!("k{i:06}").as_bytes()).unwrap().as_deref(),
            Some(&b"old"[..]),
            "image must hold the pre-mutation value at key {i}",
        );
    }
    // The live source moved on.
    assert_eq!(m.get(b"k000000").unwrap().as_deref(), Some(&b"new"[..]));
}

#[test]
fn install_rejects_corrupt_image() {
    let db = memory_db();
    let truncated = CheckpointImage::from_bytes(vec![0u8; 4]);
    assert!(truncated.validate().is_err());
    assert!(db.install_checkpoint(&truncated).is_err());

    let bad_header = CheckpointImage::from_bytes(b"holtckp1".to_vec());
    assert!(bad_header.validate().is_err());
    assert!(db.install_checkpoint(&bad_header).is_err());
}
