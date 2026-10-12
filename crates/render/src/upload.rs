//! Where a GPU virtual address can be read on the CPU.
//!
//! A root CBV is a bare GPU address. It is readable when it lies in a buffer
//! the engine mapped: `ID3D12Resource::Map` records `[gpu, gpu+size) -> cpu`
//! ([`UploadMap::mapped`]), `Unmap` drops it when the map count reaches zero.
//!
//! The engine's constant buffer arena (`mTempCpuConstantBuffer`, 48 MiB,
//! upload heap) is mapped **once**, inside its device init, CPU pointer at
//! `Device+0x1018`, VA at `+0x1060` (`ReLoadInitDx11` §2.3). A mod loaded
//! after that misses the `Map`, so the region can be seeded from those
//! fields instead ([`UploadMap::seed`]). The same call covers the arena's
//! GPU copy (`[Device+0xff0] == 0`: binds use the default-heap copy at
//! `+0x1080` whose bytes equal the CPU arena at the same offset).

/// How a region became known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Through `Map`, with the outstanding map count.
    Mapped(u32),
    /// Seeded by the caller; never dropped by `Unmap`.
    Seeded,
}

/// A readable GPU address range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// First GPU virtual address.
    pub gpu: u64,
    /// Length in bytes.
    pub size: u64,
    /// CPU address of `gpu`.
    pub cpu: usize,
    /// How it became known.
    pub origin: Origin,
}

impl Region {
    fn covers(&self, address: u64, len: usize) -> bool {
        address >= self.gpu
            && (address - self.gpu)
                .checked_add(len as u64)
                .is_some_and(|end| end <= self.size)
    }
}

/// The set of readable regions. Not synchronised.
#[derive(Debug, Default)]
pub struct UploadMap {
    regions: Vec<Region>,
}

impl UploadMap {
    /// `Map` returned `cpu` for the buffer at `gpu` of `size` bytes. A
    /// second `Map` of the same buffer replaces the pointer (the engine is
    /// saying where the memory lives now) and counts.
    pub fn mapped(&mut self, gpu: u64, size: u64, cpu: usize) {
        if gpu == 0 || cpu == 0 || size == 0 {
            return;
        }
        match self.regions.iter_mut().find(|r| r.gpu == gpu) {
            Some(region) => {
                region.size = size;
                region.cpu = cpu;
                region.origin = match region.origin {
                    Origin::Mapped(depth) => Origin::Mapped(depth.saturating_add(1)),
                    Origin::Seeded => Origin::Seeded,
                };
            }
            None => self.regions.push(Region {
                gpu,
                size,
                cpu,
                origin: Origin::Mapped(1),
            }),
        }
    }

    /// `Unmap` of the buffer at `gpu`; drops it at the last one. Returns
    /// whether a region was dropped.
    pub fn unmapped(&mut self, gpu: u64) -> bool {
        let Some(index) = self.regions.iter().position(|r| r.gpu == gpu) else {
            return false;
        };
        let region = &mut self.regions[index];
        match region.origin {
            Origin::Mapped(depth) if depth > 1 => {
                region.origin = Origin::Mapped(depth - 1);
                false
            }
            Origin::Mapped(_) => {
                self.regions.swap_remove(index);
                true
            }
            Origin::Seeded => false,
        }
    }

    /// Make `[gpu, gpu+size)` readable at `cpu` without a `Map`.
    pub fn seed(&mut self, gpu: u64, size: u64, cpu: usize) {
        if gpu == 0 || cpu == 0 || size == 0 {
            return;
        }
        self.regions.retain(|r| r.gpu != gpu);
        self.regions.push(Region {
            gpu,
            size,
            cpu,
            origin: Origin::Seeded,
        });
    }

    /// The CPU address of `len` bytes at GPU `address`, if one region
    /// covers them all.
    pub fn resolve(&self, address: u64, len: usize) -> Option<usize> {
        let region = self.regions.iter().find(|r| r.covers(address, len))?;
        let offset = usize::try_from(address - region.gpu).ok()?;
        region.cpu.checked_add(offset)
    }

    /// Drop the region at `gpu` that maps to `cpu` (its memory went away
    /// without an `Unmap`: the buffer was released while mapped).
    pub fn drop_region(&mut self, gpu: u64, cpu: usize) {
        self.regions.retain(|r| !(r.gpu == gpu && r.cpu == cpu));
    }

    /// The region `resolve` would use for `address`.
    pub fn region_of(&self, address: u64) -> Option<Region> {
        self.regions.iter().copied().find(|r| r.covers(address, 1))
    }

    /// Regions known.
    pub fn len(&self) -> usize {
        self.regions.len()
    }

    /// Whether no region is known.
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_inside_and_never_across_the_end() {
        let mut map = UploadMap::default();
        map.mapped(0x1_0000, 0x1000, 0x7000_0000);
        assert_eq!(map.resolve(0x1_0400, 64), Some(0x7000_0400));
        assert_eq!(map.resolve(0x1_0fc0, 64), Some(0x7000_0fc0));
        assert_eq!(map.resolve(0x1_0fc1, 64), None);
        assert_eq!(map.resolve(0xffff, 1), None);
        assert_eq!(map.resolve(u64::MAX, 2), None);
    }

    #[test]
    fn map_counts_and_the_last_unmap_drops() {
        let mut map = UploadMap::default();
        map.mapped(0x1_0000, 0x1000, 0x7000_0000);
        map.mapped(0x1_0000, 0x1000, 0x7100_0000);
        assert_eq!(map.resolve(0x1_0000, 4), Some(0x7100_0000));
        assert!(!map.unmapped(0x1_0000));
        assert!(map.resolve(0x1_0000, 4).is_some());
        assert!(map.unmapped(0x1_0000));
        assert_eq!(map.resolve(0x1_0000, 4), None);
        assert!(!map.unmapped(0x1_0000));
    }

    #[test]
    fn seeded_regions_survive_unmap_and_alias_the_gpu_copy() {
        // The arena (VA 0x5000_0000) and its default-heap copy (VA
        // 0x9000_0000) share the CPU bytes at 0x2000_0000.
        let mut map = UploadMap::default();
        map.seed(0x5000_0000, 0x300_0000, 0x2000_0000);
        map.seed(0x9000_0000, 0x300_0000, 0x2000_0000);
        assert_eq!(map.resolve(0x9000_0430, 1072), Some(0x2000_0430));
        assert_eq!(map.resolve(0x5000_0430, 1072), Some(0x2000_0430));
        assert!(!map.unmapped(0x5000_0000));
        assert_eq!(map.len(), 2);
        map.drop_region(0x9000_0000, 0x2000_0000);
        assert_eq!(map.resolve(0x9000_0430, 4), None);
    }
}
