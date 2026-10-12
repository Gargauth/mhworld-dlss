//! DLSS-G and Reflex options: what the mod asks for, what is sent, and when.
//!
//! The mod sets [`Options`] (policy, live). Each present, the glue derives
//! the [`DlssgRequest`] and [`ReflexRequest`] from them and the frame's
//! [`Conditions`], and calls `slDLSSGSetOptions`/`slReflexSetOptions` only
//! when the request differs from the last one accepted ([`Sent`]):
//! Streamline applies DLSS-G options at the next `Present`, wants them set
//! from the presenting thread, and has no use for a call per frame.
//!
//! On/off is live: `eOff` with `eRetainResourcesWhenOff` suspends
//! generation without freeing its memory. Loading the plugin (and with it
//! the proxy swapchain) is not: that is decided once, at swapchain
//! creation, by `[FrameGeneration] Enabled` (restart).

use mhw_core::config::FrameGeneration;
use mhw_core::frame::Size;

use crate::abi;
use crate::pacing::{frame_limit_us, FrameLimit, SleepPoint};

/// What the mod asks of frame generation. Live.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    /// Generate frames (`eOn`) or not (`eOff`, resources retained).
    pub enabled: bool,
    /// Frames shown per rendered frame, 2–4 (clamped; further limited by
    /// what the system reports it can generate).
    pub multiplier: u32,
    /// Reflex low latency with boost instead of without.
    pub reflex_boost: bool,
    /// Optional Reflex frame limit.
    pub frame_limit: FrameLimit,
    /// Ask for user-interface recomposition (needs HUD-less and UI alpha
    /// tagged; off by default, see the crate README).
    pub ui_recomposition: bool,
    /// Turn generation off while the game window is not in the foreground
    /// (DLSS-G stops interpolating unfocused anyway; turning it off says so
    /// instead of leaving the plugin to toggle itself).
    pub pause_unfocused: bool,
    /// Where `slReflexSleep` and `SIMULATION_START` happen.
    pub sleep_point: SleepPoint,
}

impl Options {
    /// Multiplier range.
    pub const MULTIPLIER: (u32, u32) = FrameGeneration::MULTIPLIER;

    /// The options `[FrameGeneration]` describes, everything else default.
    pub fn from_settings(settings: &FrameGeneration) -> Options {
        Options {
            enabled: settings.enabled,
            multiplier: settings.multiplier,
            reflex_boost: settings.reflex_boost,
            ..Options::default()
        }
    }

    /// The multiplier within [`Options::MULTIPLIER`].
    pub fn clamped_multiplier(&self) -> u32 {
        self.multiplier
            .clamp(Self::MULTIPLIER.0, Self::MULTIPLIER.1)
    }
}

impl Default for Options {
    fn default() -> Options {
        Options {
            enabled: true,
            multiplier: 2,
            reflex_boost: false,
            frame_limit: FrameLimit::None,
            ui_recomposition: false,
            pause_unfocused: true,
            sleep_point: SleepPoint::AfterPresent,
        }
    }
}

/// What the frame looks like when options are applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Conditions {
    /// Depth and motion vectors were tagged for the frame being presented.
    pub inputs: bool,
    /// The game window is the foreground window.
    pub focused: bool,
    /// A swapchain resize is being settled ([`Hold`]).
    pub held: bool,
    /// `DLSSGState::numFramesToGenerateMax`, once sampled.
    pub max_generated: Option<u32>,
    /// `DLSSGState::minWidthOrHeight`, once sampled.
    pub min_dimension: Option<u32>,
    /// Size of the depth and motion-vector textures.
    pub render: Size,
    /// Size of the back buffers.
    pub display: Size,
}

/// Why generation is off for a frame, the first reason that applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OffReason {
    /// [`Options::enabled`] is false.
    Disabled,
    /// No depth or motion vectors for the frame.
    NoInputs,
    /// The window is not in the foreground.
    Unfocused,
    /// A swapchain resize is being settled.
    Resizing,
    /// The back buffer is smaller than DLSS-G supports.
    TooSmall,
}

