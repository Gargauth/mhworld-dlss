//! `NVSDK_NGX_Result`: the status every NGX entry point returns.

use std::fmt;

/// An `NVSDK_NGX_Result` value.
///
/// Success is judged like `NVSDK_NGX_SUCCEED`: everything outside the
/// `0xBAD0_0xxx` failure range succeeds. `Display` prints the hexadecimal
/// code and its name (`0xbad00004 FeatureNotFound`), because a bare code in a
/// log costs more to look up than to print properly.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Status(pub u32);

impl Status {
    /// `NVSDK_NGX_Result_Success`.
    pub const SUCCESS: Status = Status(0x1);
    /// `NVSDK_NGX_Result_Fail`, the generic failure.
    pub const FAIL: Status = Status(0xBAD0_0000);
    /// `FAIL_FeatureNotSupported`.
    pub const FEATURE_NOT_SUPPORTED: Status = Status(0xBAD0_0001);
    /// `FAIL_PlatformError`.
    pub const PLATFORM_ERROR: Status = Status(0xBAD0_0002);
    /// `FAIL_FeatureAlreadyExists`.
    pub const FEATURE_ALREADY_EXISTS: Status = Status(0xBAD0_0003);
    /// `FAIL_FeatureNotFound`.
    pub const FEATURE_NOT_FOUND: Status = Status(0xBAD0_0004);
    /// `FAIL_InvalidParameter`.
    pub const INVALID_PARAMETER: Status = Status(0xBAD0_0005);
    /// `FAIL_NotInitialized`.
    pub const NOT_INITIALIZED: Status = Status(0xBAD0_0007);

    /// Failure codes and their names, `NVSDK_NGX_Result_FAIL_*`.
    const NAMES: [(u32, &'static str); 19] = [
        (0xBAD0_0000, "Fail"),
        (0xBAD0_0001, "FeatureNotSupported"),
        (0xBAD0_0002, "PlatformError"),
        (0xBAD0_0003, "FeatureAlreadyExists"),
        (0xBAD0_0004, "FeatureNotFound"),
        (0xBAD0_0005, "InvalidParameter"),
        (0xBAD0_0006, "ScratchBufferTooSmall"),
        (0xBAD0_0007, "NotInitialized"),
        (0xBAD0_0008, "UnsupportedInputFormat"),
        (0xBAD0_0009, "RWFlagMissing"),
        (0xBAD0_000A, "MissingInput"),
        (0xBAD0_000B, "UnableToInitializeFeature"),
        (0xBAD0_000C, "OutOfDate"),
        (0xBAD0_000D, "OutOfGPUMemory"),
        (0xBAD0_000E, "UnsupportedFormat"),
        (0xBAD0_000F, "UnableToWriteToAppDataPath"),
        (0xBAD0_0010, "UnsupportedParameter"),
        (0xBAD0_0011, "Denied"),
        (0xBAD0_0012, "NotImplemented"),
    ];

    /// A status stored in a signed parameter (`*.FeatureInitResult`), which
    /// is how `0xBAD00004` reads as `-1160773628`.
    pub const fn from_i32(value: i32) -> Status {
        Status(value as u32)
    }

    /// `NVSDK_NGX_SUCCEED`: not in the `0xBAD00000` failure range.
    pub const fn is_success(self) -> bool {
        self.0 & 0xFFF0_0000 != 0xBAD0_0000
    }

    /// The SDK's name for the code, without the `NVSDK_NGX_Result_` prefix;
    /// `None` for a code the SDK does not define.
    pub fn name(self) -> Option<&'static str> {
        if self == Status::SUCCESS {
            return Some("Success");
        }
        Status::NAMES
            .iter()
            .find(|(code, _)| *code == self.0)
            .map(|(_, name)| *name)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{:#010x} {name}", self.0),
            None => write!(f, "{:#010x}", self.0),
        }
    }
}

impl fmt::Debug for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_is_everything_outside_the_failure_range() {
        assert!(Status::SUCCESS.is_success());
        // Not only 1: NVSDK_NGX_SUCCEED accepts any non-failure code.
        assert!(Status(0).is_success());
        assert!(Status(0xBAC0_0000).is_success());
        assert!(!Status::FAIL.is_success());
        assert!(!Status(0xBAD0_0012).is_success());
        assert!(!Status(0xBAD0_FFFF).is_success());
    }

    #[test]
    fn statuses_print_as_code_and_name() {
        assert_eq!(Status::SUCCESS.to_string(), "0x00000001 Success");
        assert_eq!(
            Status(0xBAD0_0004).to_string(),
            "0xbad00004 FeatureNotFound"
        );
        assert_eq!(Status(0xBAD0_000C).to_string(), "0xbad0000c OutOfDate");
        assert_eq!(Status(0x1234).to_string(), "0x00001234");
    }

    #[test]
    fn a_signed_init_result_reads_as_its_status() {
        let stored = 0xBAD0_0004u32 as i32;
        assert_eq!(stored, -1_160_773_628);
        assert_eq!(Status::from_i32(stored), Status::FEATURE_NOT_FOUND);
    }
}
