//! The render hooks: find the engine's temporal pass and everything it
//! reads, and replace it with DLSS.
//!
//! Creates a D3D12 device of our own purely to read vtables out of it, then
//! patches the slots we need so the game's own command lists route through
//! here. Most of the hooks only record what the engine binds. The dispatch
//! hook does more: when DLSS is ready and the dispatch is the temporal pass,
//! it records the motion vector decode and the DLSS evaluation on the
//! engine's list and does not let the engine's own dispatch run.
//!
//! The pass is recognised by the *shape* of its shader bindings rather than by
//! a bytecode hash, so a game patch that recompiles the shader does not break
//! detection.

use crate::bindings::{self, Slot, View};
use crate::dxbc::{self, Kind, Shader};
use crate::fasthash::AddressMap;
use crate::slot;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use windows::core::Interface;
use windows::core::HRESULT;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIFactory4, DXGI_ADAPTER_FLAG_SOFTWARE};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_F10};

/// Dispatch events logged per capture burst.
const BURST: usize = 6000;
/// Detailed binding reports per burst. One is plenty: it is verbose.
const REPORTS: usize = 1;
/// Bytes of a root constant buffer to read. Large enough for the whole of
/// CBViewProjection, whose fields are then printed by name.
const CB_DUMP: usize = 4096;
/// How far past a root table base to look for its descriptor ranges. A table
/// may hold several ranges, each placed at its own offset from the base, so
/// the SRVs are not necessarily right behind the UAV.
const TABLE_SCAN: usize = 96;

/// Where the temporal pass's table holds what DLSS reads, from its root
/// signature (`root0[u0..8@+0 t0..31@+9]`): +9 is t0, `tBaseMap`, this
/// frame's colour; +13 is t4, `tGBuffer4`, the packed motion vectors.
/// Measured. +10 is the pass's own history, and feeding DLSS that smears.
const COLOUR_OFFSET: usize = 9;
const MV_OFFSET: usize = 13;
/// How `tGBuffer4` packs its two components: 2 is two half floats with
/// horizontal in the high half. Measured with `mv_probe`.
const MV_MODE: u32 = 2;
/// How much of the jitter the decode adds back to the vectors, as shipped:
/// none. The engine applies its jitter matrix to the previous frame's
/// projection as well as the current one, so its vectors are already
/// jitter-free (see `native.rs`); adding the jitter here would apply it a
/// second time. `expert_mv_dejitter` is the key.
pub(crate) const MV_DEJITTER: f32 = 0.0;
/// Multiplier on the jitter reported to DLSS, as shipped: the projection
/// shift is +ndc because this projection has w = -z, and the viewport then
/// flips Y. `expert_jitter_report_x` and `_y` are the keys.
pub(crate) const JITTER_REPORT: (f32, f32) = (1.0, -1.0);

static BUDGET: AtomicUsize = AtomicUsize::new(0);
static REPORT_BUDGET: AtomicUsize = AtomicUsize::new(0);
static CAMERA_BUDGET: AtomicUsize = AtomicUsize::new(0);
static BURST_ID: AtomicU64 = AtomicU64::new(0);
static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static O_SET_PIPELINE: AtomicUsize = AtomicUsize::new(0);
static O_DISPATCH: AtomicUsize = AtomicUsize::new(0);
static O_CLOSE: AtomicUsize = AtomicUsize::new(0);
static O_EXECUTE_COMMAND_LISTS: AtomicUsize = AtomicUsize::new(0);
static O_SET_HEAPS: AtomicUsize = AtomicUsize::new(0);
static O_SET_ROOT_TABLE: AtomicUsize = AtomicUsize::new(0);
static O_SET_ROOT_SIG: AtomicUsize = AtomicUsize::new(0);
static O_SET_ROOT_CBV: AtomicUsize = AtomicUsize::new(0);
static O_SET_ROOT_SRV: AtomicUsize = AtomicUsize::new(0);
static O_SET_ROOT_UAV: AtomicUsize = AtomicUsize::new(0);
static O_SET_ROOT_CONSTANTS: AtomicUsize = AtomicUsize::new(0);
static O_CREATE_COMPUTE_PSO: AtomicUsize = AtomicUsize::new(0);
static O_CREATE_SRV: AtomicUsize = AtomicUsize::new(0);
static O_CREATE_UAV: AtomicUsize = AtomicUsize::new(0);
static O_COPY_DESCRIPTORS: AtomicUsize = AtomicUsize::new(0);
static O_COPY_DESCRIPTORS_SIMPLE: AtomicUsize = AtomicUsize::new(0);
static O_MAP: AtomicUsize = AtomicUsize::new(0);
static O_UNMAP: AtomicUsize = AtomicUsize::new(0);
static O_CREATE_CBV: AtomicUsize = AtomicUsize::new(0);
static O_CREATE_SAMPLER: AtomicUsize = AtomicUsize::new(0);
static O_CREATE_ROOT_SIG: AtomicUsize = AtomicUsize::new(0);
static O_CREATE_DSV: AtomicUsize = AtomicUsize::new(0);
static O_CREATE_RTV: AtomicUsize = AtomicUsize::new(0);
static O_OM_SET_TARGETS: AtomicUsize = AtomicUsize::new(0);
static O_DRAW_INDEXED: AtomicUsize = AtomicUsize::new(0);
static O_DRAW: AtomicUsize = AtomicUsize::new(0);
static O_RESOURCE_BARRIER: AtomicUsize = AtomicUsize::new(0);

/// Depth-stencil descriptor handle -> the resource behind it.
///
/// Six full-resolution depth buffers exist; only one belongs to the main
/// camera. Resolving the handle that OMSetRenderTargets binds is what tells
/// them apart.
static DSV_HANDLES: Mutex<Option<AddressMap<usize, usize>>> = Mutex::new(None);

/// The same for render target views.
///
/// `OMSetRenderTargets` is given descriptor handles, and the interface-free
/// frame hunt needs the resource behind the first of them. Recorded at view
/// creation for the same reason the depth map is: a handle resolves to nothing
/// otherwise, and the engine never hands the resource to the bind call.
static RTV_HANDLES: Mutex<Option<AddressMap<usize, usize>>> = Mutex::new(None);
/// The depth resource most recently bound for rendering, and the frame it was
/// bound on.
///
/// The frame matters. Holding the pointer across frames handed DLSS a freed
/// resource when the engine recreated its targets during a scene transition,
/// which took the process down about thirty seconds in. Colour, output and
/// motion vectors never had this problem because they are resolved from the
/// descriptor table every frame; depth is the one input that came from
/// somewhere else.
static LAST_DEPTH: Mutex<(usize, u64)> = Mutex::new((0, 0));
/// Every depth target bound recently, as resource -> (frame, width, height).
///
/// "The last one bound, if it is at least 1920x1080" picked up a 2048x2048
/// shadow map: the engine binds shadow cascades after the scene depth, and on
/// several threads at that. Size alone cannot separate them, but the scene
/// depth is the one whose dimensions match the rectangle being dispatched, so
/// the choice is deferred until that is known.
static DEPTHS: Mutex<Option<AddressMap<usize, (u64, u32, u32)>>> = Mutex::new(None);
/// The motion vector descriptor last reported, so a change is logged once.
static REPORTED_MV: AtomicUsize = AtomicUsize::new(usize::MAX);
/// Latches, so a persistent reason to skip is reported once rather than every
/// frame, and never not at all.
static NO_DEPTH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static MISSING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static UNKNOWN_STATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static REPORTED_STATES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CLOSE_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static STILL_BOUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Evaluations done and temporal dispatches seen. A picture that trembles while
/// the camera is still means some frames keep the jitter without resolving it,
/// and only the ratio of these two says whether that is what is happening.
/// Whether our hooks fire at all, and how often.
///
/// Three numbers rather than one, because they separate three different
/// failures that look identical from outside. A run where `[dls]` is zero can
/// mean our command-list hooks are on objects the engine never uses, or that
/// they fire but no pipeline was ever recognised, or that pipelines were
/// recognised but the pass never dispatched -- and those have nothing in
/// common as fixes.
///
/// Kept permanently. The question "do the hooks fire" has now been asked twice
/// from different causes and answered by inference both times.
static ALL_DISPATCHES: AtomicU64 = AtomicU64::new(0);
static LISTS_CLOSED: AtomicU64 = AtomicU64::new(0);
static PSOS_SEEN: AtomicU64 = AtomicU64::new(0);

/// What the hooks have seen, for the periodic report.
pub fn activity() -> (u64, u64, u64, u64) {
    (
        ALL_DISPATCHES.load(Ordering::Relaxed),
        LISTS_CLOSED.load(Ordering::Relaxed),
        PSOS_SEEN.load(Ordering::Relaxed),
        PSOS.lock()
            .as_ref()
            .map(|known| known.len() as u64)
            .unwrap_or(0),
    )
}

static EVALUATED: AtomicU64 = AtomicU64::new(0);
static DISPATCHES: AtomicU64 = AtomicU64::new(0);
/// The inputs have been described once; describing them every frame would
/// bury the log.
static DESCRIBED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static MISMATCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ALIASED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The focal terms of the view the temporal pass actually resolves.
///
/// Measured, and it settled the artefact: six different views were being
/// offset, four of them square -- cubemap faces, probes, the perspective
/// shadow projections of point and spot lights, none of which the shape test
/// excludes, since only directional cascades are orthographic. An offset
/// computed for a 3840-wide frame shifts a 2048-square shadow map every frame,
/// and a shadow map that moves gives self-shadowing on any surface at a
/// grazing angle to its light. Walls, roofs, snow.
///
/// The view being resolved is the one whose camera the temporal dispatch
/// binds, so the pass names it rather than a heuristic guessing at it. Until
/// the first dispatch nothing is offset, which costs one frame of jitter and
/// no correctness: an offset of zero is a valid offset.
static RESOLVED_VIEW: Mutex<Option<(f32, f32)>> = Mutex::new(None);

/// How many bytes CBViewProjection declares, from the shader's reflection.
///
/// Seeded with the measured value so the very first frames copy the right
/// amount, and corrected from the reflection as soon as the temporal pass is
/// recognised.
static CAMERA_BYTES: AtomicUsize = AtomicUsize::new(1072);
static SIZES_REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Said once, when the scene depth turns out to be larger than the dispatch.
static SUBRECT_DEPTH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static MANY_DEPTHS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static NO_CAMERA_CBV: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// References to the resources handed to NGX, held for a few frames.
///
/// Those are passed as raw pointers and the commands recorded with them are
/// executed later. Nothing here was keeping the resources alive in between,
/// so an engine release during a scene change left the driver dereferencing
/// freed memory -- an access violation inside nvwgf2umx, after a while rather
/// than at the call. Holding a reference for longer than the GPU can lag
/// closes that window.
static KEEP_ALIVE: Mutex<Option<std::collections::VecDeque<Vec<ID3D12Resource>>>> =
    Mutex::new(None);

/// How many frames of references to keep. The GPU runs at most two or three
/// frames behind the processor.
const KEEP_FRAMES: usize = 4;

/// Take a counted reference to each resource for the next few frames.
unsafe fn keep_alive(resources: &[usize]) {
    let held: Vec<ID3D12Resource> = resources
        .iter()
        .filter_map(|resource| {
            let raw = *resource as *mut c_void;
            ID3D12Resource::from_raw_borrowed(&raw).cloned()
        })
        .collect();
    let mut guard = KEEP_ALIVE.lock();
    let frames = guard.get_or_insert_with(std::collections::VecDeque::new);
    frames.push_back(held);
    while frames.len() > KEEP_FRAMES {
        frames.pop_front();
    }
}

thread_local! {
    /// True while this thread is recording our own work.
    ///
    /// The decoder and the DLSS evaluation drive the command list through the
    /// very vtable entries we patched, so without this their calls run through
    /// our hooks and overwrite the engine state we are about to restore --
    /// which restored our descriptor heap instead of the engine's and faulted
    /// the GPU moments later.
    static REENTRANT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn reentrant() -> bool {
    REENTRANT.with(|flag| flag.get())
}

/// Hold the guard for the duration of our own recording.
pub(crate) struct Guard;

impl Guard {
    pub(crate) fn enter() -> Self {
        REENTRANT.with(|flag| flag.set(true));
        Guard
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        REENTRANT.with(|flag| flag.set(false));
    }
}

/// The engine's command list state, taken before we disturb it.
#[derive(Default, Clone)]
struct Snapshot {
    heaps: Vec<usize>,
    signature: usize,
    pipeline: usize,
    tables: Vec<(u32, u64)>,
    cbvs: Vec<(u32, u64)>,
    views: Vec<(u32, u64, bool)>,
    constants: Vec<(u32, Vec<u32>)>,
}
static O_CREATE_COMMITTED: AtomicUsize = AtomicUsize::new(0);

/// Textures seen at render resolution, with how they are used.
///
/// The temporal pass hands back resource pointers but no indication of what
/// they are. An inventory taken at creation, cross-referenced against those
/// pointers, is what identifies tGBuffer1 and tGBuffer4 after the fact --
/// without needing another run of the game to ask a follow-up question.
static TEXTURES: Mutex<Option<std::collections::HashMap<usize, String>>> = Mutex::new(None);
/// How every surface was created, by resource address.
///
/// `GetDesc` is the obvious way to ask a resource about itself, and it must
/// not be used on an address taken from the descriptor registry: the
/// registry outlives the resources it names, and calling a method on
/// released memory is an access violation. Recording the answer at creation
/// turns a stale entry into a wrong size instead.
static DESCS: Mutex<Option<AddressMap<usize, D3D12_RESOURCE_DESC>>> = Mutex::new(None);

/// Depth resources seen, so the frame's main depth buffer can be found.
///
/// DLSS needs depth, and the temporal pass cannot supply it: it binds a 1x1
/// dummy for `tDepthMap`. The depth buffer is in the frame regardless --
/// the engine renders one -- so it is picked up where it is created.
static DEPTH: Mutex<Option<std::collections::HashMap<usize, (u64, u32, i32)>>> = Mutex::new(None);

/// The display size the temporal pass is running at, packed width then height.
///
/// The conversion from pixels to normalised device coordinates needs it:
/// the offset reported to DLSS and the offset recovered from the matrix both
/// depend on it, so it is read from the engine and not assumed.
static DISPLAY: AtomicU64 = AtomicU64::new(0);
static FRAME: AtomicU64 = AtomicU64::new(0);

struct Pso {
    hash: u64,
    shader: Shader,
    /// Which temporal pass this is, while DLSS is replacing it. This is what
    /// `hook_dispatch` asks, so it is `None` whenever the engine's own pass
    /// is to run: before DLSS is ready, and after it is found unusable.
    kind: Option<&'static str>,
    /// Which temporal pass this is, whether or not it is being replaced.
    shape: Option<&'static str>,
}

static PSOS: Mutex<Option<HashMap<usize, Pso>>> = Mutex::new(None);
/// Per-list state, by the list's address. Each list has a lock of its own
/// and the map is only consulted to find it; see `with_list`.
static LISTS: Mutex<Option<AddressMap<usize, &'static Mutex<ListState>>>> = Mutex::new(None);
/// Times a list's state was found locked by another thread. See `with_list`.
static LIST_COLLISIONS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The lists this thread has recorded into lately, and where their state
    /// lives, so the common case does not touch the shared map.
    static RECENT_LISTS: [std::cell::Cell<(usize, usize)>; 8] =
        const { [const { std::cell::Cell::new((0, 0)) }; 8] };
}

#[derive(Default)]
struct ListState {
    pipeline: usize,
    /// The bound compute root signature, whose layout says which descriptor
    /// of a table belongs to which register.
    signature: usize,
    /// Root parameter index -> descriptor table base, in the order set.
    tables: Vec<(u32, u64)>,
    /// Root parameter index -> constant buffer GPU address.
    cbvs: Vec<(u32, u64)>,
    /// Root parameter index -> shader resource or unordered access address.
    ///
    /// Setting a root signature invalidates every root binding, and this
    /// library sets one. Replaying only the tables and the constant buffers
    /// left these cleared behind us, so whatever the engine recorded next
    /// read from nothing.
    views: Vec<(u32, u64, bool)>,
    /// Root parameter index -> inline constants, as the engine set them.
    ///
    /// The motion vector decode sets its own on the engine's list, and
    /// nothing put the engine's back.
    constants: Vec<(u32, Vec<u32>)>,
    /// Descriptor heaps the engine last bound on this list.
    ///
    /// SetDescriptorHeaps is sticky: the engine sets it once and every later
    /// draw relies on it. Our own dispatch has to put it back, or the rest of
    /// the frame reads descriptors out of our heap.
    heaps: Vec<usize>,
    /// The render targets and depth target last bound on this list.
    ///
    /// A resource cannot be transitioned while it is bound as a depth target,
    /// and the engine leaves the scene depth bound across the temporal
    /// dispatch. Skipping those frames left the jitter unresolved and made the
    /// picture tremble, so the depth is unbound instead -- which means knowing
    /// what to bind back.
    rtvs: Vec<D3D12_CPU_DESCRIPTOR_HANDLE>,
    /// How many targets the engine passed, which with a single-handle range is
    /// more than the number of handles stored.
    rtv_count: u32,
    rtv_single_range: i32,
    dsv: Option<D3D12_CPU_DESCRIPTOR_HANDLE>,
    dsv_resource: usize,
}

/// Where a list's state lives.
///
/// Allocated once per list address and never freed -- the map never forgot an
/// address before either, and the engine draws its lists from a pool -- which
/// is what makes it sound for a thread to remember the reference.
impl ListState {
    /// Back to what `default()` gives, keeping the vectors' storage.
    fn reset(&mut self) {
        self.pipeline = 0;
        self.signature = 0;
        self.tables.clear();
        self.cbvs.clear();
        self.views.clear();
        self.constants.clear();
        self.heaps.clear();
        self.rtvs.clear();
        self.rtv_count = 0;
        self.rtv_single_range = 0;
        self.dsv = None;
        self.dsv_resource = 0;
    }
}

