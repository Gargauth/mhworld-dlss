//! Runtime settings, re-read while the game runs.
//!
//! The file is polled on a thread of its own, so edits take effect within a
//! second without leaving the game, and `get()`, which the render hooks call
//! constantly, only copies a value out of memory: a render thread never
//! stops to read a file.
//!
//! ## What is a setting and what is not
//!
//! Four kinds of key are read, and the file the mod writes (`SHIPPED`)
//! groups them the same way:
//!
//! * settings somebody changes: the render scale, the mip bias, the DLSS
//!   preset, frame generation and what goes with it;
//! * expert keys, `expert_*`: values the mod otherwise holds at the one that
//!   was measured -- the jitter, the motion vectors, what DLSS is told --
//!   which as shipped are exactly that, so the file changes nothing;
//! * diagnostics, all off by default, worth having for a bug report;
//! * a few experiments on one open artefact of frame generation, which as
//!   shipped change nothing.
//!
//! Everything else the mod works from is a constant in the module that uses
//! it. A key this build does not read is ignored and named in the log once;
//! if a development build read it, the line says what is done instead
//! (`RETIRED`). That is not an error and never stops anything.
//!
//! A few fields of `Config` are not keys: they are fixed at one value and
//! are fields only because `fg.rs`, `hudless.rs` and `streamline.rs` read
//! them from here. They are grouped at the end of the struct.

use parking_lot::RwLock;
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// How often the file is checked. Fast enough to tune by, far longer than a
/// frame.
const POLL: Duration = Duration::from_millis(500);

/// Pack a short ASCII string into a fixed, NUL-terminated buffer.
///
/// `Config` is `Copy` and is handed out by value, so it cannot hold a
/// `String`; the two identity strings `slInit` takes are short and bounded, so
/// they live inline. A too-long literal is a compile error rather than a
/// silent truncation -- a truncated GUID is a different GUID.
const fn packed<const N: usize>(text: &str) -> [u8; N] {
    let bytes = text.as_bytes();
    assert!(bytes.len() < N, "does not fit, with room for the NUL");
    let mut out = [0u8; N];
    let mut i = 0;
    while i < bytes.len() {
        out[i] = bytes[i];
        i += 1;
    }
    out
}

/// Read a packed buffer back as a `&str`, stopping at the NUL.
///
/// Empty for an empty buffer, which is the signal every caller wants: an
/// empty identity string is one Streamline must be given as a null pointer.
pub fn packed_str(buffer: &[u8]) -> &str {
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    std::str::from_utf8(&buffer[..end]).unwrap_or("")
}

#[derive(Clone, Copy, PartialEq)]
pub struct Config {
    // ----- settings -------------------------------------------------------
    /// Fraction of the window's resolution the engine renders at, with DLSS
    /// reconstructing the rest: 1.0 is DLAA, 0.667 Quality, 0.5 Performance.
    /// The only control of render resolution: the game's own Image Quality
    /// option is held at High and has no effect. The engine supports 0.5 to
    /// 1.0; anything outside is clamped, and 0 is taken as the default,
    /// 0.667 (`hijack::ratio_from_file`). Live.
    pub render_scale: f32,
    /// What the file said for `render_scale`, before the clamp, so that a
    /// difference can be said once in the log.
    pub render_scale_asked: f32,
    /// Which of the engine's fifteen sampler banks every material binds
    /// through, which is the only texture mip bias it has. 0 leaves the
    /// engine's own choice, bank 0: no bias, however small the scene is
    /// rendered. 1 is the bias the render ratio wants, log2 of
    /// `render_scale` as it reads at start-up: bank 1 is rebuilt for it and
    /// selected, and at 1.0 nothing is built or selected. Read at start-up.
    /// See `hijack::mip_bias_bank` and `tune::prepare_exact_bias`.
    pub mip_bias: u32,
    /// Stop the screen-space effects resolving their detail across frames
    /// with patterns only the engine's own temporal pass understands. Cannot
    /// be undone within a session.
    pub undither: bool,
    /// `NVSDK_NGX_DLSS_Hint_Render_Preset`: 0 leaves the choice to the
    /// driver, 1 and up name presets A onward. `dlss::create` refuses values
    /// above the highest it has verified the library to carry. Changing it
    /// rebuilds the feature, so it applies live.
    pub dlss_preset: i32,
    /// Generate frames and present them, through Streamline. Read once, at
    /// startup: with it on the engine renders off-screen into Streamline's
    /// swapchain, and there is no halfway state.
    pub fg: bool,
    /// Presented frames per rendered one: 2 doubles, 3 and 4 are the
    /// multi-frame modes. Live.
    pub fg_multiplier: u32,
    /// Hand the kept interface-free frame to frame generation as the
    /// HUD-less colour. Live. See `hudless`.
    pub fg_hudless: bool,
    /// Ask for Reflex low latency boost, which keeps clocks up at the cost of
    /// power. Reflex itself is not a setting: frame generation forces it on,
    /// because `slReflexSetOptions(mode 0)` is `eOff` and DLSS-G then
    /// presents the real frame and generates nothing. See
    /// `fg::note_frame_inputs`.
    pub reflex_boost: bool,
    /// Clamp what the engine asks for with `SetMaximumFrameLatency`. It asks
    /// for three, and because it owns the frame-latency waitable object
    /// Streamline leaves that number alone, so this is the one lever on it.
    /// 0 leaves it alone.
    pub fg_max_latency: u32,

