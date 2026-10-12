//! `capture`: readback of the slot's resources through the capture
//! service, saved under `CaptureDir/<stamp>-r<request>/` with a JSON
//! sidecar, and sent as one binary part per resource and frame.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mhw_core::frame::{CameraFrame, FrameInfo, JitterSample};
use mhw_core::math::Mat4;
use serde_json::{json, Value};

use super::methods::frame_info;
use super::{resolve, Part, Shared};
use crate::analysis::Image;
use crate::exec::Ctx;
use crate::expr::float;
use crate::game::CaptureItem;
use crate::proto::{Params, Point, RpcError, RpcResult};
use crate::settings;

/// Resources a capture may name. `backbuffer` is accepted for the client's
/// sake but is not a slot resource, so asking for it reports what the slot
/// has instead.
pub const RESOURCES: &[&str] = &[
    "colour",
    "depth",
    "mv",
    "mv_raw",
    "pass_depth",
    "dlss_out",
    "hudless",
    "backbuffer",
    "game_mv",
    "game_depth",
];
/// Most frames one capture records.
pub const MAX_FRAMES: u32 = 16;
/// How long readbacks may take after the last copy was recorded.
const READBACK_TIMEOUT: Duration = Duration::from_secs(10);

/// What the slot looked like when a frame was recorded.
#[derive(Clone, Debug)]
pub(super) struct FrameMeta {
    pub frame: FrameInfo,
    pub camera: Option<CameraFrame>,
    pub jitter: Option<JitterSample>,
}

/// How long `frames` occurrences of a point may take (10 fps or better).
pub(super) fn points_timeout(frames: u32) -> Duration {
    Duration::from_secs(5) + Duration::from_millis(100) * frames
}

struct Registered<'a>(&'a Shared, u64);

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.0.images.unregister(self.1);
    }
}

fn no_service() -> RpcError {
    RpcError::not_ready("no capture service: not running inside the game")
}

/// Why `name` cannot be captured at `slot`: the mod's reason when it gave
/// one, else what the slot does have.
fn absent_error(name: &str, slot: &crate::SlotContext) -> RpcError {
    match slot.absent.iter().find(|(absent, _)| *absent == name) {
        Some((_, why)) => RpcError::not_ready(format!("{name} is not offered at the slot: {why}")),
        None => {
            let have: Vec<&str> = slot.resources.iter().map(|r| r.name).collect();
            RpcError::not_ready(format!(
                "{name} is not offered at the slot in this configuration (have: {})",
                have.join(", ")
            ))
        }
    }
}

/// The capture items for `names` at a slot occurrence. With `distinct`,
/// two names backed by the same resource are `not_ready`: comparing a
/// texture with itself proves nothing.
fn items_for(
    names: &[String],
    distinct: bool,
    ctx: &Ctx<'_>,
) -> RpcResult<Option<(*mut std::ffi::c_void, u64, Vec<CaptureItem>)>> {
    let Ctx::Slot { list, slot } = ctx else {
        return Ok(None);
    };
    let mut items = Vec::with_capacity(names.len());
    for name in names {
        let Some(r) = slot
            .resources
            .iter()
            .find(|r| r.name == name && !r.resource.is_null())
        else {
            return Err(absent_error(name, slot));
        };
        if let Some(same) = items
            .iter()
            .find(|i: &&CaptureItem| distinct && i.resource == r.resource)
        {
            return Err(RpcError::not_ready(format!(
                "{} and {name} are the same resource at the slot ({:p}): nothing to compare",
                same.name, r.resource
            )));
        }
        items.push(CaptureItem {
            name: name.clone(),
            resource: r.resource,
            state: r.state,
        });
    }
    Ok(Some((*list, slot.frame.frame, items)))
}

