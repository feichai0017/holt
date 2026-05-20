//! Prometheus text-format renderer for [`TreeStats`](crate::TreeStats).
//!
//! Enabled via the `metrics` feature flag. Pure Rust — no
//! `prometheus` / `metrics` crate dependency. The caller hosts
//! its own HTTP `/metrics` endpoint (axum, hyper, warp, …) and
//! invokes [`render_prometheus`](crate::metrics::render_prometheus) inside the handler.
//!
//! ```ignore
//! use holt::{Tree, TreeConfig};
//! use holt::metrics::render_prometheus;
//!
//! let tree = Tree::open(TreeConfig::memory()).unwrap();
//! let stats = tree.stats().unwrap();
//! let body = render_prometheus(&stats);
//! println!("{body}");
//! ```
//!
//! ## Metrics emitted
//!
//! All metric names are prefixed with `holt_` and follow the
//! [Prometheus naming convention][1]:
//!
//! - **`_total` suffix**: monotonically-non-decreasing counters,
//!   never reset across a process's lifetime.
//! - **`_bytes` suffix**: gauges measured in bytes.
//! - **No suffix**: arbitrary gauges (instantaneous values that
//!   can go up or down).
//!
//! [1]: https://prometheus.io/docs/practices/naming/
//!
//! | Metric                                  | Type    | Source field |
//! | --------------------------------------- | ------- | ------------ |
//! | `holt_blob_count`                       | gauge   | `TreeStats::blob_count`                |
//! | `holt_space_used_bytes`                 | gauge   | `TreeStats::total_space_used`          |
//! | `holt_gap_space_bytes`                  | gauge   | `TreeStats::total_gap_space`           |
//! | `holt_slots`                            | gauge   | `TreeStats::total_slots`               |
//! | `holt_compactions`                      | gauge   | `TreeStats::total_compactions` (sum of `compact_times` across **currently reachable** blobs — can go DOWN if a blob is merged/deleted; it's a tree-shape metric, not a lifetime counter) |
//! | `holt_tombstones`                       | gauge   | `TreeStats::total_tombstones`          |
//! | `holt_bm_dirty_count`                   | gauge   | `TreeStats::bm_dirty_count`            |
//! | `holt_bm_pending_delete_count`          | gauge   | `TreeStats::bm_pending_delete_count`   |
//! | `holt_bm_cache_hits_total`              | counter | `TreeStats::bm_cache_hits`             |
//! | `holt_bm_cache_misses_total`            | counter | `TreeStats::bm_cache_misses`           |
//! | `holt_bm_optimistic_restarts_total`     | counter | `TreeStats::bm_optimistic_restarts`    |
//! | `holt_bm_walker_ops_total`              | counter | `TreeStats::bm_walker_ops`             |
//! | `holt_bm_walker_blob_hops_total`        | counter | `TreeStats::bm_walker_blob_hops`       |
//! | `holt_bm_avg_blob_hops`                 | gauge   | `TreeStats::bm_avg_blob_hops()`        |
//! | `holt_bm_max_blob_hops`                 | gauge   | `TreeStats::bm_max_blob_hops`          |
//! | `holt_bm_max_cross_blob_depth`          | gauge   | `TreeStats::bm_max_cross_blob_depth`   |
//! | `holt_bm_spillovers_total`              | counter | `TreeStats::bm_spillovers`             |
//! | `holt_bm_merges_total`                  | counter | `TreeStats::bm_merges`                 |
//! | `holt_journal_appends_total`             | counter | `JournalStats::appends`                |
//! | `holt_journal_batches_total`             | counter | `JournalStats::batches`                |
//! | `holt_journal_syncs_total`               | counter | `JournalStats::syncs`                  |
//! | `holt_checkpoint_rounds_attempted_total`| counter | `CheckpointerStats::rounds_attempted`  |
//! | `holt_checkpoint_rounds_succeeded_total`| counter | `CheckpointerStats::rounds_succeeded`  |
//! | `holt_checkpoint_blobs_flushed_total`   | counter | `CheckpointerStats::blobs_flushed`     |
//! | `holt_checkpoint_merges_total`          | counter | `CheckpointerStats::merges_total`      |
//! | `holt_checkpoint_truncates_total`       | counter | `CheckpointerStats::truncates`         |
//! | `holt_checkpoint_evictions_total`       | counter | `CheckpointerStats::evictions`         |
//!
//! `JournalStats` and `CheckpointerStats` lines are emitted only
//! when the corresponding worker exists. The journal worker exists
//! for persistent trees opened through `Tree::open`; the background
//! checkpointer exists when `TreeStats::checkpointer` is `Some`.

use std::fmt::Write;

use crate::api::stats::TreeStats;

