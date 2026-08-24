//! Shared, byte-capped store of decompressed arena frames.
//!
//! A published table is immutable, so a decompressed frame never goes stale - this is
//! pure memoisation with nothing to invalidate. Frames are keyed by a dense, contiguous
//! index, which lets the store be an N-way set-associative table sized once at open:
//! no map, no eviction list, no per-insert allocation, and one lock per set, so
//! concurrent readers stay off each other's backs without a sharding decision.
//!
//! Buffers from evicted frames are recycled, so a steady-state miss decompresses into a
//! buffer that already exists instead of allocating a new one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// One decompressed arena frame, shared by every path resolved out of it.
pub(crate) struct Frame(Vec<u8>);

impl Frame {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Frame {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

/// Frames per set. Four is enough associativity that the arena's forward-walking
/// access pattern never thrashes, and small enough that a set scan stays trivial.
const WAYS: usize = 4;

/// Recycled buffers kept for reuse. Scales with concurrent decompressions, not with
/// the cache size - a buffer only sits here between one frame's eviction and the next
/// frame's decompression.
const SPARE_BUFFERS: usize = 8;

/// A fixed-size set-associative cache of decompressed frames.
pub(crate) struct FrameCache {
    /// Empty when caching is disabled; every operation then no-ops.
    sets: Box<[Mutex<[Slot; WAYS]>]>,

    /// Monotonic access counter; a slot's last value orders eviction within its set.
    clock: AtomicU64,

    spare: Mutex<Vec<Vec<u8>>>,
}

#[derive(Default)]
struct Slot {
    /// Meaningful only while `frame` is `Some`.
    index: u32,

    used: u64,

    frame: Option<Arc<Frame>>,
}

impl FrameCache {
    /// A cache holding at most `budget` bytes worth of `frame_size` frames.
    ///
    /// Never sized beyond the `frames` the file actually has, so a small table costs a
    /// small table's worth of slots. A zero budget (or a table with no frames at all)
    /// disables caching outright.
    pub(crate) fn new(budget: usize, frame_size: usize, frames: usize) -> Self {
        let wanted = match budget.checked_div(frame_size) {
            Some(n) => n.min(frames),
            None => 0,
        };

        Self {
            sets: (0..wanted.div_ceil(WAYS))
                .map(|_| Mutex::new(std::array::from_fn(|_| Slot::default())))
                .collect(),
            clock: AtomicU64::new(0),
            spare: Mutex::new(Vec::new()),
        }
    }

    /// How many frames this cache can hold at once; `0` when disabled.
    pub(crate) fn capacity(&self) -> usize {
        self.sets.len() * WAYS
    }

    /// The cached frame `index`, if it is still resident.
    pub(crate) fn get(&self, index: u32) -> Option<Arc<Frame>> {
        let mut slots = self
            .set(index)?
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let slot = slots
            .iter_mut()
            .find(|slot| slot.frame.is_some() && slot.index == index)?;

        slot.used = self.clock.fetch_add(1, Ordering::Relaxed);
        slot.frame.clone()
    }

    /// Publish `frame` under `index`, evicting the coldest frame in its set.
    pub(crate) fn insert(&self, index: u32, frame: &Arc<Frame>) {
        let Some(set) = self.set(index) else { return };
        let used = self.clock.fetch_add(1, Ordering::Relaxed);

        let evicted = {
            let mut slots = set.lock().unwrap_or_else(PoisonError::into_inner);
            // Prefer the slot this frame already occupies (a racing reader decompressed
            // it too), then an empty one, then the least recently used. `+ 1` keeps an
            // occupied slot from tying with an empty one at clock zero.
            let slot = match slots
                .iter()
                .position(|slot| slot.frame.is_some() && slot.index == index)
            {
                Some(position) => &mut slots[position],
                None => slots
                    .iter_mut()
                    .min_by_key(|slot| match slot.frame {
                        None => 0,
                        Some(_) => slot.used.saturating_add(1),
                    })
                    .expect("WAYS is nonzero"),
            };

            slot.index = index;
            slot.used = used;
            slot.frame.replace(Arc::clone(frame))
        };

        if let Some(evicted) = evicted {
            self.recycle(evicted);
        }
    }