fn list_state(this: *mut c_void) -> &'static Mutex<ListState> {
    let key = this as usize;
    RECENT_LISTS.with(|recent| {
        let slot = &recent[((key >> 4) ^ (key >> 12)) & 7];
        let (seen, state) = slot.get();
        if seen == key {
            // SAFETY: only ever set below, from a leaked allocation.
            return unsafe { &*(state as *const Mutex<ListState>) };
        }
        let found: &'static Mutex<ListState> = LISTS
            .lock()
            .get_or_insert_with(AddressMap::default)
            .entry(key)
            .or_insert_with(|| Box::leak(Box::new(Mutex::new(ListState::default()))));
        slot.set((key, found as *const Mutex<ListState> as usize));
        found
    })
}

/// Work on a command list's recorded state.
///
/// Each list has its own lock, found through a few thread-local slots, so
/// two threads recording two lists do not meet. One lock for every list's
/// state, with a hash lookup under it, would be taken by every recorded call
/// from every recording thread.
///
/// A lock is kept at all, rather than plain per-thread access, because it
/// costs the same as checking would. A Direct3D 12 command list is not
/// free-threaded: an application may record one from one thread at a time and
/// must do its own hand-over between them, so this lock is never contended by
/// a correct engine and an uncontended acquisition is two atomic operations
/// on a line no other thread is using -- which is also exactly what a counter
/// to *detect* a violation would cost. So it is the counter and the cure in
/// one: `try_lock` failing is the violation, it is counted and reported on
/// the `[dls]` line, and the state is still not corrupted.
fn with_list<R>(this: *mut c_void, f: impl FnOnce(&mut ListState) -> R) -> R {
    let state = list_state(this);
    let wait = crate::perf::start(crate::perf::Family::ListsWait);
    let mut guard = match state.try_lock() {
        Some(guard) => guard,
        None => {
            LIST_COLLISIONS.fetch_add(1, Ordering::Relaxed);
            state.lock()
        }
    };
    wait.stop();
    f(&mut guard)
}

fn claim(budget: &AtomicUsize) -> bool {
    budget
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
        .is_ok()
}

fn arm(reason: &str) {
    let id = BURST_ID.fetch_add(1, Ordering::Relaxed) + 1;
    BUDGET.store(BURST, Ordering::Relaxed);
    REPORT_BUDGET.store(REPORTS, Ordering::Relaxed);
    CAMERA_BUDGET.store(1, Ordering::Relaxed);
    crate::log!("");
    crate::log!("===== capture burst #{id} armed ({reason}) =====");
}

/// The engine's temporal passes, by the shape of their bindings rather than by
/// a bytecode hash a game patch would invalidate.
///
/// Both are of interest. `CSTemporalAA2` resolves at native resolution but,
/// as the captures show, runs with an unjittered projection -- it is an
/// accumulation filter, not a sub-pixel reconstruction. `CSTemporalUpsample`
/// is the one that carries `fJitterOffset` and a content scale, which is what
/// DLSS actually needs.
fn temporal_pass_kind(shader: &Shader) -> Option<&'static str> {
    let summary = shader.summary();
    let has = |needles: &[&str]| needles.iter().all(|needle| summary.contains(needle));

    if has(&["tBaseMap", "tBlendMap", "tDepthMap", "rwOutput"]) {
        return Some("TemporalAA");
    }
    if has(&["tInput", "tHistory", "rwOuput"]) {
        return Some("TemporalUpsample");
    }
    None
}

pub(crate) fn format_name(format: DXGI_FORMAT) -> String {
    let name = match format.0 {
        2 => "R32G32B32A32_FLOAT",
        10 => "R16G16B16A16_FLOAT",
        11 => "R16G16B16A16_UNORM",
        16 => "R32G32_FLOAT",
        24 => "R10G10B10A2_UNORM",
        26 => "R11G11B10_FLOAT",
        28 => "R8G8B8A8_UNORM",
        29 => "R8G8B8A8_UNORM_SRGB",
        34 => "R16G16_FLOAT",
        35 => "R16G16_UNORM",
        37 => "R16G16_SINT",
        39 => "R32_TYPELESS",
        40 => "D32_FLOAT",
        41 => "R32_FLOAT",
        42 => "R32_UINT",
        44 => "R24G8_TYPELESS",
        45 => "D24_UNORM_S8_UINT",
        46 => "R24_UNORM_X8_TYPELESS",
        49 => "R8G8_UNORM",
        54 => "R16_FLOAT",
        56 => "R16_UNORM",
        57 => "R16_UINT",
        61 => "R8_UNORM",
        62 => "R8_UINT",
        other => return format!("DXGI_FORMAT({other})"),
    };
    name.to_string()
}

/// Full report of what the temporal dispatch reads and writes.
unsafe fn report(this: *mut c_void, pso: &Pso, x: u32, y: u32, z: u32) {
    crate::log!("");
    crate::log!(
        "[taa] {} dispatch {x} x {y} x {z}  hash={:#018x}",
        pso.shape.unwrap_or("?"),
        pso.hash
    );

    for (kind, label) in [
        (Kind::Texture, "srv"),
        (Kind::Unordered, "uav"),
        (Kind::ConstantBuffer, "cb "),
    ] {
        let listed = pso
            .shader
            .by_slot(kind)
            .iter()
            .map(|binding| binding.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        crate::log!("[taa] {label} {listed}");
    }

    let (tables, cbvs, signature) = with_list(this, |state| {
        (state.tables.clone(), state.cbvs.clone(), state.signature)
    });
    crate::log!("[taa] registry: {}", bindings::summary());

    let layout = crate::rootsig::layout_of(signature);
    if layout.is_none() {
        crate::log!("[taa] root signature {signature:#x} was created before hooking");
    }

    for (root, base) in tables {
        crate::log!("[taa] root {root}: table base={base:#x}");

        // With the root signature in hand each descriptor can be named: the
        // range says which register sits at which offset, and the shader says
        // what that register is called.
        let ranges = layout.as_ref().and_then(|l| l.tables.get(&root));
        let Some(ranges) = ranges else {
            let found = bindings::resolve_table(base, TABLE_SCAN)
                .iter()
                .filter(|slot| matches!(slot, Slot::Resource(..)))
                .count();
            crate::log!("[taa]   no layout; {found} descriptors recorded in {TABLE_SCAN}");
            continue;
        };

        for range in ranges {
            for step in 0..range.count {
                let offset = (range.offset + step) as usize;
                let register = range.base_register + step;
                let name = pso
                    .shader
                    .bindings
                    .iter()
                    .find(|b| b.kind.prefix() == range.prefix() && b.slot == register)
                    .map(|b| b.name.as_str())
                    .unwrap_or("<unbound>");
                let label = format!("{}{register}:{name}", range.prefix());

                match bindings::resolve_table(base, offset + 1).get(offset) {
                    Some(Slot::Resource(resource, _)) => {
                        let usage = TEXTURES
                            .lock()
                            .as_ref()
                            .and_then(|seen| seen.get(resource).cloned())
                            .unwrap_or_else(|| "?".into());
                        match described(*resource) {
                        Some(desc) => crate::log!(
                            "[taa]   {label:<24} +{offset:<3} res={resource:#x}  {}x{}  {}  flags={:#x}  [{usage}]",
                            desc.Width,
                            desc.Height,
                            format_name(desc.Format),
                            desc.Flags.0
                        ),
                        None => crate::log!("[taa]   {label:<24} +{offset:<3} res={resource:#x}"),
                        }
                    }
                    _ => crate::log!(
                        "[taa]   {label:<24} +{offset:<3} not recorded (views of depth targets \
                         and of textures under 960 wide are not followed)"
                    ),
                }
            }
        }
    }

    if let Some(depth) = DEPTH.lock().as_ref() {
        let mut full: Vec<_> = depth
            .iter()
            .filter(|(_, (w, h, _))| *w >= 1920 && *h >= 1080)
            .collect();
        full.sort_by_key(|(_, (w, h, _))| std::cmp::Reverse((*w, *h)));
        let main = LAST_DEPTH.lock().0;
        crate::log!("[taa] depth bound most recently before this pass: {main:#x}");
        crate::log!("[taa] depth candidates at render size or larger:");
        for (resource, (w, h, format)) in full.iter().take(8) {
            crate::log!(
                "[taa]   res={resource:#x}  {w}x{h}  {}",
                format_name(DXGI_FORMAT(*format))
            );
        }
    }

    for (root, address) in cbvs {
        // Root CBVs appear to map to registers in order from the first one, so
        // root 2 is b0. The field names printed below are the check: if the
        // assumption were wrong the values would be nonsense.
        let register = root.saturating_sub(2);
        let Some(bytes) = bindings::read_gpu_memory(address, CB_DUMP) else {
            crate::log!("[taa] root {root}: cbv va={address:#x}  <no mapping>");
            continue;
        };
        match pso.shader.buffer_at(register) {
            Some(buffer) => {
                crate::log!(
                    "[taa] root {root} = b{register} {} ({} bytes declared)",
                    buffer.name,
                    buffer.size
                );
                for variable in &buffer.variables {
                    dump_variable(&buffer.name, variable, &bytes);
                }
            }
            None => crate::log!(
                "[taa] root {root} = b{register}, not bound by this shader (va={address:#x})"
            ),
        }
    }
}

/// Print one constant buffer field by name, as floats where that makes sense.
fn dump_variable(buffer: &str, variable: &crate::dxbc::Variable, bytes: &[u8]) {
    let start = variable.offset as usize;
    let end = start + variable.size as usize;
    let Some(slice) = bytes.get(start..end) else {
        crate::log!(
            "[cbuf] {buffer}.{} +{start:#x} <past the mapping>",
            variable.name
        );
        return;
    };

    let values: Vec<f32> = slice
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| f32::from_le_bytes([word[0], word[1], word[2], word[3]]))
        .collect();

    // A 4x4 matrix is far easier to read, and to compare against its NoJitter
    // twin, laid out as rows.
    if values.len() == 16 {
        crate::log!("[cbuf] {buffer}.{} +{start:#x}", variable.name);
        for row in values.as_chunks::<4>().0 {
            crate::log!(
                "[cbuf]     {:>14.6} {:>14.6} {:>14.6} {:>14.6}",
                row[0],
                row[1],
                row[2],
                row[3]
            );
        }
        return;
    }

    let floats: Vec<String> = values.iter().take(8).map(|v| format!("{v:.6}")).collect();
    let integers: Vec<String> = slice
        .as_chunks::<4>()
        .0
        .iter()
        .take(8)
        .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]).to_string())
        .collect();
    crate::log!(
        "[cbuf] {buffer}.{} +{start:#x} = {}   | as u32: {}",
        variable.name,
        floats.join(" "),
        integers.join(" ")
    );
}

/// The R32_UINT input of the temporal pass, which is the motion vector
/// candidate. Resolved through the bound table rather than remembered, so it
/// follows the engine's per-frame descriptor rotation.
/// Dump only the camera state, from any dispatch that binds it.
///
/// Answers one question: does the engine jitter the projection? If `fProj`
/// equals `fNoJitterProj` then it does not, and DLSS cannot get sub-pixel
/// offsets from the engine.
unsafe fn camera_report(this: *mut c_void, pso: &Pso) {
    let cbvs = with_list(this, |state| state.cbvs.clone());
    let Some(buffer) = pso.shader.buffer_at(0) else {
        return;
    };

    for (root, address) in cbvs {
        if root.saturating_sub(2) != 0 {
            continue;
        }
        let Some(bytes) = bindings::read_gpu_memory(address, CB_DUMP) else {
            continue;
        };
        crate::log!("");
        crate::log!("[cam] {} via dispatch pso={:#018x}", buffer.name, pso.hash);
        for variable in &buffer.variables {
            if matches!(
                variable.name.as_str(),
                "fProj" | "fNoJitterProj" | "fViewProj" | "fNoJitterViewProj"
            ) {
                dump_variable(&buffer.name, variable, &bytes);
            }
        }
        return;
    }
}

// ---------------------------------------------------------------------------
// Command list hooks
// ---------------------------------------------------------------------------

type SetPipelineFn = unsafe extern "system" fn(*mut c_void, *mut c_void);
type DispatchFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32);
type DrawIndexedInstancedFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, i32, u32);
type DrawInstancedFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32);
type CloseFn = unsafe extern "system" fn(*mut c_void) -> HRESULT;
type ExecuteCommandListsFn = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void);
type SetHeapsFn = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void);
type SetRootTableFn = unsafe extern "system" fn(*mut c_void, u32, D3D12_GPU_DESCRIPTOR_HANDLE);
type SetRootSigFn = unsafe extern "system" fn(*mut c_void, *mut c_void);
type SetRootCbvFn = unsafe extern "system" fn(*mut c_void, u32, u64);
type SetRootViewFn = unsafe extern "system" fn(*mut c_void, u32, u64);
type SetRootConstantsFn = unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void, u32);
type CreateComputePsoFn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_COMPUTE_PIPELINE_STATE_DESC,
    *const windows::core::GUID,
    *mut *mut c_void,
) -> HRESULT;
type CreateSrvFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *const D3D12_SHADER_RESOURCE_VIEW_DESC,
    D3D12_CPU_DESCRIPTOR_HANDLE,
);
type CreateUavFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *mut c_void,
    *const D3D12_UNORDERED_ACCESS_VIEW_DESC,
    D3D12_CPU_DESCRIPTOR_HANDLE,
);
type CopyDescriptorsSimpleFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    D3D12_CPU_DESCRIPTOR_HANDLE,
    D3D12_CPU_DESCRIPTOR_HANDLE,
    D3D12_DESCRIPTOR_HEAP_TYPE,
);
type CopyDescriptorsFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
    *const u32,
    u32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
    *const u32,
    D3D12_DESCRIPTOR_HEAP_TYPE,
);
type MapFn =
    unsafe extern "system" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> HRESULT;
type UnmapFn = unsafe extern "system" fn(*mut c_void, u32, *const c_void);
type CreateCbvFn =
    unsafe extern "system" fn(*mut c_void, *const c_void, D3D12_CPU_DESCRIPTOR_HANDLE);
type CreateSamplerFn =
    unsafe extern "system" fn(*mut c_void, *const c_void, D3D12_CPU_DESCRIPTOR_HANDLE);
type CreateDsvFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *const c_void, D3D12_CPU_DESCRIPTOR_HANDLE);
type CreateRtvFn = CreateDsvFn;
type ResourceBarrierFn = unsafe extern "system" fn(*mut c_void, u32, *const D3D12_RESOURCE_BARRIER);
type OmSetTargetsFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
    i32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
);
type CreateCommittedFn = unsafe extern "system" fn(
    *mut c_void,
    *const c_void,
    u32,
    *const D3D12_RESOURCE_DESC,
    i32,
    *const c_void,
    *const windows::core::GUID,
    *mut *mut c_void,
) -> HRESULT;
type CreateRootSigFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const c_void,
    usize,
    *const windows::core::GUID,
    *mut *mut c_void,
) -> HRESULT;

unsafe extern "system" fn hook_set_pipeline(this: *mut c_void, pipeline: *mut c_void) {
    let perf = crate::perf::start(crate::perf::Family::ListState);
    if !reentrant() {
        with_list(this, |state| state.pipeline = pipeline as usize);
    }
    perf.stop();
    let original: SetPipelineFn = std::mem::transmute(O_SET_PIPELINE.load(Ordering::Relaxed));
    original(this, pipeline);
}

unsafe extern "system" fn hook_set_root_srv(this: *mut c_void, root: u32, address: u64) {
    let perf = crate::perf::start(crate::perf::Family::ListState);
    if !reentrant() {
        with_list(this, |state| {
            state.views.retain(|(index, _, _)| *index != root);
            state.views.push((root, address, false));
        });
    }
    perf.stop();
    let original: SetRootViewFn = std::mem::transmute(O_SET_ROOT_SRV.load(Ordering::Relaxed));
    original(this, root, address);
}

unsafe extern "system" fn hook_set_root_uav(this: *mut c_void, root: u32, address: u64) {
    let perf = crate::perf::start(crate::perf::Family::ListState);
    if !reentrant() {
        with_list(this, |state| {
            state.views.retain(|(index, _, _)| *index != root);
            state.views.push((root, address, true));
        });
    }
    perf.stop();
    let original: SetRootViewFn = std::mem::transmute(O_SET_ROOT_UAV.load(Ordering::Relaxed));
    original(this, root, address);
}

unsafe extern "system" fn hook_set_root_constants(
    this: *mut c_void,
    root: u32,
    count: u32,
    data: *const c_void,
    offset: u32,
) {
    let perf = crate::perf::start(crate::perf::Family::ListState);
    // Only a whole-parameter set is worth recording: a partial write at an
    // offset would have to be merged with what is already there, and this is
    // a record of what to put back, not a shadow of the real state.
    if !reentrant() && !data.is_null() && offset == 0 && count <= 64 {
        let values = std::slice::from_raw_parts(data as *const u32, count as usize).to_vec();
        with_list(this, |state| {
            state.constants.retain(|(index, _)| *index != root);
            state.constants.push((root, values));
        });
    }
    perf.stop();
    let original: SetRootConstantsFn =
        std::mem::transmute(O_SET_ROOT_CONSTANTS.load(Ordering::Relaxed));
    original(this, root, count, data, offset);
}

unsafe extern "system" fn hook_set_heaps(this: *mut c_void, count: u32, heaps: *const *mut c_void) {
    let perf = crate::perf::start(crate::perf::Family::ListState);
    if !heaps.is_null() && !reentrant() {
        for index in 0..count as usize {
            bindings::record_heap(*heaps.add(index));
        }
        with_list(this, |state| {
            state.heaps.clear();
            for index in 0..count as usize {
                state.heaps.push(*heaps.add(index) as usize);
            }
        });
    }
    perf.stop();
    let original: SetHeapsFn = std::mem::transmute(O_SET_HEAPS.load(Ordering::Relaxed));
    original(this, count, heaps);
}

unsafe extern "system" fn hook_set_root_table(
    this: *mut c_void,
    root: u32,
    base: D3D12_GPU_DESCRIPTOR_HANDLE,
) {
    let perf = crate::perf::start(crate::perf::Family::ListState);
    if !reentrant() {
        with_list(this, |state| {
            state.tables.retain(|(index, _)| *index != root);
            state.tables.push((root, base.ptr));
        });
    }
    perf.stop();
    let original: SetRootTableFn = std::mem::transmute(O_SET_ROOT_TABLE.load(Ordering::Relaxed));
    original(this, root, base);
}

