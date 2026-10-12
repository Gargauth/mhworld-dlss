//! Giving DLSS up when it cannot run, and trying again. Pure.
//!
//! Below a render ratio of 1 the engine's display-size target is written
//! only by our evaluation at the upscale slot: a slot on which nothing is
//! evaluated leaves the target unwritten and the 3D picture stands still
//! under a live HUD. So every slot reports what happened at it
//! ([`SlotOutcome`]), and two runs of bad slots hand the frame back to the
//! game's own temporal AA at full resolution ([`Policy::fallen_back`]),
//! as 0.2 did (`hijack.rs`, `PATIENCE_WITHOUT_PASS`/`PATIENCE_UNRESOLVED`):
//!
//! * **No temporal pass** (no record of it, or no supersampling call) for
//!   [`PATIENCE_WITHOUT_PASS`] slots: the engine is not drawing a scene the
//!   usual way. A *hold*: the game's path until the temporal pass is seen
//!   again ([`Fallback::pass_seen`]), so the picture keeps moving. The pass
//!   going away again within [`RELAPSE_WITHIN`] slots of coming back,
//!   [`RELAPSES_BORNE`] times running, means it runs only at the whole
//!   window, and following it would rebuild the targets for ever: given up.
//! * **The pass ran and nothing was evaluated** (inputs unusable, an
//!   evaluation failed, or paused after one) for [`PATIENCE_UNRESOLVED`]
//!   slots: given up. Longer, because a rebuild of the engine's targets
//!   accounts for a few such slots by itself and a failure is followed by a
//!   pause of [`EVALUATE_PAUSE`] frames.
//!
//! After a failed evaluation nothing is attempted for [`EVALUATE_PAUSE`]
//! frames ([`Fallback::may_evaluate`]): trying again at once with the same
//! inputs is how one bad frame becomes a broken list, and the engine is
//! usually in the middle of a rebuild that a short pause lets finish.
//!
//! Neither state is permanent: a settings change that could fix it (a new
//! [`Fallback::note_generation`]) clears both and tries again.
//!
//! [`Policy::fallen_back`]: crate::policy::Policy::fallen_back

/// Slots in a row without a temporal pass before the game's path holds.
pub const PATIENCE_WITHOUT_PASS: u32 = 5;
/// Slots in a row with a temporal pass and nothing evaluated before DLSS
/// is given up.
pub const PATIENCE_UNRESOLVED: u32 = 12;
/// Frames after a failed evaluation on which none is attempted.
pub const EVALUATE_PAUSE: u64 = 3;
/// A hold within this many slots of the pass coming back is a relapse.
pub const RELAPSE_WITHIN: u32 = 30;
/// Relapses in a row that give DLSS up.
pub const RELAPSES_BORNE: u32 = 3;

/// What happened at one upscale slot (or DLAA point) that should have been
/// evaluated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotOutcome {
    /// DLSS evaluated.
    Evaluated,
    /// An evaluation was attempted and failed.
    Failed,
    /// The temporal pass ran, but its inputs could not be used (or the
    /// slot is paused after a failure).
    Unresolved,
    /// No temporal pass (or supersampling call) this frame.
    WithoutPass,
}

/// What a call changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    /// Nothing.
    None,
    /// The game's path holds until the temporal pass is seen again.
    Hold,
    /// The temporal pass is back: DLSS again.
    Resume,
    /// DLSS was just given up until a settings change.
    GaveUp,
    /// A held or given-up DLSS is to be tried again after a settings change.
    Retry,
}

/// The slot counts and whether DLSS is held or given up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fallback {
    unresolved: u32,
    without_pass: u32,
    paused_until: u64,
    holding: bool,
    given_up: bool,
    /// Slots since the pass last came back; `None` before it ever did.
    since_resume: Option<u32>,
    relapses: u32,
    generation: u64,
    total_failures: u64,
    gave_up_times: u32,
}

impl Fallback {
    /// Nothing happened yet.
    pub const fn new() -> Fallback {
        Fallback {
            unresolved: 0,
            without_pass: 0,
            paused_until: 0,
            holding: false,
            given_up: false,
            since_resume: None,
            relapses: 0,
            generation: 0,
            total_failures: 0,
            gave_up_times: 0,
        }
    }

    /// Whether an evaluation may be attempted on render `frame` (not inside
    /// the pause after a failure).
    pub fn may_evaluate(&self, frame: u64) -> bool {
        frame >= self.paused_until
    }

