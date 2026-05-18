//! Journal — physiological WAL, replay, checkpoint.
//!
//! Layered design:
//!
//! - [`txn_op`] — the `TxnOp` variant union; one variant per
//!   walker-level mutation kind (`Insert`, `Erase`, `Split`,
//!   `Merge`, `Compact`, two `Rename` flavours, `NewTree`,
//!   `RmTree`, `MemMarker`).
//! - [`codec`] — binary record codec (Stage 5a). Encodes /
//!   decodes a single `TxnOp` to / from a length-prefixed,
//!   CRC32-stamped record. Pure in-memory, no I/O. See its
//!   module docs for the exact on-disk record layout.
//! - `writer` / `reader` (Stage 5b — queued) — append-only WAL
//!   file with `fdatasync`-on-flush, plus a forward scanner
//!   that resyncs after a torn tail and yields records to a
//!   replay callback.
//! - [`checkpoint`] (Stage 5c — queued) — trim the log past the
//!   last durable blob commit.

pub mod checkpoint;
pub mod codec;
pub mod txn_op;
