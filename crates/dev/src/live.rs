//! The running game (Windows): addresses and views from `mhw-engine`,
//! render/NGX/frame-generation introspection, checked memory access through
//! `mhw-win`, and the module watcher behind `probe.modules`.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use mhw_engine::views::{Camera, RenderConfig, RenderSettings};
use mhw_engine::Addresses;
use parking_lot::Mutex;
use serde_json::{json, Value};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use crate::game::{CaptureService, Game, ResetSample, WaitSample};
use crate::modules::{Listed, ModuleLog};
use crate::proto::{RpcError, RpcResult};
use crate::symbols::{field_table, struct_len, Symbols};

/// Module-list polling before the device exists, and after.
const POLL_EARLY: Duration = Duration::from_millis(50);
const POLL_LATE: Duration = Duration::from_millis(500);

/// The game in this process.
pub(crate) struct Live {
    modules: Arc<Mutex<ModuleLog>>,
    capture: crate::capture_glue::Glue,
}

fn filetime_ms(t: FILETIME) -> f64 {
    ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 10_000.0
}

/// Milliseconds since this process was created.
fn process_uptime_ms() -> f64 {
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: the pseudo-handle of this process and four valid out-pointers.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok.is_err() {
        return 0.0;
    }
    // SAFETY: no arguments; returns the system time.
    let now = unsafe { GetSystemTimeAsFileTime() };
    (filetime_ms(now) - filetime_ms(created)).max(0.0)
}

fn watch_modules(log: Arc<Mutex<ModuleLog>>) {
    loop {
        let listed: Vec<Listed> = mhw_win::loaded_modules()
            .into_iter()
            .map(|m| Listed {
                name: m.name,
                path: m.path,
                base: m.base as u64,
            })
            .collect();
        let device = mhw_render::counters().devices_created > 0;
        let interval = {
            let mut log = log.lock();
            log.observe(process_uptime_ms(), &listed, device);
            if log.device_seen() {
                POLL_LATE
            } else {
                POLL_EARLY
            }
        };
        thread::sleep(interval);
    }
}

impl Live {
    /// The game; nothing runs until [`Live::start_watcher`].
    pub fn new() -> Live {
        Live {
            modules: Arc::new(Mutex::new(ModuleLog::default())),
            capture: crate::capture_glue::Glue,
        }
    }

    /// Start polling the module list (`probe.modules`).
    pub fn start_watcher(&self) {
        let log = self.modules.clone();
        if let Err(error) = thread::Builder::new()
            .name("mhw-dev-modules".into())
            .spawn(move || watch_modules(log))
        {
            log::warn!(target: "dev", "module watcher not started: {error}");
        }
    }
}

fn addresses() -> Option<&'static Addresses> {
    mhw_engine::addresses()
}

fn engine_json() -> Value {
    let s = mhw_engine::status();
    let c = &s.counters;
    json!({
        "installed": s.installed,
        "handler": s.handler,
        "depth_alias": s.depth_alias,
        "params": format!("{:?}", s.params),
        "counters": {
            "camera_setups": c.camera_setups, "frames": c.frames, "jitter_writes": c.jitter_writes,
            "cameras_published": c.cameras_published, "cameras_unusable": c.cameras_unusable,
            "optimal_answers": c.optimal_answers, "creates": c.creates, "eval_ss": c.eval_ss,
            "eval_isp": c.eval_isp, "unhandled": c.unhandled, "enqueues": c.enqueues,
            "enqueues_skipped": c.enqueues_skipped, "game_resets": c.game_resets,
            "depth_aliases": c.depth_aliases, "fatal_reports": c.fatal_reports,
        },
        "tune": format!("{:?}", s.tune),
        "wait": s.wait.map(|w| json!({"presented": w.presented, "latched": w.latched, "waitable": format!("{:#x}", w.waitable)})),
        "taa": mhw_engine::dev::taa().map(|t| format!("{t:?}")),
    })
}

fn render_json() -> Value {
    let c = mhw_render::counters();
    json!({
        "installed": c.installed,
        "devices_created": c.devices_created,
        "factories_created": c.factories_created,
        "resources_created": c.resources_created,
        "pipelines_recognised": c.pipelines_recognised,
        "dispatches": c.dispatches,
        "lists_closed": c.lists_closed,
        "close_failures": c.close_failures,
        "list_collisions": c.list_collisions,
        "unseen_submissions": c.unseen_submissions,
        "depth_targets": c.depth_targets,
        "upload_regions": c.upload_regions,
        "pass": format!("{:?}", c.pass),
        "board": format!("{:?}", c.board),
        "last_identify_error": c.last_identify_error.map(|e| format!("{e:?}")),
        "last_inputs": mhw_render::last_inputs().map(|i| json!({
            "seq": i.seq,
            "render": [i.render.w, i.render.h],
            "colour": mhw_render::names::format(i.colour.format),
            "motion": i.motion.is_some(),
            "depth": i.depth.map(|d| mhw_render::names::format(d.format)),
            "depth_match": i.depth_match.map(|m| format!("{m:?}")),
            "camera": i.camera.as_ref().map(|cb| format!("{cb:?}")),
        })),
    })
}

