//! Per-scan [`ScanStats`] — visited / returned / rollup / restarts.

use holt::{KeyRangeEntry, Tree, TreeConfig};

#[test]
fn stats_count_returned_and_visited() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    for i in 0..50u32 {
        tree.put(format!("d/{i:04}").as_bytes(), b"v").unwrap();
    }
    let mut iter = tree.scan(b"d/").into_iter();
    let mut n = 0;
    for entry in &mut iter {
        entry.unwrap();
        n += 1;
    }
    let stats = iter.stats();
    assert_eq!(n, 50);
    assert_eq!(stats.returned, 50);
    assert_eq!(stats.visited, 50); // each live leaf examined once, no skips
    assert_eq!(stats.rollup, 0);
    assert_eq!(stats.restarts, 0);
}

#[test]
fn stats_count_rollups_under_delimiter() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    for d in 0..3u32 {
        for f in 0..10u32 {
            tree.put(format!("dir{d}/f{f}").as_bytes(), b"v").unwrap();
        }
    }
    // delimiter '/' folds each dirN/ subtree into one CommonPrefix.
    let mut iter = tree.range_keys().delimiter(b'/').into_iter();
    let mut rollups = 0;
    for entry in &mut iter {
        if let KeyRangeEntry::CommonPrefix(_) = entry.unwrap() {
            rollups += 1;
        }
    }
    let stats = iter.stats();
    assert_eq!(rollups, 3);
    assert_eq!(stats.rollup, 3);
    assert_eq!(stats.returned, 0); // every leaf folded away
    // Invariant: each emission is an examined unit (here every dirN/
    // subtree folds at its inner node, so visited == rollup).
    assert!(stats.visited >= stats.returned + stats.rollup);
}

#[test]
fn visit_terminal_returns_stats() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    for i in 0..20u32 {
        tree.put(format!("k{i:04}").as_bytes(), b"v").unwrap();
    }
    let mut seen = 0;
    let stats = tree
        .scan_keys(b"k")
        .visit(100, |_| {
            seen += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(seen, 20);
    assert_eq!(stats.returned, 20);
    assert_eq!(stats.visited, 20);
    assert_eq!(stats.restarts, 0);
}

#[test]
fn cache_hit_reports_zero_visited() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    for i in 0..8u32 {
        tree.put(format!("p/{i}").as_bytes(), b"v").unwrap();
    }
    // First visit walks the tree and populates the prefix-list cache.
    let first = tree.scan_keys(b"p/").visit(16, |_| Ok(())).unwrap();
    assert!(first.visited > 0);
    assert_eq!(first.returned, 8);
    // Second identical visit (no writes between) is served from cache —
    // same entries, but visited == 0 because nothing was walked.
    let second = tree.scan_keys(b"p/").visit(16, |_| Ok(())).unwrap();
    assert_eq!(second.returned, 8);
    assert_eq!(second.visited, 0);
    assert_eq!(second.restarts, 0);
}