/// Render `stats` as a Prometheus text-format payload.
///
/// The output is one HELP + TYPE + sample line per metric,
/// terminated by a `\n`. Suitable as the body of an HTTP 200
/// response with `Content-Type: text/plain; version=0.0.4`.
#[allow(clippy::too_many_lines)] // one `metric(...)` call per emit — splitting hides the export shape
#[must_use]
pub fn render_prometheus(stats: &TreeStats) -> String {
    // Pre-size for the typical payload (~2.5 KB) to avoid the
    // first few `String::push_str` reallocations.
    let mut out = String::with_capacity(2560);

    metric(
        &mut out,
        "holt_blob_count",
        "Number of distinct blobs reachable from the tree root.",
        "gauge",
        u64::from(stats.blob_count),
    );
    metric(
        &mut out,
        "holt_space_used_bytes",
        "Sum of `space_used` across every blob (live extent bytes).",
        "gauge",
        stats.total_space_used,
    );
    metric(
        &mut out,
        "holt_gap_space_bytes",
        "Sum of `gap_space` across every blob (reclaimable on compact).",
        "gauge",
        stats.total_gap_space,
    );
    metric(
        &mut out,
        "holt_slots",
        "Sum of `num_slots` across every reachable blob.",
        "gauge",
        stats.total_slots,
    );
    metric(
        &mut out,
        "holt_compactions",
        "Sum of `compact_times` across currently-reachable blobs. \
         A gauge, not a counter — a blob that merges into its \
         parent (or is deleted) takes its `compact_times` with \
         it, so this can go down.",
        "gauge",
        stats.total_compactions,
    );
    metric(
        &mut out,
        "holt_tombstones",
        "Sum of `tombstone_leaf_cnt` across every reachable blob.",
        "gauge",
        stats.total_tombstones,
    );
    metric(
        &mut out,
        "holt_bm_dirty_count",
        "Number of blobs in the buffer manager dirty set.",
        "gauge",
        stats.bm_dirty_count as u64,
    );
    metric(
        &mut out,
        "holt_bm_pending_delete_count",
        "Number of blobs queued for deferred backend deletion.",
        "gauge",
        stats.bm_pending_delete_count as u64,
    );
    metric(
        &mut out,
        "holt_bm_cache_hits_total",
        "Cumulative buffer-manager cache hits.",
        "counter",
        stats.bm_cache_hits,
    );
    metric(
        &mut out,
        "holt_bm_cache_misses_total",
        "Cumulative buffer-manager cache misses (fell through to backend).",
        "counter",
        stats.bm_cache_misses,
    );
    metric(
        &mut out,
        "holt_bm_optimistic_restarts_total",
        "Cumulative wait-free read restarts (concurrent writer lapped snapshot).",
        "counter",
        stats.bm_optimistic_restarts,
    );
    metric(
        &mut out,
        "holt_bm_walker_ops_total",
        "Cumulative mutation walker invocations.",
        "counter",
        stats.bm_walker_ops,
    );
    metric(
        &mut out,
        "holt_bm_walker_blob_hops_total",
        "Total blob hops across mutation walkers.",
        "counter",
        stats.bm_walker_blob_hops,
    );
    metric_f64(
        &mut out,
        "holt_bm_avg_blob_hops",
        "Average blob hops per mutation walker invocation.",
        "gauge",
        stats.bm_avg_blob_hops(),
    );
    metric(
        &mut out,
        "holt_bm_max_blob_hops",
        "Maximum blob hops observed for one mutation walker call.",
        "gauge",
        stats.bm_max_blob_hops,
    );
    metric(
        &mut out,
        "holt_bm_max_cross_blob_depth",
        "Largest key-depth at which a mutation walker entered a blob.",
        "gauge",
        stats.bm_max_cross_blob_depth,
    );
    metric(
        &mut out,
        "holt_bm_spillovers_total",
        "Successful foreground spillover events.",
        "counter",
        stats.bm_spillovers,
    );
    metric(
        &mut out,
        "holt_bm_merges_total",
        "BlobNode children folded back into parents by compact or merge passes.",
        "counter",
        stats.bm_merges,
    );

    if let Some(journal) = &stats.journal {
        metric(
            &mut out,
            "holt_journal_appends_total",
            "WAL append requests submitted to the journal worker.",
            "counter",
            journal.appends,
        );
        metric(
            &mut out,
            "holt_journal_batches_total",
            "Append batches processed by the journal worker.",
            "counter",
            journal.batches,
        );
        metric(
            &mut out,
            "holt_journal_syncs_total",
            "WAL sync_data calls issued by the journal worker.",
            "counter",
            journal.syncs,
        );
    }

    if let Some(ck) = &stats.checkpointer {
        metric(
            &mut out,
            "holt_checkpoint_rounds_attempted_total",
            "Background checkpoint rounds the planner started.",
            "counter",
            ck.rounds_attempted,
        );
        metric(
            &mut out,
            "holt_checkpoint_rounds_succeeded_total",
            "Background checkpoint rounds that completed without error.",
            "counter",
            ck.rounds_succeeded,
        );
        metric(
            &mut out,
            "holt_checkpoint_blobs_flushed_total",
            "Blobs the checkpointer's I/O worker wrote through to backend.",
            "counter",
            ck.blobs_flushed,
        );
        metric(
            &mut out,
            "holt_checkpoint_merges_total",
            "Cross-blob `BlobNode` crossings folded back into a parent.",
            "counter",
            ck.merges_total,
        );
        metric(
            &mut out,
            "holt_checkpoint_truncates_total",
            "WAL truncations performed at the round's truncate gate.",
            "counter",
            ck.truncates,
        );
        metric(
            &mut out,
            "holt_checkpoint_evictions_total",
            "Cache entries the eviction thread dropped.",
            "counter",
            ck.evictions,
        );
    }
    out
}

