//! Public API surface — `Tree`, `TxnBatch`, `TreeBuilder`,
//! plus the curated re-export modules ([`range`], [`stats`]).
//!
//! This module is what users will write `use holt::{...}` for.

pub mod builder;
pub mod config;
pub mod errors;
pub mod range;
pub mod stats;
pub mod tree;
pub mod txn;
