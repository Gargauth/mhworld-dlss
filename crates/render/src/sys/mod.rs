//! Everything that touches Direct3D 12 (Windows only).
//!
//! * creation interception: [`intercept`], [`intercept_ordinal`],
//!   [`set_original`], [`configure_device`], [`set_factory_hook`];
//! * the vtable hooks: [`install`];
//! * tracking: resource descriptions ([`describe`]), liveness
//!   ([`is_alive`]), resource states ([`resource_state`], [`Transitions`]),
//!   the constant buffer arena ([`seed_upload_region`]), the engine's queue
//!   ([`note_presenting_queue`]);
//! * the temporal pass: [`configure_pass`], [`set_pass_handler`],
//!   [`take_inputs`];
//! * recording on engine lists: [`record_on`], [`copy_resource`];
//! * our own passes, exposed through the crate's public modules:
//!   `sharpen` (RCAS), `hudless` (capture + conversion), `capture` (dev);
//! * introspection: [`counters`], [`recognised_pipelines`],
//!   [`pass_bytecode`], [`last_inputs`];
//! * ReShade: [`detect_reshade`], [`unwrap_reshade`], [`is_reshade_proxy`],
//!   [`watch_proxy_lists`] (its command list proxies, for
//!   [`PassContext::engine_list`]).

mod alive;
#[cfg(feature = "dev")]
pub(crate) mod capture;
mod convert;
mod creation;
mod device_hooks;
mod diag;
mod gpu;
pub(crate) mod hudless;
mod install;
mod introspect;
mod list_hooks;
mod liststate;
mod mvdecode;
mod pass;
mod proxylist;
mod pso;
mod queue;
mod record;
mod registry;
mod reshade;
#[cfg(feature = "dev")]
mod shaderdump;
pub(crate) mod sharpen;
mod submit;
mod volumetric;
mod vt;

pub use alive::{is_alive, serial_of};
pub use creation::{
    configure_device, intercept, intercept_ordinal, set_factory_hook, set_original, DeviceOptions,
    FactoryCreated, FactoryEntry, FactoryHook, D3D12_CREATE_DEVICE_ORDINAL,
};
pub use install::{install, installed, InstallError};
pub use introspect::{counters, Counters};
#[cfg(feature = "dev")]
pub use introspect::{resource_record, upload_region};
pub use mvdecode::DecodeError;
pub use pass::{
    configure_pass, last_inputs, set_pass_handler, take_inputs, IdentifyError, PassConfig,
    PassContext, PassHandler, PassStats, PassVerdict,
};
pub use proxylist::{proxy_dispatches, proxy_lists_watched, watch_proxy_lists};
pub use pso::{pass_bytecode, recognised_pipelines, RecognisedPipeline};
pub use queue::{
    engine_queue, is_engine_queue, note_presenting_queue, set_submit_observer, Submission,
    SubmitObserver,
};
pub use record::{copy_resource, native_list, record_on, resource_state, CopyError, Transitions};
pub use registry::{describe, note_display, ResourceInfo};
pub use reshade::{
    detect_reshade, is_reshade_proxy, native_interface, reshade_present, unwrap_reshade,
};
#[cfg(feature = "dev")]
pub use shaderdump::configure_shader_dump;
pub use volumetric::configure_volume_upsample;

/// Make GPU range `[gpu, gpu+size)` readable at CPU address `cpu` without a
/// `Map` having been seen: for the engine's constant buffer arena when the
/// mod loaded after the device init mapped it (`Device+0x1018` CPU,
/// `+0x1060` VA, `3 × 0x1000000` bytes), and for its default-heap copy
/// (`+0x1080` VA, same CPU bytes) when binds use that.
///
/// # Safety
/// `cpu` must stay valid for `size` bytes for the session (it is read when
/// a temporal dispatch binds an address in the range; reads are checked
/// for committed memory, not for meaning).
pub unsafe fn seed_upload_region(gpu: u64, size: u64, cpu: usize) {
    registry::UPLOAD.lock().seed(gpu, size, cpu);
    log::info!(target: "render", "upload region seeded: va {gpu:#x}+{size:#x} -> {cpu:#x}");
}

/// The engine presented a frame: advances the descriptor clock, so copies
/// made during loading screens (no temporal pass) do not pass for fresh;
/// closes the HUD-less frame and advances the clock our own objects are
/// retired by. Call once per present, from the present hook.
pub fn note_present() {
    registry::DESCRIPTORS.tick();
    registry::tick_presents();
    hudless::end_frame();
}
