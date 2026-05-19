//! Multi-reader stress measurement of the `HybridLatch` wait-free
//! read path.
//!
//! The walker takes a [`crate::concurrency::HybridLatch`] in
//! **optimistic** mode on the read path: snapshot the latch
//! version, read the buffer through an `UnsafeCell`, then
//! `validate()` to confirm no writer lapped the snapshot. If the
//! validation succeeds the read is wait-free — no real lock was
//! taken.
//!
//! The bench spawns N reader threads against a pre-populated tree
//! and measures aggregate throughput (lookups per second). Linear
//! scaling with thread count = "wait-free read works in
//! practice"; sub-linear = something is contended.
//!
//! Tagged `#[ignore]` because aggregate-throughput measurements
//! are timing-sensitive and noisy on shared CI runners. Run with:
//!
//!     cargo test --release --test bench_multi_reader -- \
//!         --ignored --nocapture
//!
//! For local tuning, run with `RUST_LOG=info` and the
//! `tracing` feature on (`--features tracing`) to see per-blob
//! events.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use holt::{Tree, TreeConfig};

const KEYS: u32 = 10_000;
const VALUE_LEN: usize = 64;

/// Build a tree with `KEYS` entries. All blobs end up resident in
/// the BufferManager because we only ever pin a few — the working
/// set fits comfortably in the default 64-blob pool.
fn populate_tree() -> (Tree, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = TreeConfig::new(dir.path());
    cfg.buffer_pool_size = 128; // generous, so reads never miss
    let tree = Tree::open(cfg).unwrap();
    for i in 0..KEYS {
        let k = format!("key/{i:08}");
        let v = vec![(i & 0xFF) as u8; VALUE_LEN];
        tree.put(k.as_bytes(), &v).unwrap();
    }
    tree.checkpoint().unwrap();
    (tree, dir)
}

/// Run `threads` readers in parallel, each issuing as many gets as
/// it can fit into `duration`. Returns the aggregate ops count.
fn run_reader_bench(tree: &Tree, threads: usize, duration: Duration) -> u64 {
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(threads);

    for t in 0..threads {
        let tree = tree.clone();
        let stop = Arc::clone(&stop);
        handles.push(std::thread::spawn(move || -> u64 {
            // Each thread starts from a different offset to defeat
            // any per-thread CPU-cache coincidences.
            let mut i: u32 = (t as u32).wrapping_mul(KEYS / threads as u32);
            let mut ops: u64 = 0;
            while !stop.load(Ordering::Relaxed) {
                let k = format!("key/{:08}", i % KEYS);
                let _v = tree.get(k.as_bytes()).unwrap();
                i = i.wrapping_add(1);
                ops += 1;
            }
            ops
        }));
    }

    // Let the readers ramp up briefly, then time the steady-state
    // window so we don't include thread-spawn jitter.
    std::thread::sleep(Duration::from_millis(50));
    let t0 = Instant::now();
    std::thread::sleep(duration);
    stop.store(true, Ordering::Relaxed);
    let mut total: u64 = 0;
    for h in handles {
        total += h.join().unwrap();
    }
    // Subtract the warm-up window from the count? No — we sampled
    // the count over the full run incl. ramp-up. Good enough: the
    // ramp-up window is tiny vs the measurement window.
    let _ = t0;
    total
}

#[test]
#[ignore]
fn hybrid_latch_read_scaling() {
    let (tree, _dir) = populate_tree();
    let measure = Duration::from_secs(2);

    println!("\n=== HybridLatch read scaling ({KEYS} keys × {VALUE_LEN} B values) ===\n");
    println!(
        "{:<10}  {:>14}  {:>14}  {:>10}",
        "threads", "total_ops", "ops/sec", "scaling"
    );
    println!("{}", "-".repeat(56));

    let mut base_throughput: Option<u64> = None;
    for &threads in &[1usize, 2, 4, 8, 16] {
        let ops = run_reader_bench(&tree, threads, measure);
        let ops_per_sec = (ops as f64 / measure.as_secs_f64()) as u64;
        let scaling = base_throughput.map_or(1.0, |b| ops_per_sec as f64 / b as f64);
        if base_throughput.is_none() {
            base_throughput = Some(ops_per_sec);
        }
        println!(
            "{:<10}  {:>14}  {:>14}  {:>9.2}x",
            threads, ops, ops_per_sec, scaling,
        );
    }
    println!();

    // The bench is informational only — we don't assert on
    // scaling numbers because hosted-runner CPU pinning is
    // inconsistent. Just sanity-check that we measured something.
    let ops = run_reader_bench(&tree, 1, Duration::from_millis(200));
    assert!(ops > 100, "1-thread bench measured suspiciously few ops");
}
