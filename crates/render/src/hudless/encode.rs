//! The output encode `System_CopyToBackBuffer` applies, for the HUD-less
//! image.
//!
//! The scene target the HUD-less image is copied from is post-tonemap and
//! pre-encode (`ReFramePresent` §2.4, 5b/5c): `System_CopyToBackBuffer`
//! (`0x1422979f0`) only applies the output encode, with `CBSystem {
//! fSourceMipLevel = 0, fGammaCorrect = 1/[sRender+0x1c8], fGammaCorrectEx =
//! [sRender+0x1cc] }`, or the PQ variant `System_CopyToBackBufferPQSingle`
//! when `[sRender+0x21a]` (HDR) is set. `sRender`'s constructor
//! (`0x142290880`) sets `+0x1c8 = 2.2` and `+0x1cc = 1.0`.
//!
//! The shaders are not in the executable (E6), so the conversion prefers the
//! engine's own pixel shader (bytecode kept by the pipeline hooks) driven by
//! a root signature built from its reflection ([`plan_engine`]). When that
//! shader is unavailable or binds something this plan cannot supply, an own
//! shader ([`OWN_HLSL`]) does what the encode is inferred to do:
//!
//! * SDR: `pow(pow(saturate(c), fGammaCorrect), fGammaCorrectEx)`. The
//!   exponent role of `fGammaCorrectEx` is an INFERENCE (it defaults to 1.0,
//!   identity either way).
//! * HDR: BT.709 → BT.2020 primaries, scale by `hdr_white_nits / 10000`,
//!   SMPTE ST 2084 (PQ). Whether the engine's HDR scene target is linear or
//!   already PQ (`bIsLinearToPQ`, E7) is not known; this assumes linear.

use crate::dxbc::{InputKind, Reflection, Stage};

/// `DXGI_FORMAT_B8G8R8A8_UNORM`, the SDR swapchain format.
pub const SDR_FORMAT: u32 = 87;
/// `DXGI_FORMAT_R10G10B10A2_UNORM`, the HDR swapchain format.
pub const HDR_FORMAT: u32 = 24;
/// `DXGI_FORMAT_R8G8B8A8_UNORM`, accepted as an SDR output too.
pub const RGBA8_FORMAT: u32 = 28;

/// The engine's output encode parameters and the swapchain format, read by
/// `mod` (engine views of `sRender`, swapchain format from frame generation
/// or the swapchain hook).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputTransform {
    /// `[sRender+0x1c8]` (2.2 by default); the shader gets its reciprocal.
    pub gamma: f32,
    /// `[sRender+0x1cc]` (`fGammaCorrectEx`, 1.0 by default).
    pub gamma_ex: f32,
    /// `[sRender+0x21a]`: the PQ encode is in use.
    pub pq: bool,
    /// The swapchain's `DXGI_FORMAT`: the HUD-less image's format.
    pub format: u32,
    /// Own PQ encode only: nits a scene value of 1.0 stands for.
    pub hdr_white_nits: f32,
}

impl OutputTransform {
    /// `fGammaCorrect = 1 / gamma`, as the engine computes it
    /// (`0x142297b1d divss`); `None` for a gamma that is not a positive
    /// finite number.
    pub fn gamma_correct(&self) -> Option<f32> {
        (self.gamma.is_finite() && self.gamma > 0.0).then(|| 1.0 / self.gamma)
    }

    /// Why this transform cannot be applied, if it cannot.
    pub fn check(&self) -> Result<(), String> {
        if self.gamma_correct().is_none() {
            return Err(format!("gamma {} is not a positive number", self.gamma));
        }
        if !self.gamma_ex.is_finite() {
            return Err(format!("gamma_ex {} is not finite", self.gamma_ex));
        }
        match (self.pq, self.format) {
            (true, HDR_FORMAT) | (false, SDR_FORMAT | RGBA8_FORMAT) => {}
            (pq, format) => {
                return Err(format!(
                    "output format {format} does not go with the {} encode",
                    if pq { "PQ" } else { "SDR" }
                ))
            }
        }
        if self.pq && !(self.hdr_white_nits.is_finite() && self.hdr_white_nits > 0.0) {
            return Err(format!(
                "hdr_white_nits {} is not positive",
                self.hdr_white_nits
            ));
        }
        Ok(())
    }
}

/// A shader register `(register, space)`.
pub type Register = (u32, u32);

