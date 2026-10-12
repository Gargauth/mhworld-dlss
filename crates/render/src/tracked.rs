//! A lock-free set of descriptor handles, for the question every descriptor
//! copy asks: "does this touch a view worth following?".
//!
//! The engine makes some 440 000 descriptor copies a frame, on every
//! recording thread. Asking a locked map each time would serialise them all
//! behind one lock, so the question is asked of an open-addressing table of
//! atomics instead: no lock, no write on the read path. Writes happen only
//! with the registry's lock held (single writer); readers may race a write
//! and see either answer, which the registry tolerates (a missed copy leaves
//! an entry that ages out by its stamp).

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::addrmap::mix;

const EMPTY: usize = 0;
/// A slot that held a handle since removed; probing continues past it.
const DEAD: usize = 1;

/// The set. Handles are never 0 or 1 (descriptor handles are addresses).
#[derive(Debug)]
pub struct TrackedSet {
    slots: Box<[AtomicUsize]>,
    used: AtomicUsize,
}

impl TrackedSet {
    /// A set of `capacity` slots (rounded up to a power of two); it
    /// reports [`TrackedSet::insert`] failure past half full.
    pub fn new(capacity: usize) -> TrackedSet {
        let capacity = capacity.next_power_of_two().max(8);
        TrackedSet {
            slots: (0..capacity).map(|_| AtomicUsize::new(EMPTY)).collect(),
            used: AtomicUsize::new(0),
        }
    }

    fn mask(&self) -> usize {
        self.slots.len() - 1
    }

    fn first(&self, handle: usize) -> usize {
        mix(handle as u64) as usize & self.mask()
    }

    /// Whether `handle` is in the set. Lock-free.
    pub fn contains(&self, handle: usize) -> bool {
        if handle <= DEAD {
            return false;
        }
        let mut slot = self.first(handle);
        for _ in 0..self.slots.len() {
            match self.slots[slot].load(Ordering::Relaxed) {
                held if held == handle => return true,
                EMPTY => return false,
                _ => slot = (slot + 1) & self.mask(),
            }
        }
        false
    }

    /// Add `handle`. Single writer only. Returns `false` when the set is
    /// past half full and the handle was not added (the caller then
    /// follows every copy instead).
    pub fn insert(&self, handle: usize) -> bool {
        if handle <= DEAD {
            return true;
        }
        let mut slot = self.first(handle);
        let mut reuse = None;
        for _ in 0..self.slots.len() {
            match self.slots[slot].load(Ordering::Relaxed) {
                held if held == handle => return true,
                DEAD if reuse.is_none() => reuse = Some(slot),
                EMPTY => break,
                _ => {}
            }
            slot = (slot + 1) & self.mask();
        }
        if let Some(dead) = reuse {
            self.slots[dead].store(handle, Ordering::Relaxed);
            return true;
        }
        if self.used.load(Ordering::Relaxed) >= self.slots.len() / 2 {
            return false;
        }
        self.slots[slot].store(handle, Ordering::Relaxed);
        self.used.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Remove `handle`. Single writer only.
    pub fn remove(&self, handle: usize) {
        if handle <= DEAD {
            return;
        }
        let mut slot = self.first(handle);
        for _ in 0..self.slots.len() {
            match self.slots[slot].load(Ordering::Relaxed) {
                held if held == handle => {
                    self.slots[slot].store(DEAD, Ordering::Relaxed);
                    return;
                }
                EMPTY => return,
                _ => slot = (slot + 1) & self.mask(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_survives_removal_of_colliding_neighbours() {
        let set = TrackedSet::new(64);
        let handles: Vec<usize> = (0..20).map(|i| 0x7ff0_0000 + i * 64).collect();
        for &handle in &handles {
            assert!(set.insert(handle));
        }
        assert!(handles.iter().all(|&h| set.contains(h)));
        set.remove(handles[3]);
        assert!(!set.contains(handles[3]));
        assert!(handles
            .iter()
            .enumerate()
            .all(|(i, &h)| i == 3 || set.contains(h)));
        // The dead slot is reused rather than growing the count.
        assert!(set.insert(handles[3]));
        assert!(set.contains(handles[3]));
        assert!(!set.contains(0x1234_5678));
        assert!(!set.contains(0));
    }

    #[test]
    fn refuses_past_half_full() {
        let set = TrackedSet::new(16);
        let accepted = (0..16usize).filter(|i| set.insert(0x1000 + i * 32)).count();
        assert_eq!(accepted, 8);
    }
}
