//! Engine settings tuned for its own temporal filter, held for DLSS
//! (`docs/re/RePostProcess.md`).
//!
//! Once per render frame, from the camera setup (or [`maintain`] when `mod`
//! wants it sooner):
//!
//! * **render ratio** through the engine's setter `0x14229be60`, with the
//!   16-alignment fix at 0.5 ([`crate::ratio`]); 1.0 written directly;
//! * with the DLSS path taken over: the engine's **own scale** held at 1.0
//!   and **dynamic resolution** off (a second scaling would crop the
//!   picture);
//! * **anti-aliasing word** `rs+0xe9e4` held at `0x0001` (TAA only): AA Off
//!   or FXAA ends the TAA draw (no inputs), and with the game's own DLSS
//!   option on the AA handler `0x14043dcb0` forces TAA+FXAA. The handler
//!   is not a gate site, so the hold is re-applied every frame, before
//!   the frame's TAA draw; released (the game's value put back, if the word
//!   is still ours) when the hold is turned off;
//! * **dithers** (SSLR dither and g-buffer jitter, contact and facial
//!   contact noise) off, originals logged and restored when turned off;
//! * **sampler bank 1** selected once the mip-bias rewrite was made;
//! * with the volumetric fog at full resolution wanted
//!   ([`hold_downsample_volume`]): the tonemap filter's
//!   **`mDownsampleVolume`** held off, written under the game's own lock
//!   (`docs/re/ReVolumetrics.md` §3), the game's value put back when the
//!   hold is turned off.
//!
//! The TAA sharpen (`taa+0x187`) is cleared per temporal pass in the SS
//! enqueue detour, which owns the TAA filter pointer.

use std::sync::atomic::{AtomicBool, Ordering};

use mhw_core::{log_changed, log_once};
use mhw_win::{CodePatch, Module};
use parking_lot::Mutex;

use crate::gate::Addresses;
use crate::hooks::hijack::{MENU_POSITION, SHARPEN_CLEARED};
use crate::layout::render_settings::{BANK_ONE, TAA_ONLY};
use crate::mipbias;
use crate::params::Params;
use crate::ratio;
use crate::runtime;
use crate::sites::{global, site};
use crate::status::{MipBias, MipBiasOutcome, TuneState};
use crate::views::{RenderConfig, RenderSettings, Tonemap};

struct Tune {
    ratio_wanted: f32,
    ratio_input: f32,
    ratio_stored: f32,
    ratio_built: f32,
    own_scale_restored: u64,
    dynamic_resolution_restored: u64,
    aa_word_restored: u64,
    aa_word_game: Option<u16>,
    dither_game: Option<[u8; 4]>,
    sampler_bank: Option<u32>,
    mip_bias: MipBiasOutcome,
    downsample_game: Option<bool>,
}

static TUNE: Mutex<Tune> = Mutex::new(Tune {
    ratio_wanted: f32::NAN,
    ratio_input: f32::NAN,
    ratio_stored: f32::NAN,
    ratio_built: f32::NAN,
    own_scale_restored: 0,
    dynamic_resolution_restored: 0,
    aa_word_restored: 0,
    aa_word_game: None,
    dither_game: None,
    sampler_bank: None,
    mip_bias: MipBiasOutcome::NotRequested,
    downsample_game: None,
});

/// Whether `mDownsampleVolume` is held off.
static DOWNSAMPLE_OFF: AtomicBool = AtomicBool::new(false);

/// Hold the tonemap filter's `mDownsampleVolume` off (`true`) from the next
/// frame, or give the game its value back (`false`, the default).
pub fn hold_downsample_volume(off: bool) {
    DOWNSAMPLE_OFF.store(off, Ordering::Release);
}

/// Apply the frame's tuning now (idempotent and cheap when nothing
/// changed). Does nothing before [`crate::install`].
pub fn maintain() {
    let Some(at) = runtime::installed() else {
        return;
    };
    if let Some(config) = RenderConfig::get(at) {
        maintain_frame(at, &runtime::params(), &config);
    }
}

pub(crate) fn maintain_frame(at: &'static Addresses, params: &Params, config: &RenderConfig) {
    let mut tune = TUNE.lock();
    follow_ratio(at, params, config, &mut tune);
    if params.hijack {
        hold_own_scale(config, &mut tune);
    }
    if let Some(rs) = RenderSettings::get(at) {
        hold_anti_aliasing(params.hijack && params.hold_anti_aliasing, &rs, &mut tune);
        undither(params.hijack && params.undither, &rs, &mut tune);
        if matches!(tune.mip_bias, MipBiasOutcome::Built(_)) && rs.sampler_bank() != Some(BANK_ONE)
        {
            let before = rs.sampler_bank();
            if rs.set_sampler_bank(BANK_ONE) {
                tune.sampler_bank = Some(BANK_ONE);
                log_once!(
                    Info,
                    "tune",
                    "sampler bank",
                    "sampler bank 1 selected (was {before:?}); materials loaded from now on bake it"
                );
            }
        }
    }
    downsample_volume(at, DOWNSAMPLE_OFF.load(Ordering::Acquire), &mut tune);
}

