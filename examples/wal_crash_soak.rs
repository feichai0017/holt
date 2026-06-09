//! Multi-process SIGKILL crash-soak for the WAL backend.
//!
//! The parent repeatedly spawns a child that hammers writes into a
//! file-backed tree, `SIGKILL`s it at jittered points (to land mid-write,
//! mid-flusher-drain, and mid-checkpoint-truncate), then reopens the tree and
//! verifies the recovered state.
//!
//! Invariant (both `sync` and `async` durability): because the child writes
//! keys `0,1,2,…` strictly in order and the WAL is an ordered log, any
//! crash-recovered state — checkpointed blobs ∪ replayed WAL prefix — must be
//! a CONTIGUOUS prefix `{0..=K}` with the correct value at every index.
//! A gap, a torn/wrong value, an extra key, or a reopen/replay failure is a
//! durability or corruption bug. Reusing one directory across rounds also
//! exercises crash → reopen → write-more → crash repeatedly.
//!
//! This is the gate `checkpoint_failpoint` (in-process error injection) does
//! not cover: a real `kill -9` exercising the async RAM→page-cache window and
//! a flusher caught mid-drain / mid-truncate.
//!
//! Run (ring backend):
//!   cargo run --release --example wal_crash_soak --features wal_ring -- [rounds]
//! Run (legacy backend):
//!   cargo run --release --example wal_crash_soak -- [rounds]

use std::process::Command;
use std::time::{Duration, SystemTime};

use holt::{Durability, KeyRangeEntryRef, Tree, TreeConfig};

fn key(i: u64) -> Vec<u8> {
    format!("obj/{i:012}").into_bytes()
}

fn value(i: u64) -> Vec<u8> {
    let mut v = i.to_le_bytes().to_vec();
    v.resize(24, (i & 0xff) as u8);
    v
}

fn parse_index(k: &[u8]) -> Option<u64> {
    std::str::from_utf8(k)
        .ok()?
        .strip_prefix("obj/")?
        .parse()
        .ok()
}

/// Child: open the tree and write `0,1,2,…` forever until SIGKILL'd.
fn run_child(dir: &str, sync: bool) -> ! {
    let mut cfg = TreeConfig::new(dir);
    cfg.durability = Durability::Wal { sync };
    let tree = Tree::open(cfg).expect("child: open tree");
    let mut i = 0u64;
    loop {
        tree.put(&key(i), &value(i)).expect("child: put");
        i = i.wrapping_add(1);
    }
}

/// Parent: reopen after a crash and verify the recovered prefix.
fn verify(dir: &str) -> u64 {
    let tree = Tree::open(TreeConfig::new(dir)).expect("reopen after crash must succeed (replay)");
    let mut indices: Vec<u64> = Vec::new();
    tree.scan_keys(b"")
        .visit(usize::MAX, |entry| {
            if let KeyRangeEntryRef::Key { key, .. } = entry {
                let i = parse_index(key)
                    .unwrap_or_else(|| panic!("corrupt/unexpected key on recovery: {key:?}"));
                indices.push(i);
            }
            Ok(())
        })
        .expect("scan recovered tree");
    indices.sort_unstable();
    for (pos, &i) in indices.iter().enumerate() {
        assert_eq!(
            i, pos as u64,
            "recovered set is NOT a contiguous prefix (gap before index {i})"
        );
        let got = tree
            .get(&key(i))
            .expect("get on recovered tree")
            .unwrap_or_else(|| panic!("index {i} scanned but get returned None"));
        assert_eq!(got, value(i), "torn/corrupt value at recovered index {i}");
    }
    indices.len() as u64
}

fn jitter_ms() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    25 + u64::from(nanos % 275) // 25..=299 ms
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("child") {
        let dir = args.get(2).expect("child: missing dir arg");
        let sync = args.get(3).map(String::as_str) == Some("sync");
        run_child(dir, sync);
    }

    let rounds: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(40);
    let dir = tempfile::tempdir().expect("tempdir");
    let dirpath = dir.path().to_str().expect("utf8 dir").to_string();
    let exe = std::env::current_exe().expect("current_exe");

    println!("wal_crash_soak: {rounds} rounds, dir={dirpath}");
    let mut max_index = 0u64;
    for round in 0..rounds {
        let sync = round % 2 == 0;
        let mode = if sync { "sync" } else { "async" };
        let mut child = Command::new(&exe)
            .arg("child")
            .arg(&dirpath)
            .arg(mode)
            .spawn()
            .expect("spawn child");
        let ms = jitter_ms();
        std::thread::sleep(Duration::from_millis(ms));
        child.kill().expect("SIGKILL child");
        let _ = child.wait();

        let count = verify(&dirpath);
        max_index = max_index.max(count.saturating_sub(1));
        println!(
            "round {round:3} [{mode:5}] killed @ {ms:3}ms -> recovered {count:7} keys (contiguous, values OK)"
        );
    }
    println!("\nOK: {rounds} SIGKILL rounds, every recovery was a contiguous valid prefix; max index survived = {max_index}");
}
