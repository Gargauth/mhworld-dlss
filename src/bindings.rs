//! Resolving descriptor tables back to resources, and root CBVs back to bytes.
//!
//! A compute dispatch says nothing about what it reads. The command list has
//! only been handed a root descriptor table -- one opaque GPU handle -- and a
//! set of root constant buffer views, which are bare GPU addresses. Recovering
//! the real textures and the real matrices takes several hooks feeding one
//! registry:
//!
//!   * `CreateShaderResourceView` / `CreateUnorderedAccessView` record which
//!     resource landed on which CPU descriptor handle;
//!   * `CopyDescriptorsSimple` / `CopyDescriptors` propagate those entries,
//!     because engines stage descriptors in a non-shader-visible heap and copy
//!     them into a per-frame shader-visible one -- without following the copy,
//!     every table in the frame resolves to nothing;
//!   * `SetDescriptorHeaps` records a heap's CPU base, GPU base and stride,
//!     which is what converts a GPU handle back into a CPU one;
//!   * `SetComputeRootDescriptorTable` records the table base in use;
//!   * `ID3D12Resource::Map` records a GPU address range against its CPU
//!     pointer, which is what makes a root CBV readable.

use crate::fasthash::AddressMap;
use parking_lot::Mutex;
use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::{
    ID3D12DescriptorHeap, ID3D12Device, ID3D12Resource, D3D12_DESCRIPTOR_HEAP_TYPE,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Srv,
    Uav,
}

/// Outcome of resolving one descriptor in a table, kept distinct so a failure
/// says *which* link of the chain is missing.
pub enum Slot {
    Resource(usize, View),
    /// The handle converted, but no view was ever recorded for it.
    Missing {
        handle: usize,
        heap_kind: i32,
    },
    /// No registered heap covers the table base.
    UnknownHeap,
}

#[derive(Clone, Copy)]
struct Descriptor {
    resource: usize,
    view: View,
    /// `CREATED` for a view made in this slot, which holds until the slot is
    /// rewritten; otherwise the tick at which a copy put it here, after
    /// which it holds for `FRESH_TICKS` and no longer.
    stamp: u64,
    /// The serial `alive` gave the resource when this was recorded. The entry
    /// names that resource and no later one at the same address.
    serial: u64,
}

impl Descriptor {
    fn holds(&self, now: u64) -> bool {
        self.stamp == CREATED || now.saturating_sub(self.stamp) <= FRESH_TICKS
    }
}

/// The stamp of a view created in its slot rather than copied into it.
const CREATED: u64 = u64::MAX;
/// How many ticks a copied entry is believed for.
///
/// The clock ticks twice a frame: at the temporal dispatch and at Present
/// (`tick`). The engine copies the descriptors of every table into a
/// per-frame heap in the frame that uses them, and cycles through three such
/// heaps, so a slot is rewritten every third frame -- six ticks. Two ticks is
/// one frame: an entry that old can still be the slot's contents, and one
/// that has reached six cannot be assumed to be. A frame of grace rather than
/// none, because the clock advances on one thread while copies are made on
/// all of them -- the same allowance `gpu::pick_depth` makes.
///
/// Present is on the clock so that it keeps running where there is no
/// temporal pass. In menus and on loading screens nothing is dispatched, and
/// a clock that stood still there would let everything copied during a long
/// loading screen pass for fresh at the first dispatch after it.
const FRESH_TICKS: u64 = 2;

/// The clock copies are stamped by.
static FRAME: AtomicU64 = AtomicU64::new(0);

/// Advance the clock, and now and then drop the copies that have aged out.
pub fn tick() {
    let now = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
    if now.is_multiple_of(512) {
        with_registry(|registry| {
            registry.descriptors.retain(|_, entry| entry.holds(now));
        });
    }
}