    /// A buffer to decompress into, reusing an evicted frame's allocation when there
    /// is one to reuse.
    pub(crate) fn take_buffer(&self, capacity: usize) -> Vec<u8> {
        let mut spare = self.spare.lock().unwrap_or_else(PoisonError::into_inner);
        match spare.pop() {
            Some(mut buffer) => {
                buffer.clear();
                buffer.reserve(capacity);
                buffer
            }
            None => Vec::with_capacity(capacity),
        }
    }

    /// Reclaim an evicted frame's buffer, unless a caller still holds a path into it.
    fn recycle(&self, frame: Arc<Frame>) {
        let Some(frame) = Arc::into_inner(frame) else {
            return;
        };

        let mut spare = self.spare.lock().unwrap_or_else(PoisonError::into_inner);
        if spare.len() < SPARE_BUFFERS {
            let mut buffer = frame.0;
            buffer.clear();
            spare.push(buffer);
        }
    }

    fn set(&self, index: u32) -> Option<&Mutex<[Slot; WAYS]>> {
        if self.sets.is_empty() {
            return None;
        }

        Some(&self.sets[index as usize % self.sets.len()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(byte: u8) -> Arc<Frame> {
        Arc::new(Frame::from(vec![byte; 16]))
    }

    #[test]
    fn a_zero_budget_disables_caching() {
        let cache = FrameCache::new(0, 16, 100);
        assert_eq!(cache.capacity(), 0);

        cache.insert(0, &frame(1));
        assert!(cache.get(0).is_none());
    }

    #[test]
    fn slots_never_exceed_the_frames_in_the_file() {
        // Budget for 1000 frames, but the file only has 3.
        let cache = FrameCache::new(16_000, 16, 3);
        assert_eq!(cache.capacity(), WAYS);
    }

    #[test]
    fn frames_round_trip_until_their_set_fills() {
        // One set: four ways, so the fifth frame mapping to it evicts the coldest.
        let cache = FrameCache::new(16 * WAYS, 16, WAYS);
        assert_eq!(cache.sets.len(), 1);

        for i in 0..WAYS as u32 {
            cache.insert(i, &frame(i as u8));
        }
        // Touch 0 so it is no longer the coldest, then force one eviction.
        assert!(cache.get(0).is_some());
        cache.insert(99, &frame(99));

        assert!(cache.get(0).is_some(), "recently used frame survived");
        assert!(cache.get(99).is_some(), "newly inserted frame is resident");
        assert!(cache.get(1).is_none(), "coldest frame was evicted");
    }

    #[test]
    fn evicted_buffers_are_recycled() {
        let cache = FrameCache::new(16 * WAYS, 16, WAYS);
        for i in 0..WAYS as u32 + 1 {
            cache.insert(i, &frame(i as u8));
        }

        // The evicted frame's allocation came back, so this buffer is not a fresh one.
        let buffer = cache.take_buffer(16);
        assert!(buffer.is_empty());
        assert!(buffer.capacity() >= 16);
        assert!(
            cache.spare.lock().unwrap().is_empty(),
            "buffer was handed out"
        );
    }

    /// A frame still held by a caller must not be recycled out from under them.
    #[test]
    fn a_held_frame_is_not_recycled() {
        let cache = FrameCache::new(16 * WAYS, 16, WAYS);
        let held = frame(0);
        cache.insert(0, &held);
        for i in 1..WAYS as u32 + 1 {
            cache.insert(i, &frame(i as u8));
        }

        assert_eq!(held.bytes(), &[0u8; 16]);
        assert!(cache.spare.lock().unwrap().is_empty());
    }
}