/// A value the conversion puts into a constant buffer dword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// `fSourceMipLevel` (0).
    SourceMip,
    /// `fGammaCorrect`.
    GammaCorrect,
    /// `fGammaCorrectEx`.
    GammaCorrectEx,
    /// Own shader: `hdr_white_nits / 10000`.
    PqScale,
    /// Own shader: 1 for PQ, 0 for SDR.
    Pq,
}

/// A constant buffer supplied as root constants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstantsPlan {
    /// Its register.
    pub register: Register,
    /// Its size in dwords.
    pub dwords: u32,
    /// `(dword, value)` pairs filled; the rest are 0.
    pub fields: Vec<(u32, Field)>,
}

/// What a conversion pixel shader binds, as the root signature supplies it:
/// one texture (the HUD-less copy) in a one-descriptor table, static
/// samplers, and the constant buffer as root constants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootPlan {
    /// The texture's register.
    pub texture: Register,
    /// Sampler registers (static samplers, linear clamp).
    pub samplers: Vec<Register>,
    /// The constant buffer, if any.
    pub constants: Option<ConstantsPlan>,
}

impl RootPlan {
    /// The constant dwords for `transform` (unfilled dwords are 0).
    pub fn values(&self, transform: &OutputTransform) -> Vec<u32> {
        let Some(constants) = &self.constants else {
            return Vec::new();
        };
        let mut values = vec![0u32; constants.dwords as usize];
        for &(at, field) in &constants.fields {
            let value = match field {
                Field::SourceMip => 0.0f32.to_bits(),
                Field::GammaCorrect => transform.gamma_correct().unwrap_or(1.0).to_bits(),
                Field::GammaCorrectEx => transform.gamma_ex.to_bits(),
                Field::PqScale => (transform.hdr_white_nits / 10_000.0).to_bits(),
                Field::Pq => u32::from(transform.pq),
            };
            if let Some(slot) = values.get_mut(at as usize) {
                *slot = value;
            }
        }
        values
    }
}

/// Root constants a root signature may carry next to one table (64 dwords
/// in all).
const MAX_CONSTANT_DWORDS: u32 = 63;

/// How to drive the engine's `System_CopyToBackBuffer*` pixel shader, from
/// its reflection; `Err` says what it binds that cannot be supplied.
pub fn plan_engine(reflection: &Reflection) -> Result<RootPlan, String> {
    if reflection.stage != Stage::Pixel {
        return Err(format!("stage {:?}, not a pixel shader", reflection.stage));
    }
    let mut texture = None;
    let mut samplers = Vec::new();
    let mut constants = None;
    for binding in &reflection.bindings {
        if binding.count != 1 {
            return Err(format!("{binding} is an array"));
        }
        let register = (binding.register, binding.space);
        match binding.kind {
            InputKind::ShaderResource if texture.is_none() => texture = Some(register),
            InputKind::Sampler => samplers.push(register),
            InputKind::ConstantBuffer if constants.is_none() => {
                let buffer = reflection
                    .buffer(&binding.name)
                    .ok_or_else(|| format!("{binding} has no declaration"))?;
                if buffer.name != "CBSystem" {
                    return Err(format!("{binding}: only CBSystem is known"));
                }
                if buffer.size % 4 != 0 || buffer.size / 4 > MAX_CONSTANT_DWORDS {
                    return Err(format!("{binding}: {} bytes", buffer.size));
                }
                let mut fields = Vec::new();
                for variable in &buffer.variables {
                    let field = match variable.name.as_str() {
                        "fSourceMipLevel" => Field::SourceMip,
                        "fGammaCorrect" => Field::GammaCorrect,
                        "fGammaCorrectEx" => Field::GammaCorrectEx,
                        other => return Err(format!("CBSystem.{other} is not known")),
                    };
                    if variable.size != 4 || variable.offset % 4 != 0 {
                        return Err(format!("CBSystem.{} is not one float", variable.name));
                    }
                    fields.push((variable.offset / 4, field));
                }
                constants = Some(ConstantsPlan {
                    register,
                    dwords: buffer.size / 4,
                    fields,
                });
            }
            _ => return Err(format!("{binding} cannot be supplied")),
        }
    }
    Ok(RootPlan {
        texture: texture.ok_or("no texture input")?,
        samplers,
        constants,
    })
}

/// The own shader's plan (see [`OWN_HLSL`]).
pub fn plan_own() -> RootPlan {
    RootPlan {
        texture: (0, 0),
        samplers: Vec::new(),
        constants: Some(ConstantsPlan {
            register: (0, 0),
            dwords: 4,
            fields: vec![
                (0, Field::GammaCorrect),
                (1, Field::GammaCorrectEx),
                (2, Field::PqScale),
                (3, Field::Pq),
            ],
        }),
    }
}

