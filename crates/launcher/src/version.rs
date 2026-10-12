//! What this build is, for the first line of the log.

/// The version in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit this was built from, when the build was told: CI and the
/// release workflow set `MHW_DLSS_COMMIT`. Nothing here asks git, so a build
/// from a tree with uncommitted changes never names a commit it is not.
pub const COMMIT: Option<&str> = option_env!("MHW_DLSS_COMMIT");

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
        None => format!("{name} {version} (no commit recorded)"),
    }
}

#[cfg(test)]
mod tests {
    use super::describe;

    #[test]
    fn a_recorded_commit_is_shortened_and_shown() {
        assert_eq!(
            describe(
                "x",
                "1.2.3",
                Some("0123456789abcdef0123456789abcdef01234567")
            ),
            "x 1.2.3 (commit 0123456789)"
        );
    }

    #[test]
    fn a_build_that_was_not_told_says_so() {
        assert_eq!(describe("x", "1.2.3", None), "x 1.2.3 (no commit recorded)");
        assert_eq!(
            describe("x", "1.2.3", Some("  ")),
            "x 1.2.3 (no commit recorded)"
        );
    }
}
