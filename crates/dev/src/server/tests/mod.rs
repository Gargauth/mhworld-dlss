//! The server on localhost against a fake mod and a fake game, driven with
//! JSON lines like `tools/devctl` does. No game, no GPU.

use std::ffi::c_void;
use std::io::BufWriter;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::Duration;

use mhw_core::config::Settings;
use mhw_core::frame::{CameraFrame, FrameInfo, JitterSample, ResetReason};
use mhw_core::math::{engine_jitter_px, engine_jitter_units, JitterScale, Mat4};
use parking_lot::Mutex;
use serde_json::{json, Value};

use super::Shared;
use crate::frame::{write_frame, Frame, FrameReader};
use crate::{DevConfig, SlotContext, SlotResource};

mod fake;
mod harden;

use fake::{projection, FakeGame, FakeHost, DISPLAY, RENDER};

/// A running server with a fake frame loop.
struct Rig {
    shared: Arc<Shared>,
    token: String,
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    game: Arc<FakeGame>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

static LOGGER: LazyLock<()> = LazyLock::new(|| {
    let dir = std::env::temp_dir().join(format!("mhw-dev-log-{}", std::process::id()));
    let _ = mhw_core::log::init(mhw_core::log::LogConfig {
        dirs: vec![dir],
        level: mhw_core::log::LevelFilter::Debug,
        targets: Vec::new(),
        profile: String::new(),
        max_file_bytes: 1 << 20,
    });
});

/// The log ring is the process's. A test that floods it with long lines and
/// one that reads it back must not overlap: the tail would be the flood,
/// past the client's line limit, and the marker pushed out of the ring.
static LOG_RING: Mutex<()> = Mutex::new(());

fn rig(name: &str, ticking: bool) -> Rig {
    LazyLock::force(&LOGGER);
    let dir = std::env::temp_dir().join(format!("mhw-dev-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let host = Arc::new(FakeHost {
        settings: Mutex::new(Settings::default()),
    });
    let game = Arc::new(FakeGame::new());
    let config = DevConfig {
        mod_folder: dir.clone(),
        listen: "127.0.0.1:0".parse().unwrap(),
        token_file: "dev.token".into(),
        capture_dir: "captures".into(),
    };
    let shared = Shared::bind(config, host, game.clone()).unwrap();
    let token = std::fs::read_to_string(dir.join("dev.token"))
        .unwrap()
        .trim()
        .to_owned();
    let stop = Arc::new(AtomicBool::new(false));
    if ticking {
        let (s, g, stop) = (shared.clone(), game.clone(), stop.clone());
        thread::spawn(move || tick(&s, &g, &stop));
    }
    Rig {
        shared,
        token,
        dir,
        stop,
        game,
    }
}

fn tick(s: &Shared, game: &FakeGame, stop: &AtomicBool) {
    let sequence = mhw_core::math::JitterSequence::new(8);
    let mut frame = 100u64;
    while !stop.load(Ordering::Acquire) {
        s.on_frame(frame);
        let px = sequence.offset(sequence.phase(frame));
        let units = engine_jitter_units(px, RENDER, DISPLAY, JitterScale::Display);
        *game.jitter.lock() = units;
        let jitter = JitterSample {
            px: engine_jitter_px(units, RENDER, DISPLAY),
            units,
            phase: sequence.phase(frame),
            frame,
        };
        let p = projection();
        let camera = CameraFrame::new(Mat4::IDENTITY, p, Mat4::IDENTITY, p, frame).unwrap();
        s.on_camera(&camera, &jitter);
        if frame.is_multiple_of(5) {
            *game.resets.lock() += 1;
        }
        let offer = game.offer.lock();
        let slot = SlotContext {
            frame: FrameInfo {
                frame,
                render: RENDER,
                display: DISPLAY,
                scale: 0.5,
                reset: if frame.is_multiple_of(5) {
                    ResetReason::GameHistory
                } else {
                    ResetReason::None
                },
            },
            camera: Some(camera),
            jitter: Some(jitter),
            resources: offer
                .resources
                .iter()
                .map(|&(name, resource)| SlotResource {
                    name,
                    resource: resource as *mut c_void,
                    state: 0x40,
                })
                .collect(),
            absent: offer.absent.clone(),
        };
        drop(offer);
        s.on_slot(std::ptr::null_mut(), &slot);
        s.on_present(frame);
        frame += 1;
        thread::sleep(Duration::from_millis(2));
    }
}

/// A JSON-lines client.
struct Client {
    out: BufWriter<TcpStream>,
    reader: FrameReader<TcpStream>,
    next: u64,
    events: Vec<Value>,
}

impl Client {
    fn connect(rig: &Rig) -> Client {
        Client::from_stream(TcpStream::connect(rig.shared.local_addr).unwrap())
    }

