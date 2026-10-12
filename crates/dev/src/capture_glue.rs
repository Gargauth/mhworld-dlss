//! The capture service on `mhw_render::capture`: the only place this crate
//! names that module.

use std::ffi::c_void;

use mhw_render::capture::{self, CaptureError, Item};

use crate::analysis::{ColourStats, Image};
use crate::game::{CaptureItem, CaptureService};
use crate::proto::{RpcError, RpcResult};

/// `mhw_render::capture` behind [`CaptureService`].
#[derive(Debug, Default)]
pub(crate) struct Glue;

fn error(e: CaptureError) -> RpcError {
    match e {
        CaptureError::NotReady => {
            RpcError::not_ready("the capture service is not ready (no engine queue yet)")
        }
        CaptureError::Unsupported(why) => RpcError::refused(format!("capture unsupported: {why}")),
        CaptureError::Device(why) => RpcError::internal(format!("capture failed: {why}")),
    }
}

fn item(i: &CaptureItem) -> Item {
    Item {
        name: i.name.clone(),
        resource: i.resource,
        state: i.state,
    }
}

impl CaptureService for Glue {
    unsafe fn record(
        &self,
        list: *mut c_void,
        request: u64,
        frame: u64,
        items: &[CaptureItem],
    ) -> RpcResult<()> {
        let items: Vec<Item> = items.iter().map(item).collect();
        // SAFETY: forwarded from the caller (the slot's open list, live
        // resources in their stated states).
        unsafe { capture::record(list, request, frame, &items) }.map_err(error)
    }

    fn poll(&self) -> Vec<Image> {
        capture::poll()
            .into_iter()
            .map(|c| Image {
                request: c.request,
                name: c.name,
                format: c.format,
                width: c.width,
                height: c.height,
                row_pitch: c.row_pitch,
                frame: c.frame,
                bytes: c.bytes,
            })
            .collect()
    }

    unsafe fn record_stats(
        &self,
        list: *mut c_void,
        request: u64,
        frame: u64,
        item_: &CaptureItem,
    ) -> RpcResult<()> {
        // SAFETY: forwarded from the caller.
        unsafe { capture::record_stats(list, request, frame, &item(item_)) }.map_err(error)
    }

    fn poll_stats(&self) -> Vec<(u64, u64, ColourStats)> {
        capture::poll_stats()
            .into_iter()
            .map(|(request, frame, s)| {
                (
                    request,
                    frame,
                    ColourStats {
                        min: s.min,
                        max: s.max,
                        mean: s.mean,
                        p50: s.p50,
                        p99: s.p99,
                        p999: s.p999,
                        above_one: s.above_one,
                        nan_or_inf: s.nan_or_inf,
                    },
                )
            })
            .collect()
    }
}
