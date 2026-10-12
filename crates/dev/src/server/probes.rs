//! The probes (`probe.*`) and `trace`: sampling at execution points, then
//! the arithmetic of `crate::analysis`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mhw_core::frame::ResetReason;
use mhw_core::math::CUT_THRESHOLDS;
use serde_json::{json, Value};

use super::capture::points_timeout;
use super::Shared;
use crate::analysis::jitter::{verdict, JitterRow};
use crate::analysis::{depth, mv, ColourStats};
use crate::exec::Ctx;
use crate::game::CaptureItem;
use crate::proto::{Params, Point, RpcError, RpcResult};
use crate::trace::clock;

/// Most frames a probe samples.
pub const MAX_FRAMES: u32 = 1024;
/// Most frames a capture-based probe (`hdr`, `mv`, `depth`) records.
pub const MAX_CAPTURE_FRAMES: u32 = 16;
/// Most frames one `trace` covers.
pub const MAX_TRACE_FRAMES: u32 = 256;

impl Shared {
    pub(super) fn probe_jitter(self: &Arc<Self>, p: Params<'_>) -> RpcResult<Value> {
        let frames = p.count("frames", 1, 1, MAX_FRAMES)?;
        let me = self.clone();
        let rows = self.collect(
            Point::Slot,
            frames as usize,
            points_timeout(frames),
            move |ctx| {
                let Ctx::Slot { slot, .. } = ctx else {
                    return Ok(None);
                };
                let Some(j) = slot.jitter else {
                    return Ok(None);
                };
                let proj = me.game.camera_projection();
                let row = JitterRow::new(
                    j.frame,
                    j.phase,
                    [j.units.x, j.units.y],
                    [j.px.x, j.px.y],
                    proj.map(|m| m[8]),
                    proj.map(|m| m[9]),
                );
                Ok(Some((row, slot.frame)))
            },
        )?;
        let Some((_, info)) = rows.last().copied() else {
            return Err(RpcError::internal("no samples"));
        };
        let rows: Vec<JitterRow> = rows.into_iter().map(|(r, _)| r).collect();
        let (v, divisor) = verdict(&rows, info.render, info.display);
        Ok(json!({
            "frames": rows,
            "verdict": v,
            "divisor": divisor,
            "render": [info.render.w, info.render.h],
            "display": [info.display.w, info.display.h],
            "rule": "divisor = -written / proj[8|9]; the engine divides by the display size (ReJitterCamera §2.4)",
        }))
    }

    pub(super) fn probe_reset(self: &Arc<Self>, p: Params<'_>) -> RpcResult<Value> {
        let frames = p.count("frames", 1, 1, MAX_FRAMES)?;
        let me = self.clone();
        let mut previous: Option<u64> = None;
        let rows = self.collect(
            Point::Slot,
            frames as usize,
            points_timeout(frames),
            move |ctx| {
                let Ctx::Slot { slot, .. } = ctx else {
                    return Ok(None);
                };
                let state = me.game.reset_state();
                let cut = me.stash().cut;
                let game_reset = match (previous, state.game_resets) {
                    (Some(before), Some(now)) => Some(now > before),
                    _ => None,
                };
                previous = state.game_resets;
                let cut = cut.map_or(Value::Null, |(frame, m)| {
                    json!({
                        "frame": frame,
                        "position_jump": m.position_jump,
                        "turn": m.turn,
                        "projection_change": m.projection_change,
                        "cut": m.exceeds(&CUT_THRESHOLDS),
                    })
                });
                Ok(Some(json!({
                    "frame": slot.frame.frame,
                    "reason": format!("{:?}", slot.frame.reset),
                    "reset_dlss": slot.frame.reset != ResetReason::None,
                    "game_reset": game_reset,
                    "game_resets": state.game_resets,
                    "history_invalid": state.history_invalid,
                    "camera_cut": cut,
                    "fg": state.fg.map(|(frame, reset)| json!({"frame": frame, "reset": reset})),
                    "reset_fg": state.fg.map(|(_, reset)| reset),
                })))
            },
        )?;
        Ok(json!({"frames": rows}))
    }

