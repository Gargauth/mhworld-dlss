//! What each command list has bound, so our own recording can put it back.
//!
//! Our recording (the motion vector decode, whatever `mod` records at the
//! temporal pass or the upscale slot) binds its own heaps, root signature,
//! pipeline and roots on the engine's list. Everything the engine records
//! afterwards assumes its own; leaving ours in place faulted the GPU.
//! Setting a root signature invalidates every root binding, so tables, root
//! CBVs/SRVs/UAVs and root constants are all replayed.
//!
//! One lock per list, found through a few thread-local slots: a command list
//! is recorded by one thread at a time (the API requires it), so the lock is
//! uncontended and costs what a check for the violation would; a failed
//! `try_lock` *is* the violation, counted.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use windows::Win32::Foundation::BOOL;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_GPU_DESCRIPTOR_HANDLE,
};

use super::vt::{self, original};
use crate::addrmap::AddressMap;

/// Root constants shadowed per parameter, at most.
const MAX_CONSTANTS: usize = 64;

/// The compute root state and output targets of one list.
#[derive(Debug, Default, Clone)]
pub(crate) struct ListState {
    pub pipeline: usize,
    pub signature: usize,
    pub tables: Vec<(u32, u64)>,
    pub cbvs: Vec<(u32, u64)>,
    /// `(root, address, unordered)`.
    pub views: Vec<(u32, u64, bool)>,
    pub constants: Vec<(u32, Vec<u32>)>,
    pub heaps: Vec<usize>,
    pub targets: Targets,
}

/// What `OMSetRenderTargets` last bound.
#[derive(Debug, Default, Clone)]
pub(crate) struct Targets {
    pub rtvs: Vec<D3D12_CPU_DESCRIPTOR_HANDLE>,
    pub count: u32,
    pub single_range: bool,
    pub dsv: Option<D3D12_CPU_DESCRIPTOR_HANDLE>,
    pub dsv_resource: usize,
}

impl ListState {
    /// Back to default, keeping the vectors' storage (a list is closed and
    /// recorded again every frame).
    pub fn reset(&mut self) {
        self.pipeline = 0;
        self.signature = 0;
        self.tables.clear();
        self.cbvs.clear();
        self.views.clear();
        self.constants.clear();
        self.heaps.clear();
        self.targets.rtvs.clear();
        self.targets.count = 0;
        self.targets.single_range = false;
        self.targets.dsv = None;
        self.targets.dsv_resource = 0;
    }

    /// A new root signature invalidates every root binding.
    pub fn set_signature(&mut self, signature: usize) {
        self.signature = signature;
        self.tables.clear();
        self.cbvs.clear();
        self.views.clear();
        self.constants.clear();
    }

    pub fn set_table(&mut self, root: u32, base: u64) {
        upsert(&mut self.tables, root, base);
    }

    pub fn set_cbv(&mut self, root: u32, address: u64) {
        upsert(&mut self.cbvs, root, address);
    }

    pub fn set_view(&mut self, root: u32, address: u64, unordered: bool) {
        self.views.retain(|(index, _, _)| *index != root);
        self.views.push((root, address, unordered));
    }

    /// `count` constants at `offset` of root `root`, merged into the shadow.
    pub fn set_constants(&mut self, root: u32, offset: u32, values: &[u32]) {
        let end = offset as usize + values.len();
        if end > MAX_CONSTANTS {
            return;
        }
        let shadow = match self.constants.iter_mut().find(|(index, _)| *index == root) {
            Some((_, shadow)) => shadow,
            None => {
                self.constants.push((root, Vec::new()));
                match self.constants.last_mut() {
                    Some((_, shadow)) => shadow,
                    None => return,
                }
            }
        };
        if shadow.len() < end {
            shadow.resize(end, 0);
        }
        shadow[offset as usize..end].copy_from_slice(values);
    }

    /// The table base bound at root `root`.
    pub fn table(&self, root: u32) -> Option<u64> {
        self.tables
            .iter()
            .find(|(r, _)| *r == root)
            .map(|(_, b)| *b)
    }

    /// The CBV address bound at root `root`.
    pub fn cbv(&self, root: u32) -> Option<u64> {
        self.cbvs.iter().find(|(r, _)| *r == root).map(|(_, a)| *a)
    }
}

fn upsert(entries: &mut Vec<(u32, u64)>, root: u32, value: u64) {
    match entries.iter_mut().find(|(index, _)| *index == root) {
        Some(entry) => entry.1 = value,
        None => entries.push((root, value)),
    }
}

/// List address -> its state. Kept per address for the session (the engine
/// pools its lists, so the set is bounded); a recycled address starts from
/// the state its last `Close` reset.
static LISTS: LazyLock<Mutex<AddressMap<usize, Arc<Mutex<ListState>>>>> =
    LazyLock::new(Default::default);
static COLLISIONS: AtomicU64 = AtomicU64::new(0);