fn downsample_volume(at: &'static Addresses, off: bool, tune: &mut Tune) {
    if !off && tune.downsample_game.is_none() {
        return;
    }
    Tonemap::with(at, |filter| {
        let Some(now) = filter.downsample_volume() else {
            return;
        };
        if off {
            if now && filter.set_downsample_volume(false) {
                tune.downsample_game.get_or_insert(now);
                log_changed!(
                    Info,
                    "tune",
                    "downsample volume",
                    filter.address(),
                    "volumetric fog at full resolution: mDownsampleVolume of the tonemap filter \
                     at {:#x} held off",
                    filter.address()
                );
            }
        } else if let Some(game) = tune.downsample_game.take() {
            if !now && filter.set_downsample_volume(game) {
                log::info!(target: "tune", "mDownsampleVolume given back to the game ({game})");
            }
        }
    });
}

type RatioSetterFn = unsafe extern "system" fn(usize, f32);

fn follow_ratio(at: &'static Addresses, params: &Params, config: &RenderConfig, tune: &mut Tune) {
    let (Some(live), Some(display)) = (config.live_ratio(), config.screen_size()) else {
        return;
    };
    tune.ratio_built = config.built_ratio().unwrap_or(f32::NAN);
    let wanted = params.ratio;
    let unchanged = tune.ratio_wanted.to_bits() == wanted.to_bits()
        && tune.ratio_stored.to_bits() == live.to_bits();
    if unchanged {
        return;
    }
    tune.ratio_wanted = wanted;
    if ratio::full_window(wanted) {
        tune.ratio_input = 1.0;
        if live != 1.0 {
            config.set_live_ratio(1.0);
        }
    } else {
        // SAFETY: the setter is a gate site (`RATIO_SETTER`, both ends
        // checked): a leaf taking the render config in rcx and the ratio in
        // xmm1, storing cfg+0x200; `config` is the live object.
        let setter: RatioSetterFn = unsafe { std::mem::transmute(at.site(site::RATIO_SETTER)) };
        let mut input = ratio::setter_input(wanted, display.w);
        // SAFETY: as above.
        unsafe { setter(config.base(), input) };
        // The setter may take its width from elsewhere than mScreenSize (the
        // PC option object, `0x14229be88`): if it still cut under half, ask
        // one step of 16 higher.
        if config
            .live_ratio()
            .is_some_and(|stored| stored < ratio::SMALLEST)
            && display.w > 0
        {
            input = (input + 16.0 / display.w as f32).min(ratio::LARGEST);
            // SAFETY: as above.
            unsafe { setter(config.base(), input) };
        }
        tune.ratio_input = input;
    }
    let stored = config.live_ratio().unwrap_or(f32::NAN);
    tune.ratio_stored = stored;
    log_changed!(
        Info,
        "tune",
        "ratio",
        stored.to_bits(),
        "render ratio {wanted:.4} wanted, cfg+0x200 now {stored:.5} ({}x{} of {}x{}); built \
         {:.5}",
        ratio::render_width(stored, display.w),
        ratio::render_width(stored, display.h),
        display.w,
        display.h,
        tune.ratio_built
    );
}

fn hold_own_scale(config: &RenderConfig, tune: &mut Tune) {
    if let Some(own) = config.own_scale().filter(|own| *own != 1.0) {
        if config.set_own_scale(1.0) {
            tune.own_scale_restored += 1;
            log_changed!(
                Info,
                "tune",
                "own scale",
                own.to_bits(),
                "the engine's own scale (cfg+0x1f0) read {own:.4}; put back to 1.0 ({} times)",
                tune.own_scale_restored
            );
        }
    }
    if let Some(switch) = config.dynamic_resolution().filter(|s| *s != 0) {
        if config.set_dynamic_resolution(0) {
            tune.dynamic_resolution_restored += 1;
            log_changed!(
                Info,
                "tune",
                "dynamic resolution",
                switch,
                "dynamic resolution (cfg+0x7b3b0) was {switch}; switched off ({} times)",
                tune.dynamic_resolution_restored
            );
        }
    }
}

fn hold_anti_aliasing(hold: bool, rs: &RenderSettings, tune: &mut Tune) {
    let Some(word) = rs.anti_aliasing() else {
        return;
    };
    if hold {
        if word != TAA_ONLY && rs.set_anti_aliasing(TAA_ONLY) {
            tune.aa_word_game.get_or_insert(word);
            tune.aa_word_restored += 1;
            log_changed!(
                Info,
                "tune",
                "aa word",
                word,
                "anti-aliasing word was {word:#06x}; held at TAA only ({} times)",
                tune.aa_word_restored
            );
        }
    } else if let Some(game) = tune.aa_word_game.take() {
        if word == TAA_ONLY && rs.set_anti_aliasing(game) {
            log::info!(target: "tune", "anti-aliasing hold released: the game's {game:#06x} put back");
        }
    }
}