/// The own encode (`ps_5_0`, entry `main`), mirroring [`sdr_encode`] and
/// [`pq_encode`].
pub const OWN_HLSL: &str = r#"
Texture2D<float4> tBaseMap : register(t0);

cbuffer CBEncode : register(b0)
{
    float fGammaCorrect;
    float fGammaCorrectEx;
    float fPqScale;
    uint  uPq;
};

float3 Pq(float3 y)
{
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float3 p = pow(saturate(y), m1);
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}

float4 main(float4 position : SV_Position) : SV_Target
{
    float3 c = tBaseMap.Load(int3(position.xy, 0)).rgb;
    bool finite = isfinite(c.r) && isfinite(c.g) && isfinite(c.b);
    c = finite ? max(c, 0.0) : float3(0.0, 0.0, 0.0);
    if (uPq != 0)
    {
        const float3x3 to2020 = {
            0.627404, 0.329283, 0.043313,
            0.069097, 0.919540, 0.011362,
            0.016391, 0.088013, 0.895595 };
        return float4(Pq(mul(to2020, c) * fPqScale), 1.0);
    }
    return float4(pow(pow(saturate(c), fGammaCorrect), fGammaCorrectEx), 1.0);
}
"#;

/// Reference of the own SDR encode for one linear colour.
pub fn sdr_encode(c: [f32; 3], transform: &OutputTransform) -> [f32; 3] {
    let g = transform.gamma_correct().unwrap_or(1.0);
    sanitize(c).map(|v| v.clamp(0.0, 1.0).powf(g).powf(transform.gamma_ex))
}

/// SMPTE ST 2084 inverse EOTF: `y` is luminance / 10000 nits.
pub fn pq(y: f32) -> f32 {
    const M1: f32 = 0.159_301_76;
    const M2: f32 = 78.843_75;
    const C1: f32 = 0.835_937_5;
    const C2: f32 = 18.851_563;
    const C3: f32 = 18.687_5;
    let p = y.clamp(0.0, 1.0).powf(M1);
    ((C1 + C2 * p) / (1.0 + C3 * p)).powf(M2)
}

/// BT.709 → BT.2020 primaries (ITU-R BT.2087), linear light.
pub fn bt709_to_bt2020(c: [f32; 3]) -> [f32; 3] {
    const M: [[f32; 3]; 3] = [
        [0.627_404, 0.329_283, 0.043_313],
        [0.069_097, 0.919_540, 0.011_362],
        [0.016_391, 0.088_013, 0.895_595],
    ];
    M.map(|row| row[0] * c[0] + row[1] * c[1] + row[2] * c[2])
}

/// Reference of the own PQ encode for one linear colour.
pub fn pq_encode(c: [f32; 3], transform: &OutputTransform) -> [f32; 3] {
    let scale = transform.hdr_white_nits / 10_000.0;
    bt709_to_bt2020(sanitize(c)).map(|v| pq(v * scale))
}

