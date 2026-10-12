//! When the temporal history is discarded. Pure.
//!
//! The engine has no camera-cut detection of its own: its only reset is the
//! TAA history flag (`taa+0x185`, or a blend rate ≤ 0) that the engine
//! detours latch (`00-SUMMARY` §1, *Reset*). Cutscene cuts and teleports
//! keep the history unless the mod detects them ([`CutDetector`], from the
//! engine's unjittered camera). Size and setting changes recreate the DLSS
//! feature, which starts without history anyway; they are reported so the
//! reason is visible and frame generation is told too.

use mhw_core::config::ResetPolicy;
use mhw_core::frame::{CameraFrame, ResetReason};
use mhw_core::math::camera_cut;

/// What happened this frame that may discard history.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResetCause {
    /// The game's own reset flag.
    pub game: bool,
    /// The camera cut ([`CutDetector`]).
    pub cut: bool,
    /// Render or display size differs from the last evaluation.
    pub resized: bool,
    /// A setting that recreates the feature changed.
    pub settings: bool,
}

/// The reason history is discarded under `policy`, the strongest first.
/// `Never` keeps the game's flag and cuts from resetting; a recreated
/// feature has no history whatever the policy.
pub fn reset_reason(policy: ResetPolicy, cause: ResetCause) -> ResetReason {
    if cause.resized {
        return ResetReason::Resize;
    }
    if cause.settings {
        return ResetReason::Settings;
    }
    match policy {
        ResetPolicy::Never => ResetReason::None,
        _ if cause.game => ResetReason::GameHistory,
        ResetPolicy::Auto if cause.cut => ResetReason::CameraCut,
        _ => ResetReason::None,
    }
}

/// Camera cuts between consecutive distinct frames.
#[derive(Clone, Debug, Default)]
pub struct CutDetector {
    previous: Option<CameraFrame>,
    last: Option<(u64, bool)>,
}

impl CutDetector {
    /// No camera seen yet.
    pub const fn new() -> CutDetector {
        CutDetector {
            previous: None,
            last: None,
        }
    }

    /// Whether `camera` is a cut from the previous frame's camera. The
    /// first camera is not a cut; asking again for the same frame gives the
    /// same answer.
    pub fn observe(&mut self, camera: &CameraFrame) -> bool {
        if let Some((frame, cut)) = self.last {
            if frame == camera.frame {
                return cut;
            }
        }
        let cut = self
            .previous
            .as_ref()
            .is_some_and(|previous| camera_cut(previous, camera));
        self.previous = Some(*camera);
        self.last = Some((camera.frame, cut));
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mhw_core::math::Mat4;

    fn camera(frame: u64, x: f32) -> CameraFrame {
        let mut view = Mat4::IDENTITY;
        view.rows[3] = [x, 0.0, 0.0, 1.0];
        let proj = Mat4::from_rows([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.7, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.0, 0.0, 0.1, 0.0],
        ]);
        CameraFrame::new(view, proj, view, proj, frame).expect("a perspective camera")
    }

    #[test]
    fn size_and_settings_win_whatever_the_policy() {
        for policy in [ResetPolicy::Auto, ResetPolicy::GameOnly, ResetPolicy::Never] {
            let all = ResetCause {
                game: true,
                cut: true,
                resized: true,
                settings: true,
            };
            assert_eq!(reset_reason(policy, all), ResetReason::Resize);
            let settings = ResetCause {
                resized: false,
                ..all
            };
            assert_eq!(reset_reason(policy, settings), ResetReason::Settings);
        }
    }

    #[test]
    fn the_policy_decides_about_the_game_flag_and_cuts() {
        let game = ResetCause {
            game: true,
            cut: true,
            ..ResetCause::default()
        };
        let cut = ResetCause {
            cut: true,
            ..ResetCause::default()
        };
        assert_eq!(
            reset_reason(ResetPolicy::Auto, game),
            ResetReason::GameHistory
        );
        assert_eq!(reset_reason(ResetPolicy::Auto, cut), ResetReason::CameraCut);
        assert_eq!(
            reset_reason(ResetPolicy::GameOnly, game),
            ResetReason::GameHistory
        );
        assert_eq!(reset_reason(ResetPolicy::GameOnly, cut), ResetReason::None);
        assert_eq!(reset_reason(ResetPolicy::Never, game), ResetReason::None);
        assert_eq!(
            reset_reason(ResetPolicy::Auto, ResetCause::default()),
            ResetReason::None
        );
    }

    #[test]
    fn a_jump_is_a_cut_once_and_the_first_camera_is_not() {
        let mut detector = CutDetector::default();
        assert!(!detector.observe(&camera(1, 0.0)));
        assert!(!detector.observe(&camera(2, 0.01)));
        assert!(detector.observe(&camera(3, 2000.0)));
        assert!(
            detector.observe(&camera(3, 2000.0)),
            "same frame, same answer"
        );
        assert!(!detector.observe(&camera(4, 2000.01)));
    }
}