    // ----- diagnostics ----------------------------------------------------
    /// Make the first of the code signatures `gate` checks read as wrong, so
    /// that what the mod does on a game build it does not know can be seen
    /// without one. Read once, when the check is first made.
    pub diag_unknown_build: bool,
    /// Treat `slInit` as having ended in an exception, without calling it,
    /// so that what the mod does after one can be seen without one: no
    /// frame generation, and NGX's initialisation made with a time limit.
    /// Read once, at start-up, and only with `fg = 1`.
    pub diag_streamline_fault: bool,
    /// Write the per-frame and per-resource dumps to the log.
    ///
    /// These are what identified the temporal pass, the descriptor layout and
    /// the constant buffer fields, and they are needed again after a game
    /// patch moves something. They are also far too noisy to leave on.
    pub log_verbose: bool,
    /// Account for the time spent inside this library per rendered frame and
    /// write it to the log every 600 frames, by hook family. Off, each hook
    /// pays one relaxed load. See `perf`.
    pub perf: bool,
    /// Sample the motion vector buffer and write the readings to the log:
    /// the instrument the encoding was established with. It adds a buffer
    /// copy and two barriers per frame.
    pub mv_probe: bool,
    /// Ask the driver to keep GPU breadcrumbs and page fault data, so a device
    /// removal says what caused it instead of just ending the process. Read
    /// at device creation.
    pub dred: bool,
    /// Turn on the Direct3D 12 debug layer and put its messages in our log.
    /// Costs a great deal of performance and needs the Graphics Tools
    /// feature, but it names invalid barriers and released resources outright.
    /// Read at device creation.
    pub debug_layer: bool,
    /// Log every render target bound between the upscale slot and Present,
    /// with its size, format and draw count. This is how the interface-free
    /// frame is identified: see `hudless`.
    pub hudless_survey: bool,
    /// Keep a window-sized target at its Nth binding of the frame, counting
    /// from 1, or 0 for none. The measured answer for this engine is 2.
    ///
    /// Note that a non-zero value arms the capture by itself, whether or not
    /// `fg_hudless` hands the result to anything: with this at 2 and
    /// `fg_hudless` at 0 a full-size frame is copied every frame and then
    /// not used. With `fg_hudless` on, 0 here means 2.
    pub hudless_occurrence: usize,
    /// Not a key: fixed at 0, the derived motion-vector scale. The other
    /// values are for comparison in a development build: 1 twice the derived
    /// scale, 2 the arithmetic for the engine's raw buffer, 3 sixteen times
    /// the derived scale. See `fg::camera_for_streamline`.
    pub fg_mvec_space: u32,
    /// Not a key: fixed at false. Tells DLSS-G the tagged vectors still
    /// carry the jitter, which they do not.
    pub fg_mvec_jittered: bool,
    /// Not a key: fixed at false. Tells DLSS-G the tagged vectors have been
    /// dilated towards the nearest depth. They have not: the decode writes
    /// one vector per pixel.
    pub fg_mvec_dilated: bool,
    /// Streamline's own `Preferences::logLevel`: 0 off, 1 default, 2 verbose.
    /// Everything it says arrives in our log through its message callback.
    pub fg_log_level: u32,
    /// Resolve at the engine's temporal pass, straight into its full-size
    /// target, instead of at its upscale stage. That route bypasses the
    /// engine's passes between the two, so its picture is paler below a
    /// render ratio of 1.0. Off. Kept for comparison, and it is also what
    /// the mod falls back to by itself if the upscale stage cannot resolve.
    /// Live.
    pub resolve_at_pass: bool,
    /// Write the engine's resolution state, what DLSS and frame generation
    /// are given, and a frame's passes, whenever they change. Off. Live.
    /// See `pathdump`.
    pub path_dump: bool,
    /// Whether DLSS-G is asked to generate at all. On, which is today's
    /// behaviour; off passes `DLSSGMode::eOff` while everything else about
    /// the Streamline path stays as it is. Read every frame, so it is the
    /// control for any judgement of the generated picture: the same spot,
    /// the same camera move, with and without generated frames, without a
    /// restart.
    pub fg_interpolate: bool,
    /// Set `DLSSGOptions::enableUserInterfaceRecomposition`. The guide wants
    /// both a HUD-less image and a UI buffer tagged for it.
    pub fg_uir: bool,
    /// Tag a constant mask as an experimental input: 0 none, 1 as
    /// `kBufferTypeNoWarpMask`, 2 as `kBufferTypeUIAlpha`. See `fgmask.rs`.
    pub fg_test_mask: u32,
    /// What the mask holds inside its box, 0 to 1.
    pub fg_test_mask_value: f32,
    /// The fraction of each screen dimension the centred box covers. 1 is the
    /// whole screen, which is the extreme; the mask is zero outside the box.
    pub fg_test_mask_box: f32,

    // ----- expert ---------------------------------------------------------
    //
    // What the mod otherwise holds at a measured value. Each is that value
    // as shipped, and the value and the reason for it are with the constant
    // named in `DEFAULT`; they are keys so that the picture can be judged
    // against a different one while the game runs. All live.
    /// Offset the engine's camera by a sub-pixel amount each frame. Off, DLSS
    /// is told what the camera then carries, which is read from the frame.
    pub expert_jitter: bool,
    /// How many render pixels the jitter samples are spread across.
    pub expert_jitter_pixels: f32,
    /// Length of the Halton sequence before it repeats.
    pub expert_jitter_phases: u32,
    /// Multiplier on the jitter DLSS is told, read from the frame's camera.
    pub expert_jitter_report: (f32, f32),
    /// How much of the jitter the decode adds back to the motion vectors.
    pub expert_mv_dejitter: f32,
    /// Multiplier on the decoded motion vectors, which frame generation is
    /// given too.
    pub expert_mv_scale: (f32, f32),
    /// `NVSDK_NGX_DLSS_Feature_Flags` bits 0, 3, 6 and 2. Changing one
    /// rebuilds the feature. See `dlss::configured_flags`.
    pub expert_dlss_hdr: bool,
    pub expert_dlss_depth_inverted: bool,
    pub expert_dlss_auto_exposure: bool,
    pub expert_dlss_mv_jittered: bool,
    /// The distance both of the engine's alpha checkerboard distances are
    /// set to, or 0 for what the mod does without it. See `tune::unroll`.
    pub expert_alpha_unroll: f32,

    // ----- not settings ---------------------------------------------------
    //
    // Fixed at the value below and not read from the file. They are fields
    // only because `fg.rs`, `hudless.rs` and `streamline.rs` read them from
    // here; each could become a constant in the module that uses it.
    /// The scale that turns the engine's raw motion vectors into pixels at
    /// 3840x2160. Upscaling derives its own from the render size; only
    /// `fg.rs` reads this, for the undecoded fallback and `fg_mvec_space = 2`.
    pub mv_scale: (f32, f32),
    /// Application id to give NGX, or zero to keep the game's own.
    pub ngx_app_id: u64,
    /// `sl::RenderAPI`: 1 is `eD3D12`.
    pub fg_render_api: u32,
    /// Which identity `slInit` presents: 1 is the engine/project route
    /// (engine type, engine version and project GUID), which is the one
    /// DLSS-G loads on in this game.
    pub fg_identity: u32,
    /// `sl::EngineType`: 0 is `eCustom`, which is the truth for MT Framework.
    pub fg_engine: u32,
    /// `sl::Preferences::engineVersion`.
    pub fg_engine_version: [u8; 32],
    /// `sl::Preferences::projectId`, a GUID of this project's own choosing.
    pub fg_project_id: [u8; 48],
    /// Let Streamline and NGX fetch and load over-the-air plugins and models,
    /// which is Streamline's own default.
    pub fg_ota: bool,
    /// The raw-index override for the HUD-less capture. 0: not used.
    pub hudless_slot: usize,
    /// The `DXGI_FORMAT` the interface buffer must have: 26,
    /// `R11G11B10_FLOAT`. Strict, because the loading screen binds a
    /// window-sized `R8_UNORM`, and handing that to frame generation as the
    /// HUD-less colour ends the process.
    pub hudless_format: u32,
}

impl Config {
    /// `Default::default` cannot be called in a `const` initialiser, and the
    /// settings have to exist before the file is read.
    const DEFAULT: Config = Config {
        render_scale: crate::hijack::DEFAULT_RATIO,
        render_scale_asked: crate::hijack::DEFAULT_RATIO,
        mip_bias: 0,
        undither: false,
        dlss_preset: 0,
        fg: false,
        fg_multiplier: 2,
        fg_hudless: false,
        reflex_boost: false,
        fg_max_latency: 0,

        diag_unknown_build: false,
        diag_streamline_fault: false,
        log_verbose: false,
        perf: false,
        mv_probe: false,
        dred: false,
        debug_layer: false,
        hudless_survey: false,
        hudless_occurrence: 0,
        fg_mvec_space: 0,
        fg_mvec_jittered: false,
        fg_mvec_dilated: false,
        // Streamline's default. It was 2, verbose, while frame generation
        // was being brought up; a short session then wrote 700 KB of log.
        // Level 1 still carries every warning and error.
        fg_log_level: 1,
        resolve_at_pass: false,
        path_dump: false,
        fg_interpolate: true,
        fg_uir: false,
        fg_test_mask: 0,
        fg_test_mask_value: 1.0,
        fg_test_mask_box: 1.0,

        expert_jitter: true,
        expert_jitter_pixels: crate::native::JITTER_PIXELS,
        expert_jitter_phases: crate::native::JITTER_PHASES,
        expert_jitter_report: crate::gpu::JITTER_REPORT,
        expert_mv_dejitter: crate::gpu::MV_DEJITTER,
        expert_mv_scale: (1.0, 1.0),
        expert_dlss_hdr: true,
        expert_dlss_depth_inverted: true,
        expert_dlss_auto_exposure: true,
        expert_dlss_mv_jittered: false,
        expert_alpha_unroll: 0.0,

        mv_scale: (-1920.0, 1080.0),
        ngx_app_id: 0,
        fg_render_api: 1,
        fg_identity: 1,
        fg_engine: 0,
        fg_engine_version: packed("0.1.0"),
        fg_project_id: packed("4d9cd95c-41aa-4c46-a179-394f337b2d61"),
        fg_ota: true,
        hudless_slot: 0,
        hudless_format: 26,
    };
}

