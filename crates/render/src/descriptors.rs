//! Resolving descriptor tables back to resources.
//!
//! A dispatch is handed one opaque GPU handle per table. Getting from it to
//! resources takes several hooks feeding this registry:
//!
//! * `CreateShaderResourceView`/`CreateUnorderedAccessView` record which
//!   resource landed on which CPU handle ([`DescriptorRegistry::created`]);
//! * `CopyDescriptors[Simple]` propagate them, because the engine stages
//!   descriptors in CPU heaps and copies them into per-frame shader-visible
//!   ones ([`DescriptorRegistry::copy`]);
//! * `SetDescriptorHeaps` records each heap's CPU/GPU bases and stride, which
//!   convert a GPU handle back into a CPU one ([`DescriptorRegistry::heap`]).
//!
//! Only views of resources that could be an answer are recorded (the caller
//! decides: frame-sized, not depth, with a liveness serial). A copy is
//! followed only when it touches such a view, decided lock-free through a
//! [`TrackedSet`]. A copied entry is believed for [`FRESH_TICKS`] ticks of a
//! clock that advances at the temporal dispatch and at present: the engine
//! cycles three per-frame heaps, so a copy older than a frame may have been
//! overwritten by something this registry does not follow.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::addrmap::AddressMap;
use crate::tracked::TrackedSet;

/// Ticks a copied entry is believed for (two ticks a frame).
pub const FRESH_TICKS: u64 = 2;
/// Stamp of a view created in place, which holds until the slot is rewritten.
const CREATED: u64 = u64::MAX;
/// Slots in the lock-free filter.
const TRACKED_CAPACITY: usize = 1 << 15;
/// Heaps remembered without the lock.
const KNOWN_HEAPS: usize = 16;

/// What kind of view a descriptor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewKind {
    /// Shader resource view.
    Srv,
    /// Unordered access view.
    Uav,
}

/// A descriptor heap's geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heap {
    /// `D3D12_DESCRIPTOR_HEAP_TYPE`.
    pub kind: u32,
    /// CPU handle of descriptor 0.
    pub cpu_start: usize,
    /// GPU handle of descriptor 0 (0 when not shader-visible).
    pub gpu_start: u64,
    /// Handle increment.
    pub stride: usize,
    /// Descriptors in the heap.
    pub count: u32,
    /// `D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE`.
    pub shader_visible: bool,
}

impl Heap {
    /// The CPU handle for a GPU handle inside this heap.
    pub fn cpu_for(&self, gpu: u64) -> Option<usize> {
        if !self.shader_visible || self.stride == 0 || gpu < self.gpu_start {
            return None;
        }
        let index = usize::try_from(gpu - self.gpu_start).ok()? / self.stride;
        if index >= self.count as usize {
            return None;
        }
        self.cpu_start.checked_add(index.checked_mul(self.stride)?)
    }
}

/// A resource a descriptor names, as recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Named {
    /// `ID3D12Resource*`.
    pub resource: usize,
    /// View kind.
    pub view: ViewKind,
    /// Liveness serial the resource had when the view was recorded.
    pub serial: u64,
}

/// One descriptor of a table, resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// A live resource.
    Named(Named),
    /// Nothing recorded for the CPU handle.
    Missing,
    /// A copied entry too old to believe.
    Stale,
    /// The resource recorded has since been destroyed (or replaced at the
    /// same address).
    Dead,
    /// No recorded shader-visible heap covers the GPU handle.
    UnknownHeap,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    named: Named,
    stamp: u64,
}

impl Entry {
    fn holds(&self, now: u64) -> bool {
        self.stamp == CREATED || now.saturating_sub(self.stamp) <= FRESH_TICKS
    }
}

#[derive(Debug, Default)]
struct Inner {
    entries: AddressMap<usize, Entry>,
    heaps: AddressMap<usize, Heap>,
}

/// Counters for introspection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DescriptorStats {
    /// Views recorded at creation.
    pub views: u64,
    /// Copies followed under the lock.
    pub copies_followed: u64,
    /// Descriptors propagated by those copies.
    pub copied: u64,
    /// Entries dropped because their slot was overwritten.
    pub forgotten: u64,
    /// Lookups refused as stale.
    pub stale: u64,
    /// Lookups refused because the resource died.
    pub dead: u64,
    /// Entries currently held.
    pub entries: u64,
    /// Heaps recorded.
    pub heaps: u64,
    /// The filter overflowed and every copy is followed under the lock.
    pub follow_all: bool,
}