    /// Record one slot on render `frame`.
    pub fn slot(&mut self, outcome: SlotOutcome, frame: u64) -> Transition {
        if let Some(since) = self.since_resume.as_mut() {
            *since = since.saturating_add(1);
        }
        if outcome == SlotOutcome::Failed {
            self.total_failures += 1;
            self.paused_until = frame.saturating_add(1 + EVALUATE_PAUSE);
        }
        if self.fallen_back() {
            return Transition::None;
        }
        match outcome {
            SlotOutcome::Evaluated => {
                self.unresolved = 0;
                self.without_pass = 0;
                Transition::None
            }
            SlotOutcome::Failed | SlotOutcome::Unresolved => {
                self.without_pass = 0;
                self.unresolved += 1;
                if self.unresolved >= PATIENCE_UNRESOLVED {
                    self.give_up()
                } else {
                    Transition::None
                }
            }
            SlotOutcome::WithoutPass => {
                self.without_pass += 1;
                if self.without_pass < PATIENCE_WITHOUT_PASS {
                    return Transition::None;
                }
                self.without_pass = 0;
                if self
                    .since_resume
                    .is_some_and(|since| since <= RELAPSE_WITHIN)
                {
                    self.relapses += 1;
                } else {
                    self.relapses = 0;
                }
                if self.relapses >= RELAPSES_BORNE {
                    self.give_up()
                } else {
                    self.holding = true;
                    Transition::Hold
                }
            }
        }
    }

    /// A temporal pass ran: ends a hold.
    pub fn pass_seen(&mut self) -> Transition {
        if !self.holding || self.given_up {
            return Transition::None;
        }
        self.holding = false;
        self.without_pass = 0;
        self.unresolved = 0;
        self.since_resume = Some(0);
        Transition::Resume
    }

    /// The settings generation in force: a new one clears every count and
    /// retries a held or given-up DLSS.
    pub fn note_generation(&mut self, generation: u64) -> Transition {
        if generation == self.generation {
            return Transition::None;
        }
        let was_off = self.fallen_back();
        *self = Fallback {
            generation,
            total_failures: self.total_failures,
            gave_up_times: self.gave_up_times,
            ..Fallback::new()
        };
        if was_off {
            Transition::Retry
        } else {
            Transition::None
        }
    }

    fn give_up(&mut self) -> Transition {
        self.given_up = true;
        self.holding = false;
        self.gave_up_times += 1;
        Transition::GaveUp
    }

    /// Whether the game's own path runs now (held or given up).
    pub fn fallen_back(&self) -> bool {
        self.holding || self.given_up
    }

    /// Whether DLSS is given up until a settings change.
    pub fn given_up(&self) -> bool {
        self.given_up
    }

    /// Whether the game's path holds until the temporal pass is back.
    pub fn holding(&self) -> bool {
        self.holding
    }

    /// Slots in a row with the pass and nothing evaluated.
    pub fn failures(&self) -> u32 {
        self.unresolved
    }

    /// Slots in a row without a temporal pass.
    pub fn without_pass(&self) -> u32 {
        self.without_pass
    }

    /// Failed evaluations in the session.
    pub fn total_failures(&self) -> u64 {
        self.total_failures
    }