// ---------------------------------------------------------------------------
// Which descriptor slots are worth following, answered without a lock
// ---------------------------------------------------------------------------
//
// The registry has one consumer that matters: once a frame, the temporal
// dispatch asks what three slots of its own table hold -- the output, the
// colour and the motion vectors (`gpu::run_dlss`). Following every
// descriptor copy the engine makes to answer that would be some 440 000
// copies a frame, each a lock acquisition and two hash operations on a map
// holding every slot of three half-million-entry heaps.
//
// What that consumer can use is narrow. It skips any resource far smaller
// than the window, and it never wants a depth target. So only views of
// resources that could be an answer are recorded at creation
// (`gpu::worth_tracking`), and a copy is only followed when its source is
// one of those. Whether it is has to be decided for every descriptor of
// every copy, on every recording thread, so that question is asked of a
// table of atomics and not of the map: no lock, no write, nothing two
// threads can make each other wait for.
//
// A copy that is not followed leaves whatever the destination slot was last
// known to hold. That is what the stamp is for: a copied entry is believed
// only in the frame it was made or the next, so a slot overwritten by
// something not worth following answers "nothing" soon after rather than
// naming a resource from an earlier scene.

/// Slots in the table. A power of two; half of it may fill.
const TRACKED_SLOTS: usize = 1 << 15;
const EMPTY: usize = 0;
/// A slot that held a handle since forgotten. Probing continues past it.
const DEAD: usize = 1;

/// The handles of views created in place and worth following. Open
/// addressing, linear probing. Written only under the registry's lock; read
/// from anywhere.
static TRACKED: [AtomicUsize; TRACKED_SLOTS] = [const { AtomicUsize::new(EMPTY) }; TRACKED_SLOTS];
/// Slots no longer `EMPTY`, live or dead.
static TRACKED_USED: AtomicUsize = AtomicUsize::new(0);
/// Set if the table ever fills past half, after which every copy is followed
/// under the registry's lock instead. Slower, and still right.
static TRACK_ALL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Table entries found at the wanted slots and not believed, for the line
/// that explains a frame being left alone.
static STALE: AtomicU64 = AtomicU64::new(0);
/// Entries refused because the resource they named has been destroyed, or
/// replaced by another at the same address.
static DEAD_REFUSED: AtomicU64 = AtomicU64::new(0);

#[inline]
fn first_slot(handle: usize) -> usize {
    crate::fasthash::mix(handle as u64) as usize & (TRACKED_SLOTS - 1)
}

/// Whether a view worth following was created at `handle`.
#[inline]
fn tracked(handle: usize) -> bool {
    let mut slot = first_slot(handle);
    for _ in 0..TRACKED_SLOTS {
        let held = TRACKED[slot].load(Ordering::Relaxed);
        if held == handle {
            return true;
        }
        if held == EMPTY {
            return false;
        }
        slot = (slot + 1) & (TRACKED_SLOTS - 1);
    }
    false
}

/// Add a handle. Only with the registry's lock held.
fn track(handle: usize) {
    let mut slot = first_slot(handle);
    let mut reuse = None;
    for _ in 0..TRACKED_SLOTS {
        let held = TRACKED[slot].load(Ordering::Relaxed);
        if held == handle {
            return;
        }
        if held == DEAD && reuse.is_none() {
            reuse = Some(slot);
        }
        if held == EMPTY {
            break;
        }
        slot = (slot + 1) & (TRACKED_SLOTS - 1);
    }
    match reuse {
        Some(dead) => TRACKED[dead].store(handle, Ordering::Relaxed),
        None => {
            TRACKED[slot].store(handle, Ordering::Relaxed);
            let used = TRACKED_USED.fetch_add(1, Ordering::Relaxed) + 1;
            if used > TRACKED_SLOTS / 2 && !TRACK_ALL.swap(true, Ordering::Relaxed) {
                crate::log!(
                    "[bind] more than {} views worth following; every descriptor copy is \
                     followed under the lock from here on, as it used to be. Correct, and \
                     slower",
                    TRACKED_SLOTS / 2
                );
            }
        }
    }
}

/// Remove a handle. Only with the registry's lock held.
fn untrack(handle: usize) {
    let mut slot = first_slot(handle);
    for _ in 0..TRACKED_SLOTS {
        let held = TRACKED[slot].load(Ordering::Relaxed);
        if held == handle {
            TRACKED[slot].store(DEAD, Ordering::Relaxed);
            return;
        }
        if held == EMPTY {
            return;
        }
        slot = (slot + 1) & (TRACKED_SLOTS - 1);
    }
}

#[derive(Clone, Copy)]
struct HeapRange {
    kind: i32,
    cpu_start: usize,
    gpu_start: u64,
    stride: usize,
    count: u32,
    shader_visible: bool,
}