impl Config {
    /// These settings with every expert key as shipped.
    fn without_expert(self) -> Config {
        let shipped = Config::DEFAULT;
        Config {
            expert_jitter: shipped.expert_jitter,
            expert_jitter_pixels: shipped.expert_jitter_pixels,
            expert_jitter_phases: shipped.expert_jitter_phases,
            expert_jitter_report: shipped.expert_jitter_report,
            expert_mv_dejitter: shipped.expert_mv_dejitter,
            expert_mv_scale: shipped.expert_mv_scale,
            expert_dlss_hdr: shipped.expert_dlss_hdr,
            expert_dlss_depth_inverted: shipped.expert_dlss_depth_inverted,
            expert_dlss_auto_exposure: shipped.expert_dlss_auto_exposure,
            expert_dlss_mv_jittered: shipped.expert_dlss_mv_jittered,
            expert_alpha_unroll: shipped.expert_alpha_unroll,
            ..self
        }
    }

    /// Whether any expert key is set to something other than what ships.
    pub fn expert_changed(&self) -> bool {
        *self != self.without_expert()
    }
}

/// What the expert keys are held to, which is also where their sliders on
/// the settings page end.
pub const JITTER_PIXELS_RANGE: (f32, f32) = (0.0, 2.0);
pub const JITTER_PHASES_RANGE: (u32, u32) = (1, 256);
/// The multipliers and the dejitter: either sign, and up to twice over.
pub const SIGNED_RANGE: (f32, f32) = (-2.0, 2.0);
pub const ALPHA_UNROLL_RANGE: (f32, f32) = (0.0, 100_000.0);

/// The expert keys, each with what it is as shipped, as the file writes it.
/// This is what the settings page puts back.
pub const EXPERT: &[(&str, &str)] = &[
    ("expert_jitter", "1"),
    ("expert_jitter_pixels", "1.0"),
    ("expert_jitter_phases", "32"),
    ("expert_jitter_report_x", "1.0"),
    ("expert_jitter_report_y", "-1.0"),
    ("expert_mv_dejitter", "0.0"),
    ("expert_mv_scale_x", "1.0"),
    ("expert_mv_scale_y", "1.0"),
    ("expert_dlss_hdr", "1"),
    ("expert_dlss_depth_inverted", "1"),
    ("expert_dlss_auto_exposure", "1"),
    ("expert_dlss_mv_jittered", "0"),
    ("expert_alpha_unroll", "0.0"),
];

impl Default for Config {
    fn default() -> Self {
        Config::DEFAULT
    }
}

/// Every key the file may set. The tests hold the parser, this list and the
/// file the mod writes to one another.
#[cfg(test)]
const KEYS: &[&str] = &[
    "render_scale",
    "mip_bias",
    "undither",
    "dlss_preset",
    "fg",
    "fg_multiplier",
    "fg_hudless",
    "reflex_boost",
    "fg_max_latency",
    "diag_unknown_build",
    "diag_streamline_fault",
    "log_verbose",
    "perf",
    "mv_probe",
    "dred",
    "debug_layer",
    "hudless_survey",
    "hudless_occurrence",
    "fg_log_level",
    "resolve_at_pass",
    "path_dump",
    "fg_interpolate",
    "fg_uir",
    "fg_test_mask",
    "fg_test_mask_value",
    "fg_test_mask_box",
    "expert_jitter",
    "expert_jitter_pixels",
    "expert_jitter_phases",
    "expert_jitter_report_x",
    "expert_jitter_report_y",
    "expert_mv_dejitter",
    "expert_mv_scale_x",
    "expert_mv_scale_y",
    "expert_dlss_hdr",
    "expert_dlss_depth_inverted",
    "expert_dlss_auto_exposure",
    "expert_dlss_mv_jittered",
    "expert_alpha_unroll",
];

/// Keys that development builds read and this one does not, with what is
/// done instead.
///
/// The value named is the one in force, which is not in every case what the
/// key defaulted to. Where the two differ the old default is named, because
/// a file that relied on it is being read differently.
const RETIRED: &[(&str, &str)] = &[
    ("jitter", "fixed at 0: the external projection jitter is removed (old default 1)"),
    (
        "jitter_pixels",
        "expert_jitter_pixels is the key now, 1.0 as shipped (old default 0.5)",
    ),
    ("jitter_phases", "expert_jitter_phases is the key now, 32 as shipped"),
    ("jitter_report_x", "expert_jitter_report_x is the key now, 1.0 as shipped"),
    ("jitter_report_y", "expert_jitter_report_y is the key now, -1.0 as shipped"),
    ("jitter_inverses", "removed with the external projection jitter"),
    ("jitter_view", "removed with the external projection jitter"),
    ("jitter_always", "removed with the external projection jitter"),
    ("jitter_no_jitter_proj", "removed with the external projection jitter"),
    ("substitute", "removed with the external projection jitter"),
    ("native_jitter", "fixed at 1: the engine's own jitter slot is always used (old default 0)"),
    ("native_detour", "fixed at 1 (old default 0)"),
    ("native_spin", "fixed at 0: the diagnostic thread is removed"),
    ("native_fixed_x", "fixed at 0: the Halton sequence is always used"),
    ("native_fixed_y", "fixed at 0: the Halton sequence is always used"),
    (
        "sharpen",
        "removed: it switched on the engine's CAS, which the engine only runs when its DLSS upscale stage is not usable, and under this mod that stage always is",
    ),
    (
        "resolve_at",
        "removed: the resolve is at the engine's upscale stage, which is what 2 was; \
         resolve_at_pass = 1 is the old route, for comparison",
    ),
    ("dlss", "fixed at 1 (old default 0)"),
    ("dlss_quality", "fixed at -1, derived from render_scale (old default 5)"),
    (
        "dlss_flags",
        "0x49 as shipped; the four expert_dlss_* keys are its bits now",
    ),
    ("dlss_evaluate", "removed: nothing reached it"),
    ("dlss_spoof", "fixed at 1 (old default 0)"),
    ("game_dlss", "fixed at 1: the game's own DLSS path is the only path (old default 0)"),
    ("game_dlss_at_taa", "fixed at 1 (old default 0)"),
    ("game_dlss_taa", "fixed at 1"),
    ("game_dlss_resolve", "fixed at 1"),
    ("game_dlss_colour", "fixed at 0"),
    ("alpha_unroll", "expert_alpha_unroll is the key now, 0.0 as shipped"),
    ("mv_decode", "fixed at 1"),
    ("mv_mode", "fixed at 2"),
    ("mv_scale_x", "fixed at -1920.0; upscaling derives its own from the render size"),
    ("mv_scale_y", "fixed at 1080.0; upscaling derives its own from the render size"),
    (
        "mv_dejitter",
        "expert_mv_dejitter is the key now, 0.0 as shipped (old default 1.0)",
    ),
    ("mv_offset", "fixed at 13"),
    ("colour_offset", "fixed at 9"),
    ("depth_index", "fixed at 0"),
    ("colour_sanitise", "fixed at 0"),
    ("ngx_version", "fixed at 21"),
    ("ngx_app_id", "fixed at 0, the game's own"),
    ("swap_hooks", "fixed at 1; only consulted with fg = 0 (old default 0)"),
    ("reflex_low_latency", "nothing read it; frame generation forces Reflex on"),
    ("fg_render_api", "fixed at 1, eD3D12"),
    ("fg_identity", "fixed at 1, the engine/project route"),
    ("fg_engine", "fixed at 0, eCustom"),
    ("fg_engine_version", "fixed at 0.1.0"),
    ("fg_project_id", "fixed at this project's own GUID"),
    ("fg_ota", "fixed at 1, Streamline's own default"),
    ("hudless_slot", "fixed at 0; hudless_occurrence is the key"),
    ("hudless_format", "fixed at 26, R11G11B10_FLOAT"),
    ("drs_probe", "removed: the driver-profile probe has answered and is gone"),
    ("fg_mvec_space", "fixed at 0: the motion-vector scale is derived from the tagged buffer"),
    ("fg_mvec_jittered", "fixed at 0: the tagged vectors carry no jitter"),
    ("fg_mvec_dilated", "fixed at 0: the tagged vectors are not dilated"),
];

