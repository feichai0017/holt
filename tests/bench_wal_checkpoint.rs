//! WAL/checkpoint fast-path probe for persistent holt trees.
//!
//! This is a holt-only microprobe for the checkpoint path's WAL
//! work, separate from the manifest/data-file pressure measured
//! by `bench_manifest_checkpoint`.
//!
//! It isolates four paths:
//!
//! - clean foreground checkpoints must not fsync or truncate an
//!   empty WAL;
//! - `wal_sync_on_commit=true` puts already made the WAL durable,
//!   so the following checkpoint must not add a second WAL fsync;
//! - default non-durable puts require the checkpoint barrier to
//!   issue the WAL fsync before flushing blobs;
//! - background idle rounds must remain no-op rounds when nothing
//!   is dirty and the WAL is already clean.
//!
//! Run explicitly:
//!
//! ```bash
//! cargo test --release --test bench_wal_checkpoint -- --ignored --nocapture
//! ```
//!
//! Short smoke:
//!
//! ```bash
//! HOLT_WAL_BENCH_CLEAN_ITERS=50 \
//! HOLT_WAL_BENCH_MUTATIONS=10 \
//! cargo test --release --test bench_wal_checkpoint -- --ignored --nocapture
//! ```

use std::env;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use holt::{CheckpointConfig, JournalStats, Tree, TreeBuilder, TreeConfig};
use tempfile::tempdir;

const DEFAULT_CLEAN_ITERS: usize = 500;
const DEFAULT_MUTATIONS: usize = 64;
const DEFAULT_VALUE_LEN: usize = 64;
const DEFAULT_BG_WAIT_MS: u64 = 250;
const HIST_MAX_NS: u64 = 60_000_000_000;
const WAL_HEADER_BYTES: u64 = 32;

#[test]
#[ignore = "WAL/checkpoint timing probe; use `cargo test --release --test bench_wal_checkpoint -- --ignored --nocapture`"]
fn wal_checkpoint_fast_paths() {
    let clean_iters = env_usize("HOLT_WAL_BENCH_CLEAN_ITERS", DEFAULT_CLEAN_ITERS);
    let mutations = env_usize("HOLT_WAL_BENCH_MUTATIONS", DEFAULT_MUTATIONS);
    let value_len = env_usize("HOLT_WAL_BENCH_VALUE_LEN", DEFAULT_VALUE_LEN);
    let bg_wait_ms = env_u64("HOLT_WAL_BENCH_BG_WAIT_MS", DEFAULT_BG_WAIT_MS);

    let clean = run_clean_checkpoint(clean_iters);
    let durable = run_durable_checkpoint(mutations, value_len);
    let nondurable = run_nondurable_checkpoint(mutations, value_len);
    let bg_idle = run_background_idle(bg_wait_ms);

    println!(
        "\n=== WAL/checkpoint fast paths (clean_iters={clean_iters}, mutations={mutations}, value_len={value_len}, bg_wait_ms={bg_wait_ms}) ===\n"
    );
    println!(
        "{:<32} {:>11} {:>11} {:>11} {:>11} {:>10} {:>10} {:>8}",
        "path", "put_p50", "ckpt_p50", "ckpt_p95", "ckpt_p99", "syncs", "wal_resets", "wal",
    );
    println!("{}", "-".repeat(110));
    print_report(&clean);
    print_report(&durable);
    print_report(&nondurable);
    print_report(&bg_idle);
    println!();
    println!(
        "counters: durable checkpoint_extra_wal_syncs={} non_durable_checkpoint_wal_syncs={} bg_idle_rounds={}/{}",
        durable.checkpoint_sync_delta,
        nondurable.checkpoint_sync_delta,
        bg_idle.rounds_succeeded,
        bg_idle.rounds_attempted,
    );

    assert_eq!(
        clean.checkpoint_sync_delta, 0,
        "clean checkpoint must not fsync an empty WAL",
    );
    assert_eq!(
        clean.final_wal_bytes, WAL_HEADER_BYTES,
        "clean WAL should stay header-only",
    );
    assert_eq!(
        durable.checkpoint_sync_delta, 0,
        "checkpoint must reuse durable group-commit WAL syncs",
    );
    assert!(
        durable.put_sync_delta >= mutations as u64,
        "durable puts should drive WAL syncs: mutations={mutations}, put_sync_delta={}",
        durable.put_sync_delta,
    );
    assert_eq!(
        nondurable.put_sync_delta, 0,
        "default puts should not issue per-op WAL fsyncs",
    );
    assert!(
        nondurable.checkpoint_sync_delta >= mutations as u64,
        "non-durable puts need checkpoint WAL syncs: mutations={mutations}, checkpoint_sync_delta={}",
        nondurable.checkpoint_sync_delta,
    );
    assert_eq!(
        bg_idle.checkpoint_sync_delta, 0,
        "background idle rounds must not fsync an empty WAL",
    );
    assert_eq!(
        bg_idle.wal_resets, 0,
        "background idle rounds must not truncate an already-clean WAL",
    );
}