impl HeapRange {
    fn cpu_for(&self, gpu: u64) -> Option<usize> {
        if self.stride == 0 || gpu < self.gpu_start {
            return None;
        }
        let offset = (gpu - self.gpu_start) as usize;
        let index = offset / self.stride;
        if index >= self.count as usize {
            return None;
        }
        Some(self.cpu_start + index * self.stride)
    }
}

/// A persistently mapped buffer, for reading root constant buffers.
#[derive(Clone, Copy)]
struct Mapping {
    gpu: u64,
    size: u64,
    cpu: usize,
    /// Outstanding `Map` calls. D3D12 reference-counts them, so the pointer
    /// stays good until as many `Unmap` calls have been made.
    depth: u32,
}

/// How many of each hook fired, so a broken link in the chain shows up as a
/// count rather than as a silent "unknown descriptor".
#[derive(Default)]
struct Counters {
    srv_views: u64,
    uav_views: u64,
    /// Copies followed under the lock: the ones that touched a view worth
    /// following. The rest are not counted, which is the point of them.
    copy_calls: u64,
    copies_hit: u64,
    copies_missed: u64,
    /// Descriptor entries dropped because the slot was overwritten by
    /// something this registry cannot describe.
    forgotten: u64,
    /// Mappings dropped because the buffer was unmapped.
    unmapped: u64,
}

#[derive(Default)]
struct Registry {
    descriptors: AddressMap<usize, Descriptor>,
    heaps: AddressMap<usize, HeapRange>,
    mappings: Vec<Mapping>,
    counters: Counters,
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);
static DEVICE: Mutex<Option<ID3D12Device>> = Mutex::new(None);

fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let wait = crate::perf::start(crate::perf::Family::RegistryWait);
    let mut guard = REGISTRY.lock();
    wait.stop();
    f(guard.get_or_insert_with(Registry::default))
}

pub fn set_device(device: ID3D12Device) {
    *DEVICE.lock() = Some(device);
}

/// Make `device` the one resources of our own are created on, if it is not
/// already, and say whether it changed.
///
/// The hooks are installed through a device this library creates for itself,
/// and that device was then also what the interface-free frame copy and the
/// test mask were created on -- textures that are used on the engine's
/// command lists. On one adapter Direct3D hands every caller the same device
/// object, so it was the engine's by coincidence of there being one GPU. The
/// engine's device is known for certain where it creates its swapchain, from
/// the queue it presents with, and that is where this is called from.
pub fn adopt_device(device: ID3D12Device) -> bool {
    let mut guard = DEVICE.lock();
    if guard
        .as_ref()
        .is_some_and(|held| held.as_raw() == device.as_raw())
    {
        return false;
    }
    *guard = Some(device);
    // The descriptor strides are the device's, and were asked of the previous one.
    for stride in &STRIDES {
        stride.store(0, Ordering::Relaxed);
    }
    true
}

/// The device the hooks were installed through, for anything that has to make
/// resources of its own.
///
/// A clone rather than a borrow, because the alternative is holding this mutex
/// across resource creation -- and `stride_for` takes the same lock from every
/// descriptor table resolution.
pub fn device() -> Option<ID3D12Device> {
    DEVICE.lock().clone()
}

/// Descriptor stride for a heap type.
///
/// Asked of the device once per type and remembered. It is a constant of the
/// device, and this is called for every `CopyDescriptors` -- several hundred
/// thousand times a frame in this engine -- where asking meant a mutex and a
/// COM call each time. Zero is not remembered: it is what an unset device
/// answers, and the next call has to ask again.
pub fn stride_for(kind: D3D12_DESCRIPTOR_HEAP_TYPE) -> usize {
    let cached = STRIDES.get(kind.0 as usize);
    if let Some(slot) = cached {
        let stride = slot.load(Ordering::Relaxed);
        if stride != 0 {
            return stride;
        }
    }
    let stride = match DEVICE.lock().as_ref() {
        Some(device) => unsafe { device.GetDescriptorHandleIncrementSize(kind) as usize },
        None => 0,
    };
    if let Some(slot) = cached {
        slot.store(stride, Ordering::Relaxed);
    }
    stride
}

