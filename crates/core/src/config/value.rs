//! Typed values: the enumerations the settings use and their parsers.
//!
//! Enumerations and booleans are case-insensitive. Numbers outside their
//! range are clamped (and reported); text that is not a value at all is
//! rejected (and reported) and the previous value kept.

use std::fmt;

/// An enumeration whose values have names in the settings file.
pub trait Named: Copy + PartialEq + 'static {
    /// Every value with its file name, in documentation order.
    const NAMES: &'static [(&'static str, Self)];

    /// The file name of this value.
    fn name(self) -> &'static str {
        Self::NAMES
            .iter()
            .find(|(_, value)| *value == self)
            .map_or("?", |(name, _)| name)
    }

    /// The value named `text`, case-insensitively.
    fn parse(text: &str) -> Option<Self> {
        Self::NAMES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(text))
            .map(|(_, value)| *value)
    }

    /// `A | B | C`, for documentation and diagnostics.
    fn choices() -> String {
        let names: Vec<&str> = Self::NAMES.iter().map(|(name, _)| *name).collect();
        names.join(" | ")
    }
}

macro_rules! named_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident { $( $(#[$vmeta:meta])* $variant:ident = $text:literal ),+ $(,)? }
        default $default:ident
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name { $( $(#[$vmeta])* $variant ),+ }

        impl $crate::config::value::Named for $name {
            const NAMES: &'static [(&'static str, $name)] = &[ $( ($text, $name::$variant) ),+ ];
        }

        impl $name {
            /// The default value.
            pub const DEFAULT: $name = $name::$default;
        }

        impl Default for $name {
            fn default() -> $name {
                $name::DEFAULT
            }
        }
    };
}
pub(crate) use named_enum;

/// Parse a boolean: `true/false`, `on/off`, `1/0`, case-insensitively.
pub fn parse_bool(text: &str) -> Option<bool> {
    const TRUE: [&str; 3] = ["true", "on", "1"];
    const FALSE: [&str; 3] = ["false", "off", "0"];
    if TRUE.iter().any(|t| t.eq_ignore_ascii_case(text)) {
        Some(true)
    } else if FALSE.iter().any(|f| f.eq_ignore_ascii_case(text)) {
        Some(false)
    } else {
        None
    }
}

/// A number held to an inclusive range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ranged<T> {
    /// The value in range.
    pub value: T,
    /// The text was a number outside the range and `value` is the bound.
    pub clamped: bool,
}

/// Parse a finite float and clamp it to `[low, high]`.
pub fn parse_f32(text: &str, low: f32, high: f32) -> Option<Ranged<f32>> {
    let parsed: f32 = text.parse().ok().filter(|v: &f32| v.is_finite())?;
    let value = parsed.clamp(low, high);
    Some(Ranged {
        value,
        clamped: value != parsed,
    })
}

/// Parse an integer and clamp it to `[low, high]`. A negative or huge
/// integer is clamped too.
pub fn parse_u32(text: &str, low: u32, high: u32) -> Option<Ranged<u32>> {
    let parsed: i64 = text.parse().ok()?;
    let value = parsed.clamp(i64::from(low), i64::from(high));
    Some(Ranged {
        value: u32::try_from(value).ok()?,
        clamped: value != parsed,
    })
}

/// The mip bias: worked out from the render scale, left alone, or fixed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MipBias {
    /// Derived from the render scale (release behaviour).
    Auto,
    /// The game's own sampling.
    Off,
    /// This bias, in mip levels.
    Value(f32),
}

impl MipBias {
    /// The release behaviour.
    pub const DEFAULT: MipBias = MipBias::Auto;
    /// Range of a fixed bias.
    pub const RANGE: (f32, f32) = (-4.0, 4.0);

    /// `Auto`, `Off` or a number (clamped to [`MipBias::RANGE`]).
    pub fn parse(text: &str) -> Option<Ranged<MipBias>> {
        if text.eq_ignore_ascii_case("auto") {
            return Some(Ranged {
                value: MipBias::Auto,
                clamped: false,
            });
        }
        if text.eq_ignore_ascii_case("off") {
            return Some(Ranged {
                value: MipBias::Off,
                clamped: false,
            });
        }
        let number = parse_f32(text, Self::RANGE.0, Self::RANGE.1)?;
        Some(Ranged {
            value: MipBias::Value(number.value),
            clamped: number.clamped,
        })
    }
}