fn undither(off: bool, rs: &RenderSettings, tune: &mut Tune) {
    let Some(now) = rs.dithers() else {
        return;
    };
    if off {
        if now != [0; 4] && rs.set_dithers([0; 4]) {
            tune.dither_game.get_or_insert(now);
            log_changed!(
                Info,
                "tune",
                "dithers",
                now,
                "SSLR dither/g-buffer jitter and contact/facial noise were {now:?}; turned off"
            );
        }
    } else if let Some(game) = tune.dither_game.take() {
        if now == [0; 4] && rs.set_dithers(game) {
            log::info!(target: "tune", "dithers restored to the game's {game:?}");
        }
    }
}

/// The mip-bias rewrite, as a patch to apply with the detours, and what to
/// report. `None` patch: nothing to do (see the outcome).
pub(crate) fn plan_mip_bias(
    at: &Addresses,
    module: &Module,
    bias: Option<f32>,
) -> (Option<CodePatch>, MipBiasOutcome) {
    let Some(bias) = bias else {
        return (None, MipBiasOutcome::NotRequested);
    };
    let Some(constant) = mipbias::bank_one_constant(bias) else {
        return (None, MipBiasOutcome::NoBias);
    };
    let instruction = at.site_rva(site::BANK_ONE_SOURCE);
    let source = at.global(global::BANK_ONE_SOURCE) - at.base;
    let read_f32 = |rva: usize| {
        let bytes = module.read(u32::try_from(rva).ok()?, 4)?;
        Some(f32::from_le_bytes(bytes.try_into().ok()?))
    };
    let found = read_f32(source).unwrap_or(f32::NAN);
    if found != mipbias::STOCK_CONSTANT {
        return (None, MipBiasOutcome::NotStock { found });
    }
    let literal = u32::try_from(source)
        .ok()
        .and_then(|rva| module.headers().section_containing(rva))
        .and_then(|section| {
            let data = module.read(section.rva, section.mapped_len() as usize)?;
            let (offset, value) = mipbias::constant_near(data, section.rva as usize, constant)?;
            Some((section.rva as usize + offset, value))
        });
    let Some((literal, value)) = literal else {
        return (None, MipBiasOutcome::NoConstant { wanted: constant });
    };
    let at_operand = instruction + mipbias::DISPLACEMENT_AT;
    let (Some(old), Some(new)) = (
        u32::try_from(at_operand)
            .ok()
            .and_then(|rva| module.read(rva, 4)),
        mipbias::displacement_to(instruction, literal),
    ) else {
        return (
            None,
            MipBiasOutcome::Failed("the constant is out of reach".into()),
        );
    };
    let patch = CodePatch::new(
        "sampler bank 1 constant",
        at.base + at_operand,
        old,
        &new.to_le_bytes(),
    );
    let outcome = MipBiasOutcome::Built(MipBias {
        bias: mipbias::bias_of(value),
        constant: value,
        literal: at.base + literal,
    });
    match patch {
        Some(patch) => (Some(patch), outcome),
        None => (
            None,
            MipBiasOutcome::Failed("patch not representable".into()),
        ),
    }
}

/// Record the mip-bias outcome after the patch was (or was not) applied.
pub(crate) fn set_mip_bias(outcome: MipBiasOutcome) {
    match &outcome {
        MipBiasOutcome::Built(built) => log::info!(
            target: "tune",
            "sampler bank 1 now loads {:.4} from {:#x}: a mip bias of {:+.3}, fixed for the session",
            built.constant,
            built.literal,
            built.bias
        ),
        MipBiasOutcome::NotRequested | MipBiasOutcome::NoBias => {}
        other => log::warn!(target: "tune", "mip bias not built: {other:?}; bank 0 stays"),
    }
    TUNE.lock().mip_bias = outcome;
}

/// The tuning state, for introspection.
pub fn state() -> TuneState {
    let tune = TUNE.lock();
    let menu = MENU_POSITION.load(Ordering::Relaxed);
    TuneState {
        ratio_wanted: tune.ratio_wanted,
        ratio_input: tune.ratio_input,
        ratio_stored: tune.ratio_stored,
        ratio_built: tune.ratio_built,
        own_scale_restored: tune.own_scale_restored,
        dynamic_resolution_restored: tune.dynamic_resolution_restored,
        aa_word_restored: tune.aa_word_restored,
        aa_word_game: tune.aa_word_game,
        sharpen_cleared: SHARPEN_CLEARED.load(Ordering::Relaxed),
        dither_game: tune.dither_game,
        sampler_bank: tune.sampler_bank,
        mip_bias: tune.mip_bias.clone(),
        menu_position: (menu != u8::MAX).then_some(menu),
    }
}