#[inline]
fn metric(out: &mut String, name: &str, help: &str, ty: &str, value: u64) {
    // `# HELP <name> <help>\n# TYPE <name> <ty>\n<name> <value>\n`
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {ty}");
    let _ = writeln!(out, "{name} {value}");
}

#[inline]
fn metric_f64(out: &mut String, name: &str, help: &str, ty: &str, value: f64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {ty}");
    let _ = writeln!(out, "{name} {value:.6}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::stats::{CheckpointerStats, JournalStats, TreeStats};

    fn stats_fixture(with_journal: bool, with_checkpointer: bool) -> TreeStats {
        TreeStats {
            blob_count: 3,
            total_space_used: 1024,
            total_gap_space: 256,
            total_slots: 42,
            total_compactions: 7,
            total_tombstones: 5,
            blobs: Vec::new(),
            bm_dirty_count: 2,
            bm_pending_delete_count: 1,
            bm_cache_hits: 1_000,
            bm_cache_misses: 25,
            bm_optimistic_restarts: 3,
            bm_walker_ops: 4,
            bm_walker_blob_hops: 10,
            bm_max_blob_hops: 3,
            bm_max_cross_blob_depth: 17,
            bm_spillovers: 2,
            bm_merges: 1,
            journal: with_journal.then_some(JournalStats {
                appends: 20,
                batches: 5,
                syncs: 4,
            }),
            checkpointer: with_checkpointer.then_some(CheckpointerStats {
                rounds_attempted: 11,
                rounds_succeeded: 10,
                blobs_flushed: 30,
                merges_total: 4,
                truncates: 8,
                evictions: 17,
            }),
        }
    }

    #[test]
    fn renders_core_metrics_for_stats_without_checkpointer() {
        let out = render_prometheus(&stats_fixture(false, false));
        assert!(out.contains("# HELP holt_blob_count "));
        assert!(out.contains("# TYPE holt_blob_count gauge\n"));
        assert!(out.contains("holt_blob_count 3\n"));
        // Monotonic counters keep the `_total` suffix...
        assert!(out.contains("holt_bm_cache_hits_total 1000\n"));
        assert!(out.contains("holt_bm_optimistic_restarts_total 3\n"));
        assert!(out.contains("holt_bm_walker_ops_total 4\n"));
        assert!(out.contains("holt_bm_avg_blob_hops 2.500000\n"));
        assert!(out.contains("holt_bm_spillovers_total 2\n"));
        // ...non-monotonic gauges (sum-over-reachable-blobs) drop it.
        assert!(out.contains("# TYPE holt_slots gauge\n"));
        assert!(out.contains("holt_slots 42\n"));
        assert!(out.contains("# TYPE holt_compactions gauge\n"));
        assert!(out.contains("holt_compactions 7\n"));
        assert!(out.contains("# TYPE holt_tombstones gauge\n"));
        assert!(out.contains("holt_tombstones 5\n"));
        // None of the gauges leak `_total`.
        assert!(!out.contains("holt_slots_total"));
        assert!(!out.contains("holt_compactions_total"));
        assert!(!out.contains("holt_tombstones_total"));
        // No checkpointer block.
        assert!(!out.contains("holt_checkpoint_"));
        assert!(!out.contains("holt_journal_"));
    }

    #[test]
    fn renders_journal_block_when_present() {
        let out = render_prometheus(&stats_fixture(true, false));
        assert!(out.contains("holt_journal_appends_total 20\n"));
        assert!(out.contains("holt_journal_batches_total 5\n"));
        assert!(out.contains("holt_journal_syncs_total 4\n"));
    }

    #[test]
    fn renders_checkpoint_block_when_present() {
        let out = render_prometheus(&stats_fixture(false, true));
        assert!(out.contains("holt_checkpoint_rounds_attempted_total 11\n"));
        assert!(out.contains("holt_checkpoint_blobs_flushed_total 30\n"));
        assert!(out.contains("holt_checkpoint_evictions_total 17\n"));
    }

    #[test]
    fn output_ends_with_newline() {
        let out = render_prometheus(&stats_fixture(false, false));
        assert!(out.ends_with('\n'), "Prometheus expects a trailing newline");
    }
}
