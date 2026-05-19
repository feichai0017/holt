//! # holt — adaptive radix tree metadata storage engine
//!
//! `holt` is an embedded Rust library that stores **path-shaped
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
//! use holt::TreeBuilder;
//!
//! let tree = TreeBuilder::new("/var/lib/myapp/meta.holt").open()?;
//! tree.put(b"img/01.jpg", b"rgb_data")?;
//! let v = tree.get(b"img/01.jpg")?.unwrap();
//! for entry in tree.range(b"img/").take(10) {
//!     println!("{} -> {}", entry.key_str(), entry.value_str());
//! }
//! # Ok::<(), holt::Error>(())
//! ```
//!
//! ## Module map
//!
//! Public modules (the supported import surface):
//!
//! - [`api`] — high-level [`Tree`] + [`TxnBatch`] +
//!   [`TreeBuilder`], plus the curated [`api::range`] /
//!   [`api::stats`] re-export modules.
//! - [`layout`] — extern struct layouts (BlobHeader, SlotEntry,
//!   per-NodeType bodies). Each struct has a
//!   `const _: () = assert!(...)` that pins its size + offsets.
//!   Exposed so advanced users can inspect on-disk frames.
//! - [`store`] — [`BufferManager`] + backend trait
//!   ([`MemoryBackend`] / [`PersistentBackend`]).
//! - [`journal`] — WAL codec + replay scanner + writer. Useful
//!   for tools that want to inspect or replay journal files.
//!
//! Internal modules (`pub(crate)`, not part of the SemVer surface):
//!
//! - `engine` — recursive walker (insert / lookup / erase /
//!   scan / rename / compact). Its public types (range iterators,
//!   stats) are re-exported via [`api::range`] / [`api::stats`].
//! - `concurrency` — `HybridLatch` 3-mode lock + guards.
//!
//! ## Platform support
//!
//! holt is **Unix-only by design**: Linux (`O_DIRECT` fast path,
//! `io_uring` on the persistent backend) and macOS (`F_NOCACHE`).
//! Windows is out of scope and the crate refuses to compile there
//! — see the platform stance in `ROADMAP.md`.

#![doc(html_no_source)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms)]
#![warn(clippy::pedantic)]
// `clippy::pedantic` opt-in is on purpose — we want the high
// signal-to-noise lints firing. The blanket allows below are
// the categories we've reviewed and judged to be either
// intentional design choices in this crate or stylistic
// preferences that don't carry their weight here.
#![allow(
    // Intentional: many `T as U` casts are guarded by upstream
    // invariants (slot < MAX_SLOTS = 10240 < u16::MAX, value
    // length already validated < u16::MAX, etc.). Replacing all
    // with `try_into().unwrap()` would be net negative.
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    // Misleading suggestion when we're explicitly choosing the
    // unchecked path for layout reasons.
    clippy::cast_ptr_alignment,
    // Many internal `Result`-returning helpers don't need an
    // `# Errors` section — the docstring already explains
    // failure modes inline.
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    // Pedantic style nits that don't affect correctness:
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value,
    clippy::similar_names,
    clippy::stable_sort_primitive,
    clippy::large_types_passed_by_value,
    clippy::struct_field_names,
    // Reserved-counter fields in `BlobHeader` (RE'd byte
    // positions) intentionally aren't read from yet; the
    // compile-time offset asserts pin them in place.
    clippy::struct_excessive_bools,
    // Docstrings already use backticks for code spans; the
    // remaining flagged identifiers (e.g. "BufferManager",
    // "BlobFrame") are prose-level references where the
    // backtick noise is net-negative.
    clippy::doc_markdown,
    // Mixing items / statements is fine in small fns; refusing
    // it forces hoisting of locally-scoped const helpers.
    clippy::items_after_statements
)]

// Hard scope gate — see the "Platform support" section in the
// module docs. Building holt for Windows is intentionally
// unsupported; the persistent backend's `O_DIRECT` / `F_NOCACHE`
// path has no Windows analog worth maintaining for this project.
#[cfg(not(unix))]
compile_error!(
    "holt is Unix-only — Linux and macOS are supported. Windows is out of scope; \
     see ROADMAP.md."
);

pub mod api;
pub mod journal;
pub mod layout;
pub mod store;

pub(crate) mod concurrency;
pub(crate) mod engine;

// -- Top-level re-exports -----------------------------------------
//
// The flat `holt::*` surface — every name a user reaches for via
// `use holt::X` lives here. Module-pathed access (e.g.
// `holt::api::stats::TreeStats`) still works for users who prefer
// it.

// Core handle + configuration.
pub use api::builder::TreeBuilder;
pub use api::config::{Storage, TreeConfig};
pub use api::errors::{Error, Result};
pub use api::tree::Tree;

// Range-scan iterator surface.
pub use api::range::{RangeBuilder, RangeEntry, RangeIter};

// Stats snapshots returned by `Tree::stats`.
pub use api::stats::{BlobStats, TreeStats};

// Single-record batched transactions.
pub use api::txn::TxnBatch;

// Backend trait + bundled backends + zero-copy blob buffer.
pub use store::backend::{AlignedBlobBuf, Backend, MemoryBackend, PersistentBackend};
pub use store::BufferManager;