    fn from_stream(stream: TcpStream) -> Client {
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        Client {
            out: BufWriter::new(stream.try_clone().unwrap()),
            reader: FrameReader::new(stream),
            next: 1,
            events: Vec::new(),
        }
    }

    fn authed(rig: &Rig) -> Client {
        let mut c = Client::connect(rig);
        let auth = c.call("auth", json!({"token": rig.token}));
        assert_eq!(auth[0].msg["ok"], true, "{:?}", auth[0].msg);
        c
    }

    fn send(&mut self, msg: Value) {
        write_frame(&mut self.out, &msg, None).unwrap();
    }

    fn read(&mut self) -> Option<Frame> {
        self.reader.read_frame().ok().flatten()
    }

    /// Every part of the answer to one request; events are set aside.
    fn call(&mut self, method: &str, params: Value) -> Vec<Frame> {
        let id = self.next;
        self.next += 1;
        self.send(json!({"id": id, "method": method, "params": params}));
        let mut parts = Vec::new();
        loop {
            let frame = self.read().expect("connection closed");
            if frame.msg.contains_key("event") {
                self.events.push(Value::Object(frame.msg));
                continue;
            }
            assert_eq!(frame.msg["id"], id);
            let more = frame.msg.get("more") == Some(&json!(true));
            parts.push(frame);
            if !more {
                return parts;
            }
        }
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let parts = self.call(method, params);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].msg["ok"], true, "{method}: {:?}", parts[0].msg);
        parts[0].msg["result"].clone()
    }

    fn err(&mut self, method: &str, params: Value) -> String {
        let parts = self.call(method, params);
        assert_eq!(parts[0].msg["ok"], false, "{method}: {:?}", parts[0].msg);
        parts[0].msg["error"]["code"].as_str().unwrap().to_owned()
    }

    fn event(&mut self, kind: &str) -> Value {
        if let Some(at) = self.events.iter().position(|e| e["event"] == kind) {
            return self.events.remove(at);
        }
        loop {
            let frame = self.read().expect("no event");
            if frame.msg["event"] == kind {
                return Value::Object(frame.msg);
            }
        }
    }
}

#[test]
fn authentication_is_required_first() {
    let rig = rig("auth", false);
    let mut c = Client::connect(&rig);
    assert_eq!(c.err("state", json!({})), "unauthorized");
    assert!(c.read().is_none(), "closed after a request before auth");
    let mut c = Client::connect(&rig);
    assert_eq!(
        c.err("auth", json!({"token": "wrong-token-value"})),
        "unauthorized"
    );
    assert!(c.read().is_none());
    let mut c = Client::connect(&rig);
    let info = c.ok("auth", json!({"token": rig.token}));
    assert_eq!(info["build"], "dev");
    assert_eq!(
        info["game_build"],
        "timestamp 0x66d13cb6, image size 0x55d3000"
    );
    assert_eq!(rig.token.len(), 64);
}

#[test]
fn framing_errors_are_answered_and_the_connection_survives() {
    let rig = rig("framing", false);
    let mut c = Client::authed(&rig);
    std::io::Write::write_all(c.out.get_mut(), b"{not json\n").unwrap();
    let bad = c.read().unwrap();
    assert_eq!(
        (bad.msg["id"].clone(), bad.msg["error"]["code"].clone()),
        (Value::Null, json!("bad_request"))
    );
    assert_eq!(c.err("nope", json!({})), "unknown_method");
    assert_eq!(c.err("watch", json!({"point": "draw"})), "bad_request");
    assert_eq!(c.ok("hello", json!({}))["gate"], "open");
}

