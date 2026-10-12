//! `settings.get` / `settings.set` / `experiment.set`: the settings as JSON
//! (section → key → value, file names and spellings) and validation of a
//! batch of changes before any of it is applied.

use mhw_core::config::{Build, LevelFilter, Named, Settings};
use serde_json::{json, Map, Value};

use crate::expr::float;
use crate::proto::{RpcError, RpcResult};

fn level_name(level: LevelFilter) -> &'static str {
    match level {
        LevelFilter::Off => "Off",
        LevelFilter::Error => "Error",
        LevelFilter::Warn => "Warn",
        LevelFilter::Info => "Info",
        LevelFilter::Debug => "Debug",
        LevelFilter::Trace => "Trace",
    }
}

/// An `f32` as the decimal the file would hold (`0.667`, not
/// `0.6669999957`).
fn decimal(x: f32) -> Value {
    format!("{x}").parse::<f64>().map_or(Value::Null, float)
}

/// Every setting, as the file names them.
pub fn to_json(s: &Settings) -> Value {
    let e = &s.experimental;
    let log_targets: Vec<String> = s
        .dev
        .log_targets
        .iter()
        .map(|t| format!("{}={}", t.target, level_name(t.level).to_ascii_lowercase()))
        .collect();
    let mip_bias = match e.mip_bias {
        mhw_core::config::MipBias::Value(v) => decimal(v),
        other => json!(other.to_string()),
    };
    let dlss_hdr = match e.dlss_hdr {
        mhw_core::config::AutoBool::Forced(b) => json!(b),
        mhw_core::config::AutoBool::Auto => json!("Auto"),
    };
    json!({
        "General": {
            "Enabled": s.general.enabled,
            "LogLevel": level_name(s.general.log_level),
        },
        "Upscaling": {
            "Mode": s.upscaling.mode.name(),
            "RenderScale": decimal(s.upscaling.render_scale),
            "Preset": s.upscaling.preset.name(),
            "Sharpness": decimal(s.upscaling.sharpness),
            "DownsampleVolume": s.upscaling.downsample_volume,
        },
        "FrameGeneration": {
            "Enabled": s.frame_generation.enabled,
            "Multiplier": s.frame_generation.multiplier,
            "ReflexBoost": s.frame_generation.reflex_boost,
        },
        "Dev": {
            "Listen": s.dev.listen.to_string(),
            "TokenFile": s.dev.token_file,
            "LogTargets": log_targets.join(","),
            "DebugLayer": s.dev.debug_layer,
            "Dred": s.dev.dred,
            "CaptureDir": s.dev.capture_dir,
            "DumpShaders": s.dev.dump_shaders,
        },
        "Experimental": {
            "InputPath": e.input_path.name(),
            "JitterScale": e.jitter_scale.name(),
            "JitterPhases": e.jitter_phases,
            "HudLess": e.hud_less,
            "TaaSharpen": e.taa_sharpen.name(),
            "HoldAntiAliasing": e.hold_anti_aliasing,
            "WaitLatch": e.wait_latch.name(),
            "Reset": e.reset.name(),
            "MipBias": mip_bias,
            "DlssHdr": dlss_hdr,
        },
    })
}

/// One requested change, values in INI syntax.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// Section as requested.
    pub section: String,
    /// Key as requested.
    pub key: String,
    /// Value as the file would hold it.
    pub value: String,
}

/// A JSON scalar in INI syntax.
pub fn ini_value(value: &Value) -> RpcResult<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        _ => Err(RpcError::bad_request(
            "a setting value is a string, number or boolean",
        )),
    }
}

/// `{section, key, value}` or a list of them.
pub fn parse_changes(params: &Value) -> RpcResult<Vec<Change>> {
    let items = match params {
        Value::Array(items) if !items.is_empty() => items.as_slice(),
        Value::Array(_) => return Err(RpcError::bad_request("no changes")),
        single => std::slice::from_ref(single),
    };
    items
        .iter()
        .map(|item| {
            let text = |key: &str| {
                item.get(key)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| RpcError::bad_request("each change needs section, key, value"))
            };
            let value = item
                .get("value")
                .ok_or_else(|| RpcError::bad_request("each change needs section, key, value"))?;
            Ok(Change {
                section: text("section")?,
                key: text("key")?,
                value: ini_value(value)?,
            })
        })
        .collect()
}

/// A change that passed validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    /// Section, file spelling.
    pub section: String,
    /// Key, file spelling.
    pub key: String,
    /// Value in INI syntax as requested.
    pub value: String,
    /// Takes effect only after a restart.
    pub restart: bool,
    /// Applied with a note (clamped, no effect).
    pub note: Option<String>,
}

/// The file spelling of `section`/`key`, if such a setting exists.
pub fn canonical(current: &Value, section: &str, key: &str) -> Option<(String, String)> {
    let (s, keys) = current
        .as_object()?
        .iter()
        .find(|(s, _)| s.eq_ignore_ascii_case(section))?;
    let (k, _) = keys
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))?;
    Some((s.clone(), k.clone()))
}