unsafe extern "system" fn hook_set_root_cbv(this: *mut c_void, root: u32, address: u64) {
    let perf = crate::perf::start(crate::perf::Family::ListCbv);
    if !reentrant() {
        // Recorded because the dispatch reads the jitter back out of the
        // buffer this names.
        with_list(this, |state| {
            state.cbvs.retain(|(index, _)| *index != root);
            state.cbvs.push((root, address));
        });
    }
    perf.stop();
    let original: SetRootCbvFn = std::mem::transmute(O_SET_ROOT_CBV.load(Ordering::Relaxed));
    original(this, root, address);
}

unsafe extern "system" fn hook_set_root_sig(this: *mut c_void, signature: *mut c_void) {
    let perf = crate::perf::start(crate::perf::Family::ListState);
    // Changing the root signature invalidates every root binding.
    if !reentrant() {
        with_list(this, |state| {
            state.tables.clear();
            state.cbvs.clear();
            // Root views and root constants go with the signature as well.
            // They were kept, and `restore_state` replays whatever is kept
            // against the signature in force at the temporal pass.
            state.views.clear();
            state.constants.clear();
            state.signature = signature as usize;
        });
    }
    perf.stop();
    let original: SetRootSigFn = std::mem::transmute(O_SET_ROOT_SIG.load(Ordering::Relaxed));
    original(this, signature);
}

