//! `put` latency distribution while a background checkpointer
//! and periodic manual `compact()` runs interfere — the
//! tail-latency story (p95 / p99 / p99.9) for a holt deployment
//! that runs maintenance concurrently with the user workload.
//!
//! Not a criterion bench (criterion reports mean ± noise band,
//! not percentiles). Uses [`hdrhistogram`] to capture every
//! sample latency and report mean / p50 / p95 / p99 / p99.9 / max.
//!
//! Wrapped in `#[ignore]` because the bench runs for ~30 s and
//! shouldn't fire on every `cargo test`. Run explicitly:
//!
//! ```bash
//! cargo test --release --test bench_contention_p95 -- --ignored --nocapture
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use holt::{CheckpointConfig, TreeBuilder};
use tempfile::tempdir;

const N_WRITERS: usize = 4;
const WORKLOAD_SECS: u64 = 20;
/// Manual compact every N writes (one writer thread issues it).
const COMPACT_EVERY: u32 = 20_000;
const PAYLOAD_LEN: usize = 256;

#[test]
#[ignore = "long-running bench; use `cargo test --release ... -- --ignored --nocapture`"]
fn put_latency_under_bg_checkpoint_and_compact_interference() {
    let dir = tempdir().unwrap();
    let tree = Arc::new(
        TreeBuilder::new(dir.path())
            // Aggressive background checkpoint cadence — the
            // round runs constantly under the writer load, so
            // any tail-latency spike from commit publication or
            // checkpoint I/O shows up here.
            .checkpoint(CheckpointConfig {
                enabled: true,
                idle_interval: Duration::from_millis(5),
                dirty_blob_threshold: 1,
                auto_merge: false,
                ..CheckpointConfig::default()
            })
            .open()
            .unwrap(),
    );

    let stop = Arc::new(AtomicBool::new(false));
    // Per-writer histogram (lock-free; merged at the end).
    let payload = vec![b'q'; PAYLOAD_LEN];

    let mut handles = Vec::with_capacity(N_WRITERS + 1);

    for writer_id in 0..N_WRITERS {
        let tree = Arc::clone(&tree);
        let stop = Arc::clone(&stop);
        let payload = payload.clone();
        handles.push(thread::spawn(move || -> Histogram<u64> {
            // 5-significant-digit precision up to 60 s in
            // nanoseconds. Sized to hold every latency we'd
            // care about; the hist is cheap (~200 KB).
            let mut hist = Histogram::<u64>::new_with_bounds(
                /*low=*/ 1,
                /*high=*/ 60_000_000_000,
                3,
            )
            .unwrap();
            let mut counter: u32 = 0;
            while !stop.load(Ordering::Relaxed) {
                let key = format!("w{writer_id:02}/k{counter:08}");
                let start = Instant::now();
                tree.put(key.as_bytes(), &payload).unwrap();
                // Saturating into the hist's range (clamped at
                // 60 s, which would be a pathological stall —
                // record it explicitly rather than panic).
                let _ = hist.record(start.elapsed().as_nanos().min(60_000_000_000) as u64);
                counter = counter.wrapping_add(1);
            }
            hist
        }));
    }

    // Compaction thread — runs `tree.compact()` periodically.
    // It races with writers + the bg checkpointer through the
    // maintenance gate and commit publication path, so this is the
    // worst-case maintenance interference shape.
    {
        let tree = Arc::clone(&tree);
        let stop = Arc::clone(&stop);
        handles.push(thread::spawn(move || -> Histogram<u64> {
            // Empty per-thread hist so the join signature matches.
            let empty = Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3).unwrap();
            let mut last_compact_at = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let total_dirty = tree.stats().map(|s| s.bm_dirty_count).unwrap_or(0);
                // Loose proxy: when the dirty set has grown
                // since the last compact, kick another one.
                if total_dirty as u32 >= last_compact_at + COMPACT_EVERY {
                    let _ = tree.compact();
                    last_compact_at = total_dirty as u32;
                }
                thread::sleep(Duration::from_millis(100));
            }
            empty
        }));
    }

    // Let the workload run.
    let start = Instant::now();
    thread::sleep(Duration::from_secs(WORKLOAD_SECS));
    stop.store(true, Ordering::Relaxed);

    let mut merged = Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3).unwrap();
    for h in handles {
        let hist = h.join().unwrap();
        // Skip the empty compactor histogram.
        if !hist.is_empty() {
            merged.add(&hist).unwrap();
        }
    }
    let elapsed = start.elapsed();

    let n = merged.len();
    let throughput = n as f64 / elapsed.as_secs_f64();
    println!("\n┌─────────────────────────────────────────────────────┐");
    println!("│ put latency under bg checkpoint + compact contention │");
    println!("├─────────────────────────────────────────────────────┤");
    println!("│ writers          : {N_WRITERS:>30} │");
    println!(
        "│ workload         : {:>30} │",
        format!("{WORKLOAD_SECS} s")
    );
    println!("│ ops              : {n:>30} │");
    println!("│ throughput       : {:>27.0} ops/s │", throughput);
    println!(
        "│ mean             : {:>27.2} µs │",
        merged.mean() / 1_000.0
    );
    println!(
        "│ p50              : {:>27.2} µs │",
        merged.value_at_quantile(0.50) as f64 / 1_000.0
    );
    println!(
        "│ p95              : {:>27.2} µs │",
        merged.value_at_quantile(0.95) as f64 / 1_000.0
    );
    println!(
        "│ p99              : {:>27.2} µs │",
        merged.value_at_quantile(0.99) as f64 / 1_000.0
    );
    println!(
        "│ p99.9            : {:>27.2} µs │",
        merged.value_at_quantile(0.999) as f64 / 1_000.0
    );
    println!(
        "│ max              : {:>27.2} µs │",
        merged.max() as f64 / 1_000.0
    );
    println!("└─────────────────────────────────────────────────────┘\n");

    // Smoke-level assertion: bg checkpoint + compact running
    // concurrently must not drive p99 into the seconds range.
    // 100 ms is generous — if we ever exceed this, something
    // really went wrong (e.g. a stuck mutex chain). The
    // important data is the printed table; this is just a
    // sanity stop on regressions.
    let p99_us = merged.value_at_quantile(0.99) as f64 / 1_000.0;
    assert!(
        p99_us < 100_000.0,
        "p99 = {p99_us:.0} µs — under 100 ms guard rail",
    );
}