impl fmt::Display for MipBias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MipBias::Auto => f.write_str("Auto"),
            MipBias::Off => f.write_str("Off"),
            MipBias::Value(value) => write!(f, "{value:?}"),
        }
    }
}

/// `Auto`, or a forced boolean.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AutoBool {
    /// Decided by the mod.
    #[default]
    Auto,
    /// Forced to this value.
    Forced(bool),
}

impl AutoBool {
    /// `Auto` or a boolean.
    pub fn parse(text: &str) -> Option<AutoBool> {
        if text.eq_ignore_ascii_case("auto") {
            Some(AutoBool::Auto)
        } else {
            parse_bool(text).map(AutoBool::Forced)
        }
    }

    /// The forced value, or `auto` when not forced.
    pub fn resolve(self, auto: bool) -> bool {
        match self {
            AutoBool::Auto => auto,
            AutoBool::Forced(value) => value,
        }
    }
}

impl fmt::Display for AutoBool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AutoBool::Auto => f.write_str("Auto"),
            AutoBool::Forced(value) => write!(f, "{value}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    named_enum! {
        /// A test enumeration.
        pub enum Colour { Red = "Red", DarkBlue = "DarkBlue" }
        default Red
    }

    #[test]
    fn named_values_parse_case_insensitively() {
        assert_eq!(Colour::parse("darkblue"), Some(Colour::DarkBlue));
        assert_eq!(Colour::parse("RED"), Some(Colour::Red));
        assert_eq!(Colour::parse("green"), None);
        assert_eq!(Colour::DarkBlue.name(), "DarkBlue");
        assert_eq!(Colour::choices(), "Red | DarkBlue");
        assert_eq!(Colour::DEFAULT, Colour::default());
    }

    #[test]
    fn booleans_accept_the_documented_spellings_only() {
        for text in ["true", "On", "1", "TRUE"] {
            assert_eq!(parse_bool(text), Some(true), "{text}");
        }
        for text in ["false", "OFF", "0"] {
            assert_eq!(parse_bool(text), Some(false), "{text}");
        }
        for text in ["yes", "2", "", "tru"] {
            assert_eq!(parse_bool(text), None, "{text}");
        }
    }

    #[test]
    fn numbers_are_clamped_and_garbage_is_refused() {
        assert_eq!(
            parse_f32("0.75", 0.5, 1.0),
            Some(Ranged {
                value: 0.75,
                clamped: false
            })
        );
        assert_eq!(
            parse_f32("0.3", 0.5, 1.0),
            Some(Ranged {
                value: 0.5,
                clamped: true
            })
        );
        assert_eq!(
            parse_f32("1e9", 0.5, 1.0),
            Some(Ranged {
                value: 1.0,
                clamped: true
            })
        );
        for text in ["NaN", "inf", "-inf", "0,5", "abc", ""] {
            assert_eq!(parse_f32(text, 0.5, 1.0), None, "{text}");
        }
        assert_eq!(
            parse_u32("3", 2, 4),
            Some(Ranged {
                value: 3,
                clamped: false
            })
        );
        assert_eq!(
            parse_u32("-7", 2, 4),
            Some(Ranged {
                value: 2,
                clamped: true
            })
        );
        assert_eq!(
            parse_u32("99999999999999", 2, 4),
            Some(Ranged {
                value: 4,
                clamped: true
            })
        );
        assert_eq!(parse_u32("2.5", 2, 4), None);
    }

    #[test]
    fn mip_bias_and_auto_bool_parse_words_and_values() {
        assert_eq!(MipBias::parse("AUTO").map(|r| r.value), Some(MipBias::Auto));
        assert_eq!(MipBias::parse("off").map(|r| r.value), Some(MipBias::Off));
        assert_eq!(
            MipBias::parse("-1.5"),
            Some(Ranged {
                value: MipBias::Value(-1.5),
                clamped: false
            })
        );
        assert_eq!(
            MipBias::parse("-9"),
            Some(Ranged {
                value: MipBias::Value(-4.0),
                clamped: true
            })
        );
        assert_eq!(MipBias::parse("sharp"), None);
        assert_eq!(MipBias::Value(-1.0).to_string(), "-1.0");
        assert_eq!(AutoBool::parse("auto"), Some(AutoBool::Auto));
        assert_eq!(AutoBool::parse("on"), Some(AutoBool::Forced(true)));
        assert_eq!(AutoBool::parse("maybe"), None);
        assert!(AutoBool::Auto.resolve(true));
        assert!(!AutoBool::Forced(false).resolve(true));
    }
}
