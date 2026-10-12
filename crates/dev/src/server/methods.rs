//! Dispatch and the methods that need no capture: hello, state, settings,
//! symbols, memory, watches and the log.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mhw_core::log::ring;
use serde_json::{json, Value};

use super::{Conn, LogSub, Part, Shared};
use crate::exec::Step;
use crate::expr::{decode, encode, hex, Expr, ValueType};
use crate::logline::{level_name, parse_level, LogFilter};
use crate::proto::{Method, Params, Point, Request, RpcError, RpcResult};
use crate::settings;
use crate::symbols::Symbols;

/// Longest `mem.read`.
pub const MAX_READ: usize = 1 << 20;
/// How long a one-shot request waits for its point.
pub const POINT_TIMEOUT: Duration = Duration::from_secs(5);

impl Shared {
    pub(super) fn handle(
        self: &Arc<Self>,
        conn: &Arc<Conn>,
        req: &Request,
    ) -> RpcResult<Vec<Part>> {
        let p = Params(&req.params);
        match req.method {
            Method::Auth => Ok(Part::json(self.auth_info())),
            Method::Hello => Ok(Part::json(self.hello())),
            Method::State => {
                let mut state = self.state();
                state["dev"]["dropped_events"] = json!(conn.dropped());
                Ok(Part::json(state))
            }
            Method::SettingsGet => Ok(Part::json(settings::to_json(&self.host.settings()))),
            Method::SettingsSet => self.settings_set(&req.params).map(Part::json),
            Method::ExperimentSet => {
                let change = json!({"section": "Experimental", "key": p.str("key")?, "value": p.value("value")?});
                self.settings_set(&change).map(Part::json)
            }
            Method::Symbols => Ok(Part::json(self.symbols()?.to_json())),
            Method::MemRead => self.mem_read(p, req.point.unwrap_or(Point::Frame)),
            Method::MemWrite => self
                .mem_write(p, req.point.unwrap_or(Point::Frame))
                .map(Part::json),
            Method::Watch => self
                .watch(conn, p, req.point.unwrap_or(Point::Frame))
                .map(Part::json),
            Method::Unwatch => {
                let name = p.str("name")?;
                let stop = conn.watches.lock().remove(name);
                let stop =
                    stop.ok_or_else(|| RpcError::bad_request(format!("no watch named {name:?}")))?;
                stop.store(true, Ordering::Release);
                Ok(Part::json(json!({"name": name})))
            }
            Method::LogSubscribe => log_subscribe(conn, p).map(Part::json),
            Method::LogTail => {
                // More than the ring holds is not an error: the client gets all of it.
                let lines = p.count("lines", 100, 1, 100_000)?;
                let lines: Vec<String> = ring::tail(lines as usize)
                    .into_iter()
                    .map(|e| e.line)
                    .collect();
                Ok(Part::json(json!({"lines": lines})))
            }
            Method::Capture => self.capture(p, req.point),
            Method::ProbeJitter => self.probe_jitter(p).map(Part::json),
            Method::ProbeReset => self.probe_reset(p).map(Part::json),
            Method::ProbeHdr => self.probe_hdr(p).map(Part::json),
            Method::ProbeMv => self.probe_mv(p).map(Part::json),
            Method::ProbeDepth => self.probe_depth(p).map(Part::json),
            Method::ProbeWait => self.probe_wait(p).map(Part::json),
            Method::ProbeModules => self.game.modules().map(Part::json),
            Method::Trace => self.trace(p).map(Part::json),
        }
    }

