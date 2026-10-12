//! Pipeline creation hooks: reflect each shader, remember the recognised
//! pipelines and their bytecode.
//!
//! Compute pipelines come through `CreateComputePipelineState`, graphics
//! ones through `CreateGraphicsPipelineState`, and both through
//! `ID3D12Device2::CreatePipelineState` when a layer (ReShade with add-ons)
//! rebuilds them as streams. A pipeline address recycled by an unrecognised
//! pipeline drops the old entry.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use parking_lot::RwLock;
use windows::core::{GUID, HRESULT};
use windows::Win32::Graphics::Direct3D12::*;

use super::vt::{self, original};
use crate::addrmap::AddressMap;
use crate::dxbc::{self, Reflection};
use crate::recognise::{classify, PassKind};
use crate::stream;

/// A pipeline this crate recognised.
#[derive(Debug, Clone)]
pub struct RecognisedPipeline {
    /// `ID3D12PipelineState*`.
    pub pipeline: usize,
    /// Which pass.
    pub kind: PassKind,
    /// FNV-1a of the shader bytecode.
    pub hash: u64,
    /// Shader bytecode, kept so the shader can be reused (e.g. the output
    /// encode for a HUD-less conversion).
    pub bytecode: Arc<[u8]>,
    /// The shader's reflection.
    pub reflection: Arc<Reflection>,
    /// First render target format (graphics pipelines).
    pub rtv0: Option<u32>,
    /// Created through a pipeline state stream.
    pub via_stream: bool,
    /// Creation order among recognised pipelines.
    pub order: u64,
}

static RECOGNISED: LazyLock<RwLock<AddressMap<usize, RecognisedPipeline>>> =
    LazyLock::new(Default::default);
static ORDER: AtomicU64 = AtomicU64::new(0);
static SEEN: AtomicU64 = AtomicU64::new(0);
static FROM_STREAMS: AtomicU64 = AtomicU64::new(0);
static UNREFLECTED: AtomicU64 = AtomicU64::new(0);

/// `(pipelines seen, of them through streams, shaders without reflection)`.
pub(crate) fn counts() -> (u64, u64, u64) {
    (
        SEEN.load(Ordering::Relaxed),
        FROM_STREAMS.load(Ordering::Relaxed),
        UNREFLECTED.load(Ordering::Relaxed),
    )
}

/// The recognised pass a pipeline is, with its reflection.
pub(crate) fn pass_of(pipeline: usize) -> Option<(PassKind, Arc<Reflection>)> {
    if pipeline == 0 {
        return None;
    }
    RECOGNISED
        .read()
        .get(&pipeline)
        .map(|entry| (entry.kind, Arc::clone(&entry.reflection)))
}

/// Every recognised pipeline, oldest first.
pub fn recognised_pipelines() -> Vec<RecognisedPipeline> {
    let mut all: Vec<RecognisedPipeline> = RECOGNISED.read().values().cloned().collect();
    all.sort_by_key(|entry| entry.order);
    all
}

/// The bytecode of the newest pipeline recognised as `kind`.
pub fn pass_bytecode(kind: PassKind) -> Option<Arc<[u8]>> {
    RECOGNISED
        .read()
        .values()
        .filter(|entry| entry.kind == kind)
        .max_by_key(|entry| entry.order)
        .map(|entry| Arc::clone(&entry.bytecode))
}