/// One per `D3D12_DESCRIPTOR_HEAP_TYPE`: CBV_SRV_UAV, SAMPLER, RTV, DSV.
static STRIDES: [AtomicUsize; 4] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

pub fn record_view(resource: *mut c_void, handle: usize, view: View) {
    if resource.is_null() || handle == 0 {
        return;
    }
    // A view of something the consumer could never use is not recorded --
    // but the slot has still been rewritten, so whatever it held is gone.
    // (`TRACK_ALL` widens which copies are followed, never which views are
    // recorded: that is decided here, by the same test, always.)
    //
    // Nor is a view of a resource `alive` has no living witness for, whatever
    // else is true of it: an entry that cannot later be shown to name a
    // resource that still exists must never be made.
    let Some(serial) = crate::gpu::worth_tracking(resource as usize) else {
        forget_view(handle);
        return;
    };
    with_registry(|registry| {
        match view {
            View::Srv => registry.counters.srv_views += 1,
            View::Uav => registry.counters.uav_views += 1,
        }
        registry.descriptors.insert(
            handle,
            Descriptor {
                resource: resource as usize,
                view,
                stamp: CREATED,
                serial,
            },
        );
        track(handle);
    });
}

/// Drop whatever was recorded for a descriptor slot.
///
/// Heap slots are recycled constantly. When a slot that once held a shader
/// resource view is rewritten as a render target, a depth stencil, a sampler
/// or a constant buffer view, the entry here would otherwise keep naming the
/// old resource -- long after the engine released it. `resolve_table` would
/// then hand that dead pointer to the caller, which passes it to NGX and the
/// driver. Forgetting the slot turns that into a `Missing`, and a missing
/// input makes the frame be left alone instead.
pub fn forget_view(handle: usize) {
    if handle == 0 {
        return;
    }
    // Nothing created here is being followed, so there is nothing to drop
    // that will not drop itself: a copied entry in this slot stops being
    // believed by its stamp.
    if !TRACK_ALL.load(Ordering::Relaxed) && !tracked(handle) {
        return;
    }
    with_registry(|registry| {
        if registry.descriptors.remove(&handle).is_some() {
            registry.counters.forgotten += 1;
        }
        untrack(handle);
    });
}

/// Samplers carry no resource, but counting them identifies sampler heaps.
pub fn record_sampler() {
    SAMPLERS.fetch_add(1, Ordering::Relaxed);
}

static SAMPLERS: AtomicU64 = AtomicU64::new(0);

/// Follow a descriptor copy so staged views stay attributable.
pub fn copy_views(destination: usize, source: usize, count: u32, stride: usize) {
    if stride == 0 || destination == 0 || source == 0 {
        return;
    }
    // The question that decides nearly every call, asked without the lock:
    // does this run copy from a view worth following, or over one? If
    // neither, there is nothing to record and nothing to drop.
    if !TRACK_ALL.load(Ordering::Relaxed) {
        let touches = (0..count as usize)
            .any(|step| tracked(source + step * stride) || tracked(destination + step * stride));
        if !touches {
            return;
        }
    }
    with_registry(|registry| {
        let frame = FRAME.load(Ordering::Relaxed);
        registry.counters.copy_calls += 1;
        for step in 0..count as usize {
            let from = source + step * stride;
            let into = destination + step * stride;
            if from == into {
                continue;
            }
            // Only from an entry that is itself believed: a copied entry gone
            // stale must not be made fresh again by being copied onward.
            let entry = registry
                .descriptors
                .get(&from)
                .copied()
                .filter(|entry| entry.holds(frame));
            let replaced = match entry {
                Some(entry) => {
                    registry.counters.copies_hit += 1;
                    registry.descriptors.insert(
                        into,
                        Descriptor {
                            stamp: frame,
                            ..entry
                        },
                    )
                }
                // The destination has just been overwritten with something
                // this registry does not follow. The old entry must go: left
                // in place, a per-frame heap slot would keep naming a
                // resource from an earlier scene, to be handed on as if it
                // were this frame's input.
                None => {
                    registry.counters.copies_missed += 1;
                    let old = registry.descriptors.remove(&into);
                    if old.is_some() {
                        registry.counters.forgotten += 1;
                    }
                    old
                }
            };
            // A view created in the destination slot is no longer what the
            // slot holds.
            if replaced.is_some_and(|old| old.stamp == CREATED) {
                untrack(into);
            }
        }
    });
}