/// The `slDLSSGSetOptions` call a frame needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DlssgRequest {
    /// `None`: `eOn`; `Some(reason)`: `eOff`.
    pub off: Option<OffReason>,
    /// `numFramesToGenerate` (kept while off, so turning on changes only
    /// the mode).
    pub frames_to_generate: u32,
    /// `enableUserInterfaceRecomposition`.
    pub ui_recomposition: bool,
    /// `mvecDepthWidth/Height`.
    pub mvec_depth: Size,
    /// `colorWidth/Height`.
    pub colour: Size,
}

impl DlssgRequest {
    /// Whether frames are generated.
    pub fn generating(&self) -> bool {
        self.off.is_none()
    }

    /// Frames shown per rendered frame under this request (1 when off).
    pub fn frames_per_render(&self) -> u32 {
        if self.generating() {
            self.frames_to_generate + 1
        } else {
            1
        }
    }

    /// The structure Streamline takes.
    pub fn to_abi(&self) -> abi::DlssgOptions {
        abi::DlssgOptions {
            base: abi::Base::new(abi::DLSSG_OPTIONS, 5),
            mode: if self.generating() {
                abi::dlssg_mode::ON
            } else {
                abi::dlssg_mode::OFF
            },
            num_frames_to_generate: self.frames_to_generate,
            flags: abi::DLSSG_RETAIN_RESOURCES_WHEN_OFF,
            // Dynamic resolution stays off: the render size is fixed for a
            // session (set through the upscale ratio) and the colour buffer
            // must stay a fixed size.
            dynamic_res_width: 0,
            dynamic_res_height: 0,
            num_back_buffers: 0,
            mvec_depth_width: self.mvec_depth.w,
            mvec_depth_height: self.mvec_depth.h,
            color_width: self.colour.w,
            color_height: self.colour.h,
            color_buffer_format: 0,
            mvec_buffer_format: 0,
            depth_buffer_format: 0,
            hud_less_buffer_format: 0,
            ui_buffer_format: 0,
            on_error_callback: std::ptr::null_mut(),
            reserved15: abi::INVALID_BOOL,
            queue_parallelism_mode: 0,
            enable_user_interface_recomposition: abi::boolean(self.ui_recomposition),
            dynamic_target_frame_rate: 0.0,
        }
    }
}

/// The DLSS-G request for `options` under `conditions`.
pub fn dlssg_request(options: &Options, conditions: &Conditions) -> DlssgRequest {
    let wanted = options.clamped_multiplier() - 1;
    let frames_to_generate = match conditions.max_generated {
        Some(max) if max >= 1 => wanted.min(max),
        _ => wanted,
    };
    let too_small = conditions
        .min_dimension
        .is_some_and(|min| conditions.display.w.min(conditions.display.h) < min);
    let off = if !options.enabled {
        Some(OffReason::Disabled)
    } else if !conditions.inputs {
        Some(OffReason::NoInputs)
    } else if options.pause_unfocused && !conditions.focused {
        Some(OffReason::Unfocused)
    } else if conditions.held {
        Some(OffReason::Resizing)
    } else if too_small {
        Some(OffReason::TooSmall)
    } else {
        None
    };
    DlssgRequest {
        off,
        frames_to_generate,
        ui_recomposition: options.ui_recomposition,
        mvec_depth: conditions.render,
        colour: conditions.display,
    }
}

/// The `slReflexSetOptions` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReflexRequest {
    /// `ReflexOptions::mode`.
    pub mode: u32,
    /// `ReflexOptions::frameLimitUs`.
    pub frame_limit_us: u32,
}

impl ReflexRequest {
    /// The structure Streamline takes.
    pub fn to_abi(&self) -> abi::ReflexOptions {
        abi::ReflexOptions {
            base: abi::Base::new(abi::REFLEX_OPTIONS, 1),
            mode: self.mode,
            frame_limit_us: self.frame_limit_us,
            use_markers_to_optimize: false,
            virtual_key: 0,
            id_thread: 0,
        }
    }
}