unsafe extern "system" fn hook_dispatch(this: *mut c_void, x: u32, y: u32, z: u32) {
    ALL_DISPATCHES.fetch_add(1, Ordering::Relaxed);
    let perf = crate::perf::start(crate::perf::Family::ListDispatch);
    if reentrant() {
        perf.stop();
        let original: DispatchFn = std::mem::transmute(O_DISPATCH.load(Ordering::Relaxed));
        original(this, x, y, z);
        return;
    }

    let pipeline = with_list(this, |state| state.pipeline);

    let mut guard = PSOS.lock();
    let psos = guard.get_or_insert_with(HashMap::new);
    let target = psos.get(&pipeline).is_some_and(|pso| pso.kind.is_some());

    // How large the engine says its constant buffers are. Parsed from the
    // reflection since the first day and never once read, while the
    // substitute copied a number of bytes chosen by guess -- first 864, then
    // up to 4096 -- with the remainder left as whatever the slot held. This
    // says how much a complete copy actually needs.
    if target && !SIZES_REPORTED.swap(true, Ordering::Relaxed) {
        if let Some(pso) = psos.get(&pipeline) {
            for slot in 0..8u32 {
                if let Some(buffer) = pso.shader.buffer_at(slot) {
                    crate::log!("[taa] b{slot}:{} is {} bytes", buffer.name, buffer.size);
                    if buffer.name.contains("ViewProjection") && buffer.size > 0 {
                        CAMERA_BYTES.store(buffer.size as usize, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    // The camera dump must not depend on the temporal pass running: with DLSS
    // enabled the game turns its own anti-aliasing off, and then no temporal
    // dispatch exists to hang a report on -- yet the projection matrices are
    // exactly what needs checking in that configuration.
    let binds_camera = psos.get(&pipeline).is_some_and(|pso| {
        pso.shader
            .buffer_at(0)
            .is_some_and(|cb| cb.name == "CBViewProjection")
    });

    if target && claim(&REPORT_BUDGET) {
        if let Some(pso) = guard.as_ref().and_then(|psos| psos.get(&pipeline)) {
            report(this, pso, x, y, z);
        }
    } else if binds_camera && claim(&CAMERA_BUDGET) {
        if let Some(pso) = guard.as_ref().and_then(|psos| psos.get(&pipeline)) {
            camera_report(this, pso);
        }
    }
    drop(guard);

    if claim(&BUDGET) {
        crate::logv!(
            "  dispatch {x:>5} x {y:>5} x {z:<3} pso={pipeline:#x} cl={this:p}{}",
            if target { "  <== TEMPORAL" } else { "" }
        );
        // `path_dump` lists a frame's compute passes through this branch,
        // which is only entered while a capture burst has budget left, so a
        // dispatch pays nothing for it otherwise.
        if crate::pathdump::recording() {
            crate::pathdump::dispatch(this as usize, (x, y, z), || {
                let mut names = PSOS
                    .lock()
                    .as_ref()
                    .and_then(|psos| psos.get(&pipeline))
                    .map_or_else(
                        || format!("pso {pipeline:#x}, not recorded"),
                        |pso| pso.shader.summary(),
                    );
                if names.len() > 160 {
                    let mut end = 160;
                    while !names.is_char_boundary(end) {
                        end -= 1;
                    }
                    names.truncate(end);
                    names.push_str(" ...");
                }
                names
            });
        }
    }

    let mut replaced = false;
    if target {
        let seen = DISPATCHES.fetch_add(1, Ordering::Relaxed) + 1;
        if seen.is_multiple_of(600) {
            crate::log!(
                "[dls] {} of {seen} temporal dispatches replaced, {} submitted lists \
                 whose recording was never seen; {} times a command list was found in use \
                 by two threads at once",
                EVALUATED.load(Ordering::Relaxed),
                crate::states::unseen(),
                LIST_COLLISIONS.load(Ordering::Relaxed)
            );
        }
        // Take the engine's state before touching anything, because our own
        // recording goes through these same hooks.
        let snapshot = with_list(this, |state| Snapshot {
            heaps: state.heaps.clone(),
            signature: state.signature,
            pipeline: state.pipeline,
            tables: state.tables.clone(),
            cbvs: state.cbvs.clone(),
            views: state.views.clone(),
            constants: state.constants.clone(),
        });

        // `path_dump`: a frame's passes are recorded from here to Present
        // when one has been asked for. The budget is what lets the compute
        // dispatches that follow reach the branch above.
        if crate::pathdump::armed()
            && crate::pathdump::begin_frame(&format!("{x}x{y}x{z} groups on list {this:p}"))
        {
            BUDGET.fetch_max(4000, Ordering::Relaxed);
        }
        let guard = Guard::enter();
        replaced = run_dlss(this, x, y);
        crate::pathdump::mark(if replaced {
            "that dispatch was replaced by this library and the engine's own did not run"
        } else {
            "that dispatch was left to the engine, or replaced by a plain copy because the resolve is made at the upscale stage"
        });
        // Unconditionally, not only when the pass was replaced. The motion
        // vector decode records into the engine's own list -- its descriptor
        // heap, root signature, pipeline and dispatch -- and `run_dlss` can
        // still bail afterwards. Restoring only on success left those bindings
        // in place, so the engine's temporal dispatch then ran as our decode
        // shader and every later draw in that list resolved its roots against
        // our heap. Whole surfaces black, an access violation in the driver,
        // and all of it keyed to whether the bail-out happened to fire.
        //
        // Restoring when nothing was recorded costs a handful of redundant
        // binds and is always safe.
        restore_state(this, &snapshot);
        drop(guard);
    }

    // The frame advances only after the evaluation, because depth is accepted
    // solely when it was bound during the frame being drawn. Advancing first
    // made that test compare against the next frame's number, so it never
    // matched and the evaluation was skipped in silence.
    if target {
        FRAME.fetch_add(1, Ordering::Relaxed);
        // Descriptor copies are stamped by a clock that ticks here and at
        // Present.
        bindings::tick();
    }
    perf.stop();
    if target {
        crate::perf::frame();
    }

    if replaced {
        // DLSS has written the output. Letting the engine's pass run as well
        // would overwrite it with the accumulation blend we are replacing.
        return;
    }

    let original: DispatchFn = std::mem::transmute(O_DISPATCH.load(Ordering::Relaxed));
    original(this, x, y, z);
}

/// Which way the resolve last went, so a change is said once: where it
/// happens, what colour it reads and what it writes.
static ROUTE_SAID: AtomicU64 = AtomicU64::new(0);

/// Say which way the resolve is going, when that changes.
///
/// `route`: 1 at the temporal pass into its own output (render ratio 1.0),
/// 2 at the temporal pass into the full-size target, 4 at the upscale stage.
fn say_route(route: u64, colour: usize, destination: Option<usize>) {
    if ROUTE_SAID.swap(route, Ordering::Relaxed) == route {
        return;
    }
    let shape = |resource: usize| {
        described(resource).map_or_else(
            || format!("{resource:#x} undescribed"),
            |info| {
                format!(
                    "{resource:#x} {}x{} {}",
                    info.Width,
                    info.Height,
                    format_name(info.Format)
                )
            },
        )
    };
    crate::log!(
        "[dls] resolve route: {}; colour read: {}{}",
        match route {
            1 => "at the temporal pass, into its own output (no upscale stage in the frame)",
            2 =>
                "at the temporal pass, into the full-size target -- the old route, used \
                  because resolve_at_pass is set or the upscale stage could not resolve; the \
                  engine's passes between here and that stage are bypassed",
            _ =>
                "at the engine's upscale stage, from the colour the engine holds there; its \
                  temporal pass is replaced by a plain copy",
        },
        shape(colour),
        destination.map_or_else(String::new, |target| format!(
            "; written: {}",
            shape(target)
        ))
    );
}

/// Put this frame's colour into the temporal pass's output as it is.
///
/// What the engine's temporal pass would have written there is this colour
/// blended with its history. Without the blend, the passes that follow get
/// this frame alone, and the temporal work is left to DLSS at the upscale
/// stage.
unsafe fn copy_through(this: *mut c_void, colour: usize, output: usize) -> bool {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let alike = match (described(colour), described(output)) {
        (Some(a), Some(b)) => {
            a.Width == b.Width
                && a.Height == b.Height
                && a.Format == b.Format
                && a.MipLevels == b.MipLevels
        }
        _ => false,
    };
    let moves = if alike && colour != output {
        crate::states::plan(
            this as usize,
            &[
                (colour, D3D12_RESOURCE_STATE_COPY_SOURCE),
                (output, D3D12_RESOURCE_STATE_COPY_DEST),
            ],
        )
    } else {
        None
    };
    let (raw_from, raw_into) = (colour as *mut c_void, output as *mut c_void);
    let (Some(moves), Some(list), Some(from), Some(into)) = (
        moves,
        ID3D12GraphicsCommandList::from_raw_borrowed(&this),
        ID3D12Resource::from_raw_borrowed(&raw_from),
        ID3D12Resource::from_raw_borrowed(&raw_into),
    ) else {
        if !SAID.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[dls] the colour cannot be copied into the temporal pass's output (they \
                 differ in shape, or a state is unknown); the engine's own pass runs instead"
            );
        }
        return false;
    };
    let forward: Vec<D3D12_RESOURCE_BARRIER> = moves
        .iter()
        .map(|step| crate::states::barrier(step, false))
        .collect();
    if !forward.is_empty() {
        list.ResourceBarrier(&forward);
    }
    list.CopyResource(into, from);
    let back: Vec<D3D12_RESOURCE_BARRIER> = moves
        .iter()
        .map(|step| crate::states::barrier(step, true))
        .collect();
    if !back.is_empty() {
        list.ResourceBarrier(&back);
    }
    true
}

/// Resolve at the engine's upscale stage, from the colour the engine holds
/// there. The route for every frame that has an upscale stage; see the note in `run_dlss`.
///
/// # Safety
/// `this` must be the native, open command list the upscale stage was given.
pub(crate) unsafe fn resolve_at_slot(
    this: *mut c_void,
    colour: usize,
    output: usize,
    display: (u32, u32),
    rectangle: Option<(u32, u32)>,
) -> bool {
    // Nothing is recorded on the engine's list while DLSS is not ready or has
    // been found unusable; the temporal pass is the engine's own then, and
    // `hijack` is already asking the engine for the whole window.
    if !crate::gate::dlss_ready() {
        return false;
    }
    let settings = crate::config::get();
    if settings.resolve_at_pass {
        return false;
    }
    // Given up at this `render_scale`; any other value is a new attempt.
    let given_up = SLOT_ROUTE_GIVEN_UP.load(Ordering::Relaxed);
    if given_up != 0 {
        if given_up == settings.render_scale.to_bits() {
            return false;
        }
        SLOT_ROUTE_GIVEN_UP.store(0, Ordering::Relaxed);
        SLOT_ROUTE_FAILURES.store(0, Ordering::Relaxed);
    }
    static REFUSED: AtomicU64 = AtomicU64::new(0);
    let refuse = |code: u64, why: &str| {
        if REFUSED.swap(code, Ordering::Relaxed) != code {
            crate::log!("[dls] not resolving at the upscale stage: {why}");
        }
        slot_route_failed(why);
        false
    };
    // No record is not a failure of this route: it is a frame without a
    // temporal pass, or one whose inputs that pass could not use, and
    // `hijack` counts those for itself.
    let Some((depth, motion, recorded, jitter, motion_scale)) =
        crate::hijack::slot_inputs(this as usize)
    else {
        if REFUSED.swap(1, Ordering::Relaxed) != 1 {
            crate::log!(
                "[dls] not resolving at the upscale stage: the temporal pass recorded no inputs \
                 for this frame, or its depth or motion vectors have been destroyed since"
            );
        }
        return false;
    };
    // The size to resolve from is the engine's own word for it, where it
    // gave one. What the temporal pass worked out from its dispatch is the
    // size the motion vectors were decoded at, and the two are expected to
    // agree.
    let render = rectangle.unwrap_or(recorded);
    if render != recorded {
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[dls] the engine's rectangle at the upscale stage is {}x{} and the temporal \
                 pass covered {}x{}; the rectangle is used",
                render.0,
                render.1,
                recorded.0,
                recorded.1
            );
        }
    }
    let covers = |resource: usize| {
        described(resource)
            .is_some_and(|info| info.Width as u32 >= render.0 && info.Height >= render.1)
    };
    if !covers(depth) || !covers(motion) {
        return refuse(
            5,
            "the depth or the motion vectors are smaller than the rectangle to resolve",
        );
    }
    if depth == colour || motion == colour || depth == output || motion == output {
        return refuse(6, "an input and the output are one texture");
    }
    let colour_fits = described(colour).is_some_and(|info| {
        info.Format.0 == FORMAT_COLOUR && info.Width as u32 >= render.0 && info.Height >= render.1
    });
    if crate::alive::serial_of(colour).is_none() || !colour_fits {
        return refuse(2, "the colour at the upscale stage is destroyed, undescribed, not R11G11B10_FLOAT or too small");
    }
    let output_fits =
        described(output).map(|info| (info.Width as u32, info.Height)) == Some(display);
    if crate::alive::serial_of(output).is_none() || !output_fits || output == colour {
        return refuse(
            3,
            "the full-size target is destroyed or not the window's size",
        );
    }
    let wanted = [
        (colour, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE),
        (motion, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE),
        (depth, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE),
        (output, D3D12_RESOURCE_STATE_UNORDERED_ACCESS),
    ];
    let Some(moves) = crate::states::plan(this as usize, &wanted) else {
        return refuse(4, "the state of an input on this list is not known");
    };
    REFUSED.store(0, Ordering::Relaxed);
    crate::pathdump::resolve(&crate::pathdump::Resolve {
        at_slot: true,
        colour,
        depth,
        motion_engine: 0,
        motion,
        pass_output: 0,
        written: output,
        groups: (0, 0),
        render,
        display,
        jitter,
        decode_scale: (0.0, 0.0),
        motion_scale,
        slotless: false,
    });

    // From here on the list is recorded into, through the same hooks the
    // engine's calls go through, so its state is taken first and put back
    // after, exactly as at the temporal pass.
    let snapshot = with_list(this, |state| Snapshot {
        heaps: state.heaps.clone(),
        signature: state.signature,
        pipeline: state.pipeline,
        tables: state.tables.clone(),
        cbvs: state.cbvs.clone(),
        views: state.views.clone(),
        constants: state.constants.clone(),
    });
    let guard = Guard::enter();
    let unbound = unbind_depth(this, depth);
    let list = ID3D12GraphicsCommandList::from_raw_borrowed(&this);
    let forward: Vec<D3D12_RESOURCE_BARRIER> = moves
        .iter()
        .map(|step| crate::states::barrier(step, false))
        .collect();
    if let (Some(list), false) = (list, forward.is_empty()) {
        list.ResourceBarrier(&forward);
    }
    keep_alive(&[colour, output, depth, motion]);
    let perf = crate::perf::start(crate::perf::Family::Evaluate);
    // Through `sharpen`, which is `dlss::evaluate` unless the settings ask
    // for the result to be sharpened on its way into the target. The target
    // is in unordered access here: the plan above put it there.
    let evaluated = crate::sharpen::evaluate(
        this,
        &crate::dlss::Frame {
            colour: colour as *mut c_void,
            output: output as *mut c_void,
            depth: depth as *mut c_void,
            motion: motion as *mut c_void,
            render,
            display,
            jitter,
            motion_scale,
            reset: false,
        },
        true,
    );
    perf.stop();
    let back: Vec<D3D12_RESOURCE_BARRIER> = moves
        .iter()
        .map(|step| crate::states::barrier(step, true))
        .collect();
    if let (Some(list), false) = (list, back.is_empty()) {
        list.ResourceBarrier(&back);
    }
    if let Some(bound) = unbound.as_ref() {
        rebind_depth(this, bound);
    }
    restore_state(this, &snapshot);
    drop(guard);
    if evaluated {
        EVALUATED.fetch_add(1, Ordering::Relaxed);
        SLOT_ROUTE_FAILURES.store(0, Ordering::Relaxed);
        crate::hijack::note_resolved();
        say_route(4, colour, Some(output));
    } else {
        slot_route_failed(
            "the evaluation was declined or failed; the lines above this one say which",
        );
    }
    evaluated
}

/// Frames in a row on which the upscale stage resolved nothing, and the
/// `render_scale` (as bits) at which that route has been given up; 0 for
/// not given up.
static SLOT_ROUTE_FAILURES: AtomicU64 = AtomicU64::new(0);
static SLOT_ROUTE_GIVEN_UP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// How many such frames are borne before the route is given up. Each is a
/// frame on which the full-size target was not written, so the picture
/// repeats; a rebuild of the engine's targets accounts for one or two.
const SLOT_ROUTE_PATIENCE: u64 = 3;

/// Whether resolving at the upscale stage has been given up for the
/// `render_scale` in force, so that the temporal pass resolves instead.
fn slot_route_given_up() -> bool {
    let given_up = SLOT_ROUTE_GIVEN_UP.load(Ordering::Relaxed);
    given_up != 0 && given_up == crate::config::get().render_scale.to_bits()
}

/// The upscale stage resolved nothing this frame, with inputs in hand. After
/// a few in a row the route is given up until `render_scale` is changed, and
/// the temporal pass takes the resolve back from the next frame, writing the
/// full-size target itself as it did before this route existed. On this
/// route the temporal pass writes nothing there, so a stage that cannot
/// resolve leaves the target unwritten and the picture stands still for as
/// long as that lasts -- which is what the first run of it did with ReShade
/// loaded.
fn slot_route_failed(why: &str) {
    if SLOT_ROUTE_FAILURES.fetch_add(1, Ordering::Relaxed) + 1 < SLOT_ROUTE_PATIENCE {
        return;
    }
    let scale = crate::config::get().render_scale;
    if SLOT_ROUTE_GIVEN_UP.swap(scale.to_bits(), Ordering::Relaxed) != scale.to_bits() {
        crate::log!(
            "[dls] nothing was resolved at the engine's upscale stage on {SLOT_ROUTE_PATIENCE} \
             frames in a row ({why}), and on each of them the full-size target went unwritten. \
             The resolve goes back to the temporal pass from the next frame -- the old route, \
             whose picture is paler because it bypasses the engine's passes between the two -- \
             until render_scale is changed from {scale:.3}"
        );
        // So that the route line is written again when the old route
        // resolves.
        ROUTE_SAID.store(0, Ordering::Relaxed);
    }
}

/// The last reason a frame was left alone, so each is said once.
static PREFLIGHT_SAID: AtomicU64 = AtomicU64::new(0);
/// The frame until which no evaluation is attempted, after one has failed.
static PAUSED_UNTIL: AtomicU64 = AtomicU64::new(0);
/// How many frames that is. Fewer than the five `hijack` reads as the
/// temporal pass having stopped.
const EVALUATE_PAUSE: u64 = 3;

/// `DXGI_FORMAT_R32_UINT` and `DXGI_FORMAT_R11G11B10_FLOAT`: what the packed
/// motion vectors and the colour are in this engine, as measured.
const FORMAT_MOTION: i32 = 42;
const FORMAT_COLOUR: i32 = 26;

/// Whether this frame's inputs are fit to record anything against, as a
/// reason not to if they are not: a number to say it once by, and the words.
///
/// The properties checked, each of which the code below this point relies on:
///
/// * the colour and the motion vectors are the formats the decode shader
///   makes views of. A view of the wrong format is an invalid descriptor;
/// * the full-size target the resolve writes exists, has not been destroyed,
///   and is the size the feature is being asked to produce;
/// * the scene depth has not been destroyed;
/// * the last evaluation did not just fail.
///
/// The colour and motion vectors themselves need no liveness check here: an
/// entry for a destroyed resource never leaves `bindings::resolve_table`.
fn preflight(
    frame: u64,
    colour: usize,
    motion: usize,
    motion_format: DXGI_FORMAT,
    render: (u32, u32),
    display: (u32, u32),
    slotless: bool,
) -> Option<(u64, String)> {
    if frame < PAUSED_UNTIL.load(Ordering::Relaxed) {
        return Some((1, "an evaluation has just failed".into()));
    }
    if motion_format.0 != FORMAT_MOTION {
        return Some((
            2,
            format!(
                "the motion vector slot holds {motion:#x}, which is {} and not R32_UINT",
                format_name(motion_format)
            ),
        ));
    }
    match described(colour) {
        Some(info) if info.Format.0 == FORMAT_COLOUR => {}
        other => {
            return Some((
                3,
                format!(
                    "the colour slot holds {colour:#x}, which is {} and not R11G11B10_FLOAT",
                    other.map_or_else(|| "undescribed".into(), |info| format_name(info.Format))
                ),
            ))
        }
    }
    // The first frame of a session has no full-size target yet; `run_dlss`
    // handles that itself, by recording what it found and not resolving. And
    // with no upscale stage in the frame the target is not written at all,
    // so what has become of it does not matter.
    let target = if slotless {
        None
    } else {
        crate::hijack::upscale_target()
    };
    if let Some((target, window)) = target {
        if crate::alive::serial_of(target).is_none() {
            return Some((
                4,
                format!(
                    "the full-size target {target:#x} has been destroyed; the engine is \
                     rebuilding and the upscale slot has not named the new one yet"
                ),
            ));
        }
        let size = described(target).map(|info| (info.Width as u32, info.Height));
        if size != Some(window) {
            return Some((
                5,
                format!(
                    "the full-size target {target:#x} is {size:?} and the window is {}x{}",
                    window.0, window.1
                ),
            ));
        }
    }
    let clamped = (render.0.min(display.0), render.1.min(display.1));
    if let Some(depth) = pick_depth(frame, clamped, display) {
        if crate::alive::serial_of(depth).is_none() {
            return Some((6, format!("the scene depth {depth:#x} has been destroyed")));
        }
    }
    None
}

/// Put back the command list state our work overwrote.
///
/// Both the motion vector decode and the DLSS evaluation bind their own
/// descriptor heaps, root signature and pipeline. Everything the engine
/// records afterwards assumes its own, so leaving ours in place is what made
/// the game fault shortly after the first evaluation.
unsafe fn restore_state(this: *mut c_void, snapshot: &Snapshot) {
    let Snapshot {
        heaps,
        signature,
        pipeline,
        tables,
        cbvs,
        views,
        constants,
    } = snapshot.clone();

    if !heaps.is_empty() {
        let original: SetHeapsFn = std::mem::transmute(O_SET_HEAPS.load(Ordering::Relaxed));
        let pointers: Vec<*mut c_void> = heaps.iter().map(|h| *h as *mut c_void).collect();
        original(this, pointers.len() as u32, pointers.as_ptr());
    }
    if signature != 0 {
        let original: SetRootSigFn = std::mem::transmute(O_SET_ROOT_SIG.load(Ordering::Relaxed));
        original(this, signature as *mut c_void);
    }
    if pipeline != 0 {
        let original: SetPipelineFn = std::mem::transmute(O_SET_PIPELINE.load(Ordering::Relaxed));
        original(this, pipeline as *mut c_void);
    }

    // Root bindings go with the root signature, so they have to be replayed.
    let original: SetRootTableFn = std::mem::transmute(O_SET_ROOT_TABLE.load(Ordering::Relaxed));
    for (root, base) in tables {
        original(this, root, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: base });
    }
    let original: SetRootCbvFn = std::mem::transmute(O_SET_ROOT_CBV.load(Ordering::Relaxed));
    for (root, address) in cbvs {
        original(this, root, address);
    }
    for (root, address, unordered) in views {
        let slot = if unordered {
            &O_SET_ROOT_UAV
        } else {
            &O_SET_ROOT_SRV
        };
        let original: SetRootViewFn = std::mem::transmute(slot.load(Ordering::Relaxed));
        original(this, root, address);
    }
    let original: SetRootConstantsFn =
        std::mem::transmute(O_SET_ROOT_CONSTANTS.load(Ordering::Relaxed));
    for (root, values) in constants {
        original(
            this,
            root,
            values.len() as u32,
            values.as_ptr() as *const c_void,
            0,
        );
    }
}

/// Gather the temporal pass's inputs and evaluate DLSS in its place.
///
/// The colour, history and motion vector textures come from the bound
/// descriptor table, so they follow the engine's per-frame rotation. Depth is
/// not among them -- the pass binds a 1x1 dummy for `tDepthMap` -- and is
/// chosen from the depth targets bound this frame, by matching the size of the
/// rectangle being dispatched.
unsafe fn run_dlss(this: *mut c_void, groups_x: u32, groups_y: u32) -> bool {
    // Only a depth buffer bound during this frame is trustworthy; anything
    // older may since have been released. Which one it is cannot be settled
    // until the render rectangle is known, further down.
    let frame = FRAME.load(Ordering::Relaxed);
    // Whether the engine's upscale stage is part of the frame at all, asked
    // once here so that every decision below is made from one answer.
    crate::hijack::note_dispatch();
    let slotless = crate::hijack::slot_idle();
    // With an upscale stage in the frame, that stage is where the resolve is
    // made (`resolve_at_slot`), and this pass identifies the frame's inputs
    // for it and steps aside. Unless the route through this pass is asked
    // for (`resolve_at_pass`), or the stage has just shown that it cannot
    // resolve.
    let at_slot = !slotless && !crate::config::get().resolve_at_pass && !slot_route_given_up();

    let mut colour = 0usize;
    let mut output = 0usize;
    let mut motion = 0usize;
    let mut motion_format = DXGI_FORMAT(0);
    let mut display = (0u32, 0u32);

    let tables = with_list(this, |state| state.tables.clone());
    for (_, base) in tables {
        for (offset, slot) in bindings::resolve_table(base, 32).iter().enumerate() {
            let Slot::Resource(resource, view) = slot else {
                continue;
            };
            let Some(info) = described(*resource) else {
                continue;
            };
            if !frame_sized(info.Width, info.Height) {
                continue;
            }
            match (view, offset) {
                // u0 is rwOutput, t0 is tBaseMap and t4 is tGBuffer4; the
                // offsets come from the root signature, not from guesswork.
                (bindings::View::Uav, 0) => {
                    output = *resource;
                    display = (info.Width as u32, info.Height);
                }
                _ => {}
            }
            if matches!(view, bindings::View::Srv) && offset == COLOUR_OFFSET {
                colour = *resource;
            }
            if matches!(view, bindings::View::Srv) && offset == MV_OFFSET {
                motion = *resource;
                motion_format = info.Format;
            }
        }
    }

    // The jitter conversion needs this and has no other way to learn it.
    if display.0 != 0 && display.1 != 0 {
        DISPLAY.store(
            ((display.0 as u64) << 32) | display.1 as u64,
            Ordering::Relaxed,
        );
        // And the render scaling needs it as the base for its ratio, which is
        // also what keeps it from firing before the engine has allocated.
        crate::native::note_output_size(display);
    }

    if colour == 0 || output == 0 || motion == 0 {
        if !MISSING.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[dls] inputs incomplete: colour={colour:#x} output={output:#x} motion={motion:#x}"
            );
            // What the registry held, which says whether an input is missing
            // because nothing was ever recorded for its slot or because what
            // was recorded was too old to believe.
            crate::log!("[dls]   registry: {}", bindings::summary());
        }
        return false;
    }
    MISSING.store(false, Ordering::Relaxed);

    // The dispatch covers the render target in 16x16 groups, which is where
    // the render resolution comes from without having to read it back. That
    // holds because the render ratio is the engine's own, set through its
    // DLSS path (`hijack`), so its targets are the size it renders at.
    //
    // But the group count rounds up, and the targets are not multiples of
    // sixteen: at 3840 wide and a ratio of 0.667 the engine builds them 2561
    // wide and the dispatch covers 161 groups, 2576. Everything below that
    // turns normalised device coordinates into pixels -- the jitter and the
    // motion-vector scale -- divides by the width the camera rasterised into,
    // which is the target's. Using the rounded figure made the vectors 0.6%
    // too long there (1.2% at 1920 wide). So the clamp to the pass's output
    // is made here, before the first use, and not after the last.
    let render = (
        (groups_x * 16).min(display.0),
        (groups_y * 16).min(display.1),
    );
    // The measured values of the jitter and motion vector arithmetic below,
    // unless the expert keys say otherwise.
    let expert = crate::config::get();
    let reported = expert.expert_jitter_report;

    // Read the offset back out of the constant buffer this dispatch is using,
    // rather than from a value set aside when that buffer was written.
    //
    // The engine records on several threads, so it can write the next frame's
    // camera before this frame's dispatch is recorded, and a single shared
    // variable then hands over an offset belonging to a different frame. It
    // showed as a uniform error of about a third of a pixel across the whole
    // screen in some readings and none at all in others -- intermittent,
    // which is what a race looks like. The pass binds CBViewProjection
    // itself, so that matrix is the one thing that cannot be out of step with
    // the frame being resolved.
    //
    // `apply` writes proj[8] = -2*px/width, so px comes back as -proj[8]*w/2.
    // The root parameter the pass's own signature assigns to b0, which is
    // CBViewProjection. Taking the most recently bound buffer that merely
    // looked like a projection picked a shadow cascade instead -- 23 degrees
    // of field of view against the camera's 83 -- and the error was invisible
    // because every buffer carried the same offset, so the cancellation still
    // came out at zero.
    let signature = with_list(this, |state| state.signature);
    let camera_root = crate::rootsig::layout_of(signature).and_then(|layout| {
        layout
            .root_cbvs
            .iter()
            .find(|(_, register)| **register == 0)
            .map(|(root, _)| *root)
    });
    let bound = with_list(this, |state| state.cbvs.clone());
    let from_buffer = bound
        .iter()
        .filter(|(root, _)| camera_root.is_none_or(|wanted| *root == wanted))
        .rev()
        .find_map(|(_, address)| {
            // As much as the buffer declares, when that much is mapped: frame
            // generation is handed the whole of it below.
            let declared = CAMERA_BYTES
                .load(Ordering::Relaxed)
                .max(crate::camera::NEEDED);
            let bytes = bindings::read_gpu_memory(*address, declared)
                .or_else(|| bindings::read_gpu_memory(*address, crate::camera::NEEDED))?;
            let no_jitter = crate::camera::read(&bytes, crate::camera::F_NO_JITTER_PROJ)?;
            if !crate::camera::looks_like_projection(&no_jitter) {
                return None;
            }
            let proj = crate::camera::read(&bytes, crate::camera::F_PROJ)?;
            // And this is the camera frame generation is given, for the same
            // reason: it is the one the pass being replaced reads, so it is
            // the main view by construction and not by resemblance.
            //
            // One read, of the right buffer. A hook on every root constant
            // buffer bind would hand over any buffer shaped like a
            // perspective camera, the latest winning, and the engine uploads
            // several such views a frame -- probes, cubemap faces, the
            // perspective shadow projections -- at the cost of reading
            // write-combined memory fifteen thousand times a frame.
            //
            // The same shape test is applied, so only a perspective camera
            // is accepted.
            if crate::camera::is_view_projection(&bytes) && crate::config::get().fg {
                crate::fg::note_camera(&bytes);
            }
            // This is the camera being resolved, by definition: the pass is
            // reading it. Its shape is said once and kept; nothing reads it
            // back, so the line claims no more than what was read.
            let mut resolved = RESOLVED_VIEW.lock();
            if resolved.is_none() {
                crate::log!(
                    "[jit] the view the temporal pass reads has focal terms ({:.5},{:.5})",
                    no_jitter[0],
                    no_jitter[5]
                );
            }
            *resolved = Some((no_jitter[0], no_jitter[5]));
            // proj[8] and proj[9] hold an offset in normalised device space,
            // and the pixels that corresponds to are the ones the camera
            // rasterised into -- the render resolution, not the output.
            //
            // The engine divides by its viewport width, which is the render
            // target. Dividing by the display size instead agrees at DLAA and
            // differs by the upscale factor everywhere else, which would hand
            // DLSS an offset too large by exactly that factor.
            let (width, height) = (render.0 as f32, render.1 as f32);
            Some((-proj[8] * width / 2.0, -proj[9] * height / 2.0))
        });
    if from_buffer.is_none() && !NO_CAMERA_CBV.swap(true, Ordering::Relaxed) {
        crate::log!(
            "[dls] the temporal pass's camera could not be read, so DLSS is told no jitter \
             and frame generation gets no camera"
        );
        // Which of the two reasons, because they have nothing in common as
        // fixes. This replaces a line that fired from the bind hook for the
        // first unmapped constant buffer of any kind and blamed the launcher
        // for it, including in sessions the launcher had started.
        if bound.is_empty() {
            crate::log!("[dls]   it binds no root constant buffer at all");
        }
        for (root, address) in &bound {
            let mapped = bindings::read_gpu_memory(*address, crate::camera::NEEDED).is_some();
            crate::log!(
                "[dls]   root {root} at {address:#x}: {}",
                if mapped {
                    "readable, but not shaped like a projection"
                } else {
                    "not readable -- its upload heap was mapped before our hooks existed, \
                     which means this library loaded after the engine came up. Launch the \
                     game through mhw-dlss-launcher.exe"
                }
            );
        }
    }
    // With no camera buffer to read it from, no offset is reported: there is
    // no second source for it since the external jitter went.
    let jitter_raw = from_buffer.unwrap_or((0.0, 0.0));
    let jitter = (jitter_raw.0 * reported.0, jitter_raw.1 * reported.1);

    if REPORTED_MV.swap(MV_OFFSET, Ordering::Relaxed) != MV_OFFSET {
        crate::log!(
            "[dls] motion vectors from descriptor +{} ({})",
            MV_OFFSET,
            format_name(motion_format)
        );
    }

    // In screen pixels the offset applied to a frame is (+px, -py): the
    // projection shift is +ndc because this projection has w = -z, and the
    // viewport then flips y.
    //
    // The decode is given this together with `expert_mv_dejitter`, the
    // fraction of it to add back to the vectors, which is zero as shipped
    // (`MV_DEJITTER`): with the engine's own jitter its vectors are already
    // jitter-free.
    let applied = (jitter_raw.0, -jitter_raw.1);

    // Everything that can be checked is checked here, before a single command
    // of ours goes onto the engine's list. What follows records a decode,
    // barriers and the evaluation itself, and a list with a bad command on it
    // cannot be mended: the engine finds out at Close, reports
    // `E_INVALIDARG : commandList->Close()` and stops. So anything that would
    // make those commands wrong has to be found first, and the answer to
    // finding it is to leave the frame to the engine's own pass.
    if let Some(reason) = preflight(
        frame,
        colour,
        motion,
        motion_format,
        render,
        display,
        slotless || at_slot,
    ) {
        if PREFLIGHT_SAID.swap(reason.0, Ordering::Relaxed) != reason.0 {
            crate::log!("[dls] leaving the frame to the engine: {}", reason.1);
            crate::log!("[dls]   registry: {}", bindings::summary());
        }
        return false;
    }
    PREFLIGHT_SAID.store(0, Ordering::Relaxed);

    // The engine packs two components into one R32_UINT word, which DLSS
    // cannot read. Unpacking into a float texture of our own also puts the
    // packing, sign and scale under config control, so they can be matched to
    // the picture while the game runs.
    //
    // The same pass looks at the colour while it is there. A pixel that is
    // not finite comes out of DLSS black and takes its neighbourhood with it
    // through the accumulation, and the decode already reads every pixel, so
    // counting and optionally replacing them costs no extra pass.
    // The engine's vectors are normalised device coordinates, so the pixels
    // they stand for are half the resolution they were produced at -- the
    // render target. Only at DLAA is that also the window, which is why the
    // configured pair, measured when the two were equal, is the display size
    // halved and becomes too long by the upscale factor the moment they are
    // not. Vectors half again too long send DLSS to the wrong part of its
    // history: what does not match is discarded, which aliases, and what does
    // is blended, which blurs. Both at once, which is what this looked like.
    //
    // `expert_mv_scale_x` and `_y` multiply it, and are 1 as shipped.
    let mv_scale = (
        -(render.0 as f32) / 2.0 * expert.expert_mv_scale.0,
        render.1 as f32 / 2.0 * expert.expert_mv_scale.1,
    );
    let motion_engine = motion;
    let (motion, motion_scale) = match crate::mv::decode(
        this,
        motion,
        colour,
        (render.0.min(display.0), render.1.min(display.1)),
        mv_scale,
        MV_MODE,
        applied,
        expert.expert_mv_dejitter,
    ) {
        Some(decoded) => (decoded as usize, (1.0, 1.0)),
        // The decode can fail, and then DLSS is handed the engine's own
        // buffer with the scale that converts it. `fg.rs` reads this case
        // off the scale not being (1, 1).
        None => (motion, mv_scale),
    };
    let render = (render.0.min(display.0), render.1.min(display.1));

    let Some(depth) = pick_depth(frame, render, display) else {
        if !NO_DEPTH.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[dls] no {}x{} depth target bound this frame",
                render.0,
                render.1
            );
            // What the pass actually looks like, so the rule for finding the
            // scene depth can be written from measurement. With resolution
            // scaling the engine may render a sub-rectangle of full-size
            // targets, in which case nothing is the render size and the
            // exact-match rule can never succeed.
            let sizes = |resource: usize| {
                described(resource)
                    .map(|info| format!("{}x{}", info.Width, info.Height))
                    .unwrap_or_else(|| "unknown".into())
            };
            crate::log!(
                "[dls]   dispatch covers {}x{}; output {} colour {} motion {}",
                render.0,
                render.1,
                sizes(output),
                sizes(colour),
                sizes(motion)
            );
            if let Some(depths) = DEPTHS.lock().as_ref() {
                let mut recent: Vec<String> = depths
                    .iter()
                    .filter(|(_, (seen, _, _))| frame.abs_diff(*seen) <= 1)
                    .map(|(resource, (_, width, height))| format!("{resource:#x} {width}x{height}"))
                    .collect();
                recent.sort();
                crate::log!(
                    "[dls]   depth targets bound this frame: {}",
                    if recent.is_empty() {
                        "none".into()
                    } else {
                        recent.join(", ")
                    }
                );
            }
        }
        return false;
    };
    NO_DEPTH.store(false, Ordering::Relaxed);

    // NGX reads the colour and writes the output. If the engine ever hands
    // back the same texture for both -- a ping-pong landing on one buffer, a
    // frame where the history and the target coincide -- it would be read and
    // written at once and the result is undefined, which on screen is a whole
    // surface going black. The engine's own pass is left to run instead.
    if colour == output || depth == colour || motion == colour || motion == output {
        if !ALIASED.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[dls] inputs alias: colour={colour:#x} output={output:#x} \
                 depth={depth:#x} motion={motion:#x}; leaving the frame alone"
            );
        }
        return false;
    }
    ALIASED.store(false, Ordering::Relaxed);

    // Everything handed to NGX, once. The first evaluation took the process
    // down without returning, and a crash inside a closed-source library can
    // only be narrowed by knowing precisely what went in.
    if !DESCRIBED.swap(true, Ordering::Relaxed) {
        for (label, resource) in [
            ("colour", colour),
            ("output", output),
            ("depth", depth),
            ("motion", motion),
        ] {
            match described(resource) {
                Some(info) => crate::log!(
                    "[dls] {label:>6} {resource:#x} {}x{} mips={} {} flags={:#x} samples={}",
                    info.Width,
                    info.Height,
                    info.MipLevels,
                    format_name(info.Format),
                    info.Flags.0,
                    info.SampleDesc.Count
                ),
                None => crate::log!("[dls] {label:>6} {resource:#x} has no description"),
            }
        }
        crate::log!(
            "[dls] render={}x{} display={}x{} groups={}x{} jitter=({:.3},{:.3}) scale=({},{})",
            render.0,
            render.1,
            display.0,
            display.1,
            groups_x,
            groups_y,
            jitter.0,
            jitter.1,
            motion_scale.0,
            motion_scale.1
        );
    }

    // NGX samples colour, depth and motion vectors over the whole render
    // rectangle. A texture smaller than that is read out of its own bounds,
    // which is a guess this library has no business making.
    let fits = |resource: usize| {
        described(resource)
            .map(|info| info.Width as u32 >= render.0 && info.Height >= render.1)
            .unwrap_or(false)
    };
    if !fits(colour) || !fits(depth) || !fits(motion) {
        if !MISMATCH.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[dls] an input is smaller than the {}x{} render rectangle; skipping",
                render.0,
                render.1
            );
        }
        return false;
    }

    // The resolve is made at the engine's upscale stage, not here, whenever
    // there is one. This pass then only identifies the frame's inputs -- the
    // motion vectors are decoded above, the depth is chosen -- and records
    // them for `resolve_at_slot`.
    //
    // Resolving here would write the full-size target directly from the
    // colour this pass reads, and everything the engine does to the picture
    // between this pass and its upscale stage would be skipped: two
    // full-screen passes at render size, which carry its tone. That was the
    // paler picture below a render ratio of 1.0, and the interface ghosting
    // under frame generation went with it. At 1.0 there is no upscale stage,
    // the resolve goes into this pass's own output and those passes run on
    // it.
    if at_slot {
        crate::hijack::note_frame(
            &crate::dlss::Frame {
                colour: colour as *mut c_void,
                output: output as *mut c_void,
                depth: depth as *mut c_void,
                motion: motion as *mut c_void,
                render,
                display,
                jitter,
                motion_scale,
                reset: false,
            },
            this as usize,
        );
        keep_alive(&[colour, output, depth, motion]);
        // This frame's colour goes into the pass's output as it is, so what
        // reaches the upscale stage has been through the engine's passes and
        // not through its temporal filter: the temporal work is DLSS's, done
        // once, there. If the copy cannot be made the engine's own pass runs
        // and the picture is filtered twice, which is soft and not wrong.
        return copy_through(this, colour, output);
    }

    // Where the resolve is to be written: the engine's full-size target once
    // its upscale slot has named one, this pass's own output when there is
    // no upscale stage in the frame. The comment at the substitution below
    // says why.
    //
    // Until the slot has run once the full-size target is not known, so
    // there is nowhere to resolve to and the engine's own pass runs for this
    // one frame. That is decided here, before anything is recorded on the
    // engine's list: once the depth target has been unbound and the forward
    // barriers recorded, returning without undoing them would leave the
    // engine's own temporal dispatch and the rest of that list running with
    // the scene depth in the state NGX wants and no depth target bound.
    let elsewhere = if slotless {
        None
    } else {
        crate::hijack::upscale_target()
    };
    if elsewhere.is_none() && !slotless {
        // What the frame would have been resolved from is still recorded:
        // the upscale slot does nothing, not even name its target, until
        // there is a record.
        keep_alive(&[colour, output, depth, motion]);
        crate::hijack::note_frame(
            &crate::dlss::Frame {
                colour: colour as *mut c_void,
                output: output as *mut c_void,
                depth: depth as *mut c_void,
                motion: motion as *mut c_void,
                render,
                display,
                jitter,
                motion_scale,
                reset: false,
            },
            this as usize,
        );
        return false;
    }

    // The texture that is written is settled before anything is planned for
    // it, so that the transitions are planned for the texture NGX is given
    // and the test for an input being the output is made against that one.
    let pass_output = output;
    let (output, display) = match elsewhere {
        Some(target) => target,
        None => (output, display),
    };
    if output == colour || output == depth || output == motion {
        if !ALIASED.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[dls] the texture to be written, {output:#x}, is also an input (colour \
                 {colour:#x}, depth {depth:#x}, motion {motion:#x}); leaving the frame alone"
            );
        }
        return false;
    }

    // NGX reads these as shader resources and writes the output through an
    // unordered access view. Anything already in the right state is left
    // alone; anything whose state was never observed means the plan cannot be
    // trusted, and skipping is better than guessing.
    //
    // With one exception, said when it is made: the engine's full-size
    // target when nothing has been seen of its state on this list. Leaving
    // the frame for that would leave the target unwritten on every frame,
    // and it was written without a transition for as long as this route was
    // the only one.
    let output_tracked =
        output == pass_output || crate::states::known(this as usize, output).is_some();
    if !output_tracked {
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[dls] the state of the full-size target {output:#x} has never been observed, \
                 so it is handed to NGX without a transition, as it always was"
            );
        }
    }
    let mut wanted = vec![
        (colour, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE),
        (motion, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE),
        (depth, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE),
    ];
    if output_tracked {
        wanted.push((output, D3D12_RESOURCE_STATE_UNORDERED_ACCESS));
    }
    let Some(moves) = crate::states::plan(this as usize, &wanted) else {
        if !UNKNOWN_STATE.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[dls] {} transitions recorded so far",
                crate::states::tracked()
            );
            for (resource, _) in wanted {
                crate::log!(
                    "[dls] state of {resource:#x}: {}",
                    crate::states::known(this as usize, resource)
                        .map(crate::states::name)
                        .unwrap_or_else(|| "never observed".into())
                );
            }
        }
        return false;
    };
    UNKNOWN_STATE.store(false, Ordering::Relaxed);

    if !REPORTED_STATES.swap(true, Ordering::Relaxed) {
        for step in &moves {
            crate::log!(
                "[dls] moving {:#x} from {} to {}",
                step.resource,
                crate::states::name(step.from),
                crate::states::name(step.to)
            );
        }
    }

    // Transitioning a resource bound as a depth target is invalid, so take it
    // off the list first and put it back when the evaluation is done.
    let unbound = unbind_depth(this, depth);
    if unbound.is_some() && !STILL_BOUND.swap(true, Ordering::Relaxed) {
        crate::log!("[dls] depth {depth:#x} unbound for the evaluation");
    }

    let list = ID3D12GraphicsCommandList::from_raw_borrowed(&this);
    let barriers: Vec<D3D12_RESOURCE_BARRIER> = moves
        .iter()
        .map(|step| crate::states::barrier(step, false))
        .collect();
    if let (Some(list), false) = (list, barriers.is_empty()) {
        list.ResourceBarrier(&barriers);
    }

    // This pass writes a render-sized target, so on its own it can only
    // resolve, never upscale. The engine's upscale slot owns a full-size one
    // -- and having seen it once, the resolve can be done here and sent
    // straight there instead, which is earlier in the frame and is where this
    // library has always worked.
    //
    // Unless the engine is not running that slot, which is the case at a
    // render ratio of 1.0. Then nothing reads the full-size target, the
    // picture the engine goes on to present is this pass's own output, and
    // that is where the resolve has to go: render and output are the same
    // size, which is DLAA. Writing the remembered target instead resolved
    // every frame into a texture nobody showed while the one that was shown
    // went unwritten -- a frozen picture.
    // (The substitution itself is made above, before the state plan.)
    let frame = crate::dlss::Frame {
        colour: colour as *mut c_void,
        output: output as *mut c_void,
        depth: depth as *mut c_void,
        motion: motion as *mut c_void,
        render,
        display,
        jitter,
        motion_scale,
        reset: false,
    };
    // Taken before the work is recorded, dropped only several frames later.
    keep_alive(&[colour, output, depth, motion]);

    // What this frame is resolved from, recorded every frame: it is what
    // frame generation tags its depth and motion vectors from.
    crate::hijack::note_frame(&frame, this as usize);
    // What is about to be handed to NGX, said when its shape changes and
    // only with `path_dump` on.
    crate::pathdump::resolve(&crate::pathdump::Resolve {
        at_slot: false,
        colour,
        depth,
        motion_engine,
        motion,
        pass_output,
        written: output,
        groups: (groups_x, groups_y),
        render,
        display,
        jitter,
        decode_scale: mv_scale,
        motion_scale,
        slotless,
    });

    let perf = crate::perf::start(crate::perf::Family::Evaluate);
    // Through `sharpen`, as at the upscale stage. Not sharpened when the
    // target's state was never observed: NGX is handed it as it is, and this
    // library's own pass would be writing a texture it cannot vouch for.
    let evaluated = crate::sharpen::evaluate(this, &frame, output_tracked);
    perf.stop();
    // Counted as a resolve once it is one. It was counted before the
    // evaluation, so an evaluation that failed every time still read, to the
    // code watching for a picture that has stopped, as a frame resolved.
    if evaluated {
        crate::hijack::note_resolved();
    }
    if !evaluated {
        // NGX has refused inputs that passed every check above, so something
        // about them is wrong in a way this code cannot see. Trying again on
        // the very next frame with the same inputs is how one bad frame
        // becomes a broken list; a short pause lets whatever the engine was
        // in the middle of -- it is usually a rebuild -- finish first. Short,
        // because `hijack` reads five missed frames as anti-aliasing having
        // been switched off.
        let until = FRAME.load(Ordering::Relaxed) + EVALUATE_PAUSE;
        PAUSED_UNTIL.store(until, Ordering::Relaxed);
        crate::log!(
            "[dls] the evaluation failed with every input checked; the next {EVALUATE_PAUSE} \
             frames are left to the engine's own pass. {}",
            crate::alive::summary()
        );
    }

    // Put everything back the way the engine left it, whether or not the
    // evaluation worked.
    let back: Vec<D3D12_RESOURCE_BARRIER> = moves
        .iter()
        .map(|step| crate::states::barrier(step, true))
        .collect();
    if let (Some(list), false) = (
        ID3D12GraphicsCommandList::from_raw_borrowed(&this),
        back.is_empty(),
    ) {
        list.ResourceBarrier(&back);
    }
    if let Some(bound) = unbound.as_ref() {
        rebind_depth(this, bound);
    }

    if evaluated {
        EVALUATED.fetch_add(1, Ordering::Relaxed);
        // Said here, after the evaluation and only when it happened: a line
        // about where the resolve went must not be written for a frame on
        // which nothing was resolved.
        say_route(if slotless { 1 } else { 2 }, colour, Some(output));
    }
    // With no upscale stage, this is also the hand-over point the slot would
    // have been: the frame's inputs exist, are identified, and the list is
    // back in the engine's states. Frame generation's token, constants and
    // tags hang off this call, and so does the window the interface-free
    // capture watches; without it at a render ratio of 1.0 the token never
    // advanced and nothing was ever filed under it.
    if evaluated && slotless {
        let window = crate::swap::chain().map_or(display, |chain| chain.size);
        crate::hudless::open_frame();
        crate::fg::note_frame_inputs(this, window);
    }
    evaluated
}

