//! # artisan — adaptive radix tree metadata storage engine
//!
//! `artisan` is an embedded Rust library that stores **path-shaped
//! metadata** with sub-microsecond lookups, per-blob concurrency,
//! and crash-safe persistence. It is built around an Adaptive
//! Radix Tree that spans multiple 512 KB blob frames.
//!
//! See `README.md` for the elevator pitch and `ARCHITECTURE.md`
//! for the deep dive.
//!
//! ## Current status
//!
//! Early development. The crate skeleton is in place; the layout
//! layer (extern-struct types + slot encoding + 4 KB blob header)
//! is complete. The walker / persistence / journal layers are
//! being built out. See `ROADMAP.md`.
//!
//! ## Quick taste (when v0.1 ships)
//!
//! ```ignore
//! use artisan::TreeBuilder;
//!
//! let tree = TreeBuilder::new("/var/lib/myapp/meta.artisan").open()?;
//! tree.put(b"img/01.jpg", b"rgb_data")?;
//! let v = tree.get(b"img/01.jpg")?.unwrap();
//! for entry in tree.range(b"img/").take(10) {
//!     println!("{} -> {}", entry.key_str(), entry.value_str());
//! }
//! # Ok::<(), artisan::Error>(())
//! ```
//!
//! ## Module map
//!
//! - [`layout`] — extern struct layouts (BlobHeader, SlotEntry,
//!   per-NodeType bodies). Each struct has a
//!   `const _: () = assert!(...)` that pins its size + offsets.
//! - [`concurrency`] — `HybridLatch` 3-mode lock + guards.
//! - [`store`] — `BlobFrame` (single-blob ops) + backend trait.
//! - [`engine`] — recursive walker (insert / lookup / erase /
//!   scan / rename / compact).
//! - [`journal`] — WAL + replay + checkpoint.
//! - [`api`] — high-level `Tree` / `Txn` / `Iter` surface.

#![doc(html_no_source)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod layout;
pub mod concurrency;
pub mod store;
pub mod engine;
pub mod journal;
pub mod api;

mod prelude_private {
    // Internal helpers shared across modules without exposing
    // them in the public API. Empty for now.
}

// -- Top-level re-exports -----------------------------------------

pub use api::config::{Storage, TreeConfig};
pub use api::errors::{Error, Result};

pub use api::tree::Tree;
pub use api::builder::TreeBuilder;
pub use store::backend::{AlignedBlobBuf, Backend, MemoryBackend, PersistentBackend};