/// Record a heap's geometry the first time it is seen.
///
/// # Safety
/// `heap` must be a live `ID3D12DescriptorHeap`.
pub unsafe fn record_heap(heap: *mut c_void) {
    if heap.is_null() {
        return;
    }
    let Some(heap_ref) = ID3D12DescriptorHeap::from_raw_borrowed(&heap) else {
        return;
    };
    // A recorded entry is only reusable if it still describes *this* heap. A
    // destroyed heap's address gets recycled, and keeping the dead one's
    // bases would make every GPU-to-processor handle conversion resolve into
    // another heap's descriptors. Comparing the base is a cheap vtable call,
    // where re-reading the whole description on every bind would not be.
    let base = heap_ref.GetCPUDescriptorHandleForHeapStart().ptr;
    // Asked first of a short list that needs no lock. This is called for
    // every heap of every SetDescriptorHeaps -- some four thousand times a
    // frame -- and the answer is nearly always "already known", which must
    // not take the registry's lock and queue behind the descriptor copies
    // holding it.
    if KNOWN_HEAPS.iter().any(|(known, start)| {
        known.load(Ordering::Relaxed) == heap as usize && start.load(Ordering::Relaxed) == base
    }) {
        return;
    }
    if with_registry(|registry| {
        registry
            .heaps
            .get(&(heap as usize))
            .is_some_and(|range| range.cpu_start == base)
    }) {
        remember_heap(heap as usize, base);
        return;
    }
    let desc = heap_ref.GetDesc();
    let stride = stride_for(desc.Type);
    if stride == 0 {
        return;
    }
    // GetGPUDescriptorHandleForHeapStart is only valid on shader-visible heaps.
    let shader_visible = desc.Flags.0 & 1 != 0;
    let range = HeapRange {
        kind: desc.Type.0,
        cpu_start: base,
        gpu_start: if shader_visible {
            heap_ref.GetGPUDescriptorHandleForHeapStart().ptr
        } else {
            0
        },
        stride,
        count: desc.NumDescriptors,
        shader_visible,
    };
    crate::logv!(
        "[heap] type={} visible={} count={:<7} stride={} cpu={:#x} gpu={:#x}",
        range.kind,
        range.shader_visible,
        range.count,
        range.stride,
        range.cpu_start,
        range.gpu_start
    );
    with_registry(|registry| registry.heaps.insert(heap as usize, range));
    remember_heap(heap as usize, base);
}

/// Heaps already recorded, with the base each was recorded at, for
/// `record_heap` to consult without the lock. A pair is two separate atomics,
/// so a reader can catch one half-written; it then finds no match and takes
/// the locked path, which is always right.
static KNOWN_HEAPS: [(AtomicUsize, AtomicUsize); 16] =
    [const { (AtomicUsize::new(0), AtomicUsize::new(0)) }; 16];

/// Note a heap as recorded at `base`: in the slot it already has, else in a
/// free one, else not at all -- past sixteen heaps the lock is simply taken.
fn remember_heap(heap: usize, base: usize) {
    let slot = KNOWN_HEAPS
        .iter()
        .find(|(known, _)| known.load(Ordering::Relaxed) == heap)
        .or_else(|| {
            KNOWN_HEAPS
                .iter()
                .find(|(known, _)| known.load(Ordering::Relaxed) == 0)
        });
    if let Some((known, start)) = slot {
        start.store(base, Ordering::Relaxed);
        known.store(heap, Ordering::Relaxed);
    }
}

