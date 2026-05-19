---
name: Bug report
about: Something in holt isn't behaving like its docs / API claim
title: "bug: "
labels: ["bug"]
---

## What happened

<!-- One or two sentences. What did you expect, what did you see? -->

## Reproducer

<!-- The shortest code that triggers it. A failing #[test] is
     ideal — paste below or link to a branch. -->

```rust
// minimal repro
```

## Environment

- `holt` version (`cargo tree -p holt`):
- `rustc --version --verbose`:
- Platform (`uname -srm`):
- Storage mode (`TreeConfig::memory()` / `::new(path)`):
- Non-default config diff (`buffer_pool_size`, `wal_sync_on_commit`, …):

## Logs / panic / backtrace

<!-- `RUST_BACKTRACE=1` output if it's a panic. Anything weird
     from the WAL / replay path is especially welcome. -->

```text

```

## Anything else

<!-- Workload characteristics: number of keys, key shape (random
     vs path), whether the bug appeared after a checkpoint /
     rename / etc. -->