unsafe extern "system" fn hook_close(this: *mut c_void) -> HRESULT {
    let perf = crate::perf::start(crate::perf::Family::ListClose);
    LISTS_CLOSED.fetch_add(1, Ordering::Relaxed);
    // The recording is held, not discarded: its transitions take effect when
    // the list is submitted, and discarding them here is what left the state
    // of everything the engine moved unknown.
    crate::states::close(this as usize);
    // Everything, not a chosen few. A command list's address is recycled, and
    // a new list allocated where a dead one stood would otherwise inherit its
    // render targets, its depth target and its root bindings.
    //
    // Emptied in place, so the vectors keep their storage: a list is closed
    // and recorded again every frame, and replacing the state with a fresh
    // one meant every bind after a close began with an allocation.
    with_list(this, ListState::reset);
    perf.stop();
    let original: CloseFn = std::mem::transmute(O_CLOSE.load(Ordering::Relaxed));
    let result = original(this);

    // A command list only fails to close when something recorded into it was
    // invalid. Since this library records into the game's lists, that is worth
    // knowing immediately rather than inferring from a crash.
    if result.is_err() && !CLOSE_FAILED.swap(true, Ordering::Relaxed) {
        crate::log!("[list] Close() failed with {:#010x}", result.0);
    }
    result
}

/// Give the engine a sub-pixel offset to build its own jittered projection
/// from.
///
/// This runs per submission rather than per frame, so the camera setup -- which
/// runs once a frame -- consumes every n-th element of the sequence rather than
/// every one. That costs nothing: the offset DLSS is told is measured back from
/// `fProj` against `fNoJitterProj`, not assumed from the index, so whichever
/// sample the engine picked up is the one DLSS hears about. A frame-exact tick
/// would need a hook on something that happens once a frame, and there is no
/// such hook yet.
unsafe fn offset_the_engines_camera() {
    // Nothing of the engine's is written until `gate` has recognised the
    // build and NGX has come up with DLSS available, and nothing after it
    // has been decided that DLSS cannot run: a session without DLSS is to be
    // the game as it is without the mod. The renderer submitting work is
    // also proof that the executable has been unpacked, which `gate` is told
    // from the same call.
    //
    // This does not change when anything is written in a session that
    // This does not delay anything in a session that works: the game
    // initialises NGX while its renderer starts, before its first
    // submission.
    if !crate::gate::engaged() || !crate::gate::dlss_was_ready() {
        crate::gate::waiting_for_dlss();
        return;
    }
    if crate::gate::dlss_unusable() {
        // DLSS was in use and has since been found unusable, so the engine
        // may still be rendering small. This one call stays, because it is
        // what takes the engine back to rendering the whole window once
        // `hijack` asks for that; it writes only when the ratio it is asked
        // for differs from the one the engine has built.
        crate::hijack::follow_configured_ratio();
        return;
    }
    let config = crate::config::get();
    // With a distance of zero, unless `expert_alpha_unroll` gives one. Zero
    // reads as "leave the engine's own distances alone" and is not quite what
    // it does: `unroll` takes the engine's *far* distance as the original and
    // writes it to both fields, so the near one goes from 0 to 1e8 --
    // `[tun] alpha is drawn whole out to 100000000.0` in every session's
    // log. Kept exactly as it is because removing it would change the
    // picture that was verified; whether that picture is the one wanted is
    // an open question.
    crate::tune::unroll(config.expert_alpha_unroll);
    crate::tune::mip_bias(crate::hijack::mip_bias_bank());
    report_sampler_biases(&config);
    crate::tune::note_sampler_override();
    crate::tune::undither(config.undither);
    // Cheap, and it is what makes the render resolution a live setting: the
    // engine rebuilds its targets by itself once the two ratios it keeps
    // disagree.
    crate::hijack::follow_configured_ratio();
    // The offset itself is not written here. The detour on the camera setup
    // writes it immediately before the engine reads it, once per frame, and
    // `gate` installed that detour with the rest; this only lets it start,
    // now that the engine is known to be rendering through Direct3D 12.
    // Writing here as well would advance the sequence several times between
    // the camera updates that consume it.
    crate::native::start_jitter();
}

