//! Pacing: where Reflex sleeps, the frame limit, and the engine's wait
//! latch.
//!
//! With frame generation the engine's frame-rate limiter
//! (`0x14225f9f0`, `[sMain+0x58]`) caps **rendered** frames: the game's cap
//! times the multiplier is what reaches the screen (60 with 2× shows 120).
//! The alternative is a Reflex frame limit ([`FrameLimit`]) with the game
//! cap set to unlimited: the driver then paces the rendered frames itself,
//! which is what Reflex and DLSS-G are tuned for.

use mhw_core::config::WaitLatch;

/// Where `slReflexSleep` and `SIMULATION_START` are issued.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SleepPoint {
    /// Right after `Present` returns (the 0.2 behaviour). Correct as long
    /// as the engine's own wait helper does not block after it (latch set).
    #[default]
    AfterPresent,
    /// At the start of the main-loop iteration, where the engine's wait
    /// helper is called (`0x14229b020`): the mod calls
    /// `frame_start` from there.
    IterationStart,
}

/// An optional Reflex frame limit.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum FrameLimit {
    /// No limit from Reflex.
    #[default]
    None,
    /// Frames per second **shown**, generated ones included.
    OutputFps(f32),
}

/// `ReflexOptions::frameLimitUs` for `limit`, when DLSS-G shows
/// `frames_per_render` frames per rendered frame (1 when it is off): Reflex
/// paces the frames the game renders, so the interval is
/// `frames_per_render / fps`. 0 (no limit) for no limit or a nonsensical rate.
pub fn frame_limit_us(limit: FrameLimit, frames_per_render: u32) -> u32 {
    match limit {
        FrameLimit::OutputFps(fps) if fps.is_finite() && fps > 0.0 => {
            let us = 1e6 * f64::from(frames_per_render.max(1)) / f64::from(fps);
            us.round().min(f64::from(u32::MAX)) as u32
        }
        _ => 0,
    }
}

/// Whether the engine's wait-helper latch (`Device+0x10DA = 1`) should be
/// set, given whose swapchain the engine presents to.
///
/// The helper (`0x1423ade30`) waits up to 1 s on the frame-latency
/// waitable at the very start of each iteration — after the Reflex sleep
/// and `SIMULATION_START` — so with Streamline it is a second, uncoordinated
/// sleep that inflates the measured simulation; the latch turns it off for
/// the rest of the process, exactly as a 1 s timeout would (RE R1). With a
/// proxy swapchain the engine still owns its waitable (it creates the chain
/// with `FRAME_LATENCY_WAITABLE_OBJECT`, so Streamline stays off it), and
/// pacing falls to the engine's fence throttle in Present, Streamline's
/// present path and Reflex. `Auto` latches exactly when frame generation
/// owns the swapchain.
pub fn wait_latch_wanted(policy: WaitLatch, fg_owns_swapchain: bool) -> bool {
    match policy {
        WaitLatch::Auto => fg_owns_swapchain,
        WaitLatch::Always => true,
        WaitLatch::Never => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_limit_paces_rendered_frames() {
        assert_eq!(frame_limit_us(FrameLimit::None, 2), 0);
        assert_eq!(frame_limit_us(FrameLimit::OutputFps(60.0), 1), 16_667);
        assert_eq!(frame_limit_us(FrameLimit::OutputFps(120.0), 2), 16_667);
        assert_eq!(frame_limit_us(FrameLimit::OutputFps(240.0), 4), 16_667);
        assert_eq!(frame_limit_us(FrameLimit::OutputFps(144.0), 0), 6_944);
        assert_eq!(frame_limit_us(FrameLimit::OutputFps(0.0), 2), 0);
        assert_eq!(frame_limit_us(FrameLimit::OutputFps(-5.0), 2), 0);
        assert_eq!(frame_limit_us(FrameLimit::OutputFps(f32::NAN), 2), 0);
        assert_eq!(frame_limit_us(FrameLimit::OutputFps(1e-9), 2), u32::MAX);
    }

    #[test]
    fn wait_latch_follows_the_policy() {
        assert!(wait_latch_wanted(WaitLatch::Auto, true));
        assert!(!wait_latch_wanted(WaitLatch::Auto, false));
        assert!(wait_latch_wanted(WaitLatch::Always, false));
        assert!(!wait_latch_wanted(WaitLatch::Never, true));
    }
}
