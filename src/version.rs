//! What this build is, for the first line of each log.
//!
//! Shared by the mod and the launcher; the launcher takes this file with
//! `#[path]`.

/// The version in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit this was built from, when the build was told.
///
/// The release workflow sets `MHW_DLSS_COMMIT`. A build made any other way
/// does not know its commit and says so, which is the honest answer: nothing
/// here asks git, and a build from a tree with uncommitted changes would
/// otherwise name a commit it is not.
const COMMIT: Option<&str> = option_env!("MHW_DLSS_COMMIT");

/// `name version (commit abcdef0123)`.
pub fn banner(name: &str) -> String {
    describe(name, VERSION, COMMIT)
}

fn describe(name: &str, version: &str, commit: Option<&str>) -> String {
    match commit.map(str::trim).filter(|commit| !commit.is_empty()) {
        Some(commit) => {
            let short: String = commit.chars().take(10).collect();
            format!("{name} {version} (commit {short})")
        }
        None => format!("{name} {version} (no commit recorded: not built by the release workflow)"),
    }
}

#[cfg(test)]
mod tests {
    use super::describe;

    #[test]
    fn a_recorded_commit_is_shortened_and_shown() {
        assert_eq!(
            describe(
                "mhw-dlss",
                "1.2.3",
                Some("0123456789abcdef0123456789abcdef01234567")
            ),
            "mhw-dlss 1.2.3 (commit 0123456789)"
        );
    }

    #[test]
    fn a_build_that_was_not_told_says_so() {
        assert!(describe("mhw-dlss", "1.2.3", None).contains("no commit recorded"));
        assert!(describe("mhw-dlss", "1.2.3", Some("  ")).contains("no commit recorded"));
    }
}
