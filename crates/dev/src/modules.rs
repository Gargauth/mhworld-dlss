//! `probe.modules`: module load order and which modules came before the
//! game's D3D12 device.
//!
//! Windows keeps no load time per module that is cheap to read, so a
//! watcher polls the module list (and render's device count) from the dev
//! server's start: a module's time is when a poll first saw it, the
//! device's time when a poll first saw a device. A module first seen by the
//! same poll that first saw the device is reported as unknown
//! (`before_device: null`), as are modules already loaded when the server
//! started if the device existed by then.

use serde_json::{json, Value};

/// One module as the OS lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    /// File name.
    pub name: String,
    /// Full path.
    pub path: String,
    /// Base address.
    pub base: u64,
}

#[derive(Clone, Debug)]
struct Seen {
    module: Listed,
    poll: u64,
    ms: f64,
    order: Option<usize>,
}

/// What the polls have seen.
#[derive(Clone, Debug, Default)]
pub struct ModuleLog {
    seen: Vec<Seen>,
    polls: u64,
    started_ms: Option<f64>,
    device: Option<(u64, f64)>,
}

impl ModuleLog {
    /// Record one poll at `now_ms` (since process start): the module list
    /// in load order and whether the device exists.
    pub fn observe(&mut self, now_ms: f64, listed: &[Listed], device_created: bool) {
        let poll = self.polls;
        self.polls += 1;
        self.started_ms.get_or_insert(now_ms);
        if device_created && self.device.is_none() {
            self.device = Some((poll, now_ms));
        }
        for s in &mut self.seen {
            s.order = None;
        }
        for (order, module) in listed.iter().enumerate() {
            match self
                .seen
                .iter_mut()
                .find(|s| s.module.base == module.base && s.module.name == module.name)
            {
                Some(s) => s.order = Some(order),
                None => self.seen.push(Seen {
                    module: module.clone(),
                    poll,
                    ms: now_ms,
                    order: Some(order),
                }),
            }
        }
    }

    /// Whether the device has been seen.
    pub fn device_seen(&self) -> bool {
        self.device.is_some()
    }

    /// Polls made.
    pub fn polls(&self) -> u64 {
        self.polls
    }

    fn before_device(&self, s: &Seen) -> Option<bool> {
        match self.device {
            None => Some(true),
            Some((poll, _)) if s.poll < poll => Some(true),
            Some((poll, _)) if s.poll > poll => Some(false),
            Some(_) => None,
        }
    }

    /// The `probe.modules` result: loaded modules in load order, then
    /// those seen and since unloaded.
    pub fn report(&self, interval_ms: f64) -> Value {
        let mut rows: Vec<&Seen> = self.seen.iter().collect();
        rows.sort_by_key(|s| (s.order.is_none(), s.order, s.poll));
        let modules: Vec<Value> = rows
            .iter()
            .map(|s| {
                json!({
                    "order": s.order,
                    "name": s.module.name,
                    "path": s.module.path,
                    "base": format!("{:#x}", s.module.base),
                    "ms": s.ms,
                    "seen_at_start": s.poll == 0,
                    "before_device": self.before_device(s),
                    "loaded": s.order.is_some(),
                })
            })
            .collect();
        json!({
            "modules": modules,
            "device_created_ms": self.device.map(|(_, ms)| ms),
            "dev_started_ms": self.started_ms,
            "resolution_ms": interval_ms,
            "polls": self.polls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &str, base: u64) -> Listed {
        Listed {
            name: name.into(),
            path: format!("C:\\{name}"),
            base,
        }
    }

    #[test]
    fn modules_are_placed_relative_to_the_device_poll() {
        let mut log = ModuleLog::default();
        let exe = m("game.exe", 0x1000);
        log.observe(900.0, &[exe.clone(), m("dxgi.dll", 0x2000)], false);
        log.observe(
            950.0,
            &[exe.clone(), m("dxgi.dll", 0x2000), m("d3d12.dll", 0x3000)],
            false,
        );
        log.observe(
            1000.0,
            &[
                exe.clone(),
                m("dxgi.dll", 0x2000),
                m("d3d12.dll", 0x3000),
                m("dlss.dll", 0x4000),
            ],
            true,
        );
        log.observe(
            1050.0,
            &[
                exe.clone(),
                m("d3d12.dll", 0x3000),
                m("dlss.dll", 0x4000),
                m("sl.dll", 0x5000),
            ],
            true,
        );
        assert!(log.device_seen());
        let r = log.report(50.0);
        assert_eq!(r["device_created_ms"], 1000.0);
        assert_eq!(r["dev_started_ms"], 900.0);
        let rows = r["modules"].as_array().unwrap();
        let get = |n: &str| rows.iter().find(|x| x["name"] == n).unwrap().clone();
        assert_eq!(get("game.exe")["before_device"], true);
        assert_eq!(get("game.exe")["seen_at_start"], true);
        assert_eq!(get("d3d12.dll")["before_device"], true);
        assert_eq!(get("d3d12.dll")["ms"], 950.0);
        assert_eq!(
            get("dlss.dll")["before_device"],
            Value::Null,
            "same poll as the device"
        );
        assert_eq!(get("sl.dll")["before_device"], false);
        assert_eq!(get("dxgi.dll")["loaded"], false);
        let names: Vec<&str> = rows.iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["game.exe", "d3d12.dll", "dlss.dll", "sl.dll", "dxgi.dll"]
        );
    }

    #[test]
    fn a_device_older_than_the_server_leaves_early_modules_unknown() {
        let mut log = ModuleLog::default();
        log.observe(5000.0, &[m("game.exe", 1)], true);
        log.observe(5050.0, &[m("game.exe", 1), m("late.dll", 2)], true);
        let r = log.report(50.0);
        assert_eq!(r["modules"][0]["before_device"], Value::Null);
        assert_eq!(r["modules"][1]["before_device"], false);
    }
}