    pub(super) fn probe_hdr(self: &Arc<Self>, p: Params<'_>) -> RpcResult<Value> {
        let frames = p.count("frames", 1, 1, MAX_CAPTURE_FRAMES)?;
        self.game.capture().ok_or_else(|| {
            RpcError::not_ready("no capture service: not running inside the game")
        })?;
        let request = self.request_id();
        let rx = self.stats.register(request);
        let me = self.clone();
        let recorded = self.collect(
            Point::Slot,
            frames as usize,
            points_timeout(frames),
            move |ctx| {
                let Ctx::Slot { list, slot } = ctx else {
                    return Ok(None);
                };
                let colour = slot
                    .resources
                    .iter()
                    .find(|r| r.name == "colour" && !r.resource.is_null())
                    .ok_or_else(|| RpcError::not_ready("colour is not available at the slot"))?;
                let item = CaptureItem {
                    name: "colour".to_owned(),
                    resource: colour.resource,
                    state: colour.state,
                };
                let service = me
                    .game
                    .capture()
                    .ok_or_else(|| RpcError::not_ready("capture service gone"))?;
                // SAFETY: the slot's open list and its live colour resource in
                // the stated state (`on_slot`'s contract).
                unsafe { service.record_stats(*list, request, slot.frame.frame, &item)? };
                Ok(Some(slot.frame.frame))
            },
        );
        let result = recorded.and_then(|recorded| {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut rows: Vec<(u64, ColourStats)> = Vec::new();
            while rows.len() < recorded.len() {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(row) => rows.push(row),
                    Err(_) => {
                        return Err(RpcError::timeout(format!(
                            "{} of {} reductions arrived",
                            rows.len(),
                            recorded.len()
                        )))
                    }
                }
            }
            rows.sort_by_key(|(frame, _)| *frame);
            let rows: Vec<Value> = rows
                .iter()
                .map(|(frame, s)| {
                    json!({
                        "frame": frame,
                        "min": s.min, "max": s.max, "mean": s.mean,
                        "p50": s.p50, "p99": s.p99, "p999": s.p999,
                        "above_one": s.above_one, "nonfinite": s.nan_or_inf,
                    })
                })
                .collect();
            Ok(json!({"frames": rows}))
        });
        self.stats.unregister(request);
        result
    }

    pub(super) fn probe_mv(self: &Arc<Self>, p: Params<'_>) -> RpcResult<Value> {
        let frames = p.count("frames", 1, 1, MAX_CAPTURE_FRAMES)?;
        let recorded =
            self.record_frames(&["game_mv".to_owned(), "mv_raw".to_owned()], true, frames)?;
        let rows: Vec<Value> = recorded
            .iter()
            .map(|(meta, images)| {
                let frame = meta.frame.frame;
                let [game, raw] = images.as_slice() else {
                    return json!({"frame": frame, "error": "readback incomplete"});
                };
                let shapes = json!({
                    "game": {"format": game.format_name(), "size": [game.width, game.height]},
                    "raw": {"format": raw.format_name(), "size": [raw.width, raw.height]},
                });
                match mv::compare(game, raw, meta.frame.render) {
                    Ok(c) => {
                        let scale: Vec<Option<f64>> = c.fit.iter().map(|f| f.map(|f| f.scale)).collect();
                        let sign: Vec<Option<i8>> = c.fit.iter().map(|f| f.map(|f| f.sign)).collect();
                        let residual = c.fit.iter().flatten().map(|f| f.residual * f.residual).sum::<f64>().sqrt();
                        json!({
                            "frame": frame, "scale": scale, "sign": sign, "residual": residual,
                            "fit": c.fit, "scale_ndc": c.scale_ndc, "samples": c.samples, "textures": shapes,
                        })
                    }
                    Err(why) => json!({"frame": frame, "error": why, "textures": shapes}),
                }
            })
            .collect();
        Ok(json!({
            "frames": rows,
            "rule": "game ≈ scale · decoded tGBuffer4 in render pixels (ndc·(−w/2, +h/2)); scale_ndc against the raw NDC",
        }))
    }

    pub(super) fn probe_depth(self: &Arc<Self>, p: Params<'_>) -> RpcResult<Value> {
        let frames = p.count("frames", 1, 1, MAX_CAPTURE_FRAMES)?;
        // `pass_depth` is the temporal pass's DSV; `depth` in the
        // supersampling path is `game_depth` itself.
        let recorded = self.record_frames(
            &["game_depth".to_owned(), "pass_depth".to_owned()],
            true,
            frames,
        )?;
        let rows: Vec<Value> = recorded
            .iter()
            .map(|(meta, images)| match images.as_slice() {
                [copy, dsv] => {
                    let mut row =
                        serde_json::to_value(depth::compare(copy, dsv)).unwrap_or(Value::Null);
                    row["frame"] = json!(meta.frame.frame);
                    row
                }
                _ => json!({"frame": meta.frame.frame, "error": "readback incomplete"}),
            })
            .collect();
        Ok(json!({"frames": rows}))
    }

    pub(super) fn probe_wait(self: &Arc<Self>, p: Params<'_>) -> RpcResult<Value> {
        let seconds = p.seconds("seconds", 5.0, 0.0, 60.0)?;
        let span = Duration::from_secs_f64(seconds);
        let game = self.game.clone();
        let mut started: Option<Instant> = None;
        let mut tally = WaitTally::default();
        let result = self.collect(Point::Frame, 1, span + Duration::from_secs(5), move |_| {
            let begun = *started.get_or_insert_with(Instant::now);
            tally.add(game.wait_state());
            Ok((begun.elapsed() >= span).then(|| tally.clone()))
        })?;
        let tally = result.into_iter().next().unwrap_or_default();
        Ok(tally.report(seconds))
    }

    pub(super) fn trace(self: &Arc<Self>, p: Params<'_>) -> RpcResult<Value> {
        let frames = p.count("frames", 1, 1, MAX_TRACE_FRAMES)?;
        let _collecting = self.trace.collect();
        let starts = self.collect(
            Point::Frame,
            frames as usize + 1,
            points_timeout(frames + 1),
            |ctx| Ok(Some(ctx.frame())),
        )?;
        let first = starts.first().copied().unwrap_or(0);
        Ok(self
            .trace
            .timeline(first..first + u64::from(frames), clock::frequency()))
    }
}

