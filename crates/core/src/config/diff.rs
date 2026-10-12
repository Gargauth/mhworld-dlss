//! Which groups of settings changed between two reads of the file.

use super::Settings;

/// When a change to a group takes effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reload {
    /// Applied while running.
    Live,
    /// Read at start only; the change waits for a restart.
    Restart,
}

/// A group of settings that changes together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Group {
    /// `[General] Enabled` — restart.
    Enabled,
    /// `[General] LogLevel` — live.
    LogLevel,
    /// `[Upscaling] Mode`/`RenderScale`, compared as the effective scale —
    /// live.
    Scale,
    /// `[Upscaling] Preset` — live, recreates the DLSS feature.
    Preset,
    /// `[Upscaling] Sharpness` — live.
    Sharpness,
    /// `[Upscaling] DownsampleVolume` — restart (the fog upsample shader is
    /// chosen when the game builds it).
    DownsampleVolume,
    /// `[FrameGeneration] Enabled` — restart.
    FrameGeneration,
    /// `[FrameGeneration] Multiplier`/`ReflexBoost` — live.
    FrameGenerationOptions,
    /// `[Dev] Listen`/`TokenFile` — restart.
    #[cfg(feature = "dev")]
    DevServer,
    /// `[Dev] LogTargets` — live.
    #[cfg(feature = "dev")]
    LogTargets,
    /// `[Dev] DebugLayer`/`Dred`/`DumpShaders` — restart.
    #[cfg(feature = "dev")]
    GpuDebug,
    /// `[Dev] CaptureDir` — live.
    #[cfg(feature = "dev")]
    CaptureDir,
    /// `[Experimental] InputPath` — live.
    #[cfg(feature = "dev")]
    InputPath,
    /// `[Experimental] JitterScale`/`JitterPhases` — live.
    #[cfg(feature = "dev")]
    Jitter,
    /// `[Experimental] HudLess` — live.
    #[cfg(feature = "dev")]
    HudLess,
    /// `[Experimental] TaaSharpen`/`HoldAntiAliasing`/`MipBias` (engine
    /// tuning) — live.
    #[cfg(feature = "dev")]
    Tune,
    /// `[Experimental] WaitLatch` — live.
    #[cfg(feature = "dev")]
    WaitLatch,
    /// `[Experimental] Reset` — live.
    #[cfg(feature = "dev")]
    Reset,
    /// `[Experimental] DlssHdr` — live, recreates the DLSS feature.
    #[cfg(feature = "dev")]
    DlssHdr,
}

impl Group {
    /// Every group, in declaration order.
    pub const ALL: &'static [Group] = &[
        Group::Enabled,
        Group::LogLevel,
        Group::Scale,
        Group::Preset,
        Group::Sharpness,
        Group::DownsampleVolume,
        Group::FrameGeneration,
        Group::FrameGenerationOptions,
        #[cfg(feature = "dev")]
        Group::DevServer,
        #[cfg(feature = "dev")]
        Group::LogTargets,
        #[cfg(feature = "dev")]
        Group::GpuDebug,
        #[cfg(feature = "dev")]
        Group::CaptureDir,
        #[cfg(feature = "dev")]
        Group::InputPath,
        #[cfg(feature = "dev")]
        Group::Jitter,
        #[cfg(feature = "dev")]
        Group::HudLess,
        #[cfg(feature = "dev")]
        Group::Tune,
        #[cfg(feature = "dev")]
        Group::WaitLatch,
        #[cfg(feature = "dev")]
        Group::Reset,
        #[cfg(feature = "dev")]
        Group::DlssHdr,
    ];

    /// When a change to this group takes effect.
    pub const fn reload(self) -> Reload {
        match self {
            Group::Enabled | Group::DownsampleVolume | Group::FrameGeneration => Reload::Restart,
            #[cfg(feature = "dev")]
            Group::DevServer | Group::GpuDebug => Reload::Restart,
            _ => Reload::Live,
        }
    }

    const fn bit(self) -> u32 {
        1 << self as u32
    }
}

/// A set of changed groups.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Changes(u32);

impl Changes {
    /// No change.
    pub const NONE: Changes = Changes(0);

    /// Mark `group` as changed.
    pub fn insert(&mut self, group: Group) {
        self.0 |= group.bit();
    }

    /// Whether `group` changed.
    pub fn contains(self, group: Group) -> bool {
        self.0 & group.bit() != 0
    }

    /// Whether nothing changed.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether any changed group only takes effect after a restart.
    pub fn needs_restart(self) -> bool {
        self.iter().any(|group| group.reload() == Reload::Restart)
    }

    /// The changed groups, in [`Group::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = Group> {
        Group::ALL
            .iter()
            .copied()
            .filter(move |group| self.contains(*group))
    }
}

pub(super) fn diff(old: &Settings, new: &Settings) -> Changes {
    let mut changes = Changes::NONE;
    let mut mark = |changed: bool, group| {
        if changed {
            changes.insert(group);
        }
    };
    mark(old.general.enabled != new.general.enabled, Group::Enabled);
    mark(
        old.general.log_level != new.general.log_level,
        Group::LogLevel,
    );
    mark(old.upscaling.scale() != new.upscaling.scale(), Group::Scale);
    mark(old.upscaling.preset != new.upscaling.preset, Group::Preset);
    mark(
        old.upscaling.sharpness != new.upscaling.sharpness,
        Group::Sharpness,
    );
    mark(
        old.upscaling.downsample_volume != new.upscaling.downsample_volume,
        Group::DownsampleVolume,
    );
    let (a, b) = (&old.frame_generation, &new.frame_generation);
    mark(a.enabled != b.enabled, Group::FrameGeneration);
    mark(
        (a.multiplier, a.reflex_boost) != (b.multiplier, b.reflex_boost),
        Group::FrameGenerationOptions,
    );
    #[cfg(feature = "dev")]
    {
        let (a, b) = (&old.dev, &new.dev);
        mark(
            (a.listen, &a.token_file) != (b.listen, &b.token_file),
            Group::DevServer,
        );
        mark(a.log_targets != b.log_targets, Group::LogTargets);
        mark(
            (a.debug_layer, a.dred, a.dump_shaders) != (b.debug_layer, b.dred, b.dump_shaders),
            Group::GpuDebug,
        );
        mark(a.capture_dir != b.capture_dir, Group::CaptureDir);
        let (a, b) = (&old.experimental, &new.experimental);
        mark(a.input_path != b.input_path, Group::InputPath);
        mark(
            (a.jitter_scale, a.jitter_phases) != (b.jitter_scale, b.jitter_phases),
            Group::Jitter,
        );
        mark(a.hud_less != b.hud_less, Group::HudLess);
        mark(
            (a.taa_sharpen, a.hold_anti_aliasing, a.mip_bias)
                != (b.taa_sharpen, b.hold_anti_aliasing, b.mip_bias),
            Group::Tune,
        );
        mark(a.wait_latch != b.wait_latch, Group::WaitLatch);
        mark(a.reset != b.reset, Group::Reset);
        mark(a.dlss_hdr != b.dlss_hdr, Group::DlssHdr);
    }
    changes
}