/// The registry. All methods take `&self`; one internal lock.
#[derive(Debug)]
pub struct DescriptorRegistry {
    inner: Mutex<Inner>,
    tracked: TrackedSet,
    follow_all: AtomicBool,
    clock: AtomicU64,
    known: [(AtomicUsize, AtomicUsize); KNOWN_HEAPS],
    views: AtomicU64,
    copies_followed: AtomicU64,
    copied: AtomicU64,
    forgotten: AtomicU64,
    stale: AtomicU64,
    dead: AtomicU64,
}

impl Default for DescriptorRegistry {
    fn default() -> Self {
        DescriptorRegistry::new()
    }
}

impl DescriptorRegistry {
    /// An empty registry.
    pub fn new() -> DescriptorRegistry {
        DescriptorRegistry {
            inner: Mutex::new(Inner::default()),
            tracked: TrackedSet::new(TRACKED_CAPACITY),
            follow_all: AtomicBool::new(false),
            clock: AtomicU64::new(0),
            known: std::array::from_fn(|_| (AtomicUsize::new(0), AtomicUsize::new(0))),
            views: AtomicU64::new(0),
            copies_followed: AtomicU64::new(0),
            copied: AtomicU64::new(0),
            forgotten: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            dead: AtomicU64::new(0),
        }
    }

    /// A view was created at `handle`. `serial` is the resource's liveness
    /// serial if it is worth following; `None` only forgets the slot.
    pub fn created(&self, handle: usize, resource: usize, view: ViewKind, serial: Option<u64>) {
        let Some(serial) = serial.filter(|_| handle != 0 && resource != 0) else {
            self.forget(handle);
            return;
        };
        let mut inner = self.inner.lock();
        inner.entries.insert(
            handle,
            Entry {
                named: Named {
                    resource,
                    view,
                    serial,
                },
                stamp: CREATED,
            },
        );
        if !self.tracked.insert(handle) {
            self.follow_all.store(true, Ordering::Relaxed);
        }
        self.views.fetch_add(1, Ordering::Relaxed);
    }

    /// `handle` was overwritten by something not followed (an RTV, a DSV,
    /// a CBV, a sampler, a view of an uninteresting resource).
    pub fn forget(&self, handle: usize) {
        if handle == 0
            || (!self.follow_all.load(Ordering::Relaxed) && !self.tracked.contains(handle))
        {
            // Nothing created here is followed; a copied entry in this
            // slot stops being believed by its stamp.
            return;
        }
        let mut inner = self.inner.lock();
        if inner.entries.remove(&handle).is_some() {
            self.forgotten.fetch_add(1, Ordering::Relaxed);
        }
        self.tracked.remove(handle);
    }

    /// `count` descriptors were copied from `source` to `destination`.
    pub fn copy(&self, destination: usize, source: usize, count: u32, stride: usize) {
        if stride == 0 || destination == 0 || source == 0 || count == 0 {
            return;
        }
        let at = |base: usize, step: usize| base.wrapping_add(step.wrapping_mul(stride));
        if !self.follow_all.load(Ordering::Relaxed) {
            let touches = (0..count as usize).any(|step| {
                self.tracked.contains(at(source, step))
                    || self.tracked.contains(at(destination, step))
            });
            if !touches {
                return;
            }
        }
        let now = self.clock.load(Ordering::Relaxed);
        let mut inner = self.inner.lock();
        self.copies_followed.fetch_add(1, Ordering::Relaxed);
        for step in 0..count as usize {
            let (from, into) = (at(source, step), at(destination, step));
            if from == into {
                continue;
            }
            // Only from a believed entry: a stale one must not be made fresh
            // by being copied onward.
            let entry = inner
                .entries
                .get(&from)
                .copied()
                .filter(|entry| entry.holds(now));
            let replaced = match entry {
                Some(entry) => {
                    self.copied.fetch_add(1, Ordering::Relaxed);
                    inner.entries.insert(
                        into,
                        Entry {
                            named: entry.named,
                            stamp: now,
                        },
                    )
                }
                None => {
                    let old = inner.entries.remove(&into);
                    if old.is_some() {
                        self.forgotten.fetch_add(1, Ordering::Relaxed);
                    }
                    old
                }
            };
            if replaced.is_some_and(|old| old.stamp == CREATED) {
                self.tracked.remove(into);
            }
        }
    }

