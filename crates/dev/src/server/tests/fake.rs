//! The fake mod and fake game behind the server tests.

use std::collections::BTreeMap;
use std::ffi::c_void;

use mhw_core::config::{Build, Diagnostic, Settings};
use mhw_core::frame::Size;
use mhw_core::math::{jitter_matrix, Mat4, Vec2};
use mhw_engine::sites::FIELDS;
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::analysis::{dxgi, ColourStats, Image};
use crate::game::{CaptureItem, CaptureService, Game, ResetSample, WaitSample};
use crate::proto::RpcResult;
use crate::symbols::{field_table, Symbols};
use crate::Host;

pub(super) const RENDER: Size = Size::new(16, 8);
pub(super) const DISPLAY: Size = Size::new(32, 16);

/// A reversed-depth, infinite-far perspective like the engine's.
pub(super) fn projection() -> Mat4 {
    Mat4::from_rows([
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.8, 0.0, 0.0],
        [0.0, 0.0, 0.0, -1.0],
        [0.0, 0.0, 0.1, 0.0],
    ])
}

const CFG: u64 = 0x10_0000;
const DEVICE: u64 = 0x20_0000;
const CODE: u64 = 0x30_0000;

pub(super) struct FakeHost {
    pub(super) settings: Mutex<Settings>,
}

impl Host for FakeHost {
    fn settings(&self) -> Settings {
        self.settings.lock().clone()
    }
    fn set_setting(
        &self,
        section: &str,
        key: &str,
        value: &str,
    ) -> Result<Option<Diagnostic>, Diagnostic> {
        self.settings
            .lock()
            .set(section, key, value, Build::CURRENT)
    }
    fn state(&self) -> Value {
        json!({"render_size": [RENDER.w, RENDER.h], "display_size": [DISPLAY.w, DISPLAY.h], "scale": 0.5})
    }
}

#[derive(Default)]
struct FakeCapture {
    done: Mutex<Vec<Image>>,
    stats: Mutex<Vec<(u64, u64, ColourStats)>>,
}

fn half(v: f32) -> u16 {
    if v == 0.0 {
        return 0;
    }
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    sign | ((exp as u16) << 10) | ((bits >> 13) & 0x3ff) as u16
}

/// tGBuffer4 NDC at a texel (exact in f16).
fn ndc(x: u32, y: u32) -> (f32, f32) {
    (x as f32 / 64.0, -(y as f32) / 64.0)
}

fn synth(name: &str, request: u64, frame: u64) -> Image {
    let (format, bpp) = match name {
        "game_mv" => (dxgi::R32G32_FLOAT, 8),
        "mv_raw" => (dxgi::R32_UINT, 4),
        "game_depth" => (dxgi::R32_FLOAT, 4),
        "depth" | "pass_depth" => (dxgi::D32_FLOAT, 4),
        _ => (26, 4),
    };
    let pitch = 256u32;
    let mut bytes = vec![0u8; (pitch * RENDER.h) as usize];
    let per_px = mhw_render::mvpack::ndc_to_pixels(RENDER);
    for y in 0..RENDER.h {
        for x in 0..RENDER.w {
            let (nx, ny) = ndc(x, y);
            let texel: Vec<u8> = match name {
                // The game's velocity: −x, +y in decoded render pixels.
                "game_mv" => [
                    (-nx * per_px.x).to_le_bytes(),
                    (ny * per_px.y).to_le_bytes(),
                ]
                .concat(),
                "mv_raw" => ((u32::from(half(nx)) << 16) | u32::from(half(ny)))
                    .to_le_bytes()
                    .to_vec(),
                "game_depth" | "depth" | "pass_depth" => {
                    (0.25 + 0.01 * x as f32).to_le_bytes().to_vec()
                }
                _ => vec![1, 2, 3, 4],
            };
            let at = (y * pitch + x * bpp) as usize;
            bytes[at..at + bpp as usize].copy_from_slice(&texel);
        }
    }
    Image {
        request,
        name: name.into(),
        format,
        width: RENDER.w,
        height: RENDER.h,
        row_pitch: pitch,
        frame,
        bytes,
    }
}

impl CaptureService for FakeCapture {
    unsafe fn record(
        &self,
        _: *mut c_void,
        request: u64,
        frame: u64,
        items: &[CaptureItem],
    ) -> RpcResult<()> {
        let mut done = self.done.lock();
        done.extend(items.iter().map(|i| synth(&i.name, request, frame)));
        Ok(())
    }
    fn poll(&self) -> Vec<Image> {
        std::mem::take(&mut *self.done.lock())
    }
    unsafe fn record_stats(
        &self,
        _: *mut c_void,
        request: u64,
        frame: u64,
        _: &CaptureItem,
    ) -> RpcResult<()> {
        let stats = ColourStats {
            max: [12.5, 3.0, 1.0],
            p50: 0.2,
            above_one: 0.01,
            ..ColourStats::default()
        };
        self.stats.lock().push((request, frame, stats));
        Ok(())
    }
    fn poll_stats(&self) -> Vec<(u64, u64, ColourStats)> {
        std::mem::take(&mut *self.stats.lock())
    }
}

