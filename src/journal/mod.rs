//! Journal — physiological WAL + replay.
//!
//! Layered design:
//!
//! - [`txn_op`] — the `TxnOp` variant union; one variant per
//!   walker-level mutation kind (`Insert`, `Erase`, `Split`,
//!   `Merge`, `Compact`, two `Rename` flavours, `NewTree`,
//!   `RmTree`, `MemMarker`, `Batch`).
//! - [`codec`] — binary record codec + file header. Pure
//!   in-memory bytes ↔ `TxnOp`.
//! - [`writer`] — append-only WAL file with
//!   `sync_data`-on-flush durability + 64 KB buffered auto-drain
//!   mechanics.
//! - [`group_commit`] — dedicated append worker; foreground
//!   writers enqueue encoded records and durable waiters share
//!   one `sync_data` per short batch window.
//! - [`reader`] — forward replay scanner with graceful
//!   torn-tail handling. Unpacks `Batch` records into per-inner
//!   callbacks so consumers don't need a `Batch` arm.
//!
//! Checkpoint (flush WAL → drain dirty → fdatasync → truncate
//! WAL) lives in [`crate::Tree::checkpoint`] and the background
//! [`crate::checkpoint`] module, not in here — it straddles the
//! tree + journal boundary.

pub mod codec;
pub(crate) mod group_commit;
pub mod reader;
pub mod txn_op;
pub mod writer;

#[cfg(test)]
mod tests;