/// A line of the file that set nothing.
#[derive(Debug, Clone, PartialEq)]
struct Ignored {
    key: String,
    value: String,
    /// What a retired key was replaced by, or `None` for a key no build knew.
    fixed: Option<&'static str>,
}

/// Keys already named in the log, so each is said once a session and not
/// twice a second.
static REPORTED: parking_lot::Mutex<Vec<String>> = parking_lot::Mutex::new(Vec::new());

fn report_ignored(ignored: &[Ignored]) {
    let mut reported = REPORTED.lock();
    for line in ignored {
        if reported.contains(&line.key) {
            continue;
        }
        reported.push(line.key.clone());
        match line.fixed {
            Some(fixed) => crate::log!(
                "[cfg] `{} = {}` is ignored: {} is no longer a setting ({fixed})",
                line.key,
                line.value,
                line.key
            ),
            None => crate::log!(
                "[cfg] `{} = {}` is ignored: this build has no setting of that name",
                line.key,
                line.value
            ),
        }
    }
}

static CURRENT: RwLock<Config> = RwLock::new(Config::DEFAULT);
static PATH: RwLock<Option<PathBuf>> = RwLock::new(None);
static WATCHING: AtomicBool = AtomicBool::new(false);

/// Counts publications of `CURRENT`. Starts at one so that a thread's first
/// call, whose own copy is stamped zero, always fetches.
static GENERATION: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// This thread's copy of the settings, and the generation it was taken at.
    static LOCAL: Cell<(u64, Config)> = const { Cell::new((0, Config::DEFAULT)) };
}

/// The settings.
///
/// One relaxed load when nothing has changed since this thread last asked,
/// which is every call but one after each edit of the file. A read lock on
/// every call would be two atomic writes to a line every recording thread
/// shares, and this is called from hooks that run for every draw and every
/// constant buffer bind.
///
/// A thread that loads the old generation just as a new file is published
/// answers with the old settings once more and catches up on its next call.
/// The lock gave no stronger promise: a caller that had just released it was
/// equally one publication behind.
pub fn get() -> Config {
    let generation = GENERATION.load(Ordering::Acquire);
    LOCAL.with(|local| {
        let (seen, config) = local.get();
        if seen == generation {
            return config;
        }
        // Stamped with the generation read *before* the copy. If a
        // publication lands in between, the stamp is the older one and the
        // next call fetches again, so a stale copy can never be kept under a
        // current stamp.
        let fresh = *CURRENT.read();
        local.set((generation, fresh));
        fresh
    })
}

/// `text` with `key` set to `value`, every other byte as it was.
///
/// Every line that sets the key is rewritten, because the parser lets a later
/// line override an earlier one and a rewrite of only the first would be
/// undone by the second. Comments, order and line endings are kept. A key the
/// file does not have is appended.
pub fn with_key(text: &str, key: &str, value: &str) -> String {
    let mut out = String::with_capacity(text.len() + key.len() + value.len() + 8);
    let mut found = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        let trimmed = body.trim_start();
        let sets_it = !trimmed.starts_with('#')
            && !trimmed.starts_with(';')
            && trimmed
                .split_once('=')
                .is_some_and(|(name, _)| name.trim() == key);
        if sets_it {
            found = true;
            out.push_str(key);
            out.push_str(" = ");
            out.push_str(value);
            out.push_str(ending);
        } else {
            out.push_str(line);
        }
    }
    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(key);
        out.push_str(" = ");
        out.push_str(value);
        out.push('\n');
    }
    out
}

/// Set one key in the settings file, for the watcher to pick up.
///
/// This is the only way anything in this library changes a setting, and it
/// goes through the file on purpose: the file stays the one configuration,
/// and whatever wrote the line -- the user's editor or the settings page in
/// ReShade's overlay -- the value takes the same path in.
///
/// Written beside the file and moved over it, so the watcher never reads a
/// half-written one; it keeps its previous values if it catches the moment
/// in between.
pub fn write_key(key: &str, value: &str) -> std::io::Result<()> {
    write_keys(&[(key, value)])
}

/// Set several keys in the settings file in one write, so the watcher sees
/// them together.
pub fn write_keys(pairs: &[(&str, &str)]) -> std::io::Result<()> {
    let Some(path) = PATH.read().clone() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no settings file is being watched",
        ));
    };
    let text = std::fs::read_to_string(&path)?;
    let updated = pairs.iter().fold(text.clone(), |text, (key, value)| {
        with_key(&text, key, value)
    });
    if updated == text {
        return Ok(());
    }
    let beside = path.with_extension("ini.writing");
    std::fs::write(&beside, updated)?;
    std::fs::rename(&beside, &path)
}

/// Start watching the file, after loading it once so startup sees real values.
pub fn set_path(path: PathBuf) {
    crate::log!("[cfg] watching {}", path.display());
    *PATH.write() = Some(path);
    reload();

    if WATCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(POLL);
        reload();
    });
}

/// Read the file and publish it, leaving the previous values in place if it
/// cannot be read -- a locked or half-written file should not reset the
/// settings out from under a running frame.
fn reload() {
    let Some(path) = PATH.read().clone() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let (parsed, ignored) = parse_reporting(&text);
    // Before the early return below: a file that only sets retired keys
    // parses to the defaults and would otherwise never be mentioned.
    report_ignored(&ignored);

    if *CURRENT.read() == parsed {
        return;
    }
    *CURRENT.write() = parsed;
    // After the value, so that a reader woken by the new number finds it.
    GENERATION.fetch_add(1, Ordering::Release);
    // The logger keeps its own copy, because deciding whether to log must not
    // go back through the settings.
    crate::logger::set_verbose(parsed.log_verbose);
    // And so does the draw hook, which runs thousands of times a frame and
    // must not take a lock to find out it has nothing to do.
    crate::hudless::follow(&parsed);
    crate::pathdump::follow(&parsed);
    crate::hijack::note_configured(parsed.render_scale_asked, parsed.render_scale);
    // And the time accounting, for the same reason.
    crate::perf::follow(parsed.perf);
    // What the file now says, in the file's own terms. The values the mod
    // works from that are not in the file are constants and are not repeated
    // here; each module logs what it observed.
    crate::log!(
        "[cfg] render_scale={} mip_bias={} undither={} preset={} | fg={} x{} \
         hudless={} occurrence={} survey={} boost={} latency={} mvec=({},{},{}) | \
         verbose={} perf={} mv_probe={} dred={} debug_layer={} sl_log={}",
        parsed.render_scale,
        parsed.mip_bias,
        parsed.undither,
        parsed.dlss_preset,
        parsed.fg,
        parsed.fg_multiplier,
        parsed.fg_hudless,
        parsed.hudless_occurrence,
        parsed.hudless_survey,
        parsed.reflex_boost,
        parsed.fg_max_latency,
        parsed.fg_mvec_space,
        parsed.fg_mvec_jittered,
        parsed.fg_mvec_dilated,
        parsed.log_verbose,
        parsed.perf,
        parsed.mv_probe,
        parsed.dred,
        parsed.debug_layer,
        parsed.fg_log_level
    );
    // Only when one of them is not what ships: a picture judged with an
    // expert key moved should say so in the log it is reported with.
    if parsed.expert_changed() {
        crate::log!(
            "[cfg] expert keys are not all as shipped: jitter={} pixels={} phases={} \
             report=({},{}) | mv dejitter={} scale=({},{}) | dlss hdr={} depth_inverted={} \
             auto_exposure={} mv_jittered={} | alpha_unroll={}",
            parsed.expert_jitter,
            parsed.expert_jitter_pixels,
            parsed.expert_jitter_phases,
            parsed.expert_jitter_report.0,
            parsed.expert_jitter_report.1,
            parsed.expert_mv_dejitter,
            parsed.expert_mv_scale.0,
            parsed.expert_mv_scale.1,
            parsed.expert_dlss_hdr,
            parsed.expert_dlss_depth_inverted,
            parsed.expert_dlss_auto_exposure,
            parsed.expert_dlss_mv_jittered,
            parsed.expert_alpha_unroll
        );
    }
}