/// Submission is where recorded barriers take effect, so it is where the
/// committed view of resource states is brought up to date.
///
/// A command list pointer is the same address through every interface in its
/// chain, so the `ID3D12CommandList` pointers handed here key the same
/// recordings that `ResourceBarrier` and `Close` saw through
/// `ID3D12GraphicsCommandList`.
unsafe extern "system" fn hook_execute_command_lists(
    this: *mut c_void,
    count: u32,
    lists: *const *mut c_void,
) {
    // Before the call, not after: the states these lists reach are what any
    // list recorded from now on has to start from, and another thread can be
    // recording the moment this returns.
    // Recorded for every queue, including our own lists: the transitions in
    // them are real state changes and the committed table has to stay true.
    let perf = crate::perf::start(crate::perf::Family::Execute);
    crate::states::submit(lists, count);
    if reentrant() {
        perf.stop();
        let original: ExecuteCommandListsFn =
            std::mem::transmute(O_EXECUTE_COMMAND_LISTS.load(Ordering::Relaxed));
        original(this, count, lists);
        return;
    }

    // Everything below belongs to an engine frame, and this hook is on the
    // vtable every queue in the process shares -- the engine's graphics, copy
    // and compute queues, and once frame generation is loaded the extra
    // graphics queue `sl.dlss_g` makes to present asynchronously. So it is
    // keyed on the queue the engine handed to `CreateSwapChainForHwnd`, which
    // is its graphics queue by definition. See `queue.rs`.
    //
    // Permissive until that is known, because this is what installs the engine
    // detours and submissions happen during renderer setup, before any
    // swapchain exists. Everything it does is latched or written only on
    // change, so an extra caller cannot change the outcome -- and with
    // `native_detour` on, which is the configuration that works, it returns
    // before the jitter is touched at all.
    if crate::queue::is_engine_or_unknown(this) {
        offset_the_engines_camera();
    }
    // Strict, because a marker needs the engine's frame boundary and nothing
    // else. Before a swapchain exists no frame is being presented, so there is
    // nothing to mark and not firing is correct rather than a gap.
    if crate::queue::is_engine(this) {
        crate::fg::note_render_submit();
    }
    perf.stop();
    let original: ExecuteCommandListsFn =
        std::mem::transmute(O_EXECUTE_COMMAND_LISTS.load(Ordering::Relaxed));
    original(this, count, lists);
}

// ---------------------------------------------------------------------------
// Device hooks
// ---------------------------------------------------------------------------

unsafe extern "system" fn hook_create_srv(
    this: *mut c_void,
    resource: *mut c_void,
    desc: *const D3D12_SHADER_RESOURCE_VIEW_DESC,
    destination: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let perf = crate::perf::start(crate::perf::Family::DeviceCreate);
    bindings::record_view(resource, destination.ptr, View::Srv);
    perf.stop();
    let original: CreateSrvFn = std::mem::transmute(O_CREATE_SRV.load(Ordering::Relaxed));
    original(this, resource, desc, destination);
}

unsafe extern "system" fn hook_create_root_sig(
    this: *mut c_void,
    node_mask: u32,
    blob: *const c_void,
    len: usize,
    riid: *const windows::core::GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let original: CreateRootSigFn = std::mem::transmute(O_CREATE_ROOT_SIG.load(Ordering::Relaxed));
    let result = original(this, node_mask, blob, len, riid, out);
    let perf = crate::perf::start(crate::perf::Family::DeviceCreate);
    if result.is_ok() && !out.is_null() && !(*out).is_null() {
        crate::rootsig::record(*out, blob, len);
    }
    perf.stop();
    result
}

/// Note a texture, once, with whatever is known about it.
/// What a resource was created as, without touching the resource itself.
pub(crate) fn described(resource: usize) -> Option<D3D12_RESOURCE_DESC> {
    if resource == 0 {
        return None;
    }
    DESCS
        .lock()
        .as_ref()
        .and_then(|known| known.get(&resource).copied())
}

/// The smallest a texture can be and still be one of the engine's full-frame
/// targets at a render ratio this library supports: half the display each
/// way, less the sixteen pixels the engine may cut the width down by.
///
/// It was three constants -- 960 wide to be followed at all, 1280x720 to be
/// a scene depth -- which were right for a 3840x2160 display and wrong below
/// it: at 1920x1080 and a ratio of 0.5 or 0.58 the scene depth is 960x540 or
/// 1113x626, no depth was ever found, and nothing was resolved. The test is
/// relative to the display the engine says it is at.
fn frame_floor(display: (u32, u32)) -> (u32, u32) {
    let floor = |size: u32| {
        ((size as f32 * crate::hijack::SMALLEST) as u32)
            .saturating_sub(16)
            .max(64)
    };
    (floor(display.0), floor(display.1))
}

/// The floor for the display size last noted, packed; until one is, the
/// floor of a 960x540 display, so that nothing a small window needs is
/// missed while the engine's configuration cannot yet be read.
static FRAME_FLOOR: AtomicU64 = AtomicU64::new((464 << 32) | 254);

/// The engine's display size, as read once per submission.
pub(crate) fn note_display(display: (u32, u32)) {
    let (width, height) = frame_floor(display);
    FRAME_FLOOR.store(((width as u64) << 32) | height as u64, Ordering::Relaxed);
}

/// Whether a texture this size could be a full-frame target, by the floor
/// last noted. One relaxed load: this is what the per-call hooks use.
fn frame_sized(width: u64, height: u32) -> bool {
    let floor = FRAME_FLOOR.load(Ordering::Relaxed);
    width >= floor >> 32 && height as u64 >= floor & 0xffff_ffff
}

/// The same against the display size the engine holds at this moment, for
/// the places that decide once, when a texture or a view of it is created:
/// the engine writes a new display size before it builds targets for it, so
/// a floor remembered from the last submission would be the old window's.
fn frame_sized_now(width: u64, height: u32) -> bool {
    match crate::hijack::display_now() {
        Some(display) => {
            let (floor_width, floor_height) = frame_floor(display);
            width >= floor_width as u64 && height >= floor_height
        }
        None => frame_sized(width, height),
    }
}

/// Whether a view of this resource could ever be one the temporal pass's
/// inputs are resolved to, and so whether `bindings` should follow it.
///
/// The rule is `run_dlss`'s own, stated once more where the views are
/// created: it skips anything without a recorded description or too small
/// to be a full-frame target (`frame_floor`). Depth targets are left out as
/// well. The pass never takes its depth
/// from a table -- it binds a 1x1 dummy there, and the real one is found
/// through its depth-stencil view -- while shadow maps and the scene depth
/// are sampled by a great many draws, each of which copies their descriptor.
///
/// Returns the serial `alive` knows the resource by, which the record of the
/// view carries from then on. No serial, no record: a resource without a
/// living witness is one whose destruction would go unnoticed.
pub(crate) fn worth_tracking(resource: usize) -> Option<u64> {
    let fits = described(resource).is_some_and(|info| {
        frame_sized_now(info.Width, info.Height)
            && info.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 == 0
    });
    if !fits {
        return None;
    }
    crate::alive::serial_of(resource)
}

fn note_texture(resource: usize, note: &str) {
    if resource == 0 {
        return;
    }
    let mut guard = TEXTURES.lock();
    let seen = guard.get_or_insert_with(std::collections::HashMap::new);
    let entry = seen.entry(resource).or_default();
    if !entry.contains(note) {
        if !entry.is_empty() {
            entry.push(' ');
        }
        entry.push_str(note);
    }
}

unsafe extern "system" fn hook_create_committed(
    this: *mut c_void,
    heap: *const c_void,
    heap_flags: u32,
    desc: *const D3D12_RESOURCE_DESC,
    state: i32,
    clear: *const c_void,
    riid: *const windows::core::GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let original: CreateCommittedFn =
        std::mem::transmute(O_CREATE_COMMITTED.load(Ordering::Relaxed));
    let result = original(this, heap, heap_flags, desc, state, clear, riid, out);

    let perf = crate::perf::start(crate::perf::Family::DeviceCreate);
    if result.is_ok() && !desc.is_null() && !out.is_null() && !(*out).is_null() {
        let info = &*desc;
        if info.Width >= 256 && info.Height >= 256 {
            DESCS
                .lock()
                .get_or_insert_with(AddressMap::default)
                .insert(*out as usize, *info);
            // And the state it starts in, so that nothing left behind by an
            // earlier resource at this address is taken for its own.
            crate::states::created(*out as usize, state);
        }
        // A witness on anything that could be handed to DLSS -- colour,
        // motion vectors, output, depth -- so that its destruction is known
        // rather than guessed at. See `alive`.
        if frame_sized_now(info.Width, info.Height) {
            crate::alive::watch(*out);
        }
        // Only full-frame surfaces are interesting; the heap is otherwise full
        // of small buffers and scratch textures.
        if info.Height >= 540 && info.Width >= 960 {
            crate::logv!(
                "[tex] res={:p}  {}x{}  mips={}  {}  flags={:#x}",
                *out,
                info.Width,
                info.Height,
                info.MipLevels,
                format_name(info.Format),
                info.Flags.0
            );
            note_texture(*out as usize, "created");
        }
    }
    perf.stop();
    result
}

unsafe extern "system" fn hook_resource_barrier(
    this: *mut c_void,
    count: u32,
    barriers: *const D3D12_RESOURCE_BARRIER,
) {
    let perf = crate::perf::start(crate::perf::Family::ListBarrier);
    // Recorded even for our own barriers: they are real state changes, and the
    // table has to stay true or the next frame's plan will be wrong.
    crate::states::observe(this as usize, barriers, count);
    perf.stop();
    let original: ResourceBarrierFn =
        std::mem::transmute(O_RESOURCE_BARRIER.load(Ordering::Relaxed));
    original(this, count, barriers);
}

