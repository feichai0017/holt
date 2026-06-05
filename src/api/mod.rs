//! Public API surface — `Tree`, `DB`, atomic batches, `Record`,
//! `RecordVersion`, scoped read [`view`]s, path-shaped key helpers,
//! record/key range iterators, `TreeBuilder`, plus the curated
//! [`stats`] module.
//!
//! This module is what users will write `use holt::{...}` for.

pub mod atomic;
pub mod builder;
pub mod config;
pub mod db;
pub mod errors;
pub mod key;
pub mod snapshot;
pub mod stats;
pub mod tree;
pub mod view;