    fn hello(&self) -> Value {
        let mut subsystems = self.game.subsystems();
        subsystems["dev"] = json!(true);
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "commit": option_env!("MHW_DLSS_COMMIT").unwrap_or("unknown"),
            "game_build": self.game.game_build(),
            "gate": self.game.gate(),
            "subsystems": subsystems,
            "features": self.auth_info()["features"],
        })
    }

    fn state(&self) -> Value {
        let mut out = match self.host.state() {
            Value::Object(map) => Value::Object(map),
            other => json!({"host": other}),
        };
        let crate::server::Stash {
            camera,
            jitter,
            cut,
            slot,
        } = self.stash();
        out["frame"] = json!(self.last_frame());
        out["settings"] = settings::to_json(&self.host.settings());
        out["last_slot"] = slot.map_or(Value::Null, |f| frame_info(&f));
        out["last_jitter"] = jitter.map_or(Value::Null, |j| {
            json!({"frame": j.frame, "phase": j.phase, "px": [j.px.x, j.px.y], "units": [j.units.x, j.units.y]})
        });
        out["last_camera_frame"] = json!(camera.map(|c| c.frame));
        out["last_camera_cut"] = cut.map_or(Value::Null, |(frame, m)| {
            json!({"frame": frame, "position_jump": m.position_jump, "turn": m.turn, "projection_change": m.projection_change})
        });
        out["introspection"] = self.game.introspect();
        out["dev"] = json!({
            "listen": self.local_addr.to_string(),
            "clients": self.connections().len(),
            "pending": Point::ALL.iter().map(|p| (p.as_str(), self.queue.pending(*p))).collect::<std::collections::BTreeMap<_, _>>(),
        });
        out
    }

    fn settings_set(&self, params: &Value) -> RpcResult<Value> {
        let changes = settings::parse_changes(params)?;
        let staged = settings::stage(&self.host.settings(), &changes)?;
        for (i, s) in staged.iter().enumerate() {
            if let Err(d) = self.host.set_setting(&s.section, &s.key, &s.value) {
                let applied: Vec<String> = staged[..i]
                    .iter()
                    .map(|s| format!("[{}] {}", s.section, s.key))
                    .collect();
                return Err(RpcError::internal(format!(
                    "the mod rejected [{}] {} after validation: {d} (already applied: {})",
                    s.section,
                    s.key,
                    if applied.is_empty() {
                        "none".to_owned()
                    } else {
                        applied.join(", ")
                    }
                )));
            }
            log::info!(target: "dev", "setting [{}] {} = {} (dev client)", s.section, s.key, s.value);
        }
        Ok(settings::result(&staged, &self.host.settings()))
    }

    fn symbols(&self) -> RpcResult<Symbols> {
        self.game.symbols().ok_or_else(|| {
            RpcError::not_ready("the engine's addresses are not resolved (gate not passed)")
        })
    }

    /// Resolve `expr`; symbols are only looked up when it names one.
    fn address(&self, expr: &Expr) -> RpcResult<u64> {
        match &expr.base {
            crate::expr::Base::Addr(_) => expr.address(|_| None),
            crate::expr::Base::Symbol(_) => {
                let book = self.symbols()?;
                expr.address(|name| book.lookup(name))
            }
        }
    }

    fn mem_read(&self, p: Params<'_>, point: Point) -> RpcResult<Vec<Part>> {
        let expr = Expr::from_value(p.0)?;
        let ty = expr.ty.filter(|t| *t != ValueType::Bytes);
        let size = ty.map_or(1, ValueType::size);
        let len = if p.has("len") {
            p.int("len", 0, 1, MAX_READ as i64)? as usize
        } else if ty.is_some() {
            size
        } else {
            return Err(RpcError::bad_request("a raw read needs len"));
        };
        if !len.is_multiple_of(size) {
            return Err(RpcError::bad_request(format!(
                "len must be a multiple of {size}"
            )));
        }
        let address = self.address(&expr)?;
        let game = self.game.clone();
        let bytes = self
            .collect(point, 1, POINT_TIMEOUT, move |_| {
                let mut buf = vec![0u8; len];
                if game.read(address, &mut buf) {
                    Ok(Some(buf))
                } else {
                    Err(RpcError::refused(format!(
                        "{len} bytes at {address:#x} are not readable"
                    )))
                }
            })?
            .pop()
            .unwrap_or_default();
        Ok(match ty {
            None => vec![Part {
                result: json!({"addr": hex(address), "len": len}),
                binary: Some(bytes),
            }],
            Some(ty) => Part::json(
                json!({"addr": hex(address), "as": ty.name(), "values": decode(&bytes, ty)}),
            ),
        })
    }

    fn mem_write(&self, p: Params<'_>, point: Point) -> RpcResult<Value> {
        let expr = Expr::from_value(p.0)?;
        let ty = expr
            .ty
            .ok_or_else(|| RpcError::bad_request("mem.write needs as (a type)"))?;
        let bytes = encode(p.value("value")?, ty)?;
        let force = p.bool("force", false)?;
        let address = self.address(&expr)?;
        let book = self.game.symbols();
        match &book {
            Some(book) => book.check_write(address, bytes.len() as u64, force)?,
            None if !force => {
                return Err(RpcError::not_ready(
                    "no engine structs resolved; force:true writes anyway",
                ))
            }
            None => {}
        }
        let game = self.game.clone();
        let written = bytes.len();
        self.collect(point, 1, POINT_TIMEOUT, move |_| {
            game.write(address, &bytes)
                .map(Some)
                .map_err(|why| RpcError::refused(format!("write at {address:#x} failed: {why}")))
        })?;
        log::info!(target: "dev", "dev client wrote {written} bytes at {address:#x}{}", if force { " (forced)" } else { "" });
        Ok(json!({"addr": hex(address), "written": written}))
    }

    fn watch(&self, conn: &Arc<Conn>, p: Params<'_>, point: Point) -> RpcResult<Value> {
        let name = p.str("name")?.to_owned();
        let every = p.count("every_frames", 1, 1, 10_000)?;
        let items = p
            .value("expr")?
            .as_array()
            .filter(|items| !items.is_empty())
            .ok_or_else(|| RpcError::bad_request("expr must be a non-empty list"))?;
        let mut targets = Vec::with_capacity(items.len());
        for item in items {
            let expr = Expr::from_value(item)?;
            let ty = expr.ty.filter(|t| *t != ValueType::Bytes).ok_or_else(|| {
                RpcError::bad_request(format!("watch expression {expr} needs a type"))
            })?;
            let address = self.address(&expr)?;
            if !self.game.read(address, &mut vec![0u8; ty.size()]) {
                return Err(RpcError::refused(format!(
                    "{expr} ({address:#x}) is not readable"
                )));
            }
            targets.push((address, ty));
        }
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(old) = conn.watches.lock().insert(name.clone(), stop.clone()) {
            old.store(true, Ordering::Release);
        }
        let weak = Arc::downgrade(conn);
        let game = self.game.clone();
        let count = targets.len();
        let mut seen = 0u32;
        let event_name = name.clone();
        self.queue.push(
            point,
            Box::new(move |ctx| {
                let Some(conn) = weak.upgrade().filter(|c| c.is_alive()) else {
                    return Step::Done;
                };
                if stop.load(Ordering::Acquire) {
                    return Step::Done;
                }
                seen += 1;
                if seen < every {
                    return Step::More;
                }
                seen = 0;
                let values: Vec<Value> = targets
                    .iter()
                    .map(|&(address, ty)| {
                        let mut buf = vec![0u8; ty.size()];
                        if game.read(address, &mut buf) {
                            decode(&buf, ty).pop().unwrap_or(Value::Null)
                        } else {
                            Value::Null
                        }
                    })
                    .collect();
                let event = json!({
                    "event": "watch",
                    "name": event_name,
                    "frame": ctx.frame(),
                    "point": ctx.point().as_str(),
                    "values": values,
                });
                if conn.watch_event(event) {
                    Step::More
                } else {
                    Step::Done
                }
            }),
        );
        Ok(json!({"name": name, "count": count, "every_frames": every, "point": point.as_str()}))
    }
}

fn log_subscribe(conn: &Conn, p: Params<'_>) -> RpcResult<Value> {
    let level_text = p.opt_str("level")?.unwrap_or("info");
    let level = parse_level(level_text).ok_or_else(|| {
        RpcError::bad_request("level must be one of error, warn, info, debug, trace")
    })?;
    let targets = p.strings("targets")?;
    let unknown: Vec<&String> = targets
        .iter()
        .filter(|t| !mhw_core::log::TARGETS.contains(&t.as_str()))
        .collect();
    if !unknown.is_empty() {
        return Err(RpcError::bad_request(format!(
            "unknown targets {unknown:?}"
        )));
    }
    *conn.log.lock() = Some(LogSub {
        filter: LogFilter {
            level,
            targets: targets.clone(),
        },
        next: ring::next_seq(),
        dropped: 0,
    });
    Ok(json!({"level": level_name(level), "targets": targets}))
}

/// A `FrameInfo` as JSON.
pub(crate) fn frame_info(f: &mhw_core::frame::FrameInfo) -> Value {
    json!({
        "number": f.frame,
        "render": [f.render.w, f.render.h],
        "display": [f.display.w, f.display.h],
        "scale": f.scale,
        "reset": format!("{:?}", f.reset),
    })
}
