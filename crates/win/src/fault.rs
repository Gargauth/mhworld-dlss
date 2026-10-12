//! What the crash logger needs that does not touch the process: which
//! exception codes are faults worth a line, how a fault location is written,
//! and a lock-free "first time for this address" set usable from inside an
//! exception handler.

use std::sync::atomic::{AtomicUsize, Ordering};

/// The name of a hardware or runtime fault code, or `None` for codes that
/// are not faults: C++ exceptions (`0xE06D7363`), debugger notifications,
/// thread naming, breakpoints. Those pass through the vectored handler by the
/// thousand in a healthy process and are not logged.
pub fn fault_name(code: u32) -> Option<&'static str> {
    Some(match code {
        0xC000_0005 => "access violation",
        0xC000_0006 => "in-page error",
        0xC000_001D => "illegal instruction",
        0xC000_0025 => "noncontinuable exception",
        0xC000_008C => "array bounds exceeded",
        0xC000_0094 => "integer divide by zero",
        0xC000_0095 => "integer overflow",
        0xC000_0096 => "privileged instruction",
        0xC000_00FD => "stack overflow",
        0xC000_0374 => "heap corruption",
        0xC000_0409 => "stack buffer overrun",
        0x8000_0002 => "datatype misalignment",
        _ => return None,
    })
}

/// What an access violation's first parameter says was attempted.
pub fn access_kind(information0: usize) -> &'static str {
    match information0 {
        0 => "read",
        1 => "write",
        8 => "execute",
        _ => "access",
    }
}

/// `module+0xoffset` when the address lies in a module, else the bare address.
pub fn location(address: usize, module: Option<(&str, usize)>) -> String {
    match module {
        Some((name, base)) if address >= base => format!("{name}+{:#x}", address - base),
        _ => format!("{address:#x}"),
    }
}

/// Outcome of [`OnceSet::insert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insert {
    /// First time: the key is now in the set.
    New,
    /// The key was already in the set.
    Seen,
    /// The set is full; the key was not recorded.
    Full,
}

/// A fixed-capacity, insert-only set of addresses, safe to use from any
/// thread at any time, including an exception handler: it never allocates,
/// locks or blocks.
#[derive(Debug)]
pub struct OnceSet<const N: usize> {
    slots: [AtomicUsize; N],
}

impl<const N: usize> Default for OnceSet<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> OnceSet<N> {
    /// An empty set.
    pub const fn new() -> Self {
        Self {
            slots: [const { AtomicUsize::new(0) }; N],
        }
    }

    /// Record `key` (any address; stored as `key + 1` so that address 0, a
    /// call through a null pointer, is recordable and an empty slot is 0).
    /// `usize::MAX` is never a user-mode address and is reported `Full`.
    pub fn insert(&self, key: usize) -> Insert {
        let Some(stored) = key.checked_add(1) else {
            return Insert::Full;
        };
        if N == 0 {
            return Insert::Full;
        }
        // Fibonacci hashing spreads nearby code addresses across slots.
        let start = stored.wrapping_mul(0x9E37_79B9_7F4A_7C15) % N;
        for probe in 0..N {
            let slot = &self.slots[(start + probe) % N];
            match slot.compare_exchange(0, stored, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Insert::New,
                Err(held) if held == stored => return Insert::Seen,
                Err(_) => continue,
            }
        }
        Insert::Full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faults_are_named_and_notifications_are_not() {
        assert_eq!(fault_name(0xC000_0005), Some("access violation"));
        assert_eq!(fault_name(0xC000_00FD), Some("stack overflow"));
        for benign in [
            0xE06D_7363,
            0x406D_1388,
            0x4001_0006,
            0x8000_0003,
            0x8000_0004,
        ] {
            assert_eq!(fault_name(benign), None, "{benign:#x}");
        }
    }

    #[test]
    fn locations_name_the_module_and_offset() {
        assert_eq!(
            location(
                0x1_4228_f3c3,
                Some(("MonsterHunterWorld.exe", 0x1_4000_0000))
            ),
            "MonsterHunterWorld.exe+0x228f3c3"
        );
        assert_eq!(location(0x1234, None), "0x1234");
        assert_eq!(location(0x10, Some(("x.dll", 0x20))), "0x10");
        assert_eq!(access_kind(1), "write");
        assert_eq!(access_kind(8), "execute");
    }

    #[test]
    fn each_address_is_new_once() {
        let set = OnceSet::<4>::new();
        assert_eq!(set.insert(0x1000), Insert::New);
        assert_eq!(set.insert(0x1000), Insert::Seen);
        assert_eq!(set.insert(0), Insert::New);
        assert_eq!(set.insert(0), Insert::Seen);
        assert_eq!(set.insert(usize::MAX), Insert::Full);
    }

    #[test]
    fn a_full_set_says_so_and_still_knows_its_members() {
        let set = OnceSet::<3>::new();
        for key in 1..=3 {
            assert_eq!(set.insert(key * 0x10), Insert::New);
        }
        assert_eq!(set.insert(0x40), Insert::Full);
        for key in 1..=3 {
            assert_eq!(set.insert(key * 0x10), Insert::Seen);
        }
        assert_eq!(OnceSet::<0>::new().insert(1), Insert::Full);
    }

    #[test]
    fn concurrent_inserts_of_one_key_yield_exactly_one_new() {
        let set = OnceSet::<64>::new();
        let news: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        (0..100)
                            .filter(|_| set.insert(0xdead) == Insert::New)
                            .count()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });
        assert_eq!(news, 1);
    }
}