#[test]
fn state_and_settings() {
    let rig = rig("settings", true);
    let mut c = Client::authed(&rig);
    let hello = c.ok("hello", json!({}));
    assert_eq!(hello["subsystems"]["dev"], true);
    assert_eq!(hello["version"], env!("CARGO_PKG_VERSION"));
    let state = c.ok("state", json!({}));
    assert_eq!(state["scale"], 0.5);
    assert_eq!(state["settings"]["Upscaling"]["Mode"], "Quality");
    assert_eq!(state["introspection"]["engine"]["installed"], true);
    assert_eq!(
        c.ok("settings.get", json!({}))["Experimental"]["JitterScale"],
        "Display"
    );

    let r = c.ok(
        "settings.set",
        json!({"section": "Upscaling", "key": "Sharpness", "value": 0.25}),
    );
    assert_eq!(r["changes"][0]["value"], 0.25);
    assert_eq!(r["restart_required"], false);
    let r = c.ok(
        "settings.set",
        json!([{"section": "frameGeneration", "key": "enabled", "value": true}]),
    );
    assert_eq!(
        (
            r["changes"][0]["key"].clone(),
            r["restart_required"].clone()
        ),
        (json!("Enabled"), json!(true))
    );
    // All or nothing.
    let bad = json!([{"section": "Upscaling", "key": "Mode", "value": "DLAA"}, {"section": "Upscaling", "key": "Mode", "value": "Ultra"}]);
    assert_eq!(c.err("settings.set", bad), "bad_request");
    assert_eq!(
        c.ok("settings.get", json!({}))["Upscaling"]["Mode"],
        "Quality"
    );
    let r = c.ok(
        "experiment.set",
        json!({"key": "JitterScale", "value": "Render"}),
    );
    assert_eq!(r["changes"][0]["value"], "Render");
    assert_eq!(
        c.err("experiment.set", json!({"key": "Nope", "value": 1})),
        "bad_request"
    );
}

#[test]
fn memory_is_read_anywhere_readable_and_written_inside_the_structs() {
    let rig = rig("memory", true);
    let mut c = Client::authed(&rig);
    let syms = c.ok("symbols", json!({}));
    assert_eq!(syms["symbols"]["RenderConfig"], "0x100000");
    assert_eq!(syms["aliases"]["cfg"], "RenderConfig");
    assert_eq!(syms["fields"]["RenderConfig"]["live_ratio"]["as"], "f32");

    let r = c.ok(
        "mem.read",
        json!({"symbol": "cfg", "offset": 0x200, "as": "f32"}),
    );
    assert_eq!(
        (r["addr"].clone(), r["values"].clone()),
        (json!("0x100200"), json!([0.5]))
    );
    let parts = c.call(
        "mem.read",
        json!({"addr": "0x300000", "offset": 0, "len": 4, "point": "present"}),
    );
    assert_eq!(parts[0].msg["binary"], 4);
    assert_eq!(parts[0].binary.as_deref(), Some(&[0xcc; 4][..]));
    assert_eq!(
        c.err("mem.read", json!({"addr": "0x300000", "len": 0x101})),
        "refused"
    );
    assert_eq!(
        c.err("mem.read", json!({"symbol": "cfg", "len": 6, "as": "u32"})),
        "bad_request"
    );
    assert_eq!(
        c.err("mem.read", json!({"symbol": "nope", "as": "u8"})),
        "bad_request"
    );

    let w = c.ok(
        "mem.write",
        json!({"symbol": "dev", "offset": 0x10, "as": "u16", "value": [1, 2]}),
    );
    assert_eq!(
        (w["addr"].clone(), w["written"].clone()),
        (json!("0x200010"), json!(4))
    );
    assert_eq!(
        c.ok(
            "mem.read",
            json!({"symbol": "Device", "offset": 0x10, "len": 4, "as": "u16"})
        )["values"],
        json!([1, 2])
    );
    assert_eq!(
        c.err(
            "mem.write",
            json!({"addr": "0x300000", "as": "u8", "value": 1})
        ),
        "refused"
    );
    assert_eq!(
        c.err(
            "mem.write",
            json!({"symbol": "dev", "offset": 0xff, "as": "u16", "value": 1})
        ),
        "refused"
    );
    c.ok(
        "mem.write",
        json!({"addr": "0x300000", "as": "bytes", "value": "90", "force": true, "point": "slot"}),
    );
    assert_eq!(
        c.ok("mem.read", json!({"addr": "0x300000", "as": "u8"}))["values"],
        json!([0x90])
    );
    let m: Vec<f32> = (0..16).map(|i| i as f32).collect();
    c.ok(
        "mem.write",
        json!({"symbol": "cfg", "as": "mat4", "value": m}),
    );
    assert_eq!(
        c.ok("mem.read", json!({"symbol": "cfg", "as": "mat4"}))["values"][0][15],
        15.0
    );
}

#[test]
fn watches_stream_until_unwatched() {
    let rig = rig("watch", true);
    let mut c = Client::authed(&rig);
    let r = c.ok("watch", json!({"name": "w", "expr": [{"symbol": "cfg", "offset": 0x200, "as": "f32"}, "dev+0x10:u8"], "every_frames": 2, "point": "camera"}));
    assert_eq!(r["count"], 2);
    let first = c.event("watch");
    assert_eq!(
        (
            first["name"].clone(),
            first["point"].clone(),
            first["values"].clone()
        ),
        (json!("w"), json!("camera"), json!([0.5, 0]))
    );
    let second = c.event("watch");
    assert_eq!(
        second["frame"].as_u64().unwrap() - first["frame"].as_u64().unwrap(),
        2
    );
    c.ok("unwatch", json!({"name": "w"}));
    assert_eq!(c.err("unwatch", json!({"name": "w"})), "bad_request");
    assert_eq!(
        c.err("watch", json!({"name": "x", "expr": ["cfg"]})),
        "bad_request",
        "needs a type"
    );
    assert_eq!(
        c.err("watch", json!({"name": "x", "expr": ["0x900000:u8"]})),
        "refused"
    );
}

