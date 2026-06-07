//! Copy-on-write frame snapshot.
//!
//! A [`Snapshot`] is a stable, point-in-time view of a tree (or a
//! prefix subtree). Unlike [`crate::View`], which eagerly copies every
//! reachable frame at capture time, a snapshot copies only the root
//! frame up front and shares all other frames with the live tree. Later
//! writes *fork* (copy-on-write) the individual frames a snapshot still
//! references instead of overwriting them in place, so the snapshot
//! stays stable with 1× read amplification and without MVCC version
//! chains.
//!
//! Creation is O(one frame copy); the per-write cost is zero while no
//! snapshot is live, and bounded by the root→leaf frame path length on
//! the first write to each region while one is. Dropping the handle (or
//! calling [`Snapshot::retire`]) retires the snapshot and lowers the
//! global fork barrier.

use std::ops::Deref;
use std::sync::Arc;

use crate::store::BufferManager;

use super::view::View;

/// A stable copy-on-write snapshot of a tree or prefix subtree.
///
/// Created by [`crate::Tree::snapshot`]. Reads see the tree exactly as
/// it was at creation time regardless of concurrent or subsequent live
/// writes. All [`View`] read operations are available through `Deref`
/// (`snapshot.get(..)`, `snapshot.range()`, `snapshot.scan(..)`, …).
///
/// The snapshot is retired when the handle is dropped; hold it for as
/// long as the stable view is needed.
pub struct Snapshot {
    view: Option<View>,
    store: Arc<BufferManager>,
    epoch: u64,
    retired: bool,
}

impl Snapshot {
    pub(crate) fn new(view: View, store: Arc<BufferManager>, epoch: u64) -> Self {
        Self {
            view: Some(view),
            store,
            epoch,
            retired: false,
        }
    }

    /// This snapshot's epoch — its position on the global copy-on-write
    /// timeline. Frames with `created_epoch <= epoch` are the ones this
    /// snapshot may reference and that live writes must fork.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The underlying scoped read view.
    #[must_use]
    pub fn view(&self) -> &View {
        self.view.as_ref().expect("snapshot retired")
    }

    /// Retire the snapshot now, releasing its hold on the fork barrier.
    /// Equivalent to dropping the handle, but explicit. Idempotent.
    pub fn retire(mut self) {
        self.retire_inner();
    }

    fn retire_inner(&mut self) {
        if !self.retired {
            self.retired = true;
            drop(self.view.take());
            self.store.retire_snapshot(self.epoch);
        }
    }
}

impl Deref for Snapshot {
    type Target = View;

    fn deref(&self) -> &View {
        self.view()
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        self.retire_inner();
    }
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("epoch", &self.epoch)
            .field("scope", &self.view.as_ref().map(View::scope))
            .finish_non_exhaustive()
    }
}