    /// How many times DLSS was given up in the session.
    pub fn gave_up_times(&self) -> u32 {
        self.gave_up_times
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(f: &mut Fallback, outcome: SlotOutcome, n: u32, frame: &mut u64) -> Vec<Transition> {
        (0..n)
            .map(|_| {
                *frame += 1;
                f.slot(outcome, *frame)
            })
            .collect()
    }

    fn only_last(transitions: &[Transition], last: Transition) {
        let (tail, head) = transitions.split_last().expect("some");
        assert!(
            head.iter().all(|t| *t == Transition::None),
            "{transitions:?}"
        );
        assert_eq!(*tail, last);
    }

    #[test]
    fn unusable_inputs_give_up_after_patience_unresolved() {
        let mut f = Fallback::new();
        let mut frame = 0;
        let t = run(
            &mut f,
            SlotOutcome::Unresolved,
            PATIENCE_UNRESOLVED,
            &mut frame,
        );
        only_last(&t, Transition::GaveUp);
        assert!(f.given_up() && f.fallen_back());
        assert_eq!(
            f.slot(SlotOutcome::Unresolved, 99),
            Transition::None,
            "once"
        );
        assert_eq!(f.gave_up_times(), 1);
    }

    #[test]
    fn failures_count_toward_the_same_patience_and_pause() {
        let mut f = Fallback::new();
        assert_eq!(f.slot(SlotOutcome::Failed, 10), Transition::None);
        assert_eq!(f.total_failures(), 1);
        for frame in 11..=10 + EVALUATE_PAUSE {
            assert!(!f.may_evaluate(frame), "paused on {frame}");
        }
        assert!(f.may_evaluate(11 + EVALUATE_PAUSE));
        // Paused slots are unresolved ones.
        let mut frame = 10;
        let t = run(
            &mut f,
            SlotOutcome::Unresolved,
            PATIENCE_UNRESOLVED - 1,
            &mut frame,
        );
        only_last(&t, Transition::GaveUp);
    }

    #[test]
    fn three_failures_alone_do_not_give_up() {
        let mut f = Fallback::new();
        let mut frame = 0;
        run(&mut f, SlotOutcome::Failed, 3, &mut frame);
        assert!(!f.fallen_back());
    }

    #[test]
    fn an_evaluation_clears_both_runs() {
        let mut f = Fallback::new();
        let mut frame = 0;
        for _ in 0..10 {
            run(
                &mut f,
                SlotOutcome::Unresolved,
                PATIENCE_UNRESOLVED - 1,
                &mut frame,
            );
            run(
                &mut f,
                SlotOutcome::WithoutPass,
                PATIENCE_WITHOUT_PASS - 1,
                &mut frame,
            );
            frame += 1;
            f.slot(SlotOutcome::Evaluated, frame);
        }
        assert!(!f.fallen_back());
        assert_eq!((f.failures(), f.without_pass()), (0, 0));
    }

    #[test]
    fn no_pass_holds_until_the_pass_is_seen() {
        let mut f = Fallback::new();
        assert_eq!(f.pass_seen(), Transition::None, "nothing held");
        let mut frame = 0;
        let t = run(
            &mut f,
            SlotOutcome::WithoutPass,
            PATIENCE_WITHOUT_PASS,
            &mut frame,
        );
        only_last(&t, Transition::Hold);
        assert!(f.holding() && f.fallen_back() && !f.given_up());
        assert_eq!(f.pass_seen(), Transition::Resume);
        assert!(!f.fallen_back());
        assert_eq!(f.pass_seen(), Transition::None);
    }

    #[test]
    fn a_pass_that_keeps_going_away_is_given_up() {
        let mut f = Fallback::new();
        let mut frame = 0;
        let t = run(
            &mut f,
            SlotOutcome::WithoutPass,
            PATIENCE_WITHOUT_PASS,
            &mut frame,
        );
        only_last(&t, Transition::Hold);
        for relapse in 1..=RELAPSES_BORNE {
            assert_eq!(f.pass_seen(), Transition::Resume);
            let t = run(
                &mut f,
                SlotOutcome::WithoutPass,
                PATIENCE_WITHOUT_PASS,
                &mut frame,
            );
            let expected = if relapse == RELAPSES_BORNE {
                Transition::GaveUp
            } else {
                Transition::Hold
            };
            only_last(&t, expected);
        }
        assert!(f.given_up());
        assert_eq!(f.pass_seen(), Transition::None, "given up is not held");
    }

    #[test]
    fn a_hold_long_after_the_pass_came_back_is_not_a_relapse() {
        let mut f = Fallback::new();
        let mut frame = 0;
        for _ in 0..RELAPSES_BORNE + 2 {
            run(
                &mut f,
                SlotOutcome::WithoutPass,
                PATIENCE_WITHOUT_PASS,
                &mut frame,
            );
            assert!(f.holding());
            assert_eq!(f.pass_seen(), Transition::Resume);
            run(
                &mut f,
                SlotOutcome::Evaluated,
                RELAPSE_WITHIN + 1,
                &mut frame,
            );
        }
        assert!(!f.given_up());
    }

    #[test]
    fn a_new_generation_retries_and_an_old_one_does_not() {
        let mut f = Fallback::new();
        assert_eq!(f.note_generation(0), Transition::None);
        let mut frame = 0;
        run(&mut f, SlotOutcome::Failed, PATIENCE_UNRESOLVED, &mut frame);
        assert!(f.given_up());
        assert_eq!(f.note_generation(0), Transition::None);
        assert!(f.given_up());
        assert_eq!(f.note_generation(1), Transition::Retry);
        assert!(!f.fallen_back());
        assert_eq!(f.failures(), 0);
        assert!(f.may_evaluate(frame), "the pause is cleared too");
        assert_eq!(
            f.total_failures(),
            u64::from(PATIENCE_UNRESOLVED),
            "session count kept"
        );
        assert_eq!(f.note_generation(2), Transition::None, "nothing to retry");
    }

    #[test]
    fn a_new_generation_ends_a_hold() {
        let mut f = Fallback::new();
        let mut frame = 0;
        run(
            &mut f,
            SlotOutcome::WithoutPass,
            PATIENCE_WITHOUT_PASS,
            &mut frame,
        );
        assert!(f.holding());
        assert_eq!(f.note_generation(1), Transition::Retry);
        assert!(!f.fallen_back());
    }
}
