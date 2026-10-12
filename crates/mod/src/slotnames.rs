//! Which resources the dev `slot` point offers, by name, and why the others
//! are absent (`docs/DEV-PROTOCOL.md`, capture). Pure.
//!
//! `probe.depth` compares `game_depth` (the supersampling call's R32F copy)
//! with `pass_depth` (the temporal pass's picked depth target) and
//! `probe.mv` compares `game_mv` with `mv_raw` (`tGBuffer4`). In the
//! supersampling path `depth` *is* `game_depth`, so the pass's own record
//! is offered under its own names, or reported absent with the reason.

use mhw_core::config::InputPath;

/// The pointers one evaluation used, and what else this frame had.
#[derive(Clone, Copy, Debug, Default)]
pub struct SlotSources {
    /// The input path in force.
    pub path: Option<InputPath>,
    /// DLSS's colour input.
    pub colour: usize,
    /// DLSS's depth input.
    pub depth: usize,
    /// DLSS's motion vectors.
    pub mv: usize,
    /// DLSS's output.
    pub dlss_out: usize,
    /// The temporal pass's record: (`tGBuffer4`, picked depth), when one
    /// is known for this frame.
    pub pass: Option<(usize, usize)>,
    /// The supersampling call's (dense velocity, depth copy), when it ran
    /// this frame.
    pub ss: Option<(usize, usize)>,
}

/// Why `mv_raw`/`pass_depth` are absent in the supersampling path.
pub const NO_PASS_RECORD: &str =
    "no live temporal-pass record this frame (with Supersampling flags the CSTemporalAA2 pass may not run)";
/// Why `game_mv`/`game_depth` are absent.
pub const NO_SS_CALL: &str =
    "no supersampling call this frame (only with InputPath = Supersampling, flags 3)";
/// A null pointer.
pub const NULL: &str = "not provided at this slot";

/// Offered `(name, pointer)` and absent `(name, reason)`.
pub type Named = (
    Vec<(&'static str, usize)>,
    Vec<(&'static str, &'static str)>,
);

/// The slot's resources by name.
pub fn slot_names(s: &SlotSources) -> Named {
    let pass = match s.path {
        Some(InputPath::Supersampling) => s.pass.ok_or(NO_PASS_RECORD),
        _ => s.pass.ok_or(NULL),
    };
    let ss = s.ss.ok_or(NO_SS_CALL);
    let candidates: [(&'static str, Result<usize, &'static str>); 9] = [
        ("colour", Ok(s.colour)),
        ("depth", Ok(s.depth)),
        ("mv", Ok(s.mv)),
        ("dlss_out", Ok(s.dlss_out)),
        ("mv_raw", pass.map(|p| p.0)),
        ("pass_depth", pass.map(|p| p.1)),
        ("game_mv", ss.map(|g| g.0)),
        ("game_depth", ss.map(|g| g.1)),
        // Listed so a request for it says why.
        (
            "backbuffer",
            Err("the back buffer is not captured at the slot"),
        ),
    ];
    let mut offered = Vec::new();
    let mut absent = Vec::new();
    for (name, value) in candidates {
        match value {
            Ok(0) => absent.push((name, NULL)),
            Ok(pointer) => offered.push((name, pointer)),
            Err(why) => absent.push((name, why)),
        }
    }
    (offered, absent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<T: Copy>(list: &[(&'static str, T)], name: &str) -> Option<T> {
        list.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }

    #[test]
    fn the_supersampling_path_offers_the_pass_depth_apart_from_its_own() {
        let (offered, absent) = slot_names(&SlotSources {
            path: Some(InputPath::Supersampling),
            colour: 1,
            depth: 20,
            mv: 21,
            dlss_out: 4,
            pass: Some((30, 31)),
            ss: Some((21, 20)),
        });
        assert_eq!(find(&offered, "game_depth"), Some(20));
        assert_eq!(find(&offered, "pass_depth"), Some(31), "not the SS copy");
        assert_eq!(find(&offered, "mv_raw"), Some(30));
        assert!(find(&absent, "backbuffer").is_some());
    }

    #[test]
    fn absent_inputs_carry_their_reason() {
        let (offered, absent) = slot_names(&SlotSources {
            path: Some(InputPath::Supersampling),
            colour: 1,
            depth: 2,
            mv: 3,
            dlss_out: 0,
            pass: None,
            ss: None,
        });
        assert_eq!(find(&absent, "mv_raw"), Some(NO_PASS_RECORD));
        assert_eq!(find(&absent, "pass_depth"), Some(NO_PASS_RECORD));
        assert_eq!(find(&absent, "game_mv"), Some(NO_SS_CALL));
        assert_eq!(find(&absent, "dlss_out"), Some(NULL));
        assert_eq!(find(&offered, "colour"), Some(1));
    }

    #[test]
    fn the_pass_path_offers_its_record_and_lacks_the_game_inputs() {
        let (offered, absent) = slot_names(&SlotSources {
            path: Some(InputPath::Pass),
            colour: 1,
            depth: 2,
            mv: 3,
            dlss_out: 4,
            pass: Some((5, 2)),
            ss: None,
        });
        assert_eq!(find(&offered, "pass_depth"), Some(2));
        assert_eq!(find(&offered, "mv_raw"), Some(5));
        assert_eq!(find(&absent, "game_depth"), Some(NO_SS_CALL));
    }
}