    /// Whether heap object `heap` is already recorded with base `cpu_start`
    /// (a recycled heap address has another base). Lock-free for the first
    /// sixteen heaps, which is every heap the engine binds.
    pub fn heap_known(&self, heap: usize, cpu_start: usize) -> bool {
        if self.known.iter().any(|(object, base)| {
            object.load(Ordering::Relaxed) == heap && base.load(Ordering::Relaxed) == cpu_start
        }) {
            return true;
        }
        self.inner
            .lock()
            .heaps
            .get(&heap)
            .is_some_and(|known| known.cpu_start == cpu_start)
    }

    /// Record heap object `heap`.
    pub fn heap(&self, heap: usize, geometry: Heap) {
        self.inner.lock().heaps.insert(heap, geometry);
        let slot = self
            .known
            .iter()
            .find(|(object, _)| object.load(Ordering::Relaxed) == heap)
            .or_else(|| {
                self.known
                    .iter()
                    .find(|(object, _)| object.load(Ordering::Relaxed) == 0)
            });
        if let Some((object, base)) = slot {
            // Base first: a reader matching the object then sees either the
            // new base or the old one, and the old one only fails the match.
            base.store(geometry.cpu_start, Ordering::Relaxed);
            object.store(heap, Ordering::Relaxed);
        }
    }

    /// Advance the clock; every 512 ticks drop entries that aged out.
    pub fn tick(&self) {
        let now = self.clock.fetch_add(1, Ordering::Relaxed) + 1;
        if now.is_multiple_of(512) {
            self.inner
                .lock()
                .entries
                .retain(|_, entry| entry.holds(now));
        }
    }

    /// The current clock.
    pub fn now(&self) -> u64 {
        self.clock.load(Ordering::Relaxed)
    }

    /// Resolve descriptor `offset` of the table at GPU handle `table`.
    /// `alive(resource, serial)` says whether a recorded resource still
    /// exists as that resource.
    pub fn resolve(&self, table: u64, offset: u32, alive: impl Fn(usize, u64) -> bool) -> Slot {
        let now = self.clock.load(Ordering::Relaxed);
        let inner = self.inner.lock();
        let Some(heap) = inner
            .heaps
            .values()
            .find(|heap| heap.cpu_for(table).is_some())
        else {
            return Slot::UnknownHeap;
        };
        let gpu = (offset as u64)
            .checked_mul(heap.stride as u64)
            .and_then(|delta| table.checked_add(delta));
        let Some(handle) = gpu.and_then(|gpu| heap.cpu_for(gpu)) else {
            return Slot::UnknownHeap;
        };
        match inner.entries.get(&handle) {
            None => Slot::Missing,
            Some(entry) if !entry.holds(now) => {
                self.stale.fetch_add(1, Ordering::Relaxed);
                Slot::Stale
            }
            Some(entry) if alive(entry.named.resource, entry.named.serial) => {
                Slot::Named(entry.named)
            }
            Some(_) => {
                self.dead.fetch_add(1, Ordering::Relaxed);
                Slot::Dead
            }
        }
    }

