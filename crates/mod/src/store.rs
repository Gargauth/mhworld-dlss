//! The settings in force and what each change means. Pure.
//!
//! One store serves the file's hot reload, the dev server's
//! `settings.set` and the overlay (which writes the file). Every change
//! goes through [`Store::replace`]: the changed groups are applied by the
//! caller when they are live; a change to a restart group is reported once
//! (until it is changed back) and takes effect at the next start, because
//! what it controls was decided at boot.

use mhw_core::config::{Build, Changes, Diagnostic, Group, Reload, Settings};

/// What one [`Store::replace`] changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Replaced {
    /// Every group that differs from the settings before.
    pub changes: Changes,
    /// Restart groups that now differ from the start-up settings and were
    /// not reported yet.
    pub newly_pending: Changes,
}

impl Replaced {
    /// The changed groups that apply while running.
    pub fn live(&self) -> impl Iterator<Item = Group> {
        self.changes
            .iter()
            .filter(|group| group.reload() == Reload::Live)
    }
}

/// The settings in force, the ones read at start, and a generation that
/// counts live changes.
#[derive(Clone, Debug)]
pub struct Store {
    startup: Settings,
    current: Settings,
    generation: u64,
    pending: Changes,
}

impl Store {
    /// The settings read at start.
    pub fn new(settings: Settings) -> Store {
        Store {
            startup: settings.clone(),
            current: settings,
            generation: 0,
            pending: Changes::NONE,
        }
    }

    /// The settings in force.
    pub fn current(&self) -> &Settings {
        &self.current
    }

    /// The settings read at start (what restart groups still follow).
    pub fn startup(&self) -> &Settings {
        &self.startup
    }

    /// Increases with every replacement that changed a live group.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Restart groups changed since start.
    pub fn pending_restart(&self) -> Changes {
        self.pending
    }

    /// Put `new` in force.
    pub fn replace(&mut self, new: Settings) -> Replaced {
        let changes = Settings::diff(&self.current, &new);
        let since_start = Settings::diff(&self.startup, &new);
        let mut pending = Changes::NONE;
        let mut newly_pending = Changes::NONE;
        for group in since_start
            .iter()
            .filter(|group| group.reload() == Reload::Restart)
        {
            pending.insert(group);
            if !self.pending.contains(group) {
                newly_pending.insert(group);
            }
        }
        self.pending = pending;
        let replaced = Replaced {
            changes,
            newly_pending,
        };
        if replaced.live().next().is_some() {
            self.generation += 1;
        }
        self.current = new;
        replaced
    }

    /// Set one key like the file would (`Settings::set`) and put the
    /// result in force.
    pub fn set(
        &mut self,
        section: &str,
        key: &str,
        value: &str,
    ) -> Result<(Option<Diagnostic>, Replaced), Diagnostic> {
        let mut next = self.current.clone();
        let note = next.set(section, key, value, Build::CURRENT)?;
        Ok((note, self.replace(next)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_changes_count_and_report_their_groups() {
        let mut store = Store::new(Settings::default());
        let mut next = store.current().clone();
        next.upscaling.sharpness = 0.5;
        let replaced = store.replace(next.clone());
        assert!(replaced.changes.contains(Group::Sharpness));
        assert_eq!(replaced.live().collect::<Vec<_>>(), vec![Group::Sharpness]);
        assert!(replaced.newly_pending.is_empty());
        assert_eq!(store.generation(), 1);
        assert_eq!(store.current().upscaling.sharpness, 0.5);
        let same = store.replace(next);
        assert!(same.changes.is_empty());
        assert_eq!(store.generation(), 1, "no change, no new generation");
    }

    #[test]
    fn a_restart_group_is_reported_once_until_changed_back() {
        let mut store = Store::new(Settings::default());
        let mut on = store.current().clone();
        on.frame_generation.enabled = true;
        let first = store.replace(on.clone());
        assert!(first.newly_pending.contains(Group::FrameGeneration));
        assert_eq!(store.generation(), 0, "restart groups are not live");
        let mut on_sharper = on.clone();
        on_sharper.upscaling.sharpness = 0.2;
        let second = store.replace(on_sharper);
        assert!(second.newly_pending.is_empty(), "already reported");
        assert!(store.pending_restart().contains(Group::FrameGeneration));
        let back = store.replace(Settings::default());
        assert!(back.newly_pending.is_empty());
        assert!(store.pending_restart().is_empty());
        let again = store.replace(on);
        assert!(
            again.newly_pending.contains(Group::FrameGeneration),
            "changed again"
        );
        assert!(!store.startup().frame_generation.enabled);
    }

    #[test]
    fn set_validates_like_the_file() {
        let mut store = Store::new(Settings::default());
        let (note, replaced) = store
            .set("Upscaling", "Sharpness", "7")
            .expect("clamped, applied");
        assert!(note.is_some(), "clamping is noted");
        assert!(replaced.changes.contains(Group::Sharpness));
        assert_eq!(store.current().upscaling.sharpness, 1.0);
        assert!(store.set("Upscaling", "Mode", "Sideways").is_err());
        assert_eq!(store.generation(), 1);
    }
}