/// Record a mapped buffer so its root CBV addresses become readable.
///
/// # Safety
/// `resource` must be a live `ID3D12Resource` and `cpu` its mapped pointer.
pub unsafe fn record_mapping(resource: *mut c_void, cpu: *mut c_void) {
    if resource.is_null() || cpu.is_null() {
        return;
    }
    let Some(resource_ref) = ID3D12Resource::from_raw_borrowed(&resource) else {
        return;
    };
    let desc = resource_ref.GetDesc();
    let gpu = resource_ref.GetGPUVirtualAddress();
    if gpu == 0 {
        return;
    }
    with_registry(|registry| {
        // Replace rather than keep the first. An upload heap can be unmapped,
        // released and its address reused, and a second `Map` on the same
        // range is the engine telling us where the memory lives *now*. The
        // old entry would otherwise still be used, writing the jitter through
        // a pointer into memory that belongs to something else.
        if let Some(existing) = registry.mappings.iter_mut().find(|m| m.gpu == gpu) {
            existing.size = desc.Width;
            existing.cpu = cpu as usize;
            existing.depth = existing.depth.saturating_add(1);
            return;
        }
        registry.mappings.push(Mapping {
            gpu,
            size: desc.Width,
            cpu: cpu as usize,
            depth: 1,
        });
    });
    // After the change, whichever branch made it: every thread's remembered
    // mapping is now out of date.
    MAPPINGS_CHANGED.fetch_add(1, Ordering::Release);
}

/// Drop a mapping once the engine has unmapped it as many times as it mapped
/// it.
///
/// Without this the registry kept handing back a processor pointer into
/// memory the runtime had already taken away, and `write_gpu_memory` wrote
/// the jitter straight into it -- silent corruption of whatever occupied that
/// address next, which is not a failure that announces itself.
///
/// # Safety
/// `resource` must be a live `ID3D12Resource`.
pub unsafe fn forget_mapping(resource: *mut c_void) {
    if resource.is_null() {
        return;
    }
    let Some(resource_ref) = ID3D12Resource::from_raw_borrowed(&resource) else {
        return;
    };
    let gpu = resource_ref.GetGPUVirtualAddress();
    if gpu == 0 {
        return;
    }
    with_registry(|registry| {
        let Some(index) = registry.mappings.iter().position(|m| m.gpu == gpu) else {
            return;
        };
        let mapping = &mut registry.mappings[index];
        mapping.depth = mapping.depth.saturating_sub(1);
        if mapping.depth == 0 {
            registry.mappings.swap_remove(index);
            registry.counters.unmapped += 1;
        }
    });
    MAPPINGS_CHANGED.fetch_add(1, Ordering::Release);
}

/// Counts changes to the set of mappings. Starts at one so a thread's first
/// lookup, whose remembered mapping is stamped zero, always asks.
static MAPPINGS_CHANGED: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The mapping this thread last found an address in, and the count of
    /// changes it was found at.
    static LAST_MAPPING: Cell<(u64, Mapping)> =
        const { Cell::new((0, Mapping { gpu: 0, size: 0, cpu: 0, depth: 0 })) };
}

/// The recorded mapping covering `len` bytes at `address`.
///
/// Without the registry's lock when the answer is the one this thread got
/// last time, which it nearly always is: the engine sub-allocates its
/// constant buffers from a few large upload heaps, so consecutive binds land
/// in the same mapping. The lock matters more than its own cost suggests,
/// because it is the lock every descriptor copy holds: taking it here would
/// queue a constant buffer bind behind the copies of every other recording
/// thread.
///
/// A remembered mapping is used only while no mapping has been added,
/// replaced or removed since it was found. One that is removed between that
/// check and the read is a window the lock would leave open too: the mapping
/// is copied out and the lock released before the memory is touched.
fn mapping_for(address: u64, len: usize) -> Option<Mapping> {
    let covers = |m: &Mapping| address >= m.gpu && address + len as u64 <= m.gpu + m.size;
    let changed = MAPPINGS_CHANGED.load(Ordering::Acquire);
    let (seen, last) = LAST_MAPPING.with(|cell| cell.get());
    if seen == changed && covers(&last) {
        return Some(last);
    }
    let found = with_registry(|registry| registry.mappings.iter().find(|m| covers(m)).copied())?;
    // Stamped with the count read before the search, so a change landing in
    // between leaves an old stamp and the next lookup searches again.
    LAST_MAPPING.with(|cell| cell.set((changed, found)));
    Some(found)
}