/// The Reflex request. Low latency is not optional: DLSS-G requires Reflex
/// ("status 0x2, Reflex not active" and nothing generated without it), so
/// the mode is always on, with boost if asked. The frame limit counts the
/// frames DLSS-G presents per rendered frame.
pub fn reflex_request(options: &Options, dlssg: &DlssgRequest) -> ReflexRequest {
    ReflexRequest {
        mode: if options.reflex_boost {
            abi::reflex_mode::LOW_LATENCY_WITH_BOOST
        } else {
            abi::reflex_mode::LOW_LATENCY
        },
        frame_limit_us: frame_limit_us(options.frame_limit, dlssg.frames_per_render()),
    }
}

/// The last request Streamline accepted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sent<T>(Option<T>);

impl<T: Copy + PartialEq> Sent<T> {
    /// Nothing sent yet.
    pub const fn new() -> Sent<T> {
        Sent(None)
    }

    /// Whether `wanted` has to be sent.
    pub fn differs(&self, wanted: &T) -> bool {
        self.0.as_ref() != Some(wanted)
    }

    /// `wanted` was accepted.
    pub fn accepted(&mut self, wanted: T) {
        self.0 = Some(wanted);
    }

    /// Forget what was sent, so the next request goes out whatever it is.
    pub fn forget(&mut self) {
        self.0 = None;
    }

    /// The last accepted request.
    pub fn last(&self) -> Option<T> {
        self.0
    }
}

/// Presents for which generation stays off after a `ResizeBuffers`.
///
/// The guide asks for DLSS-G to be off around any window manipulation; the
/// engine resizes without warning (mode change at the start of an
/// iteration, or after losing exclusive fullscreen inside Present), so the
/// hook turns it off at once and it stays off until the new buffers have
/// been presented a few times.
pub const RESIZE_HOLD_PRESENTS: u32 = 3;

/// A countdown in presents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hold {
    remaining: u32,
}

impl Hold {
    /// Hold for `presents` presents from now.
    pub fn start(&mut self, presents: u32) {
        self.remaining = self.remaining.max(presents);
    }

    /// One present happened.
    pub fn tick(&mut self) {
        self.remaining = self.remaining.saturating_sub(1);
    }