impl Shared {
    /// Record `names` at `frames` consecutive slots and wait for the
    /// readbacks. Per frame: what the slot looked like and the images, in
    /// `names` order. `distinct` as for `items_for`.
    pub(super) fn record_frames(
        self: &Arc<Self>,
        names: &[String],
        distinct: bool,
        frames: u32,
    ) -> RpcResult<Vec<(FrameMeta, Vec<Image>)>> {
        self.game.capture().ok_or_else(no_service)?;
        let request = self.request_id();
        let rx = self.images.register(request);
        let _registered = Registered(self, request);
        let me = self.clone();
        let wanted = names.to_vec();
        let metas = self.collect(
            Point::Slot,
            frames as usize,
            points_timeout(frames),
            move |ctx| {
                let Some((list, frame, items)) = items_for(&wanted, distinct, ctx)? else {
                    return Ok(None);
                };
                let service = me.game.capture().ok_or_else(no_service)?;
                // SAFETY: `list` is the slot's open list and the items its live
                // resources in their stated states (`on_slot`'s contract), for
                // the duration of this call.
                unsafe { service.record(list, request, frame, &items)? };
                let Ctx::Slot { slot, .. } = ctx else {
                    return Ok(None);
                };
                Ok(Some(FrameMeta {
                    frame: slot.frame,
                    camera: slot.camera,
                    jitter: slot.jitter,
                }))
            },
        )?;
        let expected = metas.len() * names.len();
        let deadline = Instant::now() + READBACK_TIMEOUT;
        let mut images: Vec<Image> = Vec::with_capacity(expected);
        while images.len() < expected {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(image) => images.push(image),
                Err(_) => {
                    return Err(RpcError::timeout(format!(
                        "{} of {expected} readbacks arrived within {} s",
                        images.len(),
                        READBACK_TIMEOUT.as_secs()
                    )))
                }
            }
        }
        Ok(metas
            .into_iter()
            .map(|meta| {
                let mine = names
                    .iter()
                    .filter_map(|n| {
                        let at = images
                            .iter()
                            .position(|i| i.frame == meta.frame.frame && &i.name == n)?;
                        Some(images.swap_remove(at))
                    })
                    .collect();
                (meta, mine)
            })
            .collect())
    }

    fn capture_root(&self) -> PathBuf {
        let live = self.host.settings().dev.capture_dir;
        if live.trim().is_empty() {
            resolve(&self.config.mod_folder, &self.config.capture_dir)
        } else {
            resolve(&self.config.mod_folder, Path::new(&live))
        }
    }

    pub(super) fn capture(
        self: &Arc<Self>,
        p: Params<'_>,
        point: Option<Point>,
    ) -> RpcResult<Vec<Part>> {
        if point.is_some_and(|p| p != Point::Slot) {
            return Err(RpcError::bad_request(
                "captures record on the engine's command list: point must be slot",
            ));
        }
        let names = p.strings("resources")?;
        if names.is_empty() {
            return Err(RpcError::bad_request("resources must be a non-empty list"));
        }
        if let Some(bad) = names.iter().find(|n| !RESOURCES.contains(&n.as_str())) {
            return Err(RpcError::bad_request(format!(
                "unknown resource {bad:?} (known: {})",
                RESOURCES.join(", ")
            )));
        }
        let frames = p.count("frames", 1, 1, MAX_FRAMES)?;
        let recorded = self.record_frames(&names, false, frames)?;
        let settings = settings::to_json(&self.host.settings());
        let dir = self.capture_root().join(format!(
            "{}-r{}",
            stamp(SystemTime::now()),
            self.request_id()
        ));
        let save_error = fs::create_dir_all(&dir).err().map(|e| e.to_string());
        let count = recorded
            .iter()
            .map(|(_, images)| images.len())
            .sum::<usize>();
        let mut parts = Vec::with_capacity(count);
        for (meta, images) in recorded {
            let sidecar = sidecar(&meta, &settings);
            for image in images {
                let mut result = json!({
                    "resource": image.name,
                    "frame": image.frame,
                    "index": parts.len(),
                    "count": count,
                    "format": image.format_name(),
                    "dxgi": image.format,
                    "width": image.width,
                    "height": image.height,
                    "row_pitch": image.row_pitch,
                    "point": Point::Slot.as_str(),
                    "sidecar": sidecar,
                });
                match save_error
                    .clone()
                    .map_or_else(|| save(&dir, &result, &image.bytes), Err)
                {
                    Ok(path) => result["saved"] = json!(path.display().to_string()),
                    Err(why) => {
                        result["saved"] = Value::Null;
                        result["save_error"] = json!(why);
                    }
                }
                parts.push(Part {
                    result,
                    binary: Some(image.bytes),
                });
            }
        }
        log::info!(target: "dev", "captured {count} resource(s) over {frames} frame(s) into {}", dir.display());
        Ok(parts)
    }
}

/// Write `<resource>_<frame>.raw` and `.json`; the path of the raw file.
fn save(dir: &Path, meta: &Value, bytes: &[u8]) -> Result<PathBuf, String> {
    let base = format!(
        "{}_{:06}",
        meta["resource"].as_str().unwrap_or("resource"),
        meta["frame"].as_u64().unwrap_or(0)
    );
    let raw = dir.join(format!("{base}.raw"));
    fs::write(&raw, bytes).map_err(|e| e.to_string())?;
    let mut json = meta.clone();
    json["raw"] = json!(format!("{base}.raw"));
    json["bytes"] = json!(bytes.len());
    let text = serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?;
    fs::write(dir.join(format!("{base}.json")), text).map_err(|e| e.to_string())?;
    Ok(raw)
}

fn mat(m: &Mat4) -> Value {
    Value::Array(m.to_array().iter().map(|v| float(f64::from(*v))).collect())
}

/// The sidecar: frame info, camera, jitter and settings of the frame.
pub(super) fn sidecar(meta: &FrameMeta, settings: &Value) -> Value {
    let camera = meta.camera.as_ref().map_or(Value::Null, |c| {
        json!({
            "frame": c.frame,
            "view": mat(&c.view),
            "proj": mat(&c.proj),
            "prev_view": mat(&c.prev_view),
            "prev_proj": mat(&c.prev_proj),
            "position": [c.position.x, c.position.y, c.position.z],
            "near": c.near,
            "far": c.far,
            "fov_y": c.fov_y,
        })
    });
    let jitter = meta.jitter.as_ref().map_or(Value::Null, |j| {
        json!({"frame": j.frame, "phase": j.phase, "px": [j.px.x, j.px.y], "units": [j.units.x, j.units.y]})
    });
    json!({"frame": frame_info(&meta.frame), "camera": camera, "jitter": jitter, "settings": settings})
}

/// `20261011-150405` (UTC) for a directory name.
pub(super) fn stamp(now: SystemTime) -> String {
    let ms = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let mut text = String::new();
    mhw_core::log::format::push_timestamp(&mut text, ms);
    // `2026-10-11T15:04:05.123`
    let digits: String = text.chars().take(19).filter(char::is_ascii_digit).collect();
    format!(
        "{}-{}",
        digits.get(..8).unwrap_or(""),
        digits.get(8..).unwrap_or("")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_sortable_utc() {
        let t = UNIX_EPOCH + Duration::from_millis(1_791_731_045_123);
        assert_eq!(stamp(t), "20261011-150405");
    }

    #[test]
    fn timeouts_grow_with_the_frame_count() {
        assert!(points_timeout(16) > points_timeout(1));
        assert!(points_timeout(1) >= Duration::from_secs(5));
    }
}