/// Read bytes behind a GPU virtual address, via a recorded mapping.
pub fn read_gpu_memory(address: u64, len: usize) -> Option<Vec<u8>> {
    let mapping = mapping_for(address, len)?;
    let offset = (address - mapping.gpu) as usize;
    let source = (mapping.cpu + offset) as *const u8;
    // A mapping is dropped when the engine unmaps the buffer, but a buffer
    // may also be released while still mapped, which Direct3D allows, and
    // then nothing told this registry: the entry stays, pointing at memory
    // the runtime has taken back. So the memory is asked about before it is
    // read, and an entry whose memory has gone is dropped here. One system
    // call per read; this is read a few times a frame, from the temporal
    // dispatch, and never from a descriptor copy or a bind.
    // The whole range in one region is the usual case and one call. A range
    // that crosses from one region into another is still good if both of its
    // ends are there.
    let there = unsafe {
        crate::native::readable(source as usize, len)
            || (crate::native::readable(source as usize, 1)
                && crate::native::readable(source as usize + len.saturating_sub(1), 1))
    };
    if !there {
        with_registry(|registry| {
            registry
                .mappings
                .retain(|m| !(m.gpu == mapping.gpu && m.cpu == mapping.cpu));
            registry.counters.unmapped += 1;
        });
        MAPPINGS_CHANGED.fetch_add(1, Ordering::Release);
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[map] the memory a recorded mapping pointed at ({:#x}, {} bytes for GPU \
                 address {:#x}) is no longer there: the buffer was released without being \
                 unmapped. The mapping is dropped and nothing was read from it",
                mapping.cpu,
                mapping.size,
                mapping.gpu
            );
        }
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(source, len) }.to_vec())
}

/// Resolve `count` consecutive descriptors starting at a table base.
pub fn resolve_table(base: u64, count: usize) -> Vec<Slot> {
    with_registry(|registry| {
        let frame = FRAME.load(Ordering::Relaxed);
        let range = registry
            .heaps
            .values()
            .find(|range| range.shader_visible && range.cpu_for(base).is_some())
            .copied();

        let Some(range) = range else {
            return (0..count).map(|_| Slot::UnknownHeap).collect();
        };

        (0..count)
            .map(|step| {
                let gpu = base + (step * range.stride) as u64;
                let Some(handle) = range.cpu_for(gpu) else {
                    return Slot::UnknownHeap;
                };
                match registry.descriptors.get(&handle) {
                    // Believed only if it is fresh *and* the resource it was
                    // recorded for still exists as that resource. The second
                    // is not inferred: `alive` is told by the runtime when a
                    // resource is destroyed. An entry from before the engine
                    // rebuilt its targets fails here, whatever else is true.
                    Some(entry) if !entry.holds(frame) => {
                        STALE.fetch_add(1, Ordering::Relaxed);
                        Slot::Missing {
                            handle,
                            heap_kind: range.kind,
                        }
                    }
                    Some(entry) if crate::alive::is(entry.resource, entry.serial) => {
                        Slot::Resource(entry.resource, entry.view)
                    }
                    Some(_) => {
                        DEAD_REFUSED.fetch_add(1, Ordering::Relaxed);
                        Slot::Missing {
                            handle,
                            heap_kind: range.kind,
                        }
                    }
                    None => Slot::Missing {
                        handle,
                        heap_kind: range.kind,
                    },
                }
            })
            .collect()
    })
}

