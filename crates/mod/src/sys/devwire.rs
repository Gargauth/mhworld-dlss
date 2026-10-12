//! The dev server's side of the mod (feature `dev` only): starting it, the
//! `Host` it asks for settings and state, and the `slot` execution point.

use std::ffi::c_void;
use std::sync::atomic::Ordering;

use mhw_core::config::{Diagnostic, Named, Settings};
use mhw_core::frame::{CameraFrame, FrameInfo, JitterSample};
use mhw_dev::{DevConfig, Host, SlotContext, SlotResource};
use serde_json::{json, Value};

use super::slot::{Run, COUNTERS, DLSS_JITTER_SCALE_BITS, SS_MV_SCALE_BITS};
use super::{gate, live, runtime};

/// The mod as the dev server sees it.
#[derive(Debug)]
struct ModHost;

static HOST: ModHost = ModHost;

impl Host for ModHost {
    fn settings(&self) -> Settings {
        runtime::settings()
    }

    fn set_setting(
        &self,
        section: &str,
        key: &str,
        value: &str,
    ) -> Result<Option<Diagnostic>, Diagnostic> {
        runtime::set_setting(section, key, value)
    }

    fn state(&self) -> Value {
        state()
    }
}

fn size(s: mhw_core::frame::Size) -> Value {
    json!([s.w, s.h])
}

fn frame_json(info: &FrameInfo) -> Value {
    json!({
        "frame": info.frame,
        "render": size(info.render),
        "display": size(info.display),
        "scale": info.scale,
        "reset": format!("{:?}", info.reset),
    })
}

fn state() -> Value {
    let policy = runtime::policy();
    let fallback = runtime::fallback();
    let ss_scale = super::slot::ss_mv_scale();
    json!({
        "policy": {
            "dlss": policy.dlss,
            "hijack": policy.engine.hijack,
            "flags": policy.engine.flags.bits(),
            "ratio": policy.engine.ratio,
            "jitter": format!("{:?}", policy.engine.jitter),
            "jitter_phases": policy.engine.jitter_phases,
            "taa_sharpen_off": policy.engine.taa_sharpen_off,
            "hold_anti_aliasing": policy.engine.hold_anti_aliasing,
            "input_path": policy.input_path.name(),
            "preset": format!("{:?}", policy.preset),
            "sharpness": policy.sharpness,
            "dlss_hdr": policy.dlss_hdr.to_string(),
            "reset": policy.reset.name(),
            "hudless": policy.hudless,
            "wait_latch": policy.wait_latch.name(),
            "mip_bias": policy.mip_bias,
            "frame_generation": policy.frame_generation,
        },
        "fallback": {
            "given_up": runtime::given_up(),
            "failures": fallback.failures(),
            "total_failures": fallback.total_failures(),
            "gave_up_times": fallback.gave_up_times(),
        },
        "generation": runtime::generation(),
        "gate_installed": gate::installed(),
        "fg_started": runtime::fg_started(),
        "display": size(live::display()),
        "last_frame": live::last_frame().map(|info| frame_json(&info)),
        "slot": {
            "upscaled": COUNTERS.upscaled.load(Ordering::Relaxed),
            "dlaa": COUNTERS.dlaa.load(Ordering::Relaxed),
            "passed_through": COUNTERS.passed_through.load(Ordering::Relaxed),
            "failed": COUNTERS.failed.load(Ordering::Relaxed),
            "without_inputs": COUNTERS.without_inputs.load(Ordering::Relaxed),
            "renumbered": COUNTERS.renumbered.load(Ordering::Relaxed),
            "unmatched": COUNTERS.unmatched.load(Ordering::Relaxed),
        },
        "ss_mv_scale": [ss_scale.x, ss_scale.y],
        "ss_mv_scale_address": format!("{:#x}", SS_MV_SCALE_BITS.as_ptr() as usize),
        "dlss_jitter_scale": [
            f32::from_bits(DLSS_JITTER_SCALE_BITS[0].load(Ordering::Relaxed)),
            f32::from_bits(DLSS_JITTER_SCALE_BITS[1].load(Ordering::Relaxed)),
        ],
        "dlss_jitter_scale_address": format!("{:#x}", DLSS_JITTER_SCALE_BITS.as_ptr() as usize),
    })
}

/// Start the server (after settings and log).
pub(crate) fn start(settings: &Settings) {
    let Some(paths) = runtime::paths() else {
        return;
    };
    let config = DevConfig {
        mod_folder: paths.mod_folder.clone(),
        listen: settings.dev.listen,
        token_file: paths.mod_folder.join(&settings.dev.token_file),
        capture_dir: paths.mod_folder.join(&settings.dev.capture_dir),
    };
    match mhw_dev::start(config, &HOST) {
        Ok(()) => log::info!(target: "dev", "dev server on {}", settings.dev.listen),
        Err(error) => log::error!(target: "dev", "dev server not started: {error}"),
    }
}

/// The `slot` execution point, after the evaluation.
pub(crate) fn slot(
    list: *mut c_void,
    info: FrameInfo,
    run: &Run,
    camera: Option<CameraFrame>,
    jitter: Option<JitterSample>,
) {
    let (offered, mut absent) = run.named();
    let mut resources: Vec<SlotResource> = Vec::with_capacity(offered.len() + 1);
    for (name, resource) in offered {
        let resource = resource as *mut c_void;
        // SAFETY: the engine's open list on this thread.
        match unsafe { mhw_render::resource_state(list, resource as usize) } {
            Some(state) => resources.push(SlotResource {
                name,
                resource,
                state,
            }),
            None => absent.push((name, "its state on the slot's list is not known")),
        }
    }
    match super::fg_glue::last_hudless() {
        Some((_, image)) => resources.push(SlotResource {
            name: "hudless",
            resource: image.resource,
            state: image.state,
        }),
        None => absent.push((
            "hudless",
            "no HUD-less image tagged yet (frame generation with HudLess only)",
        )),
    }
    let context = SlotContext {
        frame: info,
        camera,
        jitter,
        resources,
        absent,
    };
    // SAFETY: the engine's open list; every resource is live and in the
    // state render tracks for it on this list.
    unsafe { mhw_dev::on_slot(list, &context) };
}