    /// Whether still holding.
    pub fn active(&self) -> bool {
        self.remaining > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Conditions {
        Conditions {
            inputs: true,
            focused: true,
            held: false,
            max_generated: None,
            min_dimension: None,
            render: Size::new(1707, 960),
            display: Size::new(2560, 1440),
        }
    }

    #[test]
    fn multiplier_maps_to_frames_to_generate_within_limits() {
        let mut options = Options::default();
        for (multiplier, frames) in [(1, 1), (2, 1), (3, 2), (4, 3), (9, 3)] {
            options.multiplier = multiplier;
            assert_eq!(dlssg_request(&options, &ready()).frames_to_generate, frames);
        }
        let mut limited = ready();
        limited.max_generated = Some(1);
        options.multiplier = 4;
        assert_eq!(dlssg_request(&options, &limited).frames_to_generate, 1);
        limited.max_generated = Some(0);
        assert_eq!(dlssg_request(&options, &limited).frames_to_generate, 3);
    }

    #[test]
    fn generation_turns_off_for_the_first_reason_that_applies() {
        let options = Options::default();
        assert_eq!(dlssg_request(&options, &ready()).off, None);
        let case = |change: fn(&mut Conditions)| {
            let mut c = ready();
            change(&mut c);
            dlssg_request(&options, &c).off
        };
        assert_eq!(case(|c| c.inputs = false), Some(OffReason::NoInputs));
        assert_eq!(case(|c| c.focused = false), Some(OffReason::Unfocused));
        assert_eq!(case(|c| c.held = true), Some(OffReason::Resizing));
        assert_eq!(
            case(|c| c.min_dimension = Some(1500)),
            Some(OffReason::TooSmall)
        );
        assert_eq!(case(|c| c.min_dimension = Some(1440)), None);
        let disabled = Options {
            enabled: false,
            ..Options::default()
        };
        assert_eq!(
            dlssg_request(&disabled, &ready()).off,
            Some(OffReason::Disabled)
        );
        let stay = Options {
            pause_unfocused: false,
            ..Options::default()
        };
        let mut unfocused = ready();
        unfocused.focused = false;
        assert_eq!(dlssg_request(&stay, &unfocused).off, None);
    }

    #[test]
    fn toggling_changes_only_the_mode() {
        let options = Options {
            multiplier: 3,
            ..Options::default()
        };
        let on = dlssg_request(&options, &ready());
        let mut c = ready();
        c.inputs = false;
        let off = dlssg_request(&options, &c);
        assert_eq!(on.frames_to_generate, off.frames_to_generate);
        let (a, b) = (on.to_abi(), off.to_abi());
        assert_eq!(
            (a.mode, b.mode),
            (abi::dlssg_mode::ON, abi::dlssg_mode::OFF)
        );
        assert_eq!(a.flags, abi::DLSSG_RETAIN_RESOURCES_WHEN_OFF);
        assert_eq!(b.flags, abi::DLSSG_RETAIN_RESOURCES_WHEN_OFF);
        assert_eq!((a.mvec_depth_width, a.color_height), (1707, 1440));
        assert_eq!(a.base.version, 5);
    }

    #[test]
    fn requests_are_sent_only_when_they_change() {
        let options = Options::default();
        let mut sent = Sent::new();
        let first = dlssg_request(&options, &ready());
        assert!(sent.differs(&first));
        sent.accepted(first);
        assert!(!sent.differs(&dlssg_request(&options, &ready())));
        let more = Options {
            multiplier: 4,
            ..options
        };
        assert!(sent.differs(&dlssg_request(&more, &ready())));
        sent.forget();
        assert!(sent.differs(&first));
    }

    #[test]
    fn reflex_is_always_low_latency_and_limits_rendered_frames() {
        let mut options = Options::default();
        let dlssg = dlssg_request(&options, &ready());
        assert_eq!(
            reflex_request(&options, &dlssg),
            ReflexRequest {
                mode: abi::reflex_mode::LOW_LATENCY,
                frame_limit_us: 0
            }
        );
        options.reflex_boost = true;
        options.frame_limit = FrameLimit::OutputFps(120.0);
        options.multiplier = 3;
        let dlssg = dlssg_request(&options, &ready());
        let reflex = reflex_request(&options, &dlssg);
        assert_eq!(reflex.mode, abi::reflex_mode::LOW_LATENCY_WITH_BOOST);
        // 120 shown with 3 per rendered frame: 40 rendered, 25 ms apart.
        assert_eq!(reflex.frame_limit_us, 25_000);
        let mut c = ready();
        c.inputs = false;
        let off = dlssg_request(&options, &c);
        assert_eq!(reflex_request(&options, &off).frame_limit_us, 8_333);
    }

    #[test]
    fn a_resize_holds_for_a_few_presents() {
        let mut hold = Hold::default();
        assert!(!hold.active());
        hold.start(RESIZE_HOLD_PRESENTS);
        for _ in 0..RESIZE_HOLD_PRESENTS - 1 {
            hold.tick();
            assert!(hold.active());
        }
        hold.tick();
        assert!(!hold.active());
        hold.tick();
        assert!(!hold.active());
    }

    #[test]
    fn settings_map_onto_options() {
        let settings = FrameGeneration {
            enabled: true,
            multiplier: 3,
            reflex_boost: true,
        };
        let options = Options::from_settings(&settings);
        assert_eq!(
            (options.enabled, options.multiplier, options.reflex_boost),
            (true, 3, true)
        );
        assert!(options.pause_unfocused);
    }
}
