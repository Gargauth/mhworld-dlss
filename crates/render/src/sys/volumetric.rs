//! The volumetric fog at full resolution (`[Upscaling] DownsampleVolume =
//! false`; `docs/re/ReVolumetrics.md`): the game's `CSVRUpsampleBilateral`
//! decides per 2×2 block (nearest sample in edge tiles), so its fog edges sit
//! on a half-resolution grid that the jitter does not move. Its compute
//! pipelines are built from `vr_upsample.hlsl` instead, a per-pixel upsample
//! weighted by the full-resolution (jittered) depth. Once one is built, the
//! callback given to [`configure_volume_upsample`] runs (the mod has the
//! engine hold the tonemap filter's `mDownsampleVolume` off then): that
//! output is what the scene composites, and DLSS resolves the fog edges with
//! the geometry. Without a built replacement the game's look is left alone;
//! its own upsample at full resolution, unblurred, is noisy.
//!
//! The three variants differ only in their tile size; the game's bytecode is
//! recognised by its FNV-1a hash (the known build).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

use crate::dxbc;

const SOURCE: &str = include_str!("vr_upsample.hlsl");

/// `(hash of the game's bytecode, log2 of its tile size)`.
const VARIANTS: [(u64, u32); 3] = [
    (0xd764_2313_94df_273e, 2),
    (0xc7ec_8d17_b7d2_9f74, 3),
    (0xd98e_a60b_1e80_6cf2, 4),
];

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Runs once, when the first replacement pipeline was built.
static ON_BUILT: OnceLock<fn()> = OnceLock::new();
static BUILT: AtomicBool = AtomicBool::new(false);

/// The replacements, compiled on first use; `None` where compiling failed.
static COMPILED: LazyLock<[Option<Arc<[u8]>>; 3]> =
    LazyLock::new(|| VARIANTS.map(|(hash, shift)| compile(hash, shift)));

fn compile(hash: u64, shift: u32) -> Option<Arc<[u8]>> {
    let source = format!("#define TILE_SHIFT {shift}\n{SOURCE}");
    match super::gpu::compile(&source, b"mhw_vr_upsample\0", b"main\0", b"cs_5_0\0") {
        Ok(code) => Some(Arc::from(code)),
        Err(error) => {
            log::error!(
                target: "render",
                "the full-resolution fog upsample for {hash:016x} does not compile; the game's \
                 stays: {error}"
            );
            None
        }
    }
}

/// Build the game's fog upsample pipelines from the per-pixel upsample from
/// now on (`true`) or leave them alone, and run `on_built` once one was
/// built. Call before the game creates them (at load).
pub fn configure_volume_upsample(on: bool, on_built: fn()) {
    let _ = ON_BUILT.set(on_built);
    ENABLED.store(on, Ordering::Release);
}

/// The bytecode to build instead of `code`, if it is one of the game's fog
/// upsample shaders and the replacement is enabled.
pub(crate) fn replacement(code: &[u8]) -> Option<Arc<[u8]>> {
    if !ENABLED.load(Ordering::Acquire) {
        return None;
    }
    let hash = dxbc::fnv1a(code);
    let index = VARIANTS.iter().position(|(known, _)| *known == hash)?;
    COMPILED[index].clone()
}

/// A pipeline was built from [`replacement`]'s bytecode for the game's
/// `code`.
pub(crate) fn built(code: &[u8]) {
    log::info!(
        target: "render",
        "fog upsample {:016x} built at full resolution",
        dxbc::fnv1a(code)
    );
    if !BUILT.swap(true, Ordering::AcqRel) {
        if let Some(on_built) = ON_BUILT.get() {
            on_built();
        }
    }
}