#[derive(Clone)]
struct BenchReport {
    label: &'static str,
    put_hist: Option<Histogram<u64>>,
    checkpoint_hist: Histogram<u64>,
    put_sync_delta: u64,
    checkpoint_sync_delta: u64,
    wal_resets: u64,
    rounds_attempted: u64,
    rounds_succeeded: u64,
    final_wal_bytes: u64,
}

fn run_clean_checkpoint(iters: usize) -> BenchReport {
    let dir = tempdir().unwrap();
    let tree = Tree::open(TreeConfig::new(dir.path())).unwrap();
    let before = journal_stats(&tree);
    let mut checkpoint_hist = new_hist();

    for _ in 0..iters {
        record_elapsed(&mut checkpoint_hist, || tree.checkpoint().unwrap());
    }

    let after = journal_stats(&tree);
    BenchReport {
        label: "clean foreground checkpoint",
        put_hist: None,
        checkpoint_hist,
        put_sync_delta: 0,
        checkpoint_sync_delta: after.syncs - before.syncs,
        wal_resets: 0,
        rounds_attempted: 0,
        rounds_succeeded: 0,
        final_wal_bytes: wal_size(dir.path()),
    }
}

fn run_durable_checkpoint(mutations: usize, value_len: usize) -> BenchReport {
    let dir = tempdir().unwrap();
    let tree = TreeBuilder::new(dir.path())
        .wal_sync_on_commit(true)
        .open()
        .unwrap();
    let value = vec![0xD1; value_len];
    let mut put_hist = new_hist();
    let mut checkpoint_hist = new_hist();
    let mut put_sync_delta = 0;
    let mut checkpoint_sync_delta = 0;
    let mut wal_resets = 0;

    for i in 0..mutations {
        let before_put = journal_stats(&tree);
        record_elapsed(&mut put_hist, || {
            tree.put(&bench_key("durable", i), &value).unwrap();
        });
        let after_put = journal_stats(&tree);
        put_sync_delta += after_put.syncs - before_put.syncs;

        record_elapsed(&mut checkpoint_hist, || tree.checkpoint().unwrap());
        let after_checkpoint = journal_stats(&tree);
        checkpoint_sync_delta += after_checkpoint.syncs - after_put.syncs;
        assert_eq!(wal_size(dir.path()), WAL_HEADER_BYTES);
        wal_resets += 1;
    }

    BenchReport {
        label: "durable put then checkpoint",
        put_hist: Some(put_hist),
        checkpoint_hist,
        put_sync_delta,
        checkpoint_sync_delta,
        wal_resets,
        rounds_attempted: 0,
        rounds_succeeded: 0,
        final_wal_bytes: wal_size(dir.path()),
    }
}

fn run_nondurable_checkpoint(mutations: usize, value_len: usize) -> BenchReport {
    let dir = tempdir().unwrap();
    let tree = Tree::open(TreeConfig::new(dir.path())).unwrap();
    let value = vec![0xA7; value_len];
    let mut put_hist = new_hist();
    let mut checkpoint_hist = new_hist();
    let mut put_sync_delta = 0;
    let mut checkpoint_sync_delta = 0;
    let mut wal_resets = 0;

    for i in 0..mutations {
        let before_put = journal_stats(&tree);
        record_elapsed(&mut put_hist, || {
            tree.put(&bench_key("nondurable", i), &value).unwrap();
        });
        let after_put = journal_stats(&tree);
        put_sync_delta += after_put.syncs - before_put.syncs;

        record_elapsed(&mut checkpoint_hist, || tree.checkpoint().unwrap());
        let after_checkpoint = journal_stats(&tree);
        checkpoint_sync_delta += after_checkpoint.syncs - after_put.syncs;
        assert_eq!(wal_size(dir.path()), WAL_HEADER_BYTES);
        wal_resets += 1;
    }

    BenchReport {
        label: "default put then checkpoint",
        put_hist: Some(put_hist),
        checkpoint_hist,
        put_sync_delta,
        checkpoint_sync_delta,
        wal_resets,
        rounds_attempted: 0,
        rounds_succeeded: 0,
        final_wal_bytes: wal_size(dir.path()),
    }
}