fn ngx_json() -> Value {
    let session = mhw_ngx::session::snapshot();
    let feature = mhw_ngx::feature::snapshot();
    json!({
        "ready": session.ready,
        "refusal": session.refusal,
        "spoof_enabled": session.spoof_enabled,
        "spoofed": session.spoofed,
        "bound_from": session.bound_from,
        "host": session.host.map(|h| format!("{h:?}")),
        "availability": session.availability.map(|a| format!("{a:?}")),
        "feature": feature.live.map(|f| json!({"desc": format!("{:?}", f.desc), "created_frame": f.created_frame})),
        "creations": feature.creations,
        "evaluations": feature.evaluations,
        "eval_failures": feature.eval_failures,
        "create_failures": feature.create_failures,
        "retiring": feature.retiring,
        "last_evaluate": feature.last.map(|l| json!({"frame": l.frame, "status": format!("{:?}", l.status), "params": format!("{:?}", l.params)})),
        "snippets": feature.snippets.iter().map(|s| json!({"path": s.path, "version": s.version.as_ref().map(|v| format!("{v:?}"))})).collect::<Vec<_>>(),
    })
}

fn fg_json() -> Value {
    let s = mhw_fg::status();
    json!({
        "ready": mhw_fg::ready(),
        "init": format!("{:?}", s.init),
        "device_set": s.device_set,
        "features_bound": s.features_bound,
        "focused": s.focused,
        "chain": s.chain.map(|c| format!("{c:?}")),
        "options": format!("{:?}", s.options),
        "dlssg_state": s.dlssg_state.map(|d| format!("{d:?}")),
        "constants": s.constants.map(|c| json!({"frame": c.frame, "index": c.index, "jitter": c.jitter, "mvec_scale": c.mvec_scale, "reset": c.reset})),
        "counters": format!("{:?}", s.counters),
        "timeline": format!("{:?}", s.timeline),
    })
}

impl Game for Live {
    fn game_build(&self) -> String {
        match mhw_win::Module::main() {
            Ok(exe) => format!(
                "timestamp {:#010x}, image size {:#x}",
                exe.timestamp(),
                exe.size()
            ),
            Err(_) => "unknown".to_owned(),
        }
    }

    fn gate(&self) -> String {
        match mhw_engine::gate_result() {
            None => "pending".to_owned(),
            Some(Ok(_)) => "open".to_owned(),
            Some(Err(why)) => format!("stand_down: {why}"),
        }
    }

    fn subsystems(&self) -> Value {
        json!({
            "engine": mhw_engine::is_installed(),
            "render": mhw_render::installed(),
            "ngx": mhw_ngx::session::ready(),
            "fg": mhw_fg::ready(),
        })
    }

    fn introspect(&self) -> Value {
        json!({"engine": engine_json(), "render": render_json(), "ngx": ngx_json(), "fg": fg_json()})
    }

    fn symbols(&self) -> Option<Symbols> {
        let at = addresses()?;
        let mut book = Symbols::default();
        for (name, address) in at.sites() {
            book.names.insert(format!("site.{name}"), address as u64);
        }
        for (name, address) in at.globals() {
            book.names.insert(format!("global.{name}"), address as u64);
        }
        let offsets: Vec<usize> = at.fields().map(|(_, offset)| offset).collect();
        let dense_velocity = at.field(mhw_engine::sites::field::TAA_1A8);
        let len = |name: &str| struct_len(name, dense_velocity).unwrap_or(0);
        let config = RenderConfig::get(at);
        if let Some(cfg) = config {
            book.add_struct("RenderConfig", cfg.address() as u64, len("RenderConfig"));
        }
        if let Some(camera) = Camera::get(at) {
            book.add_struct("Camera", camera.address() as u64, len("Camera"));
        }
        if let Some(settings) = RenderSettings::get(at) {
            book.add_struct(
                "RenderSettings",
                settings.address() as u64,
                len("RenderSettings"),
            );
        }
        if let Some(taa) = mhw_engine::dev::taa() {
            book.add_struct("TaaFilter", taa.address as u64, len("TaaFilter"));
        }
        if let Some(device) = config.and_then(|c| c.device()) {
            book.add_struct("Device", device.address() as u64, len("Device"));
        }
        book.fields = field_table(&offsets);
        Some(book)
    }

    fn read(&self, address: u64, out: &mut [u8]) -> bool {
        // SAFETY: a checked read: every page is verified committed and
        // readable first; the game does not free these regions under us
        // (the dev protocol accepts the documented race).
        usize::try_from(address).is_ok_and(|a| unsafe { mhw_win::mem::read_bytes(a, out) })
    }

    fn write(&self, address: u64, bytes: &[u8]) -> Result<(), String> {
        let address = usize::try_from(address).map_err(|_| "address out of range".to_owned())?;
        // SAFETY: the server only writes inside the resolved engine structs
        // unless the client forced it, which is the client's decision.
        unsafe { mhw_win::mem::write_bytes(address, bytes) }.map_err(|e| format!("{e:?}"))
    }

    fn camera_projection(&self) -> Option<[f32; 16]> {
        let inputs = mhw_render::last_inputs()?;
        Some(inputs.camera?.mat4("fProj")?.to_array())
    }

    fn reset_state(&self) -> ResetSample {
        ResetSample {
            game_resets: Some(mhw_engine::counters().game_resets),
            history_invalid: mhw_engine::dev::taa().and_then(|t| t.history_invalid),
            fg: mhw_fg::status().constants.map(|c| (c.frame, c.reset)),
        }
    }

    fn wait_state(&self) -> Option<WaitSample> {
        mhw_engine::wait_state().map(|w| WaitSample {
            presented: w.presented,
            latched: w.latched,
            waitable: w.waitable as u64,
        })
    }

    fn modules(&self) -> RpcResult<Value> {
        let log = self.modules.lock();
        if log.polls() == 0 {
            return Err(RpcError::not_ready("the module watcher has not polled yet"));
        }
        Ok(log.report(POLL_EARLY.as_secs_f64() * 1000.0))
    }

    fn capture(&self) -> Option<&dyn CaptureService> {
        Some(&self.capture)
    }
}
