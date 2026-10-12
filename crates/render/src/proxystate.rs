//! Bookkeeping for ReShade's command list proxies (the Windows side is
//! `sys/proxylist.rs`): which proxy's `Dispatch` is on this thread's stack,
//! and the descriptor heaps the engine last set on each proxy.
//!
//! Pure: addresses are plain integers here, so the nesting and the bound
//! are tested on any host.

#![cfg_attr(not(windows), allow(dead_code))]

use std::cell::Cell;
use std::collections::HashMap;

thread_local! {
    /// The ReShade proxy list whose `Dispatch` is on this thread's stack.
    static DISPATCHING: Cell<usize> = const { Cell::new(0) };
}

/// The proxy whose `Dispatch` is on this thread's stack (0: none).
pub(crate) fn dispatching() -> usize {
    DISPATCHING.with(Cell::get)
}

/// Holds a proxy as this thread's dispatching list for the length of one
/// `Dispatch`, and puts back whatever was there when dropped: NGX dispatches
/// on the same proxy from inside the outer call.
pub(crate) struct Dispatching {
    previous: usize,
}

impl Dispatching {
    pub(crate) fn enter(proxy: usize) -> Dispatching {
        Dispatching {
            previous: DISPATCHING.with(|current| current.replace(proxy)),
        }
    }
}

impl Drop for Dispatching {
    fn drop(&mut self) {
        DISPATCHING.with(|current| current.set(self.previous));
    }
}

/// Descriptor heaps as passed to `SetDescriptorHeaps` (at most one CBV/SRV/
/// UAV and one sampler heap can be bound, so at most two).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Heaps {
    count: u32,
    heaps: [usize; 2],
}

impl Heaps {
    /// The heaps of one call; `None` when there are more than two (an
    /// invalid call, not recorded).
    pub(crate) fn new(heaps: &[usize]) -> Option<Heaps> {
        let mut held = [0usize; 2];
        held.get_mut(..heaps.len())?.copy_from_slice(heaps);
        Some(Heaps {
            count: heaps.len() as u32,
            heaps: held,
        })
    }

    /// How many heaps.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// The heaps, in call order.
    pub(crate) fn as_slice(&self) -> &[usize] {
        &self.heaps[..self.count as usize]
    }
}

/// Lists are created and destroyed for the life of a session and the map is
/// keyed by address, so it is emptied when it reaches this many entries
/// rather than left to grow. The engine sets its heaps again on every list
/// every frame, so what is lost is relearnt at once.
pub(crate) const MAX_HELD_LISTS: usize = 4096;

/// The heaps last set on each proxy list, keyed by the proxy's address.
#[derive(Debug, Default)]
pub(crate) struct HeldHeaps {
    lists: HashMap<usize, Heaps>,
}

impl HeldHeaps {
    /// Note `heaps` as the last set on `list`.
    pub(crate) fn record(&mut self, list: usize, heaps: Heaps) {
        if self.lists.len() >= MAX_HELD_LISTS && !self.lists.contains_key(&list) {
            self.lists.clear();
        }
        self.lists.insert(list, heaps);
    }

    /// The heaps last set on `list`.
    pub(crate) fn get(&self, list: usize) -> Option<Heaps> {
        self.lists.get(&list).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatching_nests_and_restores() {
        assert_eq!(dispatching(), 0);
        let outer = Dispatching::enter(0x10);
        assert_eq!(dispatching(), 0x10);
        {
            // NGX dispatching on the same proxy inside the outer call.
            let _inner = Dispatching::enter(0x10);
            assert_eq!(dispatching(), 0x10);
            let _other = Dispatching::enter(0x20);
            assert_eq!(dispatching(), 0x20);
        }
        assert_eq!(dispatching(), 0x10);
        drop(outer);
        assert_eq!(dispatching(), 0);
    }

    #[test]
    fn dispatching_is_per_thread() {
        let _held = Dispatching::enter(0x30);
        let seen = std::thread::spawn(dispatching).join().unwrap();
        assert_eq!(seen, 0);
        assert_eq!(dispatching(), 0x30);
    }

    #[test]
    fn heaps_hold_up_to_two() {
        assert_eq!(Heaps::new(&[]).map(|h| h.count()), Some(0));
        let one = Heaps::new(&[0xa]).unwrap();
        assert_eq!(one.as_slice(), &[0xa]);
        let two = Heaps::new(&[0xa, 0xb]).unwrap();
        assert_eq!((two.count(), two.as_slice()), (2, &[0xa, 0xb][..]));
        assert_eq!(Heaps::new(&[1, 2, 3]), None);
    }

    #[test]
    fn the_last_call_wins() {
        let mut held = HeldHeaps::default();
        held.record(1, Heaps::new(&[0xa, 0xb]).unwrap());
        held.record(1, Heaps::new(&[0xc]).unwrap());
        assert_eq!(held.get(1).unwrap().as_slice(), &[0xc]);
        assert_eq!(held.get(2), None);
    }

    #[test]
    fn the_map_is_bounded() {
        let mut held = HeldHeaps::default();
        let heaps = Heaps::new(&[0xa]).unwrap();
        for list in 0..MAX_HELD_LISTS {
            held.record(list, heaps);
        }
        assert_eq!(held.lists.len(), MAX_HELD_LISTS);
        // A list already held is updated in place.
        held.record(0, heaps);
        assert_eq!(held.lists.len(), MAX_HELD_LISTS);
        // A new one empties the map first.
        held.record(MAX_HELD_LISTS, heaps);
        assert_eq!(held.lists.len(), 1);
        assert_eq!(held.get(MAX_HELD_LISTS), Some(heaps));
    }
}