fn sanitize(c: [f32; 3]) -> [f32; 3] {
    if c.iter().all(|v| v.is_finite()) {
        c.map(|v| v.max(0.0))
    } else {
        [0.0; 3]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxbc::{fixture::container, reflect};

    const PIXEL: u16 = 0xFFFF;

    fn sdr() -> OutputTransform {
        OutputTransform {
            gamma: 2.2,
            gamma_ex: 1.0,
            pq: false,
            format: SDR_FORMAT,
            hdr_white_nits: 100.0,
        }
    }

    #[test]
    fn gamma_is_the_engines_reciprocal() {
        assert!((sdr().gamma_correct().unwrap() - 1.0 / 2.2).abs() < 1e-7);
        for gamma in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let t = OutputTransform { gamma, ..sdr() };
            assert_eq!(t.gamma_correct(), None);
            assert!(t.check().is_err());
        }
        assert!(sdr().check().is_ok());
    }

    #[test]
    fn format_must_match_the_encode() {
        assert!(OutputTransform { pq: true, ..sdr() }.check().is_err());
        let hdr = OutputTransform {
            pq: true,
            format: HDR_FORMAT,
            ..sdr()
        };
        assert!(hdr.check().is_ok());
        assert!(OutputTransform { pq: false, ..hdr }.check().is_err());
        assert!(OutputTransform {
            hdr_white_nits: 0.0,
            ..hdr
        }
        .check()
        .is_err());
    }

    #[test]
    fn sdr_encode_is_the_display_gamma() {
        let out = sdr_encode([0.5, 0.0, 1.0], &sdr());
        assert!((out[0] - 0.5f32.powf(1.0 / 2.2)).abs() < 1e-6);
        assert_eq!(out[1], 0.0);
        assert_eq!(out[2], 1.0);
        // Over-range and non-finite input stay displayable.
        assert_eq!(sdr_encode([4.0, -1.0, 0.2], &sdr())[..2], [1.0, 0.0]);
        assert_eq!(sdr_encode([f32::NAN, 1.0, 1.0], &sdr()), [0.0; 3]);
        // fGammaCorrectEx as a further exponent: identity at its default.
        let ex = OutputTransform {
            gamma_ex: 2.0,
            ..sdr()
        };
        let base = sdr_encode([0.5; 3], &sdr())[0];
        assert!((sdr_encode([0.5; 3], &ex)[0] - base * base).abs() < 1e-6);
    }

    #[test]
    fn pq_matches_st2084_reference_points() {
        assert!(pq(0.0) < 1e-6);
        assert!((pq(1.0) - 1.0).abs() < 1e-6);
        // 100 nits and 1000 nits (published code values 0.5081, 0.7518).
        assert!((pq(0.01) - 0.508_08).abs() < 2e-4, "{}", pq(0.01));
        assert!((pq(0.1) - 0.751_83).abs() < 2e-4, "{}", pq(0.1));
        assert!(pq(2.0) == pq(1.0), "clamped above 10000 nits");
    }

    #[test]
    fn primaries_keep_white_and_hdr_white_lands_on_its_nits() {
        let white = bt709_to_bt2020([1.0; 3]);
        assert!(white.iter().all(|v| (v - 1.0).abs() < 1e-5), "{white:?}");
        let hdr = OutputTransform {
            pq: true,
            format: HDR_FORMAT,
            hdr_white_nits: 200.0,
            ..sdr()
        };
        let out = pq_encode([1.0; 3], &hdr);
        assert!(out.iter().all(|v| (v - pq(0.02)).abs() < 1e-4), "{out:?}");
    }

    #[test]
    fn engine_shader_plan_from_reflection() {
        let shader = reflect(&container(
            PIXEL,
            &[("sLinear", 3, 1), ("tBaseMap", 2, 0), ("CBSystem", 0, 2)],
            &[(
                "CBSystem",
                16,
                &[
                    ("fSourceMipLevel", 0, 4),
                    ("fGammaCorrect", 4, 4),
                    ("fGammaCorrectEx", 8, 4),
                ],
            )],
        ))
        .unwrap();
        let plan = plan_engine(&shader).unwrap();
        assert_eq!(plan.texture, (0, 0));
        assert_eq!(plan.samplers, [(1, 0)]);
        let values = plan.values(&OutputTransform {
            gamma_ex: 1.5,
            ..sdr()
        });
        assert_eq!(
            values,
            [0, (1.0f32 / 2.2).to_bits(), 1.5f32.to_bits(), 0],
            "the fourth dword is padding"
        );
    }

    #[test]
    fn engine_shader_with_unknown_inputs_is_refused() {
        let extra_cb = reflect(&container(
            PIXEL,
            &[("tBaseMap", 2, 0), ("CBSystem", 0, 0)],
            &[(
                "CBSystem",
                16,
                &[("fGammaCorrect", 0, 4), ("fHdrPaperWhite", 4, 4)],
            )],
        ))
        .unwrap();
        assert!(plan_engine(&extra_cb)
            .unwrap_err()
            .contains("fHdrPaperWhite"));
        let two_textures = reflect(&container(
            PIXEL,
            &[("tBaseMap", 2, 0), ("tLut", 2, 1)],
            &[],
        ))
        .unwrap();
        assert!(plan_engine(&two_textures).is_err());
        let compute = reflect(&container(0x4353, &[("tBaseMap", 2, 0)], &[])).unwrap();
        assert!(plan_engine(&compute).is_err());
    }

    #[test]
    fn own_plan_fills_its_four_constants() {
        let hdr = OutputTransform {
            pq: true,
            format: HDR_FORMAT,
            hdr_white_nits: 250.0,
            ..sdr()
        };
        assert_eq!(
            plan_own().values(&hdr),
            [
                (1.0f32 / 2.2).to_bits(),
                1.0f32.to_bits(),
                0.025f32.to_bits(),
                1
            ]
        );
    }
}
