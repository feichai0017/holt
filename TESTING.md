# Testing Matrix

Holt uses four validation layers. They are intentionally split by cost:
fast checks stay in PR CI, long durability campaigns run in nightly or
release-gate jobs.

## PR CI

Run on every push and pull request:

| Layer | Command | Coverage |
| --- | --- | --- |
| Unit/integration | `cargo test --workspace --all-features --lib --tests --examples --locked` | API semantics, walker behavior, WAL replay, checkpoint recovery, view/range behavior, atomic batches |
| Doctests/examples | `cargo test --workspace --all-features --doc --locked`; example binaries | Public API examples stay buildable |
| Lint | `cargo fmt --all --check`; `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | Formatting and warning-free code |
| Fuzz smoke | `cargo +nightly fuzz run atomic_model -- -runs=512`; `cargo +nightly fuzz run db_model -- -runs=256` | Keeps the fuzz targets building and exercises random API traces |
| Soak smoke | `tools/soak --mode normal`; `tools/soak --mode db-normal` with short runs | Keeps the lifecycle harness buildable and catches obvious reopen regressions |
| Coverage | `cargo llvm-cov ...` | Prevents accidental coverage drops |
| Regression bench | `benches/ regression` | Catches large local performance regressions without comparator dependencies |

## Nightly Validation

`.github/workflows/nightly.yml` runs the expensive matrix on `main`.

| Job | Coverage |
| --- | --- |
| `fault-matrix` | Checkpoint write/delete/flush failpoints and WAL integration tests |
| `property-matrix` | Higher-count proptest oracle runs |
| `soak-normal` | Multi-thread lifecycle soak across async WAL, sync WAL, and constrained buffer-pool cases |
| `soak-db-normal` | Multi-thread named-tree DB lifecycle soak with cross-tree atomic batches, DB views, checkpoint, and reopen |
| `soak-crash` | Repeated `SIGKILL` with sync WAL (`Durability::Wal { sync: true }`); every acknowledged write must survive reopen |
| `soak-db-crash` | Repeated `SIGKILL` with sync WAL; every acknowledged cross-tree DB atomic transaction must survive replay as a whole |
| `fuzz-long` | Time-bounded libFuzzer campaigns over the single-tree and multi-tree DB models |
| `verified-model` | Manual Verus run for ART shape specs when a Verus binary is available |

## Fuzz Model

`fuzz/fuzz_targets/atomic_model.rs` compares one `Tree` against a
`BTreeMap` oracle. The model covers:

- `put`, `delete`, `get`;
- `checkpoint` and `reopen` with `Durability::Wal { sync: true }`;
- `atomic` batches with create-only, compare-and-put, versioned delete,
  version assertions, prefix-empty assertions, and rename;
- record range scans and prefix scans;
- delimiter rollup / `CommonPrefix` behavior;
- key-only scans through both owned iteration and borrowed visitor paths;
- scoped `View` snapshots and view-local key scans.

Long campaigns should preserve the minimized corpus under
`fuzz/corpus/atomic_model` only when the corpus entry represents a
useful new edge case.

`fuzz/fuzz_targets/db_model.rs` compares `DB` against a catalog plus
per-tree `BTreeMap` oracle. The model covers:

- named tree create/drop/list/open semantics;
- dropped-tree fencing across checkpoint and reopen;
- cross-tree `DB::atomic` batches;
- per-tree point operations through DB-opened tree handles;
- DB-level scoped `View` snapshots;
- record scans, delimiter key scans, checkpoint, and WAL replay.

## Soak Harness

`tools/soak` is the long-lifecycle validator.

Normal mode checks single-tree multi-threaded mixed operations,
checkpoint, reopen, and final oracle equality. `db-normal` repeats the
lifecycle check through named trees, cross-tree atomic batches, and DB
views. Crash modes check durability only for writes or transactions that
Holt acknowledged and the child recorded in the fsynced ack log.

Recommended release gate:

```sh
cargo run --manifest-path tools/soak/Cargo.toml --locked -- \
  --mode normal --dir target/holt-soak-release --reset \
  --duration-secs 21600 --keys 10000000 --ops 20000000 \
  --threads 8 --buffer-pool 256 --wal-sync false

cargo run --manifest-path tools/soak/Cargo.toml --locked -- \
  --mode db-normal --dir target/holt-soak-db-release --reset \
  --duration-secs 21600 --keys 1000000 --ops 5000000 \
  --threads 8 --buffer-pool 256 --wal-sync false

cargo run --manifest-path tools/soak/Cargo.toml --locked -- \
  --mode crash --dir target/holt-soak-crash-release --reset \
  --duration-secs 21600 --keys 100000 --ops 2000000 \
  --buffer-pool 64 --wal-sync true \
  --kill-min-ms 50 --kill-max-ms 5000

cargo run --manifest-path tools/soak/Cargo.toml --locked -- \
  --mode db-crash --dir target/holt-soak-db-crash-release --reset \
  --duration-secs 21600 --keys 100000 --ops 2000000 \
  --buffer-pool 64 --wal-sync true \
  --kill-min-ms 50 --kill-max-ms 5000
```

## Verified Model

`verified/` contains Verus specs for local ART invariants:

- Node4/16/48/256 capacity;
- grow/shrink capacity preservation;
- sorted child lookup and live-child guarantees;
- insert/remove arity preservation;
- prefix split branch shape;
- delimiter rollup bounds;
- virtual user-key terminator;
- leaf extent alignment.
- DB catalog state transitions for create/drop/finalize visibility;
- DB tree id allocation monotonicity and reserved catalog-id skipping.

Run it manually:

```sh
VERUS=/path/to/verus ./verified/verify.sh
```

The Verus model is not a proof of the entire Rust implementation. It
documents and checks the small structural invariants that the production
layout and walker code depend on.

## Known Gaps

These are not fully automated yet:

- disk-full / `ENOSPC`;
- permission changes and directory removal during operation;
- systematic manifest/blob bit-flip corruption injection;
- multi-process open contention;
- ThreadSanitizer or Loom-style concurrency model checking;
- long io_uring crash/soak on a real Linux NVMe host.
