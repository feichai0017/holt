use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;

use crate::layout::BlobGuid;

const ROWS: usize = 4;
const WIDTH: usize = 4096;
const DECAY_PERIOD: u64 = 65_536;

/// TinyLFU-style frequency sketch for cache admission decisions.
///
/// This is intentionally advisory: correctness is still owned by
/// dirty/flushing/pending-delete bookkeeping in `BufferManager`.
pub(super) struct TinyLFU {
    counters: Box<[AtomicU8]>,
    samples: AtomicU64,
    decay_lock: Mutex<()>,
}

impl TinyLFU {
    pub(super) fn new() -> Self {
        let counters = (0..ROWS * WIDTH)
            .map(|_| AtomicU8::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            counters,
            samples: AtomicU64::new(0),
            decay_lock: Mutex::new(()),
        }
    }

    pub(super) fn record(&self, guid: BlobGuid) {
        let sample = self.samples.fetch_add(1, Ordering::Relaxed) + 1;
        for row in 0..ROWS {
            let idx = Self::idx(guid, row);
            let cell = &self.counters[row * WIDTH + idx];
            let mut cur = cell.load(Ordering::Relaxed);
            while cur != u8::MAX {
                match cell.compare_exchange_weak(
                    cur,
                    cur.saturating_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(next) => cur = next,
                }
            }
        }
        if sample % DECAY_PERIOD == 0 {
            self.decay();
        }
    }

    pub(super) fn estimate(&self, guid: BlobGuid) -> u8 {
        let mut out = u8::MAX;
        for row in 0..ROWS {
            let idx = Self::idx(guid, row);
            out = out.min(self.counters[row * WIDTH + idx].load(Ordering::Relaxed));
        }
        out
    }

    fn decay(&self) {
        let Ok(_guard) = self.decay_lock.try_lock() else {
            return;
        };
        for cell in &self.counters {
            let cur = cell.load(Ordering::Relaxed);
            if cur > 0 {
                cell.store(cur / 2, Ordering::Relaxed);
            }
        }
    }

    #[inline]
    fn idx(guid: BlobGuid, row: usize) -> usize {
        (mix64(guid, row as u64) as usize) & (WIDTH - 1)
    }
}

#[inline]
fn mix64(guid: BlobGuid, salt: u64) -> u64 {
    let lo = u64::from_le_bytes(guid[0..8].try_into().expect("guid lo"));
    let hi = u64::from_le_bytes(guid[8..16].try_into().expect("guid hi"));
    let mut x = lo ^ hi.rotate_left(29) ^ salt.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_repeated_guid_above_one_shot() {
        let sketch = TinyLFU::new();
        let hot = [0x11; 16];
        let cold = [0x22; 16];

        for _ in 0..8 {
            sketch.record(hot);
        }
        sketch.record(cold);

        assert!(sketch.estimate(hot) > sketch.estimate(cold));
    }
}
