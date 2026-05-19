//! End-to-end measurement of the v0.2 background checkpointer.
//!
//! Not a criterion microbench — wall-clock + on-disk numbers
//! comparing the same write workload with the checkpointer
//! disabled vs enabled. Gated as a regular test so `cargo test`
//! can sanity-check the numbers; the assertions only check
//! correctness (the bg-checkpointer-enabled run actually keeps
//! the WAL bounded), the numeric printout is the interesting
//! output.
//!
//! Run with `cargo test --release --test bench_bg_checkpointer -- --nocapture`.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use holt::{CheckpointConfig, Tree, TreeConfig};
use tempfile::TempDir;

const KEYS: u32 = 5_000;

fn wal_size(dir: &std::path::Path) -> u64 {
    fs::metadata(dir.join("journal.wal"))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn data_size(dir: &std::path::Path) -> u64 {
    fs::metadata(dir.join("blobs.dat"))
        .map(|m| m.len())
        .unwrap_or(0)
}

struct Run {
    label: &'static str,
    write_total: Duration,
    /// Peak WAL size observed during the workload (sampled every
    /// `KEYS / SAMPLES` writes). The whole point of the bg
    /// checkpointer is keeping this bounded — `bg disabled` lets
    /// it grow monotonically, `bg enabled` keeps it small.
    peak_wal: u64,
    /// WAL size right before tree drop (no foreground checkpoint
    /// — we let the bg thread (or its absence) decide what's
    /// durable).
    final_wal: u64,
    /// Backend data file size after drop (bg's Drop runs a final
    /// round so this should always be populated).
    final_data: u64,
    /// `Tree::open` time on a fresh handle pointing at the same
    /// directory. WAL replay dominates this for `bg disabled`
    /// runs that left a big WAL behind.
    reopen: Duration,
}

fn pretty_bytes(b: u64) -> String {
    let kib = b as f64 / 1024.0;
    if kib < 1024.0 {
        format!("{kib:.1} KiB")
    } else {
        format!("{:.2} MiB", kib / 1024.0)
    }
}

fn print_results(results: &[Run]) {
    println!(
        "\n=== Background checkpointer impact ({KEYS} keys × 64 B value, persistent) ===\n"
    );
    println!(
        "{:<22}  {:>11}  {:>10}  {:>10}  {:>11}  {:>9}",
        "config", "write_total", "peak_wal", "final_wal", "data_file", "reopen"
    );
    println!("{}", "-".repeat(86));
    for r in results {
        println!(
            "{:<22}  {:>10.1?}  {:>10}  {:>10}  {:>11}  {:>8.1?}",
            r.label,
            r.write_total,
            pretty_bytes(r.peak_wal),
            pretty_bytes(r.final_wal),
            pretty_bytes(r.final_data),
            r.reopen,
        );
    }
    println!();
}

fn run_workload(label: &'static str, dir_path: PathBuf, cfg: TreeConfig) -> Run {
    const SAMPLES: u32 = 10;
    let sample_every = KEYS / SAMPLES;

    // ---- write phase, with periodic WAL-size sampling. We do
    //      NOT call tree.checkpoint() — the whole point is to see
    //      what the WAL looks like when only the bg thread (or
    //      its absence) is managing durability.
    let tree = Tree::open(cfg.clone()).expect("open");
    let mut peak_wal = 0u64;
    let t0 = Instant::now();
    for i in 0..KEYS {
        let k = format!("bench/{i:06}");
        let v = vec![0xAB_u8; 64];
        tree.put(k.as_bytes(), &v).expect("put");
        if i > 0 && i % sample_every == 0 {
            peak_wal = peak_wal.max(wal_size(&dir_path));
        }
    }
    let write_total = t0.elapsed();
    peak_wal = peak_wal.max(wal_size(&dir_path));

    // Let the bg checkpointer (if any) settle — one idle interval
    // is enough to pick up the last writes.
    std::thread::sleep(Duration::from_millis(300));
    peak_wal = peak_wal.max(wal_size(&dir_path));

    let final_wal = wal_size(&dir_path);

    // Drop the tree — for bg-enabled, this runs Checkpointer::Drop
    // which does a final synchronous round.
    drop(tree);

    let final_data = data_size(&dir_path);

    // ---- reopen (WAL replay drives this for bg-disabled runs)
    let t2 = Instant::now();
    let tree = Tree::open(cfg).expect("reopen");
    let reopen = t2.elapsed();

    let probe = format!("bench/{:06}", KEYS / 2);
    let got = tree.get(probe.as_bytes()).unwrap();
    assert_eq!(
        got.as_deref(),
        Some(&[0xAB_u8; 64][..]),
        "{label}: reopened tree missing probe key"
    );

    Run {
        label,
        write_total,
        peak_wal,
        final_wal,
        final_data,
        reopen,
    }
}

fn run_paced_workload(
    label: &'static str,
    dir_path: PathBuf,
    cfg: TreeConfig,
    pause_every: u32,
    pause_for: Duration,
) -> Run {
    // Same as run_workload but pauses every `pause_every` writes
    // for `pause_for` — gives the bg thread a chance to run a
    // round between bursts, exposing the steady-state WAL
    // boundedness instead of just the Drop-time cleanup.

    let tree = Tree::open(cfg.clone()).expect("open");
    let mut peak_wal = 0u64;
    let t0 = Instant::now();
    for i in 0..KEYS {
        let k = format!("paced/{i:06}");
        let v = vec![0xAB_u8; 64];
        tree.put(k.as_bytes(), &v).expect("put");
        if i > 0 && i % pause_every == 0 {
            peak_wal = peak_wal.max(wal_size(&dir_path));
            std::thread::sleep(pause_for);
            peak_wal = peak_wal.max(wal_size(&dir_path));
        }
    }
    let write_total = t0.elapsed();
    peak_wal = peak_wal.max(wal_size(&dir_path));

    std::thread::sleep(Duration::from_millis(300));
    peak_wal = peak_wal.max(wal_size(&dir_path));

    let final_wal = wal_size(&dir_path);
    drop(tree);
    let final_data = data_size(&dir_path);

    let t2 = Instant::now();
    let tree = Tree::open(cfg).expect("reopen");
    let reopen = t2.elapsed();

    let probe = format!("paced/{:06}", KEYS / 2);
    assert_eq!(
        tree.get(probe.as_bytes()).unwrap().as_deref(),
        Some(&[0xAB_u8; 64][..]),
    );

    Run {
        label,
        write_total,
        peak_wal,
        final_wal,
        final_data,
        reopen,
    }
}

#[test]
fn bg_checkpointer_bounds_wal_under_paced_writes() {
    // Pause 100 ms every 500 writes — that's ~10 bursts spread
    // over ~1 s, giving bg's 200 ms idle interval ~5 round
    // opportunities to keep the WAL bounded.

    let mut results = Vec::new();
    for (label, mk_cfg) in [
        (
            "bg disabled (default)",
            (|p: &std::path::Path| TreeConfig::new(p)) as fn(&std::path::Path) -> TreeConfig,
        ),
        ("bg enabled (default)", |p: &std::path::Path| {
            let mut c = TreeConfig::new(p);
            c.checkpoint = CheckpointConfig::enabled();
            c
        }),
    ] {
        let dir = TempDir::new().unwrap();
        results.push(run_paced_workload(
            label,
            dir.path().into(),
            mk_cfg(dir.path()),
            500,
            Duration::from_millis(100),
        ));
        drop(dir);
    }

    println!(
        "\n=== Paced workload ({KEYS} keys × 64 B, 100 ms pause every 500) ===\n"
    );
    println!(
        "{:<22}  {:>11}  {:>10}  {:>10}  {:>9}",
        "config", "write_total", "peak_wal", "final_wal", "reopen"
    );
    println!("{}", "-".repeat(72));
    for r in &results {
        println!(
            "{:<22}  {:>10.1?}  {:>10}  {:>10}  {:>8.1?}",
            r.label,
            r.write_total,
            pretty_bytes(r.peak_wal),
            pretty_bytes(r.final_wal),
            r.reopen,
        );
    }
    println!();

    // Under paced writes, bg's 200 ms idle interval lands inside
    // some pauses and truncates the WAL. We don't pin a specific
    // ratio (round timing depends on whether the round happens
    // to start during a pause or during a burst), but bg-enabled
    // should leave the WAL strictly smaller at peak than
    // disabled, and the final_wal must be ≈ 0.
    assert!(
        results[1].peak_wal < results[0].peak_wal,
        "expected paced bg-enabled to bound peak WAL below disabled: \
         disabled={}, bg_default={}",
        pretty_bytes(results[0].peak_wal),
        pretty_bytes(results[1].peak_wal),
    );
    assert!(
        results[1].final_wal <= 1024,
        "expected bg-enabled to drop WAL via Drop-time round, got {}",
        pretty_bytes(results[1].final_wal),
    );
}

#[test]
fn bg_checkpointer_keeps_wal_bounded_and_speeds_reopen() {
    let mut results = Vec::new();

    // 1. baseline — bg disabled (default)
    {
        let dir = TempDir::new().unwrap();
        let cfg = TreeConfig::new(dir.path());
        results.push(run_workload("bg disabled (default)", dir.path().into(), cfg));
        drop(dir);
    }

    // 2. bg enabled, default cadence
    {
        let dir = TempDir::new().unwrap();
        let mut cfg = TreeConfig::new(dir.path());
        cfg.checkpoint = CheckpointConfig::enabled();
        results.push(run_workload(
            "bg enabled (default)",
            dir.path().into(),
            cfg,
        ));
        drop(dir);
    }

    // 3. bg enabled, aggressive cadence (50ms idle)
    {
        let dir = TempDir::new().unwrap();
        let mut cfg = TreeConfig::new(dir.path());
        cfg.checkpoint = CheckpointConfig {
            idle_interval: Duration::from_millis(50),
            ..CheckpointConfig::enabled()
        };
        results.push(run_workload(
            "bg enabled (50ms idle)",
            dir.path().into(),
            cfg,
        ));
        drop(dir);
    }

    print_results(&results);

    // Correctness asserts.
    for r in &results {
        assert!(
            r.final_data > 0,
            "{}: data file is empty after drop",
            r.label
        );
        assert!(
            r.reopen < Duration::from_secs(5),
            "{}: reopen too slow ({:?})",
            r.label,
            r.reopen,
        );
    }

    // Directional asserts only — magnitudes vary per machine but
    // the bg-enabled run must (a) truncate the WAL on Drop and
    // (b) replay strictly less than the bg-disabled run.
    let disabled = &results[0];
    let bg_default = &results[1];
    assert!(
        bg_default.final_wal <= 1024,
        "bg-enabled should truncate WAL on Drop, got final_wal={}",
        pretty_bytes(bg_default.final_wal),
    );
    assert!(
        disabled.final_wal > 100 * 1024,
        "bg-disabled run should leave a multi-KiB WAL behind, got {}",
        pretty_bytes(disabled.final_wal),
    );
    assert!(
        bg_default.reopen < disabled.reopen,
        "expected bg-enabled reopen to be strictly faster: \
         disabled={:?}, bg_default={:?}",
        disabled.reopen,
        bg_default.reopen,
    );
}
