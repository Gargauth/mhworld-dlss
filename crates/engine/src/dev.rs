//! Dev-build introspection (feature `dev`): raw views of the engine's
//! objects for the dev server's probes. Read-only.

use std::sync::atomic::Ordering;

use mhw_win::mem;

use crate::hooks::hijack::LAST_TAA;
use crate::layout::camera::VIEWPORT_COUNT;
use crate::runtime;
use crate::sites::FIELDS;
use crate::viewport::ViewportSnapshot;
use crate::views::{Camera, RenderConfig, RenderSettings, TaaFilter};

/// Every viewport of `sCamera`, by index.
pub fn viewports() -> Vec<(usize, ViewportSnapshot)> {
    let Some(camera) = runtime::addresses().and_then(Camera::get) else {
        return Vec::new();
    };
    (0..VIEWPORT_COUNT)
        .filter_map(|index| Some((index, camera.viewport(index)?)))
        .collect()
}

/// One anchored field's current raw value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldValue {
    /// Name in `signatures.json`.
    pub name: &'static str,
    /// Reflection name, or empty.
    pub reflection: &'static str,
    /// The field's offset in this build.
    pub offset: usize,
    /// Its address.
    pub address: usize,
    /// The 8 bytes there, little-endian.
    pub raw: u64,
}

/// Every anchored field of the singletons (`cfg`, `ctx`, `settings`) and of
/// the TAA filter last seen, read raw.
pub fn fields() -> Vec<FieldValue> {
    let Some(at) = runtime::addresses() else {
        return Vec::new();
    };
    let base = |name: &str| -> Option<usize> {
        let object = name.split('+').next()?;
        match object {
            "cfg" => RenderConfig::get(at).map(|o| o.address()),
            "ctx" => Camera::get(at).map(|o| o.address()),
            "settings" => RenderSettings::get(at).map(|o| o.address()),
            "ss_node" => Some(LAST_TAA.load(Ordering::Relaxed)).filter(|a| *a != 0),
            _ => None,
        }
    };
    FIELDS
        .iter()
        .zip(at.fields())
        .filter_map(|(field, (name, offset))| {
            let address = base(name)?.checked_add(offset)?;
            // SAFETY: a checked read of plain bytes.
            let raw = unsafe { mem::read::<u64>(address) }?;
            Some(FieldValue {
                name,
                reflection: field.reflection,
                offset,
                address,
                raw,
            })
        })
        .collect()
}

/// The TAA filter last seen by the SS enqueue.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TaaSnapshot {
    /// Its address.
    pub address: usize,
    /// Colour texture object.
    pub colour: Option<usize>,
    /// History ring.
    pub history: [Option<usize>; 3],
    /// Dense velocity texture object.
    pub dense_velocity: Option<usize>,
    /// `mBlendRate`.
    pub blend_rate: Option<f32>,
    /// `mJitterEnable`.
    pub jitter_enable: Option<bool>,
    /// History invalid.
    pub history_invalid: Option<bool>,
    /// `mSharpen`.
    pub sharpen: Option<bool>,
    /// `mSharpenAmount`.
    pub sharpen_amount: Option<f32>,
}

/// The TAA filter last seen, read now.
pub fn taa() -> Option<TaaSnapshot> {
    let at = runtime::addresses()?;
    let taa = TaaFilter::at(at, LAST_TAA.load(Ordering::Relaxed))?;
    Some(TaaSnapshot {
        address: taa.address(),
        colour: taa.colour(),
        history: [taa.history(0), taa.history(1), taa.history(2)],
        dense_velocity: taa.dense_velocity(),
        blend_rate: taa.blend_rate(),
        jitter_enable: taa.jitter_enable(),
        history_invalid: taa.history_invalid(),
        sharpen: taa.sharpen(),
        sharpen_amount: taa.sharpen_amount(),
    })
}