type Recent = [Option<(usize, Arc<Mutex<ListState>>)>; 8];

thread_local! {
    /// The lists this thread recorded into lately, so the common case does
    /// not take the shared map's lock.
    static RECENT: RefCell<Recent> = const { RefCell::new([const { None }; 8]) };
}

fn state_of(list: usize) -> Arc<Mutex<ListState>> {
    let index = ((list >> 4) ^ (list >> 12)) & 7;
    RECENT.with(|recent| {
        let mut recent = recent.borrow_mut();
        if let Some((seen, state)) = &recent[index] {
            if *seen == list {
                return Arc::clone(state);
            }
        }
        let found = Arc::clone(LISTS.lock().entry(list).or_default());
        recent[index] = Some((list, Arc::clone(&found)));
        found
    })
}

/// Work on `list`'s state.
pub(crate) fn with<R>(list: *mut c_void, f: impl FnOnce(&mut ListState) -> R) -> R {
    let state = state_of(list as usize);
    let mut guard = match state.try_lock() {
        Some(guard) => guard,
        None => {
            COLLISIONS.fetch_add(1, Ordering::Relaxed);
            state.lock()
        }
    };
    f(&mut guard)
}

/// Times a list's state was found in use by another thread.
pub(crate) fn collisions() -> u64 {
    COLLISIONS.load(Ordering::Relaxed)
}

/// Put `saved`'s root state back on `list`, through the originals (our
/// hooks are not re-entered and the record stays the engine's).
///
/// # Safety
/// `list` must be a live, open native graphics command list and `saved` a
/// snapshot of its own state.
pub(crate) unsafe fn restore(list: *mut c_void, saved: &ListState) {
    // SAFETY: each original has the signature of its slot (vt.rs) and is
    // called on the live list it was recorded from, with values the engine
    // set on it.
    unsafe {
        if !saved.heaps.is_empty() {
            let heaps: Vec<*mut c_void> = saved.heaps.iter().map(|h| *h as *mut c_void).collect();
            original::<vt::SetHeapsFn>(&vt::O_SET_HEAPS)(list, heaps.len() as u32, heaps.as_ptr());
        }
        if saved.signature != 0 {
            original::<vt::SetObjectFn>(&vt::O_SET_ROOT_SIG)(list, saved.signature as *mut c_void);
        }
        if saved.pipeline != 0 {
            original::<vt::SetObjectFn>(&vt::O_SET_PIPELINE)(list, saved.pipeline as *mut c_void);
        }
        let table = original::<vt::SetRootTableFn>(&vt::O_SET_ROOT_TABLE);
        for &(root, base) in &saved.tables {
            table(list, root, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: base });
        }
        let cbv = original::<vt::SetRootAddressFn>(&vt::O_SET_ROOT_CBV);
        for &(root, address) in &saved.cbvs {
            cbv(list, root, address);
        }
        for &(root, address, unordered) in &saved.views {
            let slot = if unordered {
                &vt::O_SET_ROOT_UAV
            } else {
                &vt::O_SET_ROOT_SRV
            };
            original::<vt::SetRootAddressFn>(slot)(list, root, address);
        }
        let constants = original::<vt::SetRootConstantsFn>(&vt::O_SET_ROOT_CONSTANTS);
        for (root, values) in &saved.constants {
            constants(list, *root, values.len() as u32, values.as_ptr().cast(), 0);
        }
    }
}

/// Bind `targets` on `list` with or without its depth target.
///
/// # Safety
/// As [`restore`].
pub(crate) unsafe fn bind_targets(list: *mut c_void, targets: &Targets, with_depth: bool) {
    let rtvs = if targets.rtvs.is_empty() {
        std::ptr::null()
    } else {
        targets.rtvs.as_ptr()
    };
    let depth = match (&targets.dsv, with_depth) {
        (Some(handle), true) => handle as *const D3D12_CPU_DESCRIPTOR_HANDLE,
        _ => std::ptr::null(),
    };
    // SAFETY: the original OMSetRenderTargets, with handles the engine
    // bound on this list (`count` handles, or one with a single range).
    unsafe {
        original::<vt::OmSetTargetsFn>(&vt::O_OM_SET_TARGETS)(
            list,
            targets.count,
            rtvs,
            BOOL::from(targets.single_range),
            depth,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_merge_and_signatures_clear_roots() {
        let mut state = ListState::default();
        state.set_constants(1, 0, &[1, 2]);
        state.set_constants(1, 3, &[4]);
        assert_eq!(state.constants, [(1, vec![1, 2, 0, 4])]);
        state.set_constants(1, 64, &[1]);
        assert_eq!(state.constants[0].1.len(), 4, "beyond the cap is ignored");
        state.set_table(0, 0x10);
        state.set_table(0, 0x20);
        assert_eq!(state.table(0), Some(0x20));
        state.set_signature(0x99);
        assert!(state.tables.is_empty() && state.constants.is_empty());
        assert_eq!(state.signature, 0x99);
    }
}
