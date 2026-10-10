//! The Direct3D 12 debug layer, with its messages in our own log.
//!
//! The defects a library like this one produces are ones the debug layer
//! names in one line: a descriptor heap left on the engine's command list, a
//! barrier whose `StateBefore` does not match, a descriptor resolving to a
//! released resource. Without it they show up as crash offsets and wrong
//! pictures.
//!
//! It is not on by default: it costs a great deal of performance and needs the
//! Graphics Tools optional feature installed, which not every machine has.
//! When it is on, the information queue is drained into the same log as
//! everything else, so a validation message sits next to the frame that caused
//! it.
//!
//! This is also the one tool that can settle whether NGX transitions the
//! resources handed to it and leaves them somewhere else -- in which case the
//! barriers this library issues to put them back would be starting from the
//! wrong state. Reading the code cannot answer that; the debug layer can.
//!
//! Not everything it reports is this library's. These belong to the engine
//! and should be left alone:
//!
//! - **1315**, four times at startup: `GetGPUDescriptorHandleForHeapStart` on
//!   a heap without `SHADER_VISIBLE`. `bindings::record_heap` guards against
//!   this and the decoder's own heap is shader visible; the four arrive while
//!   the engine is building its heaps, before either exists.
//! - **1319**, once: a query resolved that was never performed. Nothing here
//!   creates or uses a query heap -- the only occurrence of the word in this
//!   codebase is a breadcrumb operation name in `dred.rs`.
//! - **820** clear-value mismatches, **1424** fence waits of zero, and
//!   **677**, the float written into an integer render target, which is
//!   useful evidence rather than a defect; see the note in `mv.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

/// The message id for "Before state ... specified by transition barrier does
/// not match with the current resource state", reported both at
/// `ResourceBarrier` and again at `ExecuteCommandLists`.
///
/// Of everything this log carries, this is the id that belongs to us: the
/// engine's own entries are clear-value mismatches, a float written into an
/// integer render target, and fence waits of zero.
const BARRIER_MISMATCH: i32 = 527;

static ENABLED: AtomicBool = AtomicBool::new(false);
static WATCHING: AtomicBool = AtomicBool::new(false);

/// Turn the debug layer on, before any device exists.
///
/// Like breadcrumbs, this has to happen first: a device already created is
/// not retrofitted with validation.
pub fn enable() {
    if ENABLED.swap(true, Ordering::Relaxed) {
        return;
    }
    unsafe {
        let mut debug: Option<ID3D12Debug> = None;
        match D3D12GetDebugInterface(&mut debug) {
            Ok(()) => match debug {
                Some(debug) => {
                    debug.EnableDebugLayer();
                    crate::log!("[val] the debug layer is on; expect it to cost performance");
                }
                None => crate::log!("[val] no debug interface returned"),
            },
            // Almost always the Graphics Tools feature not being installed.
            Err(error) => crate::log!("[val] the debug layer is unavailable: {error}"),
        }
    }
}

/// Drain the device's information queue into the log, for as long as the game
/// runs.
pub fn watch(device: ID3D12Device) {
    if WATCHING.swap(true, Ordering::Relaxed) {
        return;
    }
    let queue: ID3D12InfoQueue = match device.cast() {
        Ok(queue) => queue,
        Err(error) => {
            crate::log!("[val] no information queue on the device: {error}");
            return;
        }
    };

    std::thread::spawn(move || unsafe {
        // Corruption and errors only. Warnings in a shipped engine are
        // numerous and mostly somebody else's business; the question being
        // asked here is what is actually invalid.
        let _ = queue.SetBreakOnSeverity(D3D12_MESSAGE_SEVERITY_CORRUPTION, false);
        let _ = queue.SetBreakOnSeverity(D3D12_MESSAGE_SEVERITY_ERROR, false);
        crate::log!("[val] draining the information queue");

        // A tally beside the messages. The run that found the barrier defect
        // produced 105853 lines, of which the 758 that mattered were ours and
        // the rest were the engine's clear-value and fence warnings. Asking
        // "is it fixed" should not mean counting lines.
        let mut tally: std::collections::HashMap<i32, u64> = std::collections::HashMap::new();
        let mut reported = std::time::Instant::now();

        let mut buffer: Vec<u8> = Vec::new();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let stored = queue.GetNumStoredMessages();
            for index in 0..stored {
                let mut length = 0usize;
                if queue.GetMessage(index, None, &mut length).is_err() || length == 0 {
                    continue;
                }
                buffer.clear();
                buffer.resize(length, 0);
                let message = buffer.as_mut_ptr() as *mut D3D12_MESSAGE;
                if queue.GetMessage(index, Some(message), &mut length).is_err() {
                    continue;
                }
                let message = &*message;
                let severity = match message.Severity {
                    D3D12_MESSAGE_SEVERITY_CORRUPTION => "corruption",
                    D3D12_MESSAGE_SEVERITY_ERROR => "error",
                    D3D12_MESSAGE_SEVERITY_WARNING => "warning",
                    D3D12_MESSAGE_SEVERITY_INFO => "info",
                    _ => "message",
                };
                if matches!(
                    message.Severity,
                    D3D12_MESSAGE_SEVERITY_INFO | D3D12_MESSAGE_SEVERITY_MESSAGE
                ) {
                    continue;
                }
                let text = std::slice::from_raw_parts(
                    message.pDescription,
                    message.DescriptionByteLength.saturating_sub(1),
                );
                *tally.entry(message.ID.0).or_default() += 1;
                crate::log!(
                    "[val] {severity} {}: {}",
                    message.ID.0,
                    String::from_utf8_lossy(text).trim()
                );
            }
            if stored > 0 {
                queue.ClearStoredMessages();
            }

            if reported.elapsed() >= std::time::Duration::from_secs(20) && !tally.is_empty() {
                reported = std::time::Instant::now();
                let mut counts: Vec<(i32, u64)> =
                    tally.iter().map(|(id, count)| (*id, *count)).collect();
                counts.sort_by_key(|entry| std::cmp::Reverse(entry.1));
                let summary: Vec<String> = counts
                    .iter()
                    .take(8)
                    .map(|(id, count)| format!("{id}x{count}"))
                    .collect();
                crate::log!("[val] tally: {}", summary.join(" "));
                // Called out by name because it is the one this library
                // causes: a transition barrier declaring a StateBefore the
                // resource is not in. Anything other than zero here means
                // the submission-order model in states.rs is still wrong.
                crate::log!(
                    "[val] barrier mismatches (527): {}",
                    tally.get(&BARRIER_MISMATCH).copied().unwrap_or(0)
                );
            }
        }
    });
}