fn parse_reporting(text: &str) -> (Config, Vec<Ignored>) {
    let mut ignored = Vec::new();
    let mut config = Config::default();
    let flag = |value: &str| matches!(value, "1" | "true" | "on" | "yes");
    // A number held to a range; anything that is not a number sets nothing.
    let ranged = |value: &str, (low, high): (f32, f32)| {
        value
            .parse::<f32>()
            .ok()
            .filter(|parsed| parsed.is_finite())
            .map(|parsed| parsed.clamp(low, high))
    };

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "render_scale" => {
                if let Ok(parsed) = value.parse::<f32>() {
                    config.render_scale_asked = parsed;
                    config.render_scale = crate::hijack::ratio_from_file(parsed);
                }
            }
            "mip_bias" => {
                if let Ok(parsed) = value.parse::<u32>() {
                    config.mip_bias = parsed.min(14);
                }
            }
            "undither" => config.undither = flag(value),
            "dlss_preset" => {
                if let Ok(parsed) = value.parse() {
                    config.dlss_preset = parsed;
                }
            }
            "fg" => config.fg = flag(value),
            "fg_multiplier" => {
                if let Ok(parsed) = value.parse::<u32>() {
                    config.fg_multiplier = parsed.clamp(2, 4);
                }
            }
            "fg_hudless" => config.fg_hudless = flag(value),
            "reflex_boost" => config.reflex_boost = flag(value),
            "fg_max_latency" => {
                if let Ok(parsed) = value.parse::<u32>() {
                    config.fg_max_latency = parsed.min(16);
                }
            }
            "diag_unknown_build" => config.diag_unknown_build = flag(value),
            "diag_streamline_fault" => config.diag_streamline_fault = flag(value),
            "log_verbose" => config.log_verbose = flag(value),
            "perf" => config.perf = flag(value),
            "mv_probe" => config.mv_probe = flag(value),
            "dred" => config.dred = flag(value),
            "debug_layer" => config.debug_layer = flag(value),
            "hudless_survey" => config.hudless_survey = flag(value),
            "hudless_occurrence" => {
                if let Ok(parsed) = value.parse() {
                    config.hudless_occurrence = parsed;
                }
            }
            "fg_log_level" => {
                if let Ok(parsed) = value.parse::<u32>() {
                    config.fg_log_level = parsed.min(2);
                }
            }
            "resolve_at_pass" => config.resolve_at_pass = flag(value),
            "path_dump" => config.path_dump = flag(value),
            "fg_interpolate" => config.fg_interpolate = flag(value),
            "fg_uir" => config.fg_uir = flag(value),
            "fg_test_mask" => {
                if let Ok(parsed) = value.parse::<u32>() {
                    config.fg_test_mask = parsed.min(2);
                }
            }
            "fg_test_mask_value" => {
                if let Ok(parsed) = value.parse::<f32>() {
                    if parsed.is_finite() {
                        config.fg_test_mask_value = parsed.clamp(0.0, 1.0);
                    }
                }
            }
            "fg_test_mask_box" => {
                if let Ok(parsed) = value.parse::<f32>() {
                    if parsed.is_finite() {
                        config.fg_test_mask_box = parsed.clamp(0.0, 1.0);
                    }
                }
            }
            "expert_jitter" => config.expert_jitter = flag(value),
            "expert_jitter_pixels" => {
                if let Some(parsed) = ranged(value, JITTER_PIXELS_RANGE) {
                    config.expert_jitter_pixels = parsed;
                }
            }
            "expert_jitter_phases" => {
                if let Ok(parsed) = value.parse::<u32>() {
                    config.expert_jitter_phases =
                        parsed.clamp(JITTER_PHASES_RANGE.0, JITTER_PHASES_RANGE.1);
                }
            }
            "expert_jitter_report_x" => {
                if let Some(parsed) = ranged(value, SIGNED_RANGE) {
                    config.expert_jitter_report.0 = parsed;
                }
            }
            "expert_jitter_report_y" => {
                if let Some(parsed) = ranged(value, SIGNED_RANGE) {
                    config.expert_jitter_report.1 = parsed;
                }
            }
            "expert_mv_dejitter" => {
                if let Some(parsed) = ranged(value, SIGNED_RANGE) {
                    config.expert_mv_dejitter = parsed;
                }
            }
            "expert_mv_scale_x" => {
                if let Some(parsed) = ranged(value, SIGNED_RANGE) {
                    config.expert_mv_scale.0 = parsed;
                }
            }
            "expert_mv_scale_y" => {
                if let Some(parsed) = ranged(value, SIGNED_RANGE) {
                    config.expert_mv_scale.1 = parsed;
                }
            }
            "expert_dlss_hdr" => config.expert_dlss_hdr = flag(value),
            "expert_dlss_depth_inverted" => config.expert_dlss_depth_inverted = flag(value),
            "expert_dlss_auto_exposure" => config.expert_dlss_auto_exposure = flag(value),
            "expert_dlss_mv_jittered" => config.expert_dlss_mv_jittered = flag(value),
            "expert_alpha_unroll" => {
                if let Some(parsed) = ranged(value, ALPHA_UNROLL_RANGE) {
                    config.expert_alpha_unroll = parsed;
                }
            }
            _ => ignored.push(Ignored {
                key: key.to_string(),
                value: value.to_string(),
                fixed: RETIRED
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, fixed)| *fixed),
            }),
        }
    }
    (config, ignored)
}

/// The file written on first run.
const SHIPPED: &str = "\
# mhw-dlss settings.
#
# The file is read while the game runs: a change takes effect within a
# second, except where a key says it is read when the game starts. The
# settings page in ReShade's overlay, if you use ReShade, edits this file.
# A key the mod does not know is ignored and named once in mhw-dlss.log.

# ---------------------------------------------------------------------------
# Picture
# ---------------------------------------------------------------------------

# render_scale  the fraction of the window resolution the game renders at;
#               DLSS reconstructs the rest. 0.50 to 1.00. 1.0 is DLAA,
#               0.667 Quality, 0.58 Balanced, 0.5 Performance. The game's own
#               Image Quality option does nothing while the mod is loaded.
render_scale = 0.667

# mip_bias      1 sharpens texture sampling to suit the render scale, 0
#               leaves it as the game has it. Read when the game starts, for
#               the render_scale set then: restart the game after changing
#               either for the bias to follow.
mip_bias = 0

# undither      1 stops screen-space reflections and contact shadows using
#               noise patterns that only the game's own anti-aliasing can
#               clean up. Costs a little banding. Turning it back off needs
#               a restart.
undither = 0