/// The depth target bound this frame holding the scene depth.
///
/// Two exact sizes are accepted, in order: the rectangle the dispatch covers,
/// and failing that the size of the pass's own output. Still exact, never
/// "big enough" -- a shadow atlas can be larger than the screen, so that is
/// not a test which separates it from the scene depth.
///
/// The second size is what the engine's own Resolution Scaling option needs
/// (cfg+0x1f0, which `hijack` now holds at 1.0, so this is a fallback and not
/// the usual case). Under that option the engine keeps its targets at full
/// size and renders a sub-rectangle of them, so with it
/// on there is no depth buffer the size of the dispatch at all and the first
/// rule can never match -- which is exactly how this failed, silently, with
/// nothing replaced and the engine's own soft upscale left on screen.
fn pick_depth(frame: u64, render: (u32, u32), output: (u32, u32)) -> Option<usize> {
    let guard = DEPTHS.lock();
    let depths = guard.as_ref()?;
    let recent = |seen: &u64| {
        // Within a frame rather than exactly it: the stamp is taken on
        // whichever thread binds the target while the frame number advances on
        // the thread that dispatches, so an exact match loses a depth bound
        // either side of the increment.
        frame.abs_diff(*seen) <= 1
    };
    let of_size = |wanted: (u32, u32)| -> Vec<(usize, u64)> {
        depths
            .iter()
            .filter(|(_, (seen, width, height))| {
                recent(seen) && *width == wanted.0 && *height == wanted.1
            })
            .map(|(resource, (seen, _, _))| (*resource, *seen))
            .collect()
    };
    let mut matches = of_size(render);
    if matches.is_empty() && output != render && output.0 != 0 {
        matches = of_size(output);
        if !matches.is_empty() && !SUBRECT_DEPTH.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[dls] no depth target of the size the dispatch covers was bound this frame; \
                 the one taken as the scene depth is {}x{}, the size of the pass's output, \
                 while the dispatch covers {}x{}",
                output.0,
                output.1,
                render.0,
                render.1
            );
        }
    }
    if matches.is_empty() {
        return None;
    }

    // Deterministically, and the most recently bound first. The candidates
    // come out of a hash map, whose order is not specified: with more than
    // one full-resolution depth target in play an unsorted choice could
    // differ from frame to frame, and handing DLSS a different depth buffer
    // on alternate frames wrecks its disocclusion test -- whole surfaces
    // losing their history in patches.
    matches.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    // Whether there is ever more than one is the question; say so once.
    if matches.len() > 1 && !MANY_DEPTHS.swap(true, Ordering::Relaxed) {
        crate::log!(
            "[dls] {} depth targets match {}x{} this frame: {}",
            matches.len(),
            render.0,
            render.1,
            matches
                .iter()
                .map(|(resource, seen)| format!("{resource:#x}@{seen}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    // The most recently bound. Two match on this engine, the log says so
    // once above, and this is the one every working session has used.
    Some(matches[0].0)
}

unsafe extern "system" fn hook_om_set_targets(
    this: *mut c_void,
    count: u32,
    targets: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    single_range: i32,
    depth: *const D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let perf = crate::perf::start(crate::perf::Family::ListTargets);
    if !depth.is_null() {
        let handle = (*depth).ptr;
        let resource = DSV_HANDLES
            .lock()
            .as_ref()
            .and_then(|map| map.get(&handle).copied())
            .unwrap_or(0);
        // With RTsSingleHandleToDescriptorRange set, the argument points at
        // one handle standing for a contiguous range, not at an array of
        // `count` of them. Reading `count` entries then runs off the end of a
        // single stack variable, which is what took the game down.
        let supplied = if single_range != 0 {
            1
        } else {
            count.min(8) as usize
        };
        let bound: Vec<D3D12_CPU_DESCRIPTOR_HANDLE> = if targets.is_null() {
            Vec::new()
        } else {
            (0..supplied).map(|index| *targets.add(index)).collect()
        };
        with_list(this, |state| {
            state.rtvs = bound;
            state.rtv_count = count;
            state.rtv_single_range = single_range;
            state.dsv = Some(*depth);
            state.dsv_resource = resource;
        });
        if resource != 0 {
            if let Some(info) = described(resource) {
                if frame_sized(info.Width, info.Height) {
                    let frame = FRAME.load(Ordering::Relaxed);
                    *LAST_DEPTH.lock() = (resource, frame);
                    let mut guard = DEPTHS.lock();
                    let depths = guard.get_or_insert_with(AddressMap::default);
                    depths.insert(resource, (frame, info.Width as u32, info.Height));
                    // Anything not bound for several frames may since have
                    // been released, and a stale pointer is worse than none.
                    depths.retain(|_, (seen, _, _)| frame.saturating_sub(*seen) <= 4);
                }
            }
        }
    }
    if depth.is_null() {
        with_list(this, |state| {
            state.dsv = None;
            state.dsv_resource = 0;
        });
    }
    // The first colour target, which is the one an interface draw would
    // inherit. Reported for every bind, with or without depth, because the
    // interface is drawn without one.
    if count > 0 && !targets.is_null() {
        let handle = (*targets).ptr;
        let resource = RTV_HANDLES
            .lock()
            .as_ref()
            .and_then(|map| map.get(&handle).copied())
            .unwrap_or(0);
        if let Some(info) = described(resource) {
            crate::hudless::note_target(
                this as usize,
                resource,
                (info.Width as u32, info.Height),
                info.Format,
            );
        }
    }
    perf.stop();
    let original: OmSetTargetsFn = std::mem::transmute(O_OM_SET_TARGETS.load(Ordering::Relaxed));
    original(this, count, targets, single_range, depth);
}

// ---------------------------------------------------------------------------
// Draw calls
// ---------------------------------------------------------------------------

/// Hooked for one reason: a draw is the only signal that says the interface
/// has started being rendered, and the target bound at that instant is the
/// finished frame without it. `hudless::note_draw` returns immediately on one
/// relaxed load when nothing needs it, which is the usual case and has to be
/// cheap -- this runs thousands of times a frame.
unsafe extern "system" fn hook_draw_indexed_instanced(
    this: *mut c_void,
    indices: u32,
    instances: u32,
    start_index: u32,
    start_vertex: i32,
    start_instance: u32,
) {
    let perf = crate::perf::start(crate::perf::Family::ListDraw);
    if !reentrant() {
        crate::hudless::note_draw(this);
    }
    perf.stop();
    let original: DrawIndexedInstancedFn =
        std::mem::transmute(O_DRAW_INDEXED.load(Ordering::Relaxed));
    original(
        this,
        indices,
        instances,
        start_index,
        start_vertex,
        start_instance,
    );
}

unsafe extern "system" fn hook_draw_instanced(
    this: *mut c_void,
    vertices: u32,
    instances: u32,
    start_vertex: u32,
    start_instance: u32,
) {
    let perf = crate::perf::start(crate::perf::Family::ListDraw);
    if !reentrant() {
        crate::hudless::note_draw(this);
    }
    perf.stop();
    let original: DrawInstancedFn = std::mem::transmute(O_DRAW.load(Ordering::Relaxed));
    original(this, vertices, instances, start_vertex, start_instance);
}

/// Unbind the depth target so it can be transitioned, returning what was
/// bound so it can be put back.
unsafe fn unbind_depth(this: *mut c_void, depth: usize) -> Option<ListState> {
    let bound = with_list(this, |state| {
        (state.dsv_resource == depth).then(|| ListState {
            rtvs: state.rtvs.clone(),
            rtv_count: state.rtv_count,
            rtv_single_range: state.rtv_single_range,
            dsv: state.dsv,
            dsv_resource: state.dsv_resource,
            ..Default::default()
        })
    })?;
    let original: OmSetTargetsFn = std::mem::transmute(O_OM_SET_TARGETS.load(Ordering::Relaxed));
    let targets = if bound.rtvs.is_empty() {
        std::ptr::null()
    } else {
        bound.rtvs.as_ptr()
    };
    original(
        this,
        bound.rtv_count,
        targets,
        bound.rtv_single_range,
        std::ptr::null(),
    );
    Some(bound)
}

/// Bind back what `unbind_depth` took away.
unsafe fn rebind_depth(this: *mut c_void, bound: &ListState) {
    let original: OmSetTargetsFn = std::mem::transmute(O_OM_SET_TARGETS.load(Ordering::Relaxed));
    let targets = if bound.rtvs.is_empty() {
        std::ptr::null()
    } else {
        bound.rtvs.as_ptr()
    };
    let depth = bound
        .dsv
        .as_ref()
        .map_or(std::ptr::null(), |handle| handle as *const _);
    original(
        this,
        bound.rtv_count,
        targets,
        bound.rtv_single_range,
        depth,
    );
}

unsafe extern "system" fn hook_create_rtv(
    this: *mut c_void,
    resource: *mut c_void,
    desc: *const c_void,
    destination: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let perf = crate::perf::start(crate::perf::Family::DeviceCreate);
    note_texture(resource as usize, "rtv");
    RTV_HANDLES
        .lock()
        .get_or_insert_with(AddressMap::default)
        .insert(destination.ptr, resource as usize);
    // This slot no longer holds whatever view was recorded for it.
    bindings::forget_view(destination.ptr);
    perf.stop();
    let original: CreateRtvFn = std::mem::transmute(O_CREATE_RTV.load(Ordering::Relaxed));
    original(this, resource, desc, destination);
}

unsafe extern "system" fn hook_create_dsv(
    this: *mut c_void,
    resource: *mut c_void,
    desc: *const c_void,
    destination: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let perf = crate::perf::start(crate::perf::Family::DeviceCreate);
    note_texture(resource as usize, "dsv");
    bindings::forget_view(destination.ptr);
    DSV_HANDLES
        .lock()
        .get_or_insert_with(AddressMap::default)
        .insert(destination.ptr, resource as usize);
    if !resource.is_null() {
        if let Some(info) = described(resource as usize) {
            let mut guard = DEPTH.lock();
            let seen = guard.get_or_insert_with(std::collections::HashMap::new);
            let entry = (info.Width, info.Height, info.Format.0);
            if seen.insert(resource as usize, entry).is_none() {
                crate::logv!(
                    "[dsv] res={resource:p}  {}x{}  {}",
                    info.Width,
                    info.Height,
                    format_name(info.Format)
                );
            }
        }
    }

    perf.stop();
    let original: CreateDsvFn = std::mem::transmute(O_CREATE_DSV.load(Ordering::Relaxed));
    original(this, resource, desc, destination);
}

/// The LOD biases of the samplers the engine has created, and how many of
/// each: up to eight distinct values, from the first few thousand samplers.
///
/// This is the observation behind `mip_bias`. What that key does is patch
/// the constant one of the engine's sampler banks is built from and select
/// the bank; whether a sampler was then actually created with the bias asked
/// for is seen here, in the description handed to `CreateSampler`, and
/// nowhere else.
///
/// Room for every value the engine makes. It builds fifteen banks, and each
/// holds six groups for its dynamic resolution whose base bias is log2 of a
/// scale less one, so there are about a hundred distinct figures. With room
/// for eight, as this had, the list was the first eight the engine happened
/// to create and said nothing about any bank after the first.
static SAMPLER_BIASES: [(AtomicU32, AtomicU32); 128] =
    [const { (AtomicU32::new(u32::MAX), AtomicU32::new(0)) }; 128];
static SAMPLERS_LOOKED_AT: AtomicU32 = AtomicU32::new(0);
const SAMPLERS_TO_LOOK_AT: u32 = 8192;

unsafe fn note_sampler_bias(desc: *const c_void) {
    if desc.is_null() || SAMPLERS_LOOKED_AT.load(Ordering::Relaxed) >= SAMPLERS_TO_LOOK_AT {
        return;
    }
    SAMPLERS_LOOKED_AT.fetch_add(1, Ordering::Relaxed);
    let bias = (*(desc as *const D3D12_SAMPLER_DESC)).MipLODBias.to_bits();
    for (value, count) in &SAMPLER_BIASES {
        let held = value.load(Ordering::Relaxed);
        if held == bias
            || (held == u32::MAX
                && value
                    .compare_exchange(u32::MAX, bias, Ordering::Relaxed, Ordering::Relaxed)
                    .map_or_else(|now| now == bias, |_| true))
        {
            count.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
}

/// Say which LOD biases samplers have been created with, when `mip_bias` is
/// set and whenever a value not seen before appears.
fn report_sampler_biases(config: &crate::config::Config) {
    static SAID: AtomicU32 = AtomicU32::new(0);
    if config.mip_bias == 0 {
        return;
    }
    let distinct = SAMPLER_BIASES
        .iter()
        .filter(|(value, _)| value.load(Ordering::Relaxed) != u32::MAX)
        .count() as u32;
    if distinct == 0 || SAID.swap(distinct, Ordering::Relaxed) == distinct {
        return;
    }
    // From no bias down to the sharpest, then the positive ones.
    let mut seen: Vec<(f32, u32)> = SAMPLER_BIASES
        .iter()
        .filter(|(value, _)| value.load(Ordering::Relaxed) != u32::MAX)
        .map(|(value, count)| {
            (
                f32::from_bits(value.load(Ordering::Relaxed)),
                count.load(Ordering::Relaxed),
            )
        })
        .collect();
    seen.sort_by(|a, b| {
        (a.0 > 0.0)
            .cmp(&(b.0 > 0.0))
            .then(b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal))
    });
    let seen: Vec<String> = seen
        .iter()
        .map(|(bias, count)| format!("{bias:+.3} x{count}"))
        .collect();
    crate::log!(
        "[tun] mip bias observed: of the first {} samplers the engine created, the LOD biases \
         were {}. {}",
        SAMPLERS_LOOKED_AT.load(Ordering::Relaxed),
        seen.join(", "),
        match crate::hijack::mip_bias_built() {
            Some((ratio, bias)) => format!(
                "Asked for: {bias:+.3}, for the render_scale read at start-up ({ratio:.3}) and \
                 fixed until the game is restarted; it is in force only if that figure is in \
                 the list"
            ),
            None if config.mip_bias == 1 =>
                "Asked for: none (the render_scale read at start-up was 1.0, or no bias could be \
                 built); restart the game for the bias to follow a different render_scale"
                    .into(),
            None => format!("Sampler bank {} is selected as it stands", config.mip_bias),
        }
    );
}

unsafe extern "system" fn hook_create_sampler(
    this: *mut c_void,
    desc: *const c_void,
    destination: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let perf = crate::perf::start(crate::perf::Family::DeviceCreate);
    note_sampler_bias(desc);
    bindings::record_sampler();
    bindings::forget_view(destination.ptr);
    perf.stop();
    let original: CreateSamplerFn = std::mem::transmute(O_CREATE_SAMPLER.load(Ordering::Relaxed));
    original(this, desc, destination);
}

unsafe extern "system" fn hook_create_uav(
    this: *mut c_void,
    resource: *mut c_void,
    counter: *mut c_void,
    desc: *const D3D12_UNORDERED_ACCESS_VIEW_DESC,
    destination: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let perf = crate::perf::start(crate::perf::Family::DeviceCreate);
    bindings::record_view(resource, destination.ptr, View::Uav);
    perf.stop();
    let original: CreateUavFn = std::mem::transmute(O_CREATE_UAV.load(Ordering::Relaxed));
    original(this, resource, counter, desc, destination);
}

unsafe extern "system" fn hook_copy_descriptors_simple(
    this: *mut c_void,
    count: u32,
    destination: D3D12_CPU_DESCRIPTOR_HANDLE,
    source: D3D12_CPU_DESCRIPTOR_HANDLE,
    kind: D3D12_DESCRIPTOR_HEAP_TYPE,
) {
    let perf = crate::perf::start(crate::perf::Family::DeviceCopy);
    bindings::copy_views(
        destination.ptr,
        source.ptr,
        count,
        bindings::stride_for(kind),
    );
    perf.stop();
    let original: CopyDescriptorsSimpleFn =
        std::mem::transmute(O_COPY_DESCRIPTORS_SIMPLE.load(Ordering::Relaxed));
    original(this, count, destination, source, kind);
}

unsafe extern "system" fn hook_copy_descriptors(
    this: *mut c_void,
    dest_range_count: u32,
    dest_starts: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    dest_sizes: *const u32,
    src_range_count: u32,
    src_starts: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    src_sizes: *const u32,
    kind: D3D12_DESCRIPTOR_HEAP_TYPE,
) {
    let perf = crate::perf::start(crate::perf::Family::DeviceCopy);
    // The destination and source ranges describe the same flat sequence of
    // descriptors but may be split differently, so walk both as streams.
    let stride = bindings::stride_for(kind);
    if stride != 0 && !dest_starts.is_null() && !src_starts.is_null() {
        let size_at = |sizes: *const u32, index: usize| {
            if sizes.is_null() {
                1
            } else {
                *sizes.add(index)
            }
        };
        let (mut di, mut si, mut doff, mut soff) = (0usize, 0usize, 0u32, 0u32);
        while di < dest_range_count as usize && si < src_range_count as usize {
            let dest_size = size_at(dest_sizes, di);
            let src_size = size_at(src_sizes, si);
            let run = (dest_size - doff).min(src_size - soff);
            bindings::copy_views(
                (*dest_starts.add(di)).ptr + doff as usize * stride,
                (*src_starts.add(si)).ptr + soff as usize * stride,
                run,
                stride,
            );
            doff += run;
            soff += run;
            if doff == dest_size {
                di += 1;
                doff = 0;
            }
            if soff == src_size {
                si += 1;
                soff = 0;
            }
        }
    }

    perf.stop();
    let original: CopyDescriptorsFn =
        std::mem::transmute(O_COPY_DESCRIPTORS.load(Ordering::Relaxed));
    original(
        this,
        dest_range_count,
        dest_starts,
        dest_sizes,
        src_range_count,
        src_starts,
        src_sizes,
        kind,
    );
}

unsafe extern "system" fn hook_map(
    this: *mut c_void,
    subresource: u32,
    read_range: *const c_void,
    data: *mut *mut c_void,
) -> HRESULT {
    let original: MapFn = std::mem::transmute(O_MAP.load(Ordering::Relaxed));
    let result = original(this, subresource, read_range, data);
    let perf = crate::perf::start(crate::perf::Family::DeviceCreate);
    if result.is_ok() && !data.is_null() {
        bindings::record_mapping(this, *data);
    }
    perf.stop();
    result
}

unsafe extern "system" fn hook_unmap(this: *mut c_void, subresource: u32, range: *const c_void) {
    let perf = crate::perf::start(crate::perf::Family::DeviceCreate);
    // Before the call: once the runtime has taken the mapping away, asking
    // the resource for its address to find the entry is still fine, but
    // there is no reason to leave a window where the registry hands out a
    // pointer the runtime no longer backs.
    bindings::forget_mapping(this);
    perf.stop();
    let original: UnmapFn = std::mem::transmute(O_UNMAP.load(Ordering::Relaxed));
    original(this, subresource, range);
}

unsafe extern "system" fn hook_create_cbv(
    this: *mut c_void,
    desc: *const c_void,
    destination: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let perf = crate::perf::start(crate::perf::Family::DeviceCreate);
    // A constant buffer view carries no resource worth recording, but it does
    // overwrite the slot, and a shader resource view recorded for the slot
    // must stop being reported from it.
    bindings::forget_view(destination.ptr);
    perf.stop();
    let original: CreateCbvFn = std::mem::transmute(O_CREATE_CBV.load(Ordering::Relaxed));
    original(this, desc, destination);
}

unsafe extern "system" fn hook_create_compute_pso(
    this: *mut c_void,
    desc: *const D3D12_COMPUTE_PIPELINE_STATE_DESC,
    riid: *const windows::core::GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    PSOS_SEEN.fetch_add(1, Ordering::Relaxed);
    let original: CreateComputePsoFn =
        std::mem::transmute(O_CREATE_COMPUTE_PSO.load(Ordering::Relaxed));
    let result = original(this, desc, riid, out);

    // The result first. A failed creation leaves `*out` as the caller had
    // it, which need not be null, and recording whatever was there filed a
    // pipeline under an address no pipeline lives at.
    if result.is_err() || desc.is_null() || out.is_null() || (*out).is_null() {
        return result;
    }
    let perf = crate::perf::start(crate::perf::Family::DeviceCreate);
    let cs = (*desc).CS;
    if cs.pShaderBytecode.is_null() || cs.BytecodeLength == 0 {
        perf.stop();
        return result;
    }
    let hash = fnv1a(std::slice::from_raw_parts(
        cs.pShaderBytecode as *const u8,
        cs.BytecodeLength,
    ));
    let shader = dxbc::inspect(cs.pShaderBytecode as *const u8, cs.BytecodeLength);
    let shape = temporal_pass_kind(&shader);

    if let Some(kind) = shape {
        crate::log!("");
        crate::log!(
            "[pso] {kind} pso={:p} cs={} hash={hash:#018x}",
            *out,
            cs.BytecodeLength
        );
        crate::log!("      {}", shader.summary());
    }

    PSOS.lock().get_or_insert_with(HashMap::new).insert(
        *out as usize,
        Pso {
            hash,
            shader,
            kind: replaced(shape),
            shape,
        },
    );
    perf.stop();
    result
}

/// What a pipeline of this shape is to `hook_dispatch`: the temporal pass to
/// replace while DLSS is ready, and nothing otherwise.
///
/// That one field is the whole of what makes the dispatch hook treat a
/// dispatch as the temporal pass. With it unset the hook records nothing on
/// the engine's lists and the engine's own pass runs exactly as it does
/// without the mod -- which is what is wanted before NGX has come up, and
/// for good in a session where it will not.
fn replaced(shape: Option<&'static str>) -> Option<&'static str> {
    if crate::gate::dlss_ready() {
        shape
    } else {
        None
    }
}

/// DLSS has become ready: the temporal pipelines recorded so far are
/// replaced from here on.
pub fn activate_temporal_pipelines() {
    let activated = PSOS
        .lock()
        .as_mut()
        .map(|known| {
            known
                .values_mut()
                .filter(|pso| pso.shape.is_some() && pso.kind.is_none())
                .map(|pso| pso.kind = pso.shape)
                .count()
        })
        .unwrap_or(0);
    crate::log!(
        "[gpu] {activated} temporal pipelines recorded so far are replaced by DLSS from here \
         on, and any created later"
    );
}

/// Stop treating any recorded pipeline as a temporal pass to replace.
///
/// Called once, when it is decided that DLSS cannot run in this session.
pub fn retire_temporal_pipelines() {
    let retired = PSOS
        .lock()
        .as_mut()
        .map(|known| known.values_mut().filter_map(|pso| pso.kind.take()).count())
        .unwrap_or(0);
    if retired > 0 {
        crate::log!(
            "[gpu] {retired} temporal pipelines are no longer replaced; the engine's own \
             temporal pass runs from here on"
        );
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Record a compute pipeline that reached D3D12 by the stream call.
///
/// The same record `hook_create_compute_pso` makes, for `pipeline.rs`, which
/// sees `ID3D12Device2::CreatePipelineState`. A layer above this mod can
/// create the engine's pipelines that way -- ReShade does when an add-on
/// listens for pipeline events -- and a pipeline that is not recorded here is
/// a temporal pass that is never recognised.
///
/// # Safety
/// `code` must be readable for `length` bytes, and `pso` the pipeline made
/// from it.
pub(crate) unsafe fn record_compute_pipeline(pso: *mut c_void, code: *const c_void, length: usize) {
    PSOS_SEEN.fetch_add(1, Ordering::Relaxed);
    let perf = crate::perf::start(crate::perf::Family::DeviceCreate);
    let hash = fnv1a(std::slice::from_raw_parts(code as *const u8, length));
    let shader = dxbc::inspect(code as *const u8, length);
    let shape = temporal_pass_kind(&shader);
    if let Some(kind) = shape {
        crate::log!("");
        crate::log!(
            "[pso] {kind} pso={pso:p} cs={length} hash={hash:#018x} (created through \
             CreatePipelineState)"
        );
        crate::log!("      {}", shader.summary());
    }
    PSOS.lock().get_or_insert_with(HashMap::new).insert(
        pso as usize,
        Pso {
            hash,
            shader,
            kind: replaced(shape),
            shape,
        },
    );
    perf.stop();
}

/// How many recorded compute pipelines are one of the engine's temporal
/// passes. Zero long after the renderer is up means DLSS has nothing to
/// replace, whatever else is working.
pub fn temporal_pipelines() -> u64 {
    PSOS.lock()
        .as_ref()
        .map(|known| known.values().filter(|pso| pso.shape.is_some()).count() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

/// Objects borrowed from the game's device, created only so that their
/// vtables can be read. A command list and a buffer are the cheapest way to
/// reach `ID3D12GraphicsCommandList` and `ID3D12Resource`.
unsafe fn scratch_objects(
    device: &ID3D12Device,
) -> windows::core::Result<(
    ID3D12GraphicsCommandList,
    ID3D12Resource,
    ID3D12CommandQueue,
)> {
    let allocator: ID3D12CommandAllocator =
        device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
    let list: ID3D12GraphicsCommandList =
        device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
    list.Close()?;

    // A queue of our own, purely to reach the vtable `ExecuteCommandLists`
    // lives in. Nothing is ever submitted to it: resource states follow
    // submission order, and that is the only place submission can be seen.
    let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
        ..Default::default()
    })?;

    // An upload buffer is the cheapest way to reach ID3D12Resource::Map, which
    // is what turns a root CBV's GPU address into readable memory.
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_UPLOAD,
        ..Default::default()
    };
    let buffer_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: 256,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_NONE,
    };
    let mut buffer: Option<ID3D12Resource> = None;
    device.CreateCommittedResource(
        &heap,
        D3D12_HEAP_FLAG_NONE,
        &buffer_desc,
        D3D12_RESOURCE_STATE_GENERIC_READ,
        None,
        &mut buffer,
    )?;
    let buffer = buffer.expect("CreateCommittedResource succeeded without a resource");

    Ok((list, buffer, queue))
}

unsafe fn install(device: &ID3D12Device) -> windows::core::Result<()> {
    let (list, buffer, queue) = scratch_objects(device)?;
    bindings::set_device(device.clone());
    // The other way a compute pipeline can be created. Not in the table
    // below: see `pipeline.rs`.
    crate::pipeline::install(device);

    let on_device = crate::vtable::of(device);
    let on_list = crate::vtable::of(&list);
    let on_resource = crate::vtable::of(&buffer);
    let on_queue = crate::vtable::of(&queue);
    crate::log!(
        "[gpu] vtables: device {on_device:p}  list {on_list:p}  resource {on_resource:p}  \
         queue {on_queue:p}"
    );

    let hooks: [(&str, *mut *mut c_void, usize, *const c_void, &AtomicUsize); 26] = [
        (
            "CommandQueue::ExecuteCommandLists",
            on_queue,
            slot!(ID3D12CommandQueue_Vtbl, ExecuteCommandLists),
            hook_execute_command_lists as *const _,
            &O_EXECUTE_COMMAND_LISTS,
        ),
        (
            "Device::CreateConstantBufferView",
            on_device,
            slot!(ID3D12Device_Vtbl, CreateConstantBufferView),
            hook_create_cbv as *const _,
            &O_CREATE_CBV,
        ),
        (
            "Resource::Unmap",
            on_resource,
            slot!(ID3D12Resource_Vtbl, Unmap),
            hook_unmap as *const _,
            &O_UNMAP,
        ),
        (
            "CommandList::SetComputeRootShaderResourceView",
            on_list,
            slot!(
                ID3D12GraphicsCommandList_Vtbl,
                SetComputeRootShaderResourceView
            ),
            hook_set_root_srv as *const _,
            &O_SET_ROOT_SRV,
        ),
        (
            "CommandList::SetComputeRootUnorderedAccessView",
            on_list,
            slot!(
                ID3D12GraphicsCommandList_Vtbl,
                SetComputeRootUnorderedAccessView
            ),
            hook_set_root_uav as *const _,
            &O_SET_ROOT_UAV,
        ),
        (
            "CommandList::SetComputeRoot32BitConstants",
            on_list,
            slot!(ID3D12GraphicsCommandList_Vtbl, SetComputeRoot32BitConstants),
            hook_set_root_constants as *const _,
            &O_SET_ROOT_CONSTANTS,
        ),
        (
            "Device::CreateComputePipelineState",
            on_device,
            slot!(ID3D12Device_Vtbl, CreateComputePipelineState),
            hook_create_compute_pso as *const _,
            &O_CREATE_COMPUTE_PSO,
        ),
        (
            "Device::CreateShaderResourceView",
            on_device,
            slot!(ID3D12Device_Vtbl, CreateShaderResourceView),
            hook_create_srv as *const _,
            &O_CREATE_SRV,
        ),
        (
            "Device::CreateUnorderedAccessView",
            on_device,
            slot!(ID3D12Device_Vtbl, CreateUnorderedAccessView),
            hook_create_uav as *const _,
            &O_CREATE_UAV,
        ),
        (
            "Device::CreateRootSignature",
            on_device,
            slot!(ID3D12Device_Vtbl, CreateRootSignature),
            hook_create_root_sig as *const _,
            &O_CREATE_ROOT_SIG,
        ),
        (
            "Device::CreateCommittedResource",
            on_device,
            slot!(ID3D12Device_Vtbl, CreateCommittedResource),
            hook_create_committed as *const _,
            &O_CREATE_COMMITTED,
        ),
        (
            "Device::CreateRenderTargetView",
            on_device,
            slot!(ID3D12Device_Vtbl, CreateRenderTargetView),
            hook_create_rtv as *const _,
            &O_CREATE_RTV,
        ),
        (
            "Device::CreateDepthStencilView",
            on_device,
            slot!(ID3D12Device_Vtbl, CreateDepthStencilView),
            hook_create_dsv as *const _,
            &O_CREATE_DSV,
        ),
        (
            "Device::CreateSampler",
            on_device,
            slot!(ID3D12Device_Vtbl, CreateSampler),
            hook_create_sampler as *const _,
            &O_CREATE_SAMPLER,
        ),
        (
            "Device::CopyDescriptors",
            on_device,
            slot!(ID3D12Device_Vtbl, CopyDescriptors),
            hook_copy_descriptors as *const _,
            &O_COPY_DESCRIPTORS,
        ),
        (
            "Device::CopyDescriptorsSimple",
            on_device,
            slot!(ID3D12Device_Vtbl, CopyDescriptorsSimple),
            hook_copy_descriptors_simple as *const _,
            &O_COPY_DESCRIPTORS_SIMPLE,
        ),
        (
            "Resource::Map",
            on_resource,
            slot!(ID3D12Resource_Vtbl, Map),
            hook_map as *const _,
            &O_MAP,
        ),
        (
            "CommandList::SetPipelineState",
            on_list,
            slot!(ID3D12GraphicsCommandList_Vtbl, SetPipelineState),
            hook_set_pipeline as *const _,
            &O_SET_PIPELINE,
        ),
        (
            "CommandList::SetDescriptorHeaps",
            on_list,
            slot!(ID3D12GraphicsCommandList_Vtbl, SetDescriptorHeaps),
            hook_set_heaps as *const _,
            &O_SET_HEAPS,
        ),
        (
            "CommandList::SetComputeRootSignature",
            on_list,
            slot!(ID3D12GraphicsCommandList_Vtbl, SetComputeRootSignature),
            hook_set_root_sig as *const _,
            &O_SET_ROOT_SIG,
        ),
        (
            "CommandList::SetComputeRootDescriptorTable",
            on_list,
            slot!(
                ID3D12GraphicsCommandList_Vtbl,
                SetComputeRootDescriptorTable
            ),
            hook_set_root_table as *const _,
            &O_SET_ROOT_TABLE,
        ),
        (
            "CommandList::ResourceBarrier",
            on_list,
            slot!(ID3D12GraphicsCommandList_Vtbl, ResourceBarrier),
            hook_resource_barrier as *const _,
            &O_RESOURCE_BARRIER,
        ),
        (
            "CommandList::OMSetRenderTargets",
            on_list,
            slot!(ID3D12GraphicsCommandList_Vtbl, OMSetRenderTargets),
            hook_om_set_targets as *const _,
            &O_OM_SET_TARGETS,
        ),
        (
            "CommandList::SetComputeRootConstantBufferView",
            on_list,
            slot!(
                ID3D12GraphicsCommandList_Vtbl,
                SetComputeRootConstantBufferView
            ),
            hook_set_root_cbv as *const _,
            &O_SET_ROOT_CBV,
        ),
        (
            "CommandList::Dispatch",
            on_list,
            slot!(ID3D12GraphicsCommandList_Vtbl, Dispatch),
            hook_dispatch as *const _,
            &O_DISPATCH,
        ),
        (
            "CommandList::Close",
            on_list,
            slot!(ID3D12GraphicsCommandList_Vtbl, Close),
            hook_close as *const _,
            &O_CLOSE,
        ),
    ];

    for (name, vtable, index, replacement, original_slot) in hooks {
        if crate::vtable::hook(vtable, index, replacement, original_slot) {
            crate::log!("[gpu] hooked slot {index:>3}  {name}");
        } else {
            crate::log!("[gpu] FAILED slot {index:>3}  {name}");
        }
    }

    // Here rather than anywhere later, because the engine creates its
    // swapchain shortly after its device and the whole value of the
    // presentation hooks is seeing the first one. A vtable patch cannot be
    // withdrawn, so this is decided at start-up only.
    //
    // With frame generation on there is nothing to install here: it owns the
    // factory's creation call -- `proxy::CreateDXGIFactory2` waits for
    // Streamline and `swap::watch_factory` patches the slot on the object
    // the engine is actually handed. Patching DXGI's vtable as well would put
    // two hooks on one job, and the one installed here is the one that loses
    // the race.
    if !crate::config::get().fg {
        crate::swap::install();
    }

    // Nothing about NGX happens here. The snippet folder is published at
    // load, by `lib.rs`, in every configuration; and what NGX offers on this
    // machine is asked when the game's own initialisation of it returns
    // (`ngx::observe_game_init`).

    // The two draw hooks are not in the table above. They exist only for the
    // interface-free frame capture, so they are installed while that is armed
    // and taken out again when it is not: see `set_draw_hooks`.
    LIST_VTABLE.store(on_list as usize, Ordering::Relaxed);
    set_draw_hooks(crate::hudless::armed());

    if crate::logger::verbose() {
        arm("startup");
    }
    Ok(())
}

/// The command list vtable, once `install` has found it.
static LIST_VTABLE: AtomicUsize = AtomicUsize::new(0);
/// Whether the draw hooks are in, and the lock that keeps two callers from
/// patching at once.
static DRAW_HOOKED: Mutex<bool> = Mutex::new(false);

/// Install or remove the hooks on `DrawInstanced` and `DrawIndexedInstanced`.
///
/// A draw is the busiest call a command list has -- some six thousand a frame
/// here -- and these hooks have exactly one use: `hudless` needs to see the
/// first draw into a render target to copy it before the interface lands on
/// it. With nothing armed there each call was a detour to read one flag and
/// come back. A hook that is not installed costs nothing, so they follow the
/// settings: `hudless::follow` calls this when the file changes.
///
/// Removing is done with care for anything that has patched the same slots
/// since: the saved original is only written back if the slot still holds our
/// own function. If something else is there now -- an overlay, ReShade -- its
/// hook is left alone and ours stays behind it, which is correct and merely
/// not free.
///
/// The originals are never cleared, so a thread already inside one of our
/// hooks when it is removed still finds the function to call.
pub fn set_draw_hooks(wanted: bool) {
    let vtable = LIST_VTABLE.load(Ordering::Relaxed) as *mut *mut c_void;
    if vtable.is_null() {
        // Not installed yet; `install` asks again when it is.
        return;
    }
    let mut hooked = DRAW_HOOKED.lock();
    if *hooked == wanted {
        return;
    }
    let sites: [(&str, usize, *const c_void, &AtomicUsize); 2] = [
        (
            "CommandList::DrawIndexedInstanced",
            slot!(ID3D12GraphicsCommandList_Vtbl, DrawIndexedInstanced),
            hook_draw_indexed_instanced as *const c_void,
            &O_DRAW_INDEXED,
        ),
        (
            "CommandList::DrawInstanced",
            slot!(ID3D12GraphicsCommandList_Vtbl, DrawInstanced),
            hook_draw_instanced as *const c_void,
            &O_DRAW,
        ),
    ];
    for (name, index, ours, original_slot) in sites {
        unsafe {
            if wanted {
                // The original is stored before the slot is written. These
                // two are patched while the engine is drawing on several
                // threads, so a draw can be through the slot at once.
                if crate::vtable::hook(vtable, index, ours, original_slot) {
                    crate::log!("[gpu] hooked slot {index:>3}  {name} (the capture is armed)");
                } else {
                    crate::log!("[gpu] FAILED slot {index:>3}  {name}");
                }
            } else if std::ptr::eq(*vtable.add(index), ours) {
                let original = original_slot.load(Ordering::Relaxed) as *const c_void;
                if !original.is_null() && crate::vtable::write(vtable, index, original).is_some() {
                    crate::log!("[gpu] unhooked slot {index:>3}  {name} (nothing is armed)");
                }
            } else {
                crate::log!(
                    "[gpu] slot {index:>3}  {name} has been patched by something else since; \
                     leaving both hooks in place"
                );
            }
        }
    }
    *hooked = wanted;
}

/// Install the render hooks on the device the game just created.
///
/// Called from a `D3D12CreateDevice` wrapper while the game is still blocked
/// inside it, so every descriptor and mapping the engine makes afterwards is
/// observed. This is the preferred path; it cannot race the renderer.
///
/// # Safety
/// `raw` must be the `ID3D12Device` returned by a successful create call.
pub unsafe fn install_from_device(raw: *mut c_void) {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(device) = ID3D12Device::from_raw_borrowed(&raw) else {
        crate::log!("[gpu] device pointer was not an ID3D12Device");
        return;
    };
    crate::log!("[gpu] installing from the game's device");
    match install(device) {
        Ok(()) => watch_hotkey(),
        Err(error) => crate::log!("[gpu] install failed: {error}"),
    }
}

/// Install the render hooks using a device of our own.
///
/// This is the path that actually runs under the launcher. Intercepting the
/// game's own `D3D12CreateDevice` is unreliable here: MHW resolves much of
/// kernel32 dynamically and keeps its own function pointers, so an IAT patch
/// on `GetProcAddress` sees some lookups (CreateDXGIFactory2, the NGX
/// entry points) and misses others, device creation among them.
///
/// It does not matter. D3D12 vtables live in read-only data and are shared by
/// every object of the same implementation, so patching through a device of
/// our own redirects the game's calls just as well. What matters is only
/// *when*: creating it immediately after injection -- which loads d3d12.dll
/// ourselves as a side effect -- puts the hooks in place before the engine
/// exists, so no descriptor, view or mapping is missed.
pub fn bootstrap_scratch(delay: std::time::Duration) {
    std::thread::spawn(move || unsafe {
        std::thread::sleep(delay);
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        crate::log!("[gpu] installing from a device of our own");
        match scratch_device() {
            Ok(device) => {
                // D3D12's own vtables, whatever answered. This runs before
                // anything else is in the process, so the device is native --
                // but had ReShade got in first, patching through its proxy
                // would hook ReShade's methods and miss the engine's calls.
                // Checked rather than assumed; with no ReShade it is one
                // module walk here and nothing afterwards.
                crate::reshade::detect();
                static SCRATCH_DEVICE: crate::reshade::Boundary = crate::reshade::Boundary::new(
                    "the device of our own that the render hooks are installed from",
                    "the vtables are read from the native device",
                );
                let device = SCRATCH_DEVICE.native_interface(device);
                let raw = device.as_raw() as usize;
                SCRATCH.store(raw, Ordering::Relaxed);
                let outcome = install(&device);
                // Never released, on purpose. If this device were dropped,
                // the engine's own device could be allocated into the same
                // block, and a pointer held anywhere to this one would then
                // be sometimes a dead object and sometimes the engine's live
                // one at the same address. Holding the reference for the
                // life of the process costs one device and makes the
                // address unique, so "the same pointer" in the log is
                // evidence of the same object.
                std::mem::forget(device);
                crate::log!(
                    "[gpu] the scratch device {raw:#x} is held for the life of \
                             the process so nothing else can be allocated at its address"
                );
                match outcome {
                    Ok(()) => watch_hotkey(),
                    Err(error) => crate::log!("[gpu] scratch install failed: {error}"),
                }
            }
            Err(error) => crate::log!("[gpu] scratch device failed: {error}"),
        }
    });
}

/// Our own device's address, so other code can *report* whether a device it
/// has been given is ours. Zero until `bootstrap_scratch` has made it.
///
/// Not a veto. See `swap::adopt_engine_device` for why an address comparison
/// is the wrong thing to put in front of `GetDevice`.
static SCRATCH: AtomicUsize = AtomicUsize::new(0);

pub fn scratch_pointer() -> usize {
    SCRATCH.load(Ordering::Relaxed)
}

/// A device of our own, on the same adapter the game would pick.
unsafe fn scratch_device() -> windows::core::Result<ID3D12Device> {
    // DXGI's own entry point, resolved by hand, and not the one this library
    // exports under the same name.
    //
    //
    // The export waits for Streamline to finish initialising when frame
    // generation is on. Going through it here would block this thread for
    // the second or so `slInit` takes, the render hooks would go in after
    // the engine had built its pipelines, the temporal pass would never be
    // recognised and DLSS would never run: a black screen with a HUD.
    //
    // It is also why the export needs its own re-entrancy guard: Streamline's
    // plugin startup enumerates adapters, and a DXGI call from inside `slInit`
    // arriving at an export that waits for `slInit` is a deadlock rather than
    // a delay.
    type CreateFactoryFn =
        unsafe extern "system" fn(u32, *const windows::core::GUID, *mut *mut c_void) -> HRESULT;
    let Some(address) = crate::proxy::entry("dxgi.dll", b"CreateDXGIFactory2\0") else {
        crate::log!("[gpu] dxgi.dll has no CreateDXGIFactory2");
        return Err(windows::core::Error::from_win32());
    };
    let create: CreateFactoryFn = std::mem::transmute(address);
    let mut raw: *mut c_void = std::ptr::null_mut();
    create(0, &IDXGIFactory4::IID, &mut raw).ok()?;
    let factory = IDXGIFactory4::from_raw(raw);
    let mut chosen: Option<IDXGIAdapter1> = None;
    for index in 0.. {
        let Ok(adapter) = factory.EnumAdapters1(index) else {
            break;
        };
        let desc = adapter.GetDesc1()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        crate::log!(
            "[gpu] adapter {index}: {}",
            String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .trim()
        );
        chosen = Some(adapter);
        break;
    }
    let adapter = chosen.ok_or_else(windows::core::Error::from_win32)?;

    let mut device: Option<ID3D12Device> = None;
    D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device)?;
    let device = device.expect("D3D12CreateDevice succeeded without a device");
    // Printed here, at the point of creation, and not only after `install`:
    // whether a `[proxy] D3D12CreateDevice` line belongs to this call or to
    // the engine has to be readable and not inferred from the order of the
    // log. Three addresses are printed from three places -- this one, the
    // proxy's, and `scratch_pointer()` at the comparison.
    crate::log!(
        "[gpu] scratch_device created {:#x} on thread {}",
        device.as_raw() as usize,
        windows::Win32::System::Threading::GetCurrentThreadId()
    );
    Ok(device)
}

/// Poll for the capture hotkey for the life of the process.
///
/// `GetAsyncKeyState` reads the keyboard whatever window has the focus, so
/// the key counts only while the window in the foreground belongs to this
/// process: F10 pressed in a browser writes nothing here.
fn watch_hotkey() {
    crate::log!(
        "[key] F10, pressed while the game window has the focus, writes one report of the \
         temporal pass's bindings to this log"
    );
    std::thread::spawn(|| {
        let mut was_down = false;
        loop {
            let down = unsafe { GetAsyncKeyState(VK_F10.0 as i32) } as u16 & 0x8000 != 0;
            if down && !was_down && game_has_the_focus() {
                arm("F10");
                crate::pathdump::request_frame();
            }
            was_down = down;
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
}

/// Whether the foreground window is one of this process's.
fn game_has_the_focus() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let window = GetForegroundWindow();
        if window.0.is_null() {
            return false;
        }
        let mut process = 0u32;
        GetWindowThreadProcessId(window, Some(&mut process));
        process == windows::Win32::System::Threading::GetCurrentProcessId()
    }
}