fn run_background_idle(wait_ms: u64) -> BenchReport {
    let dir = tempdir().unwrap();
    let tree = TreeBuilder::new(dir.path())
        .checkpoint(CheckpointConfig {
            enabled: true,
            idle_interval: Duration::from_millis(25),
            dirty_blob_threshold: 1,
            auto_merge: true,
            ..CheckpointConfig::default()
        })
        .open()
        .unwrap();
    let before = tree.stats().unwrap();
    thread::sleep(Duration::from_millis(wait_ms));
    let after = tree.stats().unwrap();
    let before_journal = before.journal.unwrap();
    let after_journal = after.journal.unwrap();
    let before_checkpointer = before.checkpointer.unwrap();
    let after_checkpointer = after.checkpointer.unwrap();

    let mut checkpoint_hist = new_hist();
    checkpoint_hist.record(1).unwrap();

    BenchReport {
        label: "background idle rounds",
        put_hist: None,
        checkpoint_hist,
        put_sync_delta: 0,
        checkpoint_sync_delta: after_journal.syncs - before_journal.syncs,
        wal_resets: after_checkpointer.truncates - before_checkpointer.truncates,
        rounds_attempted: after_checkpointer.rounds_attempted
            - before_checkpointer.rounds_attempted,
        rounds_succeeded: after_checkpointer.rounds_succeeded
            - before_checkpointer.rounds_succeeded,
        final_wal_bytes: wal_size(dir.path()),
    }
}

fn bench_key(prefix: &str, i: usize) -> Vec<u8> {
    format!("tenant-00/{prefix}/dir-{}/file-{i:08}.meta", i % 16).into_bytes()
}

fn journal_stats(tree: &Tree) -> JournalStats {
    tree.stats()
        .unwrap()
        .journal
        .expect("persistent tree has journal stats")
}

fn print_report(report: &BenchReport) {
    let put_p50 = report
        .put_hist
        .as_ref()
        .map(|hist| format!("{:.2}ms", hist_ms(hist, 50.0)))
        .unwrap_or_else(|| "-".to_owned());
    println!(
        "{:<32} {:>11} {:>10.2}ms {:>10.2}ms {:>10.2}ms {:>10} {:>10} {:>8}",
        report.label,
        put_p50,
        hist_ms(&report.checkpoint_hist, 50.0),
        hist_ms(&report.checkpoint_hist, 95.0),
        hist_ms(&report.checkpoint_hist, 99.0),
        report.checkpoint_sync_delta,
        report.wal_resets,
        pretty_bytes(report.final_wal_bytes),
    );
}

fn wal_size(dir: &Path) -> u64 {
    fs::metadata(dir.join("journal.wal"))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn pretty_bytes(bytes: u64) -> String {
    let kib = bytes as f64 / 1024.0;
    if kib < 1024.0 {
        format!("{kib:.1}K")
    } else {
        format!("{:.2}M", kib / 1024.0)
    }
}

fn new_hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, HIST_MAX_NS, 3).unwrap()
}

fn record_elapsed<T>(hist: &mut Histogram<u64>, f: impl FnOnce() -> T) -> Duration {
    let start = Instant::now();
    let _ = f();
    let elapsed = start.elapsed();
    let nanos = elapsed.as_nanos().min(u128::from(HIST_MAX_NS)) as u64;
    let _ = hist.record(nanos.max(1));
    elapsed
}

fn hist_ms(hist: &Histogram<u64>, percentile: f64) -> f64 {
    hist.value_at_percentile(percentile) as f64 / 1_000_000.0
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}