/// Registry state, for diagnosing which link of the chain is missing.
pub fn summary() -> String {
    with_registry(|registry| {
        let visible = registry.heaps.values().filter(|h| h.shader_visible).count();
        let c = &registry.counters;
        format!(
            "{} descriptors, {} heaps ({} visible), {} mappings | \
             followed views: srv={} uav={}; samplers={}; followed copies={} (hit {} miss {}) \
             forgotten={} unmapped={} | {} of {} filter slots used{}, {} stale entries refused, \
             {} refused for naming a destroyed resource; {}",
            registry.descriptors.len(),
            registry.heaps.len(),
            visible,
            registry.mappings.len(),
            c.srv_views,
            c.uav_views,
            SAMPLERS.load(Ordering::Relaxed),
            c.copy_calls,
            c.copies_hit,
            c.copies_missed,
            c.forgotten,
            c.unmapped,
            TRACKED_USED.load(Ordering::Relaxed),
            TRACKED_SLOTS,
            if TRACK_ALL.load(Ordering::Relaxed) {
                " (full: following everything)"
            } else {
                ""
            },
            STALE.load(Ordering::Relaxed),
            DEAD_REFUSED.load(Ordering::Relaxed),
            crate::alive::summary()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(handle: usize) -> Option<Descriptor> {
        with_registry(|registry| registry.descriptors.get(&handle).copied())
    }

    /// Stand in for `record_view` on a view worth following, without the
    /// resource description that decision is normally read from.
    fn create(handle: usize, resource: usize) {
        with_registry(|registry| {
            registry.descriptors.insert(
                handle,
                Descriptor {
                    resource,
                    view: View::Srv,
                    stamp: CREATED,
                    serial: 0,
                },
            );
            track(handle);
        });
    }

    /// One test, because the registry, the filter and the clock are shared by
    /// everything in this module and the order of events is the whole point.
    #[test]
    fn copies_are_followed_only_from_views_worth_following() {
        const STRIDE: usize = 32;
        // Handles nothing else uses. Staging slots, then per-frame slots.
        let staging = 0x5000_0000usize;
        let frame_heap = 0x7000_0000usize;

        // The filter by itself: present, absent, forgotten, and a forgotten
        // slot taken up again.
        assert!(!tracked(staging));
        create(staging, 0xAAA0);
        assert!(tracked(staging));
        with_registry(|_| untrack(staging));
        assert!(!tracked(staging));
        let used = TRACKED_USED.load(Ordering::Relaxed);
        create(staging, 0xAAA0);
        assert!(tracked(staging));
        assert_eq!(
            TRACKED_USED.load(Ordering::Relaxed),
            used,
            "a dead slot was not reused"
        );

        // A copy from a followed view is recorded, stamped with the clock.
        let now = FRAME.load(Ordering::Relaxed);
        copy_views(frame_heap, staging, 1, STRIDE);
        let entry = held(frame_heap).expect("the copy was not followed");
        assert_eq!(entry.resource, 0xAAA0);
        assert_eq!(entry.stamp, now);
        assert!(entry.holds(now) && entry.holds(now + FRESH_TICKS));
        assert!(
            !entry.holds(now + FRESH_TICKS + 1),
            "a copied entry never goes stale"
        );

        // A copy from a view not worth following, over that slot, is not
        // followed at all: the entry is left, and it is its age that stops it
        // being believed.
        copy_views(frame_heap, staging + 64 * STRIDE, 1, STRIDE);
        assert!(held(frame_heap).is_some());

        // The same over a slot a followed view was *created* in must drop it,
        // because a created entry has no age to go stale by.
        let created_in_place = frame_heap + 8 * STRIDE;
        create(created_in_place, 0xBBB0);
        copy_views(created_in_place, staging + 64 * STRIDE, 1, STRIDE);
        assert!(
            held(created_in_place).is_none(),
            "an overwritten view is still named"
        );
        assert!(!tracked(created_in_place));

        // A run is followed whole if any descriptor in it is: here the second
        // of three comes from a followed view and its neighbours do not.
        let run = frame_heap + 16 * STRIDE;
        create(staging + STRIDE, 0xCCC0);
        with_registry(|registry| {
            registry.descriptors.remove(&staging);
            untrack(staging);
        });
        copy_views(run, staging, 3, STRIDE);
        assert!(held(run).is_none());
        assert_eq!(held(run + STRIDE).map(|entry| entry.resource), Some(0xCCC0));
        assert!(held(run + 2 * STRIDE).is_none());

        // And a copied entry that has gone stale is not made fresh again by
        // being copied onward.
        let stale = frame_heap + 32 * STRIDE;
        with_registry(|registry| {
            registry.descriptors.insert(
                stale,
                Descriptor {
                    resource: 0xDDD0,
                    view: View::Srv,
                    stamp: 0,
                    serial: 0,
                },
            );
        });
        for _ in 0..=FRESH_TICKS {
            FRAME.fetch_add(1, Ordering::Relaxed);
        }
        create(stale + STRIDE, 0xEEE0);
        copy_views(frame_heap + 48 * STRIDE, stale, 2, STRIDE);
        assert!(
            held(frame_heap + 48 * STRIDE).is_none(),
            "a stale entry was copied onward"
        );
        assert_eq!(
            held(frame_heap + 49 * STRIDE).map(|entry| entry.resource),
            Some(0xEEE0)
        );
    }
}
