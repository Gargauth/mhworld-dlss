//! Recognising the engine's passes from their shaders' reflection.
//!
//! By the *shape* of the bindings, not a bytecode hash, so a game patch that
//! recompiles a shader does not break detection. Technique names
//! (`CSTemporalAA2`, `System_CopyToBackBuffer…`) are engine-side and not in
//! the bytecode; the names here are what the RE calls the passes.

use crate::dxbc::{InputKind, Reflection, Stage};

/// A pass this crate recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PassKind {
    /// `CSTemporalAA2`, the temporal AA resolve (TAA draw `0x142392150`):
    /// binds `tBaseMap` (colour), `tBlendMap` (history), `tDepthMap` (a 1×1
    /// dummy), `tGBuffer4` (packed motion vectors) and `rwOutput`.
    TemporalAa,
    /// `CSTemporalUpsample`, used instead of TAA in the engine's
    /// temporal-upsample (dynamic resolution) mode: `tInput`, `tHistory`,
    /// `rwOuput` (sic).
    TemporalUpsample,
    /// `System_CopyToBackBuffer`, the SDR output encode (gamma, `CBSystem`).
    CopyToBackBuffer,
    /// `System_CopyToBackBufferPQSingle`, the HDR (PQ) output encode.
    CopyToBackBufferPq,
}

impl PassKind {
    /// Every kind, for iteration in introspection.
    pub const ALL: [PassKind; 4] = [
        PassKind::TemporalAa,
        PassKind::TemporalUpsample,
        PassKind::CopyToBackBuffer,
        PassKind::CopyToBackBufferPq,
    ];

    /// The engine's technique name.
    pub fn name(self) -> &'static str {
        match self {
            PassKind::TemporalAa => "CSTemporalAA2",
            PassKind::TemporalUpsample => "CSTemporalUpsample",
            PassKind::CopyToBackBuffer => "System_CopyToBackBuffer",
            PassKind::CopyToBackBufferPq => "System_CopyToBackBufferPQSingle",
        }
    }
}

/// `DXGI_FORMAT_R10G10B10A2_UNORM`, the HDR swapchain format
/// (`ReFramePresent` §6).
const HDR_BACKBUFFER: u32 = 24;

/// What pass a shader belongs to, if any.
///
/// `rtv0` is the first render target format of a graphics pipeline. It is
/// what tells the two output encodes apart: both bind `tBaseMap` and
/// `CBSystem{fGammaCorrect}`, and only the PQ one targets the
/// R10G10B10A2 HDR backbuffer (INFERENCE from the swapchain formats; the
/// shaders themselves are not in the executable).
pub fn classify(reflection: &Reflection, rtv0: Option<u32>) -> Option<PassKind> {
    use InputKind::{ShaderResource as T, Unordered as U};
    match reflection.stage {
        Stage::Compute => {
            if reflection.binds_all(&[
                (T, "tBaseMap"),
                (T, "tBlendMap"),
                (T, "tDepthMap"),
                (U, "rwOutput"),
            ]) {
                return Some(PassKind::TemporalAa);
            }
            if reflection.binds_all(&[(T, "tInput"), (T, "tHistory"), (U, "rwOuput")]) {
                return Some(PassKind::TemporalUpsample);
            }
            None
        }
        Stage::Pixel => {
            let encodes = reflection.binding(T, "tBaseMap").is_some()
                && reflection
                    .buffer("CBSystem")
                    .is_some_and(|cb| cb.variable("fGammaCorrect").is_some());
            if !encodes {
                return None;
            }
            Some(if rtv0 == Some(HDR_BACKBUFFER) {
                PassKind::CopyToBackBufferPq
            } else {
                PassKind::CopyToBackBuffer
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxbc::{fixture::container, reflect};

    const COMPUTE: u16 = 0x4353;
    const PIXEL: u16 = 0xFFFF;

    fn of(program: u16, bindings: &[(&str, u32, u32)]) -> Reflection {
        reflect(&container(program, bindings, &[])).unwrap()
    }

    #[test]
    fn temporal_aa_by_its_bindings() {
        let taa = of(
            COMPUTE,
            &[
                ("tBaseMap", 2, 0),
                ("tBlendMap", 2, 1),
                ("tDepthMap", 2, 2),
                ("tGBuffer4", 2, 4),
                ("rwOutput", 4, 0),
            ],
        );
        assert_eq!(classify(&taa, None), Some(PassKind::TemporalAa));
        // TemporalAA2_Sharpen binds tBaseMap, tGBuffer1, rwOutput: not TAA.
        let sharpen = of(
            COMPUTE,
            &[("tBaseMap", 2, 0), ("tGBuffer1", 2, 1), ("rwOutput", 4, 0)],
        );
        assert_eq!(classify(&sharpen, None), None);
        // The same names in a pixel shader are not the compute pass.
        let pixel = of(
            PIXEL,
            &[
                ("tBaseMap", 2, 0),
                ("tBlendMap", 2, 1),
                ("tDepthMap", 2, 2),
                ("rwOutput", 4, 0),
            ],
        );
        assert_eq!(classify(&pixel, None), None);
    }

    #[test]
    fn temporal_upsample_keeps_the_engines_typo() {
        let upsample = of(
            COMPUTE,
            &[("tInput", 2, 0), ("tHistory", 2, 1), ("rwOuput", 4, 0)],
        );
        assert_eq!(classify(&upsample, None), Some(PassKind::TemporalUpsample));
        let corrected = of(
            COMPUTE,
            &[("tInput", 2, 0), ("tHistory", 2, 1), ("rwOutput", 4, 0)],
        );
        assert_eq!(classify(&corrected, None), None);
    }

    #[test]
    fn copy_to_backbuffer_by_cbsystem_and_target_format() {
        let bytes = container(
            PIXEL,
            &[("CBSystem", 0, 0), ("tBaseMap", 2, 0), ("sPoint", 3, 0)],
            &[(
                "CBSystem",
                16,
                &[
                    ("fSourceMipLevel", 0, 4),
                    ("fGammaCorrect", 4, 4),
                    ("fGammaCorrectEx", 8, 4),
                ],
            )],
        );
        let encode = reflect(&bytes).unwrap();
        assert_eq!(
            classify(&encode, Some(87)),
            Some(PassKind::CopyToBackBuffer)
        );
        assert_eq!(classify(&encode, None), Some(PassKind::CopyToBackBuffer));
        assert_eq!(
            classify(&encode, Some(24)),
            Some(PassKind::CopyToBackBufferPq)
        );
        // tBaseMap alone is any number of copies.
        let copy = of(PIXEL, &[("tBaseMap", 2, 0)]);
        assert_eq!(classify(&copy, Some(87)), None);
    }

    #[test]
    fn names_are_the_engines() {
        let names: Vec<&str> = PassKind::ALL.iter().map(|k| k.name()).collect();
        assert_eq!(
            names,
            [
                "CSTemporalAA2",
                "CSTemporalUpsample",
                "System_CopyToBackBuffer",
                "System_CopyToBackBufferPQSingle"
            ]
        );
    }
}
