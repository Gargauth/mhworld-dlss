//! How often feature creation is attempted.
//!
//! Without a bound, a machine on which creation cannot succeed allocated a
//! parameter block, ran a real `CreateFeature` on the engine's command list
//! and logged every few frames for as long as the game ran (0.2). So: at most
//! one attempt per distinct frame, and after [`MAX_CREATE_ATTEMPTS`]
//! consecutive failures for one description, none until the description
//! changes or [`CreateBudget::retry`] is called.

use crate::dlss::FeatureDesc;
use crate::status::Status;

/// Consecutive failed creations of one description before giving up on it.
pub const MAX_CREATE_ATTEMPTS: u32 = 5;

/// Whether a creation may be attempted now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attempt {
    /// Try.
    Go,
    /// Already tried in this frame.
    Wait,
    /// Gave up on this description.
    GaveUp,
}

/// Failure bookkeeping for feature creation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CreateBudget {
    desc: Option<FeatureDesc>,
    failures: u32,
    last_tick: Option<u64>,
    last_failure: Option<Status>,
}

impl CreateBudget {
    /// No attempts made.
    pub const fn new() -> CreateBudget {
        CreateBudget {
            desc: None,
            failures: 0,
            last_tick: None,
            last_failure: None,
        }
    }

    /// May `desc` be created at frame tick `tick`? A description other than
    /// the last one attempted starts with a clean slate.
    pub fn check(&mut self, desc: &FeatureDesc, tick: u64) -> Attempt {
        if self.desc.as_ref() != Some(desc) {
            *self = CreateBudget {
                desc: Some(*desc),
                ..CreateBudget::new()
            };
        }
        if self.failures >= MAX_CREATE_ATTEMPTS {
            Attempt::GaveUp
        } else if self.last_tick == Some(tick) {
            Attempt::Wait
        } else {
            Attempt::Go
        }
    }

    /// The attempt allowed by the last `check` failed with `status`.
    pub fn failed(&mut self, tick: u64, status: Status) {
        self.failures = self.failures.saturating_add(1);
        self.last_tick = Some(tick);
        self.last_failure = Some(status);
    }

    /// The attempt succeeded.
    pub fn succeeded(&mut self) {
        self.failures = 0;
        self.last_failure = None;
        self.last_tick = None;
    }

    /// Forget the failures (e.g. a new DLSS library was put in place).
    pub fn retry(&mut self) {
        self.failures = 0;
        self.last_tick = None;
    }

    /// Consecutive failures of the current description.
    pub const fn failures(&self) -> u32 {
        self.failures
    }

    /// The status of the last failure, if the last attempt failed.
    pub const fn last_failure(&self) -> Option<Status> {
        self.last_failure
    }
}

#[cfg(test)]
mod tests {
    use mhw_core::frame::Size;

    use super::*;
    use crate::dlss::{CreateFlags, Quality, RenderPreset};

    fn desc(w: u32) -> FeatureDesc {
        FeatureDesc {
            render: Size::new(w, 1000),
            display: Size::new(3000, 2000),
            quality: Quality::MaxPerf,
            preset: RenderPreset::Default,
            flags: CreateFlags::default(),
            output_subrects: false,
        }
    }

    #[test]
    fn one_attempt_per_frame_and_five_in_all() {
        let mut budget = CreateBudget::new();
        let d = desc(1500);
        for tick in 1..=MAX_CREATE_ATTEMPTS as u64 {
            assert_eq!(budget.check(&d, tick), Attempt::Go, "tick {tick}");
            budget.failed(tick, Status::FEATURE_NOT_FOUND);
            assert_eq!(
                budget.check(&d, tick),
                if tick < 5 {
                    Attempt::Wait
                } else {
                    Attempt::GaveUp
                }
            );
        }
        assert_eq!(budget.check(&d, 100), Attempt::GaveUp);
        assert_eq!(budget.failures(), 5);
        assert_eq!(budget.last_failure(), Some(Status::FEATURE_NOT_FOUND));
    }

    #[test]
    fn a_new_description_or_a_retry_starts_over() {
        let mut budget = CreateBudget::new();
        for tick in 1..=5 {
            budget.check(&desc(1500), tick);
            budget.failed(tick, Status::FAIL);
        }
        assert_eq!(budget.check(&desc(1500), 6), Attempt::GaveUp);
        assert_eq!(budget.check(&desc(1501), 6), Attempt::Go);
        for tick in 6..=10 {
            budget.check(&desc(1501), tick);
            budget.failed(tick, Status::FAIL);
        }
        assert_eq!(budget.check(&desc(1501), 11), Attempt::GaveUp);
        budget.retry();
        assert_eq!(budget.check(&desc(1501), 11), Attempt::Go);
    }

    #[test]
    fn success_clears_the_count() {
        let mut budget = CreateBudget::new();
        budget.check(&desc(1500), 1);
        budget.failed(1, Status::FAIL);
        budget.check(&desc(1500), 2);
        budget.succeeded();
        assert_eq!(budget.failures(), 0);
        assert_eq!(budget.last_failure(), None);
        assert_eq!(budget.check(&desc(1500), 2), Attempt::Go);
    }
}