#[test]
fn the_log_is_tailed_and_streamed() {
    let _ring = LOG_RING.lock();
    let rig = rig("log", false);
    let mut c = Client::authed(&rig);
    log::info!(target: "render", "tail marker kind=probe");
    // Few enough lines to stay inside the client's line limit when the ring
    // still holds the flood test's 4 KB lines.
    let tail = c.ok("log.tail", json!({"lines": 128}));
    assert!(tail["lines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l.as_str().unwrap().contains("tail marker")));
    let r = c.ok(
        "log.subscribe",
        json!({"level": "debug", "targets": ["fg"]}),
    );
    assert_eq!(r["targets"], json!(["fg"]));
    log::debug!(target: "render", "not streamed");
    log::debug!(target: "fg", "streamed marker reason=test");
    let e = c.event("log");
    assert_eq!(
        (
            e["target"].clone(),
            e["message"].clone(),
            e["fields"]["reason"].clone()
        ),
        (json!("fg"), json!("streamed marker"), json!("test"))
    );
    assert_eq!(
        c.err("log.subscribe", json!({"targets": ["nope"]})),
        "bad_request"
    );
    assert_eq!(
        c.err("log.subscribe", json!({"level": "loud"})),
        "bad_request"
    );
}

#[test]
fn probes_and_trace() {
    let rig = rig("probes", true);
    let mut c = Client::authed(&rig);
    let j = c.ok("probe.jitter", json!({"frames": 8}));
    assert_eq!(j["verdict"], "display", "{j}");
    assert_eq!(j["frames"].as_array().unwrap().len(), 8);
    assert!((j["divisor"][0].as_f64().unwrap() - 32.0).abs() < 0.5);
    let r = c.ok("probe.reset", json!({"frames": 10}));
    let rows = r["frames"].as_array().unwrap();
    assert_eq!(rows.len(), 10);
    assert!(
        rows.iter().any(|r| r["reset_dlss"] == true)
            && rows.iter().any(|r| r["game_reset"] == true)
    );
    assert_eq!(rows[1]["camera_cut"]["cut"], false);
    let w = c.ok("probe.wait", json!({"seconds": 0.05}));
    assert_eq!(
        (w["latch"].clone(), w["waitable"].clone()),
        (json!(false), json!("0x1234"))
    );
    assert!(w["frames"].as_u64().unwrap() >= 1 && w.get("timeouts").is_some());
    assert!(
        c.ok("probe.modules", json!({}))["modules"]
            .as_array()
            .unwrap()
            .len()
            == 1
    );
    let t = c.ok("trace", json!({"frames": 3}));
    let frames = t["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 3, "{t}");
    let whats: Vec<&str> = frames[0]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["what"].as_str().unwrap())
        .collect();
    assert_eq!(whats, ["frame", "camera", "slot", "present"]);
    assert_eq!(c.err("probe.jitter", json!({"frames": 0})), "bad_request");
}

#[test]
fn captures_and_capture_probes() {
    let rig = rig("capture", true);
    let mut c = Client::authed(&rig);
    let parts = c.call(
        "capture",
        json!({"resources": ["depth", "mv_raw"], "frames": 2}),
    );
    assert_eq!(parts.len(), 4);
    for (i, part) in parts.iter().enumerate() {
        let r = &part.msg["result"];
        assert_eq!(
            (r["index"].as_u64(), r["count"].as_u64()),
            (Some(i as u64), Some(4))
        );
        assert_eq!(part.msg.get("more").is_some(), i < 3);
        assert_eq!(part.binary.as_ref().map(Vec::len), Some(256 * 8));
        assert_eq!(r["row_pitch"], 256);
        assert_eq!(r["sidecar"]["frame"]["render"], json!([16, 8]));
        assert!(
            r["sidecar"]["camera"]["proj"].is_array() && r["sidecar"]["jitter"]["units"].is_array()
        );
        let saved = PathBuf::from(r["saved"].as_str().unwrap());
        assert_eq!(
            std::fs::read(&saved).unwrap(),
            *part.binary.as_ref().unwrap()
        );
        assert!(saved.with_extension("json").exists());
        assert!(saved.starts_with(rig.dir.join("captures")));
    }
    assert_eq!(parts[0].msg["result"]["format"], "D32_FLOAT");
    assert_eq!(parts[1].msg["result"]["resource"], "mv_raw");
    assert_eq!(
        c.err("capture", json!({"resources": ["backbuffer"]})),
        "not_ready"
    );
    assert_eq!(
        c.err("capture", json!({"resources": ["nope"]})),
        "bad_request"
    );
    assert_eq!(
        c.err(
            "capture",
            json!({"resources": ["colour"], "point": "frame"})
        ),
        "bad_request"
    );

    let hdr = c.ok("probe.hdr", json!({"frames": 2}));
    assert_eq!(hdr["frames"][1]["max"][0], 12.5);
    let mv = c.ok("probe.mv", json!({"frames": 1}));
    let row = &mv["frames"][0];
    assert_eq!(
        (row["sign"].clone(), row["samples"].clone()),
        (json!([-1, 1]), json!(128))
    );
    assert!(
        (row["scale"][0].as_f64().unwrap() + 1.0).abs() < 1e-6,
        "{row}"
    );
    assert!(row["residual"].as_f64().unwrap() < 1e-6);
    let depth = c.ok("probe.depth", json!({"frames": 1}));
    assert_eq!(depth["frames"][0]["diff"]["max"], 0.0);
    assert_eq!(depth["frames"][0]["dsv"]["format"], "D32_FLOAT");
}

/// Sets what the fake slot offers, then waits for a slot showing it.
fn offer(rig: &Rig, c: &mut Client, edit: impl FnOnce(&mut fake::Offer)) {
    edit(&mut rig.game.offer.lock());
    c.ok("probe.wait", json!({"seconds": 0.02}));
}

#[test]
fn capture_probes_compare_the_right_resources() {
    let rig = rig("capture-offer", true);
    let mut c = Client::authed(&rig);
    // probe.depth reads `pass_depth`, not `depth`: without it, not_ready
    // naming it, even though `depth` is offered.
    offer(&rig, &mut c, |o| {
        o.resources.retain(|r| r.0 != "pass_depth")
    });
    let parts = c.call("probe.depth", json!({"frames": 1}));
    assert_eq!(parts[0].msg["error"]["code"], "not_ready");
    let message = parts[0].msg["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("pass_depth") && message.contains("have:"),
        "{message}"
    );

    // An absent resource carries the mod's reason.
    offer(&rig, &mut c, |o| {
        o.resources.retain(|r| r.0 != "mv_raw");
        o.absent
            .push(("mv_raw", "the temporal pass did not run this frame"));
    });
    let parts = c.call("probe.mv", json!({"frames": 1}));
    assert_eq!(parts[0].msg["error"]["code"], "not_ready");
    let message = parts[0].msg["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("mv_raw") && message.contains("temporal pass did not run"),
        "{message}"
    );
    let parts = c.call("capture", json!({"resources": ["mv_raw"]}));
    assert!(parts[0].msg["error"]["message"]
        .as_str()
        .unwrap()
        .contains("temporal pass did not run"));

    // The same texture under both names is not a comparison.
    offer(&rig, &mut c, |o| {
        *o = fake::Offer::default();
        let game_depth = o.resources.iter().find(|r| r.0 == "game_depth").unwrap().1;
        o.resources
            .iter_mut()
            .find(|r| r.0 == "pass_depth")
            .unwrap()
            .1 = game_depth;
    });
    let parts = c.call("probe.depth", json!({"frames": 1}));
    assert_eq!(parts[0].msg["error"]["code"], "not_ready");
    assert!(parts[0].msg["error"]["message"]
        .as_str()
        .unwrap()
        .contains("same resource"));
}

/// Not a check: serves the fake game for a client under development
/// (`tools/devctl`). Writes `HOST:PORT TOKEN` to `$MHW_DEV_SERVE_FILE` and
/// serves for `$MHW_DEV_SERVE_SECS` seconds (default 60):
/// `cargo test -p mhw-dev serve_fake_game -- --ignored`.
#[test]
#[ignore = "serves until a timeout; run on demand"]
fn serve_fake_game() {
    let rig = rig("serve", true);
    let line = format!("{} {}\n", rig.shared.local_addr, rig.token);
    match std::env::var_os("MHW_DEV_SERVE_FILE") {
        Some(path) => std::fs::write(path, line).unwrap(),
        None => print!("{line}"),
    }
    let secs = std::env::var("MHW_DEV_SERVE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    thread::sleep(Duration::from_secs(secs));
}