/// Record the pipeline at `pipeline` made from `code`. Our own pipelines
/// (created while this thread records for us) are never recognised: the
/// HUD-less conversion reuses the encode's shader and must not pass for it.
fn record(pipeline: usize, code: &[u8], rtv0: Option<u32>, via_stream: bool) {
    if super::record::reentrant() {
        RECOGNISED.write().remove(&pipeline);
        return;
    }
    SEEN.fetch_add(1, Ordering::Relaxed);
    let reflection = match dxbc::reflect(code) {
        Ok(reflection) => reflection,
        Err(_) => {
            UNREFLECTED.fetch_add(1, Ordering::Relaxed);
            RECOGNISED.write().remove(&pipeline);
            return;
        }
    };
    #[cfg(feature = "dev")]
    super::shaderdump::offer(code, &reflection);
    let Some(kind) = classify(&reflection, rtv0) else {
        RECOGNISED.write().remove(&pipeline);
        return;
    };
    let hash = dxbc::fnv1a(code);
    mhw_core::log_once!(
        Info,
        "taa",
        (kind.name(), hash),
        "recognised {} pso={pipeline:#x} hash={hash:#018x}{}: {}",
        kind.name(),
        if via_stream { " (stream)" } else { "" },
        reflection.summary()
    );
    RECOGNISED.write().insert(
        pipeline,
        RecognisedPipeline {
            pipeline,
            kind,
            hash,
            bytecode: Arc::from(code),
            reflection: Arc::new(reflection),
            rtv0,
            via_stream,
            order: ORDER.fetch_add(1, Ordering::Relaxed),
        },
    );
}

/// The bytes of a shader the caller passed.
///
/// # Safety
/// `code` must describe readable bytecode for the duration of the call.
unsafe fn bytes(code: &D3D12_SHADER_BYTECODE) -> Option<&[u8]> {
    if code.pShaderBytecode.is_null() || code.BytecodeLength == 0 {
        return None;
    }
    // SAFETY: caller guarantees the range is readable.
    Some(unsafe { std::slice::from_raw_parts(code.pShaderBytecode.cast(), code.BytecodeLength) })
}

/// The bytecode to build a compute pipeline from instead of the game's:
/// a dev override (the dump folder's `override/<hash>.hlsl`) first, then
/// the full-resolution fog upsample ([`super::volumetric`]).
struct Replacement {
    code: std::sync::Arc<[u8]>,
    /// The full-resolution fog upsample (not a dev override).
    fog: bool,
}

fn replacement_for(code: &[u8]) -> Option<Replacement> {
    #[cfg(feature = "dev")]
    if let Some(code) = super::shaderdump::replacement(code) {
        return Some(Replacement { code, fog: false });
    }
    super::volumetric::replacement(code).map(|code| Replacement { code, fog: true })
}

/// After creating a pipeline from `replacement` for the game's `code`:
/// whether it was built (`result`); a failure is logged for a retry with
/// the game's own shader.
fn replaced(result: HRESULT, replacement: &Replacement, code: &[u8]) -> bool {
    if result.is_err() {
        log::warn!(
            target: "render",
            "a pipeline from the replacement for shader {:016x} failed ({result:?}); \
             the game's shader is built instead",
            dxbc::fnv1a(code)
        );
        return false;
    }
    if replacement.fog {
        super::volumetric::built(code);
    }
    true
}

pub(crate) unsafe extern "system" fn create_compute_pso(
    this: *mut c_void,
    desc: *const D3D12_COMPUTE_PIPELINE_STATE_DESC,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let create = |desc| {
        // SAFETY: the original with the caller's arguments (or the copy).
        unsafe {
            original::<vt::CreateComputePsoFn>(&vt::O_CREATE_COMPUTE_PSO)(this, desc, riid, out)
        }
    };
    // SAFETY: the caller's description with readable bytecode.
    let game = unsafe { desc.as_ref().and_then(|d| bytes(&d.CS)) };
    let replacement = game.and_then(replacement_for);
    let mut result = None;
    if let (Some(replacement), Some(code)) = (replacement.as_ref(), game) {
        // SAFETY: a bitwise copy of the caller's description; the root
        // signature reference is not dropped (`ManuallyDrop`).
        let mut copy = unsafe { std::ptr::read(desc) };
        copy.CS = D3D12_SHADER_BYTECODE {
            pShaderBytecode: replacement.code.as_ptr().cast(),
            BytecodeLength: replacement.code.len(),
        };
        // A cached blob belongs to the game's shader.
        copy.CachedPSO = Default::default();
        let attempt = create(&copy);
        if replaced(attempt, replacement, code) {
            result = Some(attempt);
        }
    }
    let result = result.unwrap_or_else(|| create(desc));
    // SAFETY: on success `out` holds the pipeline.
    unsafe {
        // A failed creation leaves `*out` as the caller had it.
        if result.is_ok() && !out.is_null() && !(*out).is_null() {
            if let Some(code) = game {
                record(*out as usize, code, None, false);
            }
        }
    }
    result
}

