//! A hasher for maps keyed by an address.
//!
//! Every registry here maps a pointer (descriptor handle, resource, command
//! list) to a small record and is consulted from hooks that run tens of
//! thousands of times a frame. SipHash resists keys chosen by an attacker;
//! nothing chooses these but the allocator, so the MurmurHash3 finaliser does
//! the job for a fraction of the time spent under each lock. It avalanches
//! fully, which matters: the keys are aligned (low bits zero) and the table
//! takes its bucket from the low bits of the hash.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// The MurmurHash3 64-bit finaliser.
#[inline]
pub fn mix(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    value ^ (value >> 33)
}

/// [`Hasher`] for address keys.
#[derive(Debug, Default, Clone, Copy)]
pub struct AddressHasher(u64);

impl Hasher for AddressHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    /// Any non-integer key; not on a hot path.
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = mix(self.0 ^ u64::from(byte));
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

    #[test]
    fn aligned_addresses_spread_over_the_low_bits() {
        let mut buckets = [0u32; 64];
        for index in 0..64_000u64 {
            let mut hasher = AddressHasher::default();
            hasher.write_usize((0x0000_01d3_8c82_0000u64 + index * 32) as usize);
            buckets[(hasher.finish() & 63) as usize] += 1;
        }
        assert!(
            buckets.iter().all(|count| (800..1200).contains(count)),
            "{buckets:?}"
        );
    }
}