/// Validate `changes` in order against a copy of `current`, as the file
/// would (same parser). All or nothing: the first rejected change fails
/// the batch.
pub fn stage(current: &Settings, changes: &[Change]) -> RpcResult<Vec<Staged>> {
    let names = to_json(current);
    let mut working = current.clone();
    changes
        .iter()
        .map(|c| {
            let (section, key) = canonical(&names, &c.section, &c.key).ok_or_else(|| {
                RpcError::bad_request(format!("unknown setting [{}] {}", c.section, c.key))
            })?;
            let before = working.clone();
            let note = working
                .set(&section, &key, &c.value, Build::CURRENT)
                .map_err(|d| RpcError::bad_request(d.to_string()))?;
            Ok(Staged {
                restart: Settings::diff(&before, &working).needs_restart(),
                note: note.map(|d| d.to_string()),
                section,
                key,
                value: c.value.clone(),
            })
        })
        .collect()
}

/// The `settings.set` result: each change with the value now in force.
pub fn result(staged: &[Staged], now: &Settings) -> Value {
    let now = to_json(now);
    let changes: Vec<Value> = staged
        .iter()
        .map(|s| {
            let mut c = Map::new();
            c.insert("section".into(), json!(s.section));
            c.insert("key".into(), json!(s.key));
            c.insert("value".into(), now[&s.section][&s.key].clone());
            c.insert("restart".into(), json!(s.restart));
            if let Some(note) = &s.note {
                c.insert("note".into(), json!(note));
            }
            Value::Object(c)
        })
        .collect();
    json!({
        "changes": changes,
        "restart_required": staged.iter().any(|s| s.restart),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_serialise_with_file_names() {
        let v = to_json(&Settings::default());
        assert_eq!(v["Upscaling"]["RenderScale"], json!(0.667));
        assert_eq!(v["Upscaling"]["Mode"], "Quality");
        assert_eq!(v["General"]["LogLevel"], "Info");
        assert_eq!(v["Dev"]["Listen"], "127.0.0.1:47800");
        assert_eq!(v["Experimental"]["MipBias"], "Auto");
        assert_eq!(v["Experimental"]["DlssHdr"], "Auto");
        assert_eq!(v["Experimental"]["JitterPhases"], 32);
        // Every key the file writer knows is present.
        let file = Settings::write_default(Build::CURRENT);
        let mut section = String::new();
        for line in file.lines().map(str::trim) {
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.to_owned();
            } else if let Some((key, _)) = line
                .split_once('=')
                .filter(|_| !line.starts_with([';', '#']))
            {
                assert!(
                    canonical(&v, &section, key.trim()).is_some(),
                    "[{section}] {key}"
                );
            }
        }
    }

    #[test]
    fn changes_parse_from_one_object_or_a_list() {
        let one = parse_changes(&json!({"section": "Upscaling", "key": "Sharpness", "value": 0.5}))
            .unwrap();
        assert_eq!(one[0].value, "0.5");
        let list = parse_changes(&json!([
            {"section": "a", "key": "b", "value": true},
            {"section": "a", "key": "c", "value": "Render"},
        ]))
        .unwrap();
        assert_eq!(
            (list[0].value.as_str(), list[1].value.as_str()),
            ("true", "Render")
        );
        assert!(parse_changes(&json!([])).is_err());
        assert!(parse_changes(&json!({"section": "a", "key": "b"})).is_err());
        assert!(parse_changes(&json!({"section": "a", "key": "b", "value": null})).is_err());
    }

    #[test]
    fn staging_validates_like_the_file_and_reports_restarts() {
        let s = Settings::default();
        let staged = stage(
            &s,
            &[
                Change {
                    section: "experimental".into(),
                    key: "jitterscale".into(),
                    value: "render".into(),
                },
                Change {
                    section: "FrameGeneration".into(),
                    key: "Enabled".into(),
                    value: "on".into(),
                },
                Change {
                    section: "Upscaling".into(),
                    key: "Sharpness".into(),
                    value: "7".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            (staged[0].section.as_str(), staged[0].key.as_str()),
            ("Experimental", "JitterScale")
        );
        assert_eq!(
            staged.iter().map(|c| c.restart).collect::<Vec<_>>(),
            [false, true, false]
        );
        assert!(
            staged[2].note.as_deref().is_some_and(|n| !n.is_empty()),
            "clamped"
        );
    }

    #[test]
    fn one_bad_change_rejects_the_batch() {
        let s = Settings::default();
        let good = Change {
            section: "Upscaling".into(),
            key: "Mode".into(),
            value: "DLAA".into(),
        };
        let bad = Change {
            section: "Upscaling".into(),
            key: "Mode".into(),
            value: "Ultra".into(),
        };
        assert!(stage(&s, &[good.clone(), bad]).is_err());
        let unknown = Change {
            section: "Upscaling".into(),
            key: "Nope".into(),
            value: "1".into(),
        };
        assert!(stage(&s, &[good, unknown])
            .unwrap_err()
            .message
            .contains("Nope"));
    }

    #[test]
    fn the_result_reports_values_in_force() {
        let mut now = Settings::default();
        now.experimental.jitter_phases = 64;
        let staged = [Staged {
            section: "Experimental".into(),
            key: "JitterPhases".into(),
            value: "64".into(),
            restart: false,
            note: None,
        }];
        let r = result(&staged, &now);
        assert_eq!(r["changes"][0]["value"], 64);
        assert_eq!(r["restart_required"], false);
    }
}