# dlss_preset   0 lets the driver choose the DLSS model, which is right
#               unless you are comparing them. 10 is preset J, 11 K, 12 L,
#               13 M; other numbers are ignored.
dlss_preset = 0

# ---------------------------------------------------------------------------
# Frame generation
# ---------------------------------------------------------------------------
#
# Needs NVIDIA's files in this folder: double-click fetch-nvidia.cmd here.

# fg              1 loads frame generation. Read when the game starts.
fg = 0

# fg_interpolate  generate frames, in a session started with fg = 1. Set it
#                 to 0 to switch generated frames off while playing.
fg_interpolate = 1

# fg_multiplier   frames shown per rendered frame: 2, 3 or 4.
fg_multiplier = 2

# reflex_boost    1 asks NVIDIA Reflex to keep the CPU and GPU clocks up as
#                 well, at the cost of power. Reflex itself is always on with
#                 frame generation.
reflex_boost = 0

# fg_hudless      1 gives frame generation the picture as it was before the
#                 interface was drawn, so the interface is not interpolated.
fg_hudless = 0

# fg_max_latency  limits the number of frames the game may queue: 1 to 16.
#                 0 leaves the game's own value, which is 3.
fg_max_latency = 0

# ---------------------------------------------------------------------------
# Diagnostics. All off. None is needed to play; they are for bug reports.
# ---------------------------------------------------------------------------

# log_verbose   write per-frame and per-resource detail to the log. F10,
#               pressed in the game window, writes one report of the
#               temporal pass with or without this.
# perf          log the time spent inside the mod's own hooks every 600
#               rendered frames, on lines that start [perf].
# path_dump     log what the game and the mod do about render resolution,
#               on lines that start [path], whenever it changes.
# mv_probe      sample the motion vectors and log the readings.
# fg_log_level  how much of NVIDIA Streamline's own log is copied into the
#               mod's log: 0 nothing, 1 the usual, 2 everything.
log_verbose = 0
perf = 0
path_dump = 0
mv_probe = 0
fg_log_level = 1

# dred          ask the driver for GPU crash data, read back if the device
#               is lost. Costs performance. Read when the game starts.
# debug_layer   turn on Direct3D 12 validation and log what it reports.
#               Costs a lot of performance and needs Windows' Graphics Tools
#               feature. Read when the game starts.
dred = 0
debug_layer = 0

# diag_unknown_build  1 makes the mod behave as it does after a game update
#                     it does not know: it changes nothing in the game and
#                     says so on a [bld] line. Read when the game starts.
# diag_streamline_fault  1, with fg = 1, makes the mod behave as it does
#                     when NVIDIA Streamline fails while starting: no frame
#                     generation. Read when the game starts.
# resolve_at_pass     1 runs DLSS at an earlier point of the frame. The
#                     picture is paler below render_scale 1.0; for
#                     comparison only.
diag_unknown_build = 0
diag_streamline_fault = 0
resolve_at_pass = 0

# hudless_survey      log every render target the game draws into after
#                     upscaling, to find where the interface begins.
# hudless_occurrence  which of those to keep for fg_hudless, counting from
#                     1. 0 uses the measured answer, 2. Any other value
#                     copies a frame every frame even with fg_hudless off.
hudless_survey = 0
hudless_occurrence = 0

# ---------------------------------------------------------------------------
# Experiments on one known artefact of frame generation: labels fixed in the
# world, such as the name over a character, smear when the camera turns. As
# written here they change nothing.
# ---------------------------------------------------------------------------

# fg_test_mask        0 nothing. 1 gives frame generation a mask of pixels
#                     not to warp; 2 gives the same mask as interface
#                     opacity.
# fg_test_mask_value  what the mask holds inside its box, 0 to 1.
# fg_test_mask_box    the fraction of the screen, centred, the box covers.
# fg_uir              1 asks frame generation to redraw the interface itself.
#                     Only means something with fg_hudless = 1 and
#                     fg_test_mask = 2.
fg_test_mask = 0
fg_test_mask_value = 1.0
fg_test_mask_box = 1.0
fg_uir = 0

# ---------------------------------------------------------------------------
# Expert. Values the mod otherwise holds at the one that was measured for
# this game. As written here they change nothing. They are for judging the
# picture against a different value while the game runs: every one applies
# within a second, and a wrong one gives a wrong picture, not a crash. The
# settings page in ReShade's overlay has them under Expert settings, with a
# button that puts them back.
# ---------------------------------------------------------------------------

# expert_jitter           0 stops the mod moving the camera by a fraction of
#                         a pixel each frame, which is what DLSS builds
#                         detail from. For comparison.
# expert_jitter_pixels    how many render pixels the samples are spread
#                         across: 0.0 to 2.0. Under 1 is steadier and softer.
# expert_jitter_phases    how many frames before the pattern repeats: 1 to
#                         256. NVIDIA's guide gives 8 divided by the square
#                         of render_scale: 8 at 1.0, 18 at 0.667, 32 at 0.5.
# expert_jitter_report_x  multipliers on the offset DLSS is told the camera
# expert_jitter_report_y  had: -2.0 to 2.0. The wrong sign on an axis blurs
#                         and shimmers along that axis.
expert_jitter = 1
expert_jitter_pixels = 1.0
expert_jitter_phases = 32
expert_jitter_report_x = 1.0
expert_jitter_report_y = -1.0

# expert_mv_dejitter  how much of the jitter is added back to the motion
#                     vectors: -2.0 to 2.0. The game's vectors carry none,
#                     so 0.
# expert_mv_scale_x   multipliers on the motion vectors DLSS and frame
# expert_mv_scale_y   generation are given: -2.0 to 2.0. Too long or too
#                     short smears or ghosts whatever moves.
expert_mv_dejitter = 0.0
expert_mv_scale_x = 1.0
expert_mv_scale_y = 1.0

# expert_dlss_hdr             what DLSS is told about its inputs. The colour
# expert_dlss_depth_inverted  is HDR, the depth buffer has near at 1, DLSS
# expert_dlss_auto_exposure   works out the exposure itself, and the motion
# expert_dlss_mv_jittered     vectors carry no jitter. Changing one rebuilds
#                             DLSS, which shows as a single rough frame.
expert_dlss_hdr = 1
expert_dlss_depth_inverted = 1
expert_dlss_auto_exposure = 1
expert_dlss_mv_jittered = 0

# expert_alpha_unroll  the game draws transparency meant for its own
#                      anti-aliasing as a checkerboard between two
#                      distances, and the mod holds both at the farther one.
#                      A number above 0 holds both at that distance instead,
#                      up to 100000; 0 leaves them as the mod has them.
expert_alpha_unroll = 0.0
";