/// `probe.wait`: what the wait state did over the sampled frames.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct WaitTally {
    frames: u64,
    samples: u64,
    latched: u64,
    presented: u64,
    last: Option<crate::game::WaitSample>,
}

impl WaitTally {
    fn add(&mut self, sample: Option<crate::game::WaitSample>) {
        self.frames += 1;
        if let Some(s) = sample {
            self.samples += 1;
            self.latched += u64::from(s.latched);
            self.presented += u64::from(s.presented);
            self.last = Some(s);
        }
    }

    fn report(&self, seconds: f64) -> Value {
        json!({
            "seconds": seconds,
            "frames": self.frames,
            "samples": self.samples,
            "latch": self.last.map(|s| s.latched),
            "latched_frames": self.latched,
            "presented_frames": self.presented,
            "waitable": self.last.map(|s| format!("{:#x}", s.waitable)),
            "calls": null,
            "waits": null,
            "timeouts": null,
            "note": "the wait helper 0x1423ade30 is not a gate site: calls, waits and timeouts need its detour; \
                     sampled per frame: device+0x10d9 (presented), +0x10da (latch), +0x10f0 (waitable)",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::WaitSample;

    #[test]
    fn the_wait_tally_counts_frames_and_states() {
        let mut t = WaitTally::default();
        t.add(None);
        t.add(Some(WaitSample {
            presented: true,
            latched: false,
            waitable: 0x40,
        }));
        t.add(Some(WaitSample {
            presented: true,
            latched: true,
            waitable: 0x40,
        }));
        let r = t.report(1.5);
        assert_eq!(
            (r["frames"].as_u64(), r["samples"].as_u64()),
            (Some(3), Some(2))
        );
        assert_eq!(
            (r["latched_frames"].as_u64(), r["presented_frames"].as_u64()),
            (Some(1), Some(2))
        );
        assert_eq!(r["latch"], true);
        assert_eq!(r["waitable"], "0x40");
        assert!(r.get("timeouts").is_some());
    }
}