/// What the fake slot offers: resources with their pointers, and the
/// absent ones with the mod's reason.
pub(super) struct Offer {
    pub(super) resources: Vec<(&'static str, usize)>,
    pub(super) absent: Vec<(&'static str, &'static str)>,
}

impl Default for Offer {
    /// Everything, each its own texture.
    fn default() -> Offer {
        let names = [
            "colour",
            "depth",
            "mv",
            "mv_raw",
            "pass_depth",
            "game_mv",
            "game_depth",
        ];
        Offer {
            resources: names
                .iter()
                .enumerate()
                .map(|(i, &n)| (n, 0x1000 * (i + 1)))
                .collect(),
            absent: Vec::new(),
        }
    }
}

pub(super) struct FakeGame {
    pub(super) offer: Mutex<Offer>,
    memory: Mutex<BTreeMap<u64, Vec<u8>>>,
    pub(super) jitter: Mutex<Vec2>,
    pub(super) resets: Mutex<u64>,
    capture: FakeCapture,
}

impl FakeGame {
    pub(super) fn new() -> FakeGame {
        let mut memory = BTreeMap::new();
        let mut cfg = vec![0u8; 0x1000];
        cfg[0x200..0x204].copy_from_slice(&0.5f32.to_le_bytes());
        memory.insert(CFG, cfg);
        memory.insert(DEVICE, vec![0u8; 0x100]);
        memory.insert(CODE, vec![0xcc; 0x100]);
        FakeGame {
            offer: Mutex::new(Offer::default()),
            memory: Mutex::new(memory),
            jitter: Mutex::new(Vec2::ZERO),
            resets: Mutex::new(0),
            capture: FakeCapture::default(),
        }
    }

    fn span(&self, address: u64, len: usize) -> Option<(u64, usize)> {
        let memory = self.memory.lock();
        let (&base, bytes) = memory.range(..=address).next_back()?;
        let off = (address - base) as usize;
        (off + len <= bytes.len()).then_some((base, off))
    }
}

impl Game for FakeGame {
    fn game_build(&self) -> String {
        "timestamp 0x66d13cb6, image size 0x55d3000".into()
    }
    fn gate(&self) -> String {
        "open".into()
    }
    fn subsystems(&self) -> Value {
        json!({"engine": true, "render": true, "ngx": false, "fg": false})
    }
    fn introspect(&self) -> Value {
        json!({"engine": {"installed": true}})
    }
    fn symbols(&self) -> Option<Symbols> {
        let mut s = Symbols::default();
        s.add_struct("RenderConfig", CFG, 0x1000);
        s.add_struct("Device", DEVICE, 0x100);
        s.names.insert("site.FATAL".into(), CODE);
        s.fields = field_table(&FIELDS.iter().map(|f| f.known).collect::<Vec<_>>());
        Some(s)
    }
    fn read(&self, address: u64, out: &mut [u8]) -> bool {
        let Some((base, off)) = self.span(address, out.len()) else {
            return false;
        };
        out.copy_from_slice(&self.memory.lock()[&base][off..off + out.len()]);
        true
    }
    fn write(&self, address: u64, bytes: &[u8]) -> Result<(), String> {
        let (base, off) = self.span(address, bytes.len()).ok_or("not mapped")?;
        self.memory.lock().get_mut(&base).unwrap()[off..off + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
    fn camera_projection(&self) -> Option<[f32; 16]> {
        let proj = projection();
        Some((proj * jitter_matrix(*self.jitter.lock(), DISPLAY)).to_array())
    }
    fn reset_state(&self) -> ResetSample {
        ResetSample {
            game_resets: Some(*self.resets.lock()),
            history_invalid: Some(false),
            fg: Some((7, false)),
        }
    }
    fn wait_state(&self) -> Option<WaitSample> {
        Some(WaitSample {
            presented: true,
            latched: false,
            waitable: 0x1234,
        })
    }
    fn modules(&self) -> RpcResult<Value> {
        let mut log = crate::modules::ModuleLog::default();
        log.observe(
            10.0,
            &[crate::modules::Listed {
                name: "game.exe".into(),
                path: "game.exe".into(),
                base: 1,
            }],
            false,
        );
        Ok(log.report(50.0))
    }
    fn capture(&self) -> Option<&dyn CaptureService> {
        Some(&self.capture)
    }
}