/// Write the file on first run, so the available settings are discoverable.
pub fn write_default(path: &std::path::Path) {
    if path.exists() {
        return;
    }
    let _ = std::fs::write(path, SHIPPED);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file the mod writes changes nothing: it parses to the defaults,
    /// and it contains no line the mod then ignores.
    ///
    /// Which is also the promise that frame generation is off until asked
    /// for, and that no diagnostic is on -- asserted by name below so a
    /// default that drifts fails here rather than in somebody's game.
    #[test]
    fn the_shipped_file_is_the_defaults() {
        let (parsed, ignored) = parse_reporting(SHIPPED);
        assert!(
            ignored.is_empty(),
            "the shipped file sets keys nothing reads: {ignored:?}"
        );
        assert!(
            parsed == Config::default(),
            "the shipped file disagrees with the defaults"
        );

        assert!(!parsed.fg);
        assert!(!parsed.fg_hudless);
        assert!(!parsed.reflex_boost);
        assert!(!parsed.diag_unknown_build);
        assert!(!parsed.diag_streamline_fault);
        assert!(!parsed.log_verbose);
        assert!(!parsed.perf);
        assert!(!parsed.mv_probe);
        assert!(!parsed.dred);
        assert!(!parsed.debug_layer);
        assert!(!parsed.hudless_survey);
        assert_eq!(parsed.hudless_occurrence, 0);
        assert_eq!(parsed.fg_multiplier, 2);
        assert_eq!(parsed.fg_max_latency, 0);
        assert_eq!(parsed.fg_mvec_space, 0);
        assert!(!parsed.resolve_at_pass);
        assert!(!parsed.path_dump);
        assert_eq!(parsed.render_scale, crate::hijack::DEFAULT_RATIO);
        assert_eq!(parsed.fg_log_level, 1);
        assert_eq!(parsed.dlss_preset, 0);
        assert!(!parsed.fg_mvec_jittered);
        assert!(!parsed.fg_mvec_dilated);
        // The five frame generation experiments, which as shipped change
        // nothing: generation on, no mask, no recomposition.
        assert!(parsed.fg_interpolate);
        assert!(!parsed.fg_uir);
        assert_eq!(parsed.fg_test_mask, 0);
        assert_eq!(parsed.fg_test_mask_value, 1.0);
        assert_eq!(parsed.fg_test_mask_box, 1.0);
        // The expert keys, which as shipped are the values the mod was
        // verified with.
        assert!(!parsed.expert_changed());
        assert!(parsed.expert_jitter);
        assert_eq!(parsed.expert_jitter_pixels, 1.0);
        assert_eq!(parsed.expert_jitter_phases, 32);
        assert!(parsed.expert_jitter_report == (1.0, -1.0));
        assert_eq!(parsed.expert_mv_dejitter, 0.0);
        assert!(parsed.expert_mv_scale == (1.0, 1.0));
        assert!(parsed.expert_dlss_hdr);
        assert!(parsed.expert_dlss_depth_inverted);
        assert!(parsed.expert_dlss_auto_exposure);
        assert!(!parsed.expert_dlss_mv_jittered);
        assert_eq!(parsed.expert_alpha_unroll, 0.0);
        // The values other modules read from here that are not settings.
        assert_eq!(parsed.hudless_format, 26);
        assert_eq!(parsed.hudless_slot, 0);
        assert_eq!(parsed.fg_render_api, 1);
        assert_eq!(parsed.fg_identity, 1);
        assert!(parsed.fg_ota);
        assert_eq!(parsed.ngx_app_id, 0);
        assert!(parsed.mv_scale == (-1920.0, 1080.0));
    }

    /// The parser, the list of keys and the shipped file name the same set.
    #[test]
    fn the_parser_and_the_shipped_file_agree() {
        // Every key is in the file exactly once, as a setting and not only in
        // a comment.
        for key in KEYS {
            let lines = SHIPPED
                .lines()
                .filter(|line| !line.starts_with('#'))
                .filter(|line| {
                    line.split_once('=')
                        .is_some_and(|(name, _)| name.trim() == *key)
                })
                .count();
            assert_eq!(lines, 1, "{key} appears {lines} times in the shipped file");
        }
        // Every setting in the file is a key.
        for line in SHIPPED.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let (name, _) = line
                .split_once('=')
                .expect("a line that is neither comment nor key");
            assert!(
                KEYS.contains(&name.trim()),
                "{} is in the file and not in KEYS",
                name.trim()
            );
        }
        // Every key is one the parser takes, and none is also retired.
        for key in KEYS {
            let (_, ignored) = parse_reporting(&format!("{key} = 1\n"));
            assert!(ignored.is_empty(), "the parser does not know {key}");
            assert!(
                !RETIRED.iter().any(|(name, _)| name == key),
                "{key} is live and retired"
            );
        }
    }

    /// The list the settings page puts back is every expert key, at the
    /// value the shipped file gives it, and putting it back over any file
    /// leaves no expert key changed.
    #[test]
    fn the_expert_list_is_the_shipped_expert_keys() {
        let named: Vec<&str> = KEYS
            .iter()
            .copied()
            .filter(|key| key.starts_with("expert_"))
            .collect();
        let listed: Vec<&str> = EXPERT.iter().map(|(key, _)| *key).collect();
        assert_eq!(named, listed);
        for (key, value) in EXPERT {
            assert!(
                SHIPPED
                    .lines()
                    .any(|line| line == format!("{key} = {value}")),
                "the shipped file does not have `{key} = {value}`"
            );
        }

        let moved = "expert_jitter = 0\nexpert_jitter_pixels = 0.5\nexpert_jitter_phases = 8\n\
                     expert_jitter_report_x = -1\nexpert_jitter_report_y = 1\n\
                     expert_mv_dejitter = 1\nexpert_mv_scale_x = -1\nexpert_mv_scale_y = 0.5\n\
                     expert_dlss_hdr = 0\nexpert_dlss_depth_inverted = 0\n\
                     expert_dlss_auto_exposure = 0\nexpert_dlss_mv_jittered = 1\n\
                     expert_alpha_unroll = 500\nrender_scale = 0.5\n";
        let (parsed, ignored) = parse_reporting(moved);
        assert!(ignored.is_empty());
        assert!(parsed.expert_changed());
        assert!(!parsed.expert_jitter);
        assert_eq!(parsed.expert_jitter_pixels, 0.5);
        assert_eq!(parsed.expert_jitter_phases, 8);
        assert!(parsed.expert_jitter_report == (-1.0, 1.0));
        assert_eq!(parsed.expert_mv_dejitter, 1.0);
        assert!(parsed.expert_mv_scale == (-1.0, 0.5));
        assert!(!parsed.expert_dlss_hdr);
        assert!(!parsed.expert_dlss_depth_inverted);
        assert!(!parsed.expert_dlss_auto_exposure);
        assert!(parsed.expert_dlss_mv_jittered);
        assert_eq!(parsed.expert_alpha_unroll, 500.0);

        let restored = EXPERT.iter().fold(moved.to_string(), |text, (key, value)| {
            with_key(&text, key, value)
        });
        let (parsed, _) = parse_reporting(&restored);
        assert!(!parsed.expert_changed());
        // And nothing else was touched.
        assert_eq!(parsed.render_scale, 0.5);
    }

    /// Out of range is clamped and nonsense sets nothing, so no value of an
    /// expert key reaches the code that divides by it or loops over it.
    #[test]
    fn expert_keys_are_held_to_their_ranges() {
        let (parsed, _) = parse_reporting(
            "expert_jitter_phases = 0\nexpert_jitter_pixels = 9\nexpert_mv_scale_x = -7\n\
             expert_alpha_unroll = -3\nexpert_mv_dejitter = nan\nexpert_jitter_report_x = inf\n",
        );
        assert_eq!(parsed.expert_jitter_phases, 1);
        assert_eq!(parsed.expert_jitter_pixels, 2.0);
        assert_eq!(parsed.expert_mv_scale.0, -2.0);
        assert_eq!(parsed.expert_alpha_unroll, 0.0);
        assert_eq!(parsed.expert_mv_dejitter, 0.0);
        assert_eq!(parsed.expert_jitter_report.0, 1.0);
        let (parsed, _) = parse_reporting("expert_jitter_phases = 100000\n");
        assert_eq!(parsed.expert_jitter_phases, 256);
    }

    /// The keys three branches added or retired side by side, in one file:
    /// the resolve path's (`resolve_at_pass`, `path_dump`, and `sharpen` and
    /// `resolve_at` retired), and the hardening's (`diag_unknown_build`,
    /// `fg_log_level`, `dlss_preset`).
    #[test]
    fn the_keys_of_every_branch_are_taken_together() {
        let text = "render_scale = 0\nresolve_at_pass = 1\npath_dump = 1\n\
                    diag_unknown_build = 1\nfg_log_level = 2\ndlss_preset = 11\n\
                    sharpen = 0.3\nresolve_at = 2\n";
        let (parsed, ignored) = parse_reporting(text);
        assert!(parsed.resolve_at_pass);
        assert!(parsed.path_dump);
        assert!(parsed.diag_unknown_build);
        assert_eq!(parsed.fg_log_level, 2);
        assert_eq!(parsed.dlss_preset, 11);
        // 0 is not a ratio and is read as the default.
        assert_eq!(parsed.render_scale, crate::hijack::DEFAULT_RATIO);
        assert_eq!(parsed.render_scale_asked, 0.0);
        let names: Vec<&str> = ignored.iter().map(|line| line.key.as_str()).collect();
        assert_eq!(names, ["sharpen", "resolve_at"]);
        for key in ["sharpen", "resolve_at"] {
            assert!(
                RETIRED.iter().any(|(name, _)| *name == key),
                "{key} is not listed as retired"
            );
            assert!(!KEYS.contains(&key), "{key} is retired and still a key");
        }
        for key in [
            "resolve_at_pass",
            "path_dump",
            "diag_unknown_build",
            "fg_log_level",
        ] {
            assert!(KEYS.contains(&key), "{key} is not a key");
        }
        // Out of range is clamped, not refused.
        let (parsed, _) = parse_reporting("render_scale = 0.3\n");
        assert_eq!(parsed.render_scale, crate::hijack::SMALLEST);
        let (parsed, _) = parse_reporting("render_scale = 1.7\n");
        assert_eq!(parsed.render_scale, crate::hijack::LARGEST);
    }

    /// `get` hands every thread the published settings, and a thread that has
    /// already asked sees a later publication on its next call.
    #[test]
    fn get_follows_a_publication_on_every_thread() {
        let mut wanted = Config::default();
        wanted.mip_bias = 3;
        let before = get();
        *CURRENT.write() = wanted;
        GENERATION.fetch_add(1, Ordering::Release);
        assert!(get() == wanted, "this thread kept its old copy");
        let other = std::thread::spawn(get).join().expect("the thread ran");
        assert!(other == wanted, "a new thread did not fetch");
        *CURRENT.write() = before;
        GENERATION.fetch_add(1, Ordering::Release);
        assert!(get() == before);
    }

    /// A key from an older ini is ignored and named, never an error, and it
    /// changes nothing; a key no build ever had is named too.
    #[test]
    fn retired_and_unknown_keys_are_ignored_and_named() {
        let (parsed, ignored) = parse_reporting("fg = 1\nno_such_key = 7\n");
        assert!(parsed.fg);
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].key, "no_such_key");
        assert_eq!(ignored[0].value, "7");
        assert!(ignored[0].fixed.is_none());

        for (key, _) in RETIRED {
            let (parsed, ignored) = parse_reporting(&format!("{key} = 1\n"));
            assert!(parsed == Config::default(), "{key} still changes a setting");
            assert_eq!(ignored.len(), 1, "{key} was not reported");
            assert!(ignored[0].fixed.is_some(), "{key} was reported as unknown");
        }
    }

    /// An ini from a development build, key for key. It must still load: the
    /// keys that remain mean what they meant, every other line is ignored
    /// and accounted for, and the four that were dropped before the list of
    /// retired keys existed are reported as unknown rather than mistaken for
    /// retired ones.
    #[test]
    fn the_deployed_file_still_loads() {
        let deployed = "\
jitter = 0
jitter_pixels = 1.0
jitter_phases = 32
dump_velocity = 0
ngx_probe = 1
ngx_version = 21
ngx_app_id = 0
dlss = 1
dlss_quality = -1
dlss_flags = 0x49
mv_scale_x = -1920.0
mv_scale_y = 1080.0
mv_offset = 13
dlss_subrects = 1
depth_offset = -1
mv_decode = 1
mv_mode = 2
jitter_report_x = 1.0
jitter_report_y = -1.0
dred = 0
dlss_evaluate = 0
mv_dejitter = 0.0
mv_probe = 0
log_verbose = 1
dlss_preset = 0
jitter_view = 0.0
colour_sanitise = 0
debug_layer = 0
depth_index = 0
colour_offset = 9
jitter_inverses = 5
substitute = 0
jitter_always = 1
jitter_no_jitter_proj = 0
native_jitter = 1
native_fixed_x = 0.0
native_fixed_y = 0.0
native_detour = 1
native_spin = 0
render_scale = 0.667
dlss_spoof = 1
game_dlss = 1
game_dlss_colour = 0
game_dlss_taa = 1
game_dlss_resolve = 1
game_dlss_at_taa = 1
alpha_unroll = 0.0
mip_bias = 1
sharpen = 0.0
undither = 0
swap_hooks = 1
fg = 1
fg_multiplier = 2
drs_probe = 1
fg_hudless = 0
hudless_survey = 0
hudless_occurrence = 2
";
        let (parsed, ignored) = parse_reporting(deployed);

        let mut expected = Config::default();
        expected.render_scale = 0.667;
        expected.mip_bias = 1;
        expected.log_verbose = true;
        expected.fg = true;
        expected.hudless_occurrence = 2;
        assert!(
            parsed == expected,
            "the deployed file no longer means what it meant"
        );

        let unknown: Vec<&str> = ignored
            .iter()
            .filter(|line| line.fixed.is_none())
            .map(|line| line.key.as_str())
            .collect();
        assert_eq!(
            unknown,
            [
                "dump_velocity",
                "ngx_probe",
                "dlss_subrects",
                "depth_offset"
            ]
        );
        // 57 lines: 13 that are still settings, 4 unknown, 40 retired.
        assert_eq!(deployed.lines().count(), 57);
        assert_eq!(ignored.len(), 44);
    }

    /// One key rewritten, nothing else touched: comments, order, the other
    /// keys and both kinds of line ending survive byte for byte.
    #[test]
    fn with_key_changes_one_value_and_nothing_else() {
        let text =
            "# sharpen  how much\r\nsharpen = 0.0\r\n; fg = 9\r\nfg=1\n  fg_multiplier   =  2\n";
        assert_eq!(
            with_key(text, "sharpen", "0.35"),
            "# sharpen  how much\r\nsharpen = 0.35\r\n; fg = 9\r\nfg=1\n  fg_multiplier   =  2\n"
        );
        assert_eq!(
            with_key(text, "fg_multiplier", "4"),
            "# sharpen  how much\r\nsharpen = 0.0\r\n; fg = 9\r\nfg=1\nfg_multiplier = 4\n"
        );
        // `fg` must not match `fg_multiplier`, nor the commented line.
        assert_eq!(with_key(text, "fg", "0").matches("fg = 0").count(), 1);
        assert!(with_key(text, "fg", "0").contains("; fg = 9"));
        assert!(with_key(text, "fg", "0").contains("fg_multiplier   =  2"));
    }

    #[test]
    fn with_key_rewrites_every_line_that_sets_it_and_appends_a_missing_one() {
        assert_eq!(with_key("a = 1\na = 2\n", "a", "3"), "a = 3\na = 3\n");
        assert_eq!(with_key("a = 1", "b", "2"), "a = 1\nb = 2\n");
        assert_eq!(with_key("", "b", "2"), "b = 2\n");
        // Unchanged when the value is already there, so nothing is written.
        assert_eq!(with_key("b = 2\n", "b", "2"), "b = 2\n");
    }

    /// What the page writes parses back to what it meant.
    #[test]
    fn a_written_key_parses_to_the_value() {
        let text = with_key(SHIPPED, "render_scale", "0.580");
        let text = with_key(&text, "reflex_boost", "1");
        let (parsed, _) = parse_reporting(&text);
        assert_eq!(parsed.render_scale, 0.58);
        assert!(parsed.reflex_boost);
    }
}