    /// Counters.
    pub fn stats(&self) -> DescriptorStats {
        let inner = self.inner.lock();
        DescriptorStats {
            views: self.views.load(Ordering::Relaxed),
            copies_followed: self.copies_followed.load(Ordering::Relaxed),
            copied: self.copied.load(Ordering::Relaxed),
            forgotten: self.forgotten.load(Ordering::Relaxed),
            stale: self.stale.load(Ordering::Relaxed),
            dead: self.dead.load(Ordering::Relaxed),
            entries: inner.entries.len() as u64,
            heaps: inner.heaps.len() as u64,
            follow_all: self.follow_all.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRIDE: usize = 32;
    const STAGING: usize = 0x10_0000;
    const VISIBLE_CPU: usize = 0x20_0000;
    const VISIBLE_GPU: u64 = 0x8000_0000;
    const COLOUR: usize = 0xc0_0000;

    fn registry() -> DescriptorRegistry {
        let registry = DescriptorRegistry::new();
        registry.heap(
            0x1,
            Heap {
                kind: 0,
                cpu_start: VISIBLE_CPU,
                gpu_start: VISIBLE_GPU,
                stride: STRIDE,
                count: 1024,
                shader_visible: true,
            },
        );
        registry
    }

    fn always(_: usize, _: u64) -> bool {
        true
    }

    #[test]
    fn a_staged_view_copied_into_a_visible_heap_resolves() {
        let registry = registry();
        registry.created(STAGING + 5 * STRIDE, COLOUR, ViewKind::Srv, Some(7));
        // Table at visible slot 100; the copy lands at its offset 9.
        registry.copy(VISIBLE_CPU + 109 * STRIDE, STAGING + 5 * STRIDE, 1, STRIDE);
        let table = VISIBLE_GPU + 100 * STRIDE as u64;
        assert_eq!(
            registry.resolve(table, 9, always),
            Slot::Named(Named {
                resource: COLOUR,
                view: ViewKind::Srv,
                serial: 7
            })
        );
        assert_eq!(registry.resolve(table, 8, always), Slot::Missing);
        assert_eq!(registry.resolve(0x10, 0, always), Slot::UnknownHeap);
        assert_eq!(registry.resolve(table, 5000, always), Slot::UnknownHeap);
    }

    #[test]
    fn copies_age_out_and_are_not_refreshed_by_copying() {
        let registry = registry();
        registry.created(STAGING, COLOUR, ViewKind::Srv, Some(1));
        registry.copy(VISIBLE_CPU, STAGING, 1, STRIDE);
        for _ in 0..FRESH_TICKS {
            registry.tick();
        }
        assert!(matches!(
            registry.resolve(VISIBLE_GPU, 0, always),
            Slot::Named(_)
        ));
        registry.tick();
        assert_eq!(registry.resolve(VISIBLE_GPU, 0, always), Slot::Stale);
        // Copying the stale entry onward clears the destination instead.
        registry.created(VISIBLE_CPU + STRIDE, COLOUR, ViewKind::Srv, Some(1));
        registry.copy(VISIBLE_CPU + STRIDE, VISIBLE_CPU, 1, STRIDE);
        assert_eq!(registry.resolve(VISIBLE_GPU, 1, always), Slot::Missing);
        // The created view is still believed at its own slot.
        registry.created(STAGING, COLOUR, ViewKind::Srv, Some(1));
        for _ in 0..10 {
            registry.tick();
        }
        registry.copy(VISIBLE_CPU + 2 * STRIDE, STAGING, 1, STRIDE);
        assert!(matches!(
            registry.resolve(VISIBLE_GPU, 2, always),
            Slot::Named(_)
        ));
    }

    #[test]
    fn dead_resources_and_overwritten_slots_are_refused() {
        let registry = registry();
        registry.created(VISIBLE_CPU, COLOUR, ViewKind::Uav, Some(3));
        assert_eq!(
            registry.resolve(VISIBLE_GPU, 0, |_, serial| serial != 3),
            Slot::Dead
        );
        registry.forget(VISIBLE_CPU);
        assert_eq!(registry.resolve(VISIBLE_GPU, 0, always), Slot::Missing);
        // A view of something not worth following forgets the slot too.
        registry.created(VISIBLE_CPU, COLOUR, ViewKind::Srv, Some(4));
        registry.created(VISIBLE_CPU, 0xd0_0000, ViewKind::Srv, None);
        assert_eq!(registry.resolve(VISIBLE_GPU, 0, always), Slot::Missing);
        let stats = registry.stats();
        assert_eq!((stats.dead, stats.forgotten), (1, 2));
    }

    #[test]
    fn copies_not_touching_followed_views_are_ignored_without_the_lock() {
        let registry = registry();
        registry.copy(VISIBLE_CPU, STAGING, 64, STRIDE);
        assert_eq!(registry.stats().copies_followed, 0);
        registry.created(STAGING + 63 * STRIDE, COLOUR, ViewKind::Srv, Some(1));
        registry.copy(VISIBLE_CPU, STAGING, 64, STRIDE);
        assert_eq!(registry.stats().copies_followed, 1);
        assert!(matches!(
            registry.resolve(VISIBLE_GPU, 63, always),
            Slot::Named(_)
        ));
    }

    #[test]
    fn a_recycled_heap_address_is_not_known_with_its_old_base() {
        let registry = registry();
        assert!(registry.heap_known(0x1, VISIBLE_CPU));
        assert!(!registry.heap_known(0x1, VISIBLE_CPU + 0x1000));
        assert!(!registry.heap_known(0x2, VISIBLE_CPU));
    }

    #[test]
    fn heap_cpu_for_bounds() {
        let heap = Heap {
            kind: 0,
            cpu_start: 0x100,
            gpu_start: 0x1000,
            stride: 32,
            count: 4,
            shader_visible: true,
        };
        assert_eq!(heap.cpu_for(0x1000 + 3 * 32 + 5), Some(0x100 + 3 * 32));
        assert_eq!(heap.cpu_for(0x1000 + 4 * 32), None);
        assert_eq!(heap.cpu_for(0xfff), None);
        let hidden = Heap {
            shader_visible: false,
            ..heap
        };
        assert_eq!(hidden.cpu_for(0x1000), None);
    }
}
