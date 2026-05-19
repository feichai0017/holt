//! Range-scan surface — the iterator types returned by
//! [`Tree::range`](crate::Tree::range) and its variants.
//!
//! The scan walker itself lives in the crate-private `engine`
//! module; this module curates the public-facing iterator types
//! so the engine can stay `pub(crate)`.

pub use crate::engine::{RangeBuilder, RangeEntry, RangeIter};
