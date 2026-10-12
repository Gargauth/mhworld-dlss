//! What the settings file got wrong. Never an error: the setting keeps its
//! previous (default) value or is clamped, and the diagnostic is logged once.

use std::fmt;

/// The settings file name, used in diagnostics.
pub const FILE_NAME: &str = "mhw-dlss.ini";

/// One problem with one line (or one `set` request).
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    /// 1-based line in the file; 0 when not from the file.
    pub line: usize,
    /// The section as written (empty outside any section).
    pub section: String,
    /// The key as written (empty for section- or file-level problems).
    pub key: String,
    /// What is wrong.
    pub kind: DiagnosticKind,
}

/// What is wrong.
#[derive(Clone, Debug, PartialEq)]
pub enum DiagnosticKind {
    /// The file is not valid UTF-8/UTF-16; undecodable bytes were replaced.
    Encoding,
    /// A line that is not a comment, `[Section]` or `Key = Value`.
    Syntax {
        /// The line, trimmed.
        text: String,
    },
    /// A section this build does not know; its keys are skipped.
    UnknownSection,
    /// A key before the first section.
    OutsideSection,
    /// A key its section does not have.
    UnknownKey,
    /// A `[Dev]`/`[Experimental]` key in a release build: ignored.
    DevOnly,
    /// Not a value of this key; the previous value is kept.
    Invalid {
        /// The value as written.
        value: String,
        /// What the key accepts.
        expected: String,
    },
    /// A number outside the key's range, replaced by the bound.
    Clamped {
        /// The value as written.
        value: String,
        /// The bound applied.
        used: String,
    },
    /// The key was set before; this later line wins.
    Duplicate {
        /// Where it was set first.
        first_line: usize,
    },
    /// The key is set but has no effect with the other settings.
    NoEffect {
        /// Why.
        reason: &'static str,
    },
}

impl Diagnostic {
    /// Whether the value was not applied (as opposed to applied with a
    /// note: clamped, duplicate, no effect).
    pub fn is_rejection(&self) -> bool {
        !matches!(
            self.kind,
            DiagnosticKind::Clamped { .. }
                | DiagnosticKind::Duplicate { .. }
                | DiagnosticKind::NoEffect { .. }
        )
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(FILE_NAME)?;
        if self.line > 0 {
            write!(f, ":{}", self.line)?;
        }
        if !self.section.is_empty() {
            write!(f, " [{}]", self.section)?;
        }
        if !self.key.is_empty() {
            write!(f, " {}", self.key)?;
        }
        f.write_str(": ")?;
        match &self.kind {
            DiagnosticKind::Encoding => f.write_str("not valid text; undecodable bytes replaced"),
            DiagnosticKind::Syntax { text } => write!(f, "not a setting: {text:?}"),
            DiagnosticKind::UnknownSection => f.write_str("unknown section, its keys are ignored"),
            DiagnosticKind::OutsideSection => f.write_str("key outside any section, ignored"),
            DiagnosticKind::UnknownKey => f.write_str("unknown key, ignored"),
            DiagnosticKind::DevOnly => f.write_str("ignored, dev build only"),
            DiagnosticKind::Invalid { value, expected } => {
                write!(f, "{value:?} is not valid (expected {expected}), ignored")
            }
            DiagnosticKind::Clamped { value, used } => {
                write!(f, "{value} is out of range, using {used}")
            }
            DiagnosticKind::Duplicate { first_line } => {
                write!(f, "also set on line {first_line}; this line wins")
            }
            DiagnosticKind::NoEffect { reason } => write!(f, "has no effect: {reason}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic(line: usize, section: &str, key: &str, kind: DiagnosticKind) -> String {
        Diagnostic {
            line,
            section: section.into(),
            key: key.into(),
            kind,
        }
        .to_string()
    }

    #[test]
    fn diagnostics_read_as_one_line() {
        assert_eq!(
            diagnostic(
                7,
                "Upscaling",
                "RenderScale",
                DiagnosticKind::Clamped {
                    value: "0.3".into(),
                    used: "0.5".into()
                }
            ),
            "mhw-dlss.ini:7 [Upscaling] RenderScale: 0.3 is out of range, using 0.5"
        );
        assert_eq!(
            diagnostic(3, "Dev", "Listen", DiagnosticKind::DevOnly),
            "mhw-dlss.ini:3 [Dev] Listen: ignored, dev build only"
        );
        assert_eq!(
            diagnostic(0, "", "", DiagnosticKind::Encoding),
            "mhw-dlss.ini: not valid text; undecodable bytes replaced"
        );
    }
}
