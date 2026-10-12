//! What the server reads from the running game, behind a trait: the
//! Windows implementation (`live`) asks `mhw-engine`, `mhw-render`,
//! `mhw-ngx`, `mhw-fg` and `mhw-win`; the host tests use a fake; a non-Windows
//! build without a game uses [`Absent`].

use std::ffi::c_void;

use serde_json::{json, Value};

use crate::analysis::{ColourStats, Image};
use crate::proto::{RpcError, RpcResult};
use crate::symbols::Symbols;

/// One resource handed to the capture service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureItem {
    /// Resource name (`colour`, `depth`, …).
    pub name: String,
    /// `ID3D12Resource*`.
    pub resource: *mut c_void,
    /// Its current `D3D12_RESOURCE_STATES` on the list.
    pub state: u32,
}

/// GPU readback (`mhw_render::capture`).
pub trait CaptureService: Send + Sync {
    /// Record copies of `items` on `list` for `request`.
    ///
    /// # Safety
    /// `list` must be the engine's open command list at the slot, and every
    /// item a live resource in the given state on it.
    unsafe fn record(
        &self,
        list: *mut c_void,
        request: u64,
        frame: u64,
        items: &[CaptureItem],
    ) -> RpcResult<()>;

    /// Completed copies, oldest first.
    fn poll(&self) -> Vec<Image>;

    /// Record the colour reduction of `item` on `list`.
    ///
    /// # Safety
    /// As for [`CaptureService::record`].
    unsafe fn record_stats(
        &self,
        list: *mut c_void,
        request: u64,
        frame: u64,
        item: &CaptureItem,
    ) -> RpcResult<()>;

    /// Completed reductions: `(request, frame, stats)`.
    fn poll_stats(&self) -> Vec<(u64, u64, ColourStats)>;
}

/// The engine's frame-latency wait state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitSample {
    /// `device+0x10d9`.
    pub presented: bool,
    /// `device+0x10da`.
    pub latched: bool,
    /// `device+0x10f0`.
    pub waitable: u64,
}

/// What resets history now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResetSample {
    /// The engine's count of game resets latched by the SS enqueue.
    pub game_resets: Option<u64>,
    /// `taa+0x185` of the TAA filter last seen.
    pub history_invalid: Option<bool>,
    /// Frame generation's last constants: `(frame, reset)`.
    pub fg: Option<(u64, bool)>,
}

/// The game, as the server sees it.
pub trait Game: Send + Sync {
    /// The game executable by its PE header (`timestamp 0x66d13cb6, image
    /// size 0x55d3000` for Steam build 15539686).
    fn game_build(&self) -> String;
    /// The gate: `open`, `pending`, or `stand_down: <why>`.
    fn gate(&self) -> String;
    /// Which subsystems run (`{engine, render, ngx, fg}` → bool).
    fn subsystems(&self) -> Value;
    /// Engine, render, NGX and frame-generation introspection.
    fn introspect(&self) -> Value;
    /// The address book, once the gate resolved it.
    fn symbols(&self) -> Option<Symbols>;
    /// Copy `out.len()` bytes from `address`; `false` (nothing copied)
    /// unless all are readable.
    fn read(&self, address: u64, out: &mut [u8]) -> bool;
    /// Write `bytes` at `address` (page protection handled).
    fn write(&self, address: u64, bytes: &[u8]) -> Result<(), String>;
    /// `fProj` of the camera buffer the last temporal pass bound.
    fn camera_projection(&self) -> Option<[f32; 16]>;
    /// Reset-related state.
    fn reset_state(&self) -> ResetSample;
    /// The wait helper's state.
    fn wait_state(&self) -> Option<WaitSample>;
    /// The `probe.modules` report.
    fn modules(&self) -> RpcResult<Value>;
    /// The capture service; `None` outside the game.
    fn capture(&self) -> Option<&dyn CaptureService>;
}

/// No game in this process (a non-Windows build).
#[derive(Debug, Default)]
pub struct Absent;

impl Game for Absent {
    fn game_build(&self) -> String {
        "none".to_owned()
    }
    fn gate(&self) -> String {
        "pending".to_owned()
    }
    fn subsystems(&self) -> Value {
        json!({"engine": false, "render": false, "ngx": false, "fg": false})
    }
    fn introspect(&self) -> Value {
        json!({})
    }
    fn symbols(&self) -> Option<Symbols> {
        None
    }
    fn read(&self, _: u64, _: &mut [u8]) -> bool {
        false
    }
    fn write(&self, _: u64, _: &[u8]) -> Result<(), String> {
        Err("no game in this process".to_owned())
    }
    fn camera_projection(&self) -> Option<[f32; 16]> {
        None
    }
    fn reset_state(&self) -> ResetSample {
        ResetSample::default()
    }
    fn wait_state(&self) -> Option<WaitSample> {
        None
    }
    fn modules(&self) -> RpcResult<Value> {
        Err(RpcError::not_ready("no game in this process"))
    }
    fn capture(&self) -> Option<&dyn CaptureService> {
        None
    }
}