pub(crate) unsafe extern "system" fn create_graphics_pso(
    this: *mut c_void,
    desc: *const D3D12_GRAPHICS_PIPELINE_STATE_DESC,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: the original with the caller's own arguments.
    let result = unsafe {
        original::<vt::CreateGraphicsPsoFn>(&vt::O_CREATE_GRAPHICS_PSO)(this, desc, riid, out)
    };
    // SAFETY: as for compute.
    unsafe {
        if result.is_ok() && !desc.is_null() && !out.is_null() && !(*out).is_null() {
            let desc = &*desc;
            let rtv0 = (desc.NumRenderTargets > 0).then_some(desc.RTVFormats[0].0 as u32);
            match bytes(&desc.PS) {
                Some(code) => record(*out as usize, code, rtv0, false),
                None => {
                    RECOGNISED.write().remove(&(*out as usize));
                }
            }
        }
    }
    result
}

pub(crate) unsafe extern "system" fn create_pipeline_state(
    this: *mut c_void,
    desc: *const D3D12_PIPELINE_STATE_STREAM_DESC,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: the stream and the bytecode it points at are the caller's,
    // readable for the call.
    let raw = unsafe {
        match desc.as_ref() {
            Some(d) if !d.pPipelineStateSubobjectStream.is_null() => {
                Some(std::slice::from_raw_parts(
                    d.pPipelineStateSubobjectStream.cast::<u8>(),
                    d.SizeInBytes,
                ))
            }
            _ => None,
        }
    };
    let parsed = raw.map(stream::parse);
    // A layer (ReShade with add-ons) rebuilds the engine's compute creations
    // as streams: the replacements apply here as in `create_compute_pso`.
    let create = |desc| {
        // SAFETY: the original with the caller's arguments (or the patched
        // copy, alive until it returns).
        unsafe {
            original::<vt::CreatePipelineStateFn>(&vt::O_CREATE_PIPELINE_STATE)(
                this, desc, riid, out,
            )
        }
    };
    // SAFETY: the caller's bytecode, readable for the call.
    let game = parsed
        .and_then(|p| p.cs)
        .map(|cs| unsafe { std::slice::from_raw_parts(cs.address as *const u8, cs.len) });
    let mut result = None;
    if let (Some(raw), Some(p), Some(code)) = (raw, parsed, game) {
        if let Some(replacement) = replacement_for(code) {
            let swapped = stream::Bytecode {
                address: replacement.code.as_ptr() as usize,
                len: replacement.code.len(),
            };
            if let Some(words) = stream::with_compute_shader(raw, &p, swapped) {
                let patched = D3D12_PIPELINE_STATE_STREAM_DESC {
                    SizeInBytes: raw.len(),
                    pPipelineStateSubobjectStream: words.as_ptr() as *mut c_void,
                };
                let attempt = create(&patched);
                if replaced(attempt, &replacement, code) {
                    result = Some(attempt);
                }
            }
        }
    }
    let result = result.unwrap_or_else(|| create(desc));
    // SAFETY: on success `out` holds the pipeline.
    unsafe {
        let Some(parsed) = parsed else {
            return result;
        };
        if result.is_err() || out.is_null() || (*out).is_null() {
            return result;
        }
        let (code, rtv0) = match (parsed.cs, parsed.ps) {
            (Some(cs), _) => (cs, None),
            (None, Some(ps)) => (ps, parsed.rtv0),
            (None, None) => {
                RECOGNISED.write().remove(&(*out as usize));
                return result;
            }
        };
        FROM_STREAMS.fetch_add(1, Ordering::Relaxed);
        mhw_core::log_once!(
            Info,
            "render",
            "stream-pso",
            "pipelines are being created through ID3D12Device2::CreatePipelineState \
             (a layer such as ReShade rebuilds the engine's creations); following both"
        );
        let shader = std::slice::from_raw_parts(code.address as *const u8, code.len);
        record(*out as usize, shader, rtv0, true);
    }
    result
}
