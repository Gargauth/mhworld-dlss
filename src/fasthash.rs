//! A hasher for maps keyed by an address.
//!
//! Every registry in this library is a map from a pointer -- a descriptor
//! handle, a resource, a command list -- to a small record, consulted from
//! hooks that run tens or hundreds of thousands of times a frame. The
//! standard library's default hasher is SipHash, which is built to resist
//! keys chosen by an attacker and costs accordingly. Nothing chooses these
//! keys but the allocator, so a plain 64-bit mix does the same job for a
//! fraction of the time spent under each registry's lock.
//!
//! The mix is the finaliser from MurmurHash3. It is a full avalanche, which
//! matters here: these keys are aligned addresses whose low bits are all
//! zero, and the table takes its bucket from the low bits of the hash, so a
//! hasher that merely multiplied would pile every key into a few buckets.
//!
//! The maps are ordinary `HashMap`s with the same contents and the same
//! answers as with the default hasher. Nothing depends on iteration order.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

#[derive(Default, Clone, Copy)]
pub struct AddressHasher(u64);

#[inline]
pub fn mix(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    value ^ (value >> 33)
}

impl Hasher for AddressHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    /// For anything that is not a single integer. Not on any hot path; here
    /// so the type is a correct hasher for every key and not only for these.
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = mix(self.0 ^ byte as u64);
        }
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.0 = mix(self.0 ^ value as u64);
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.0 = mix(self.0 ^ value);
    }
}

/// A `HashMap` keyed by an address.
pub type AddressMap<K, V> = HashMap<K, V, BuildHasherDefault<AddressHasher>>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the choice of mix rests on: aligned addresses, whose low
    /// bits are identical, must still spread over the low bits of the hash.
    #[test]
    fn aligned_addresses_spread_over_the_low_bits() {
        let mut buckets = [0u32; 64];
        for index in 0..64_000u64 {
            let mut hasher = AddressHasher::default();
            hasher.write_usize((0x0000_01d3_8c82_0000u64 + index * 32) as usize);
            buckets[(hasher.finish() & 63) as usize] += 1;
        }
        // A thousand each if perfectly even; anything near that will do.
        assert!(
            buckets.iter().all(|count| (800..1200).contains(count)),
            "{buckets:?}"
        );
    }

    /// And it is still a map: what goes in comes out.
    #[test]
    fn the_map_behaves_as_a_map() {
        let mut map: AddressMap<usize, u32> = AddressMap::default();
        for index in 0..10_000usize {
            map.insert(0x7ff6_0000_0000 + index * 16, index as u32);
        }
        assert_eq!(map.len(), 10_000);
        assert_eq!(map.get(&(0x7ff6_0000_0000 + 4321 * 16)), Some(&4321));
        assert_eq!(map.remove(&0x7ff6_0000_0000), Some(0));
        assert_eq!(map.get(&0x7ff6_0000_0000), None);
    }
}
