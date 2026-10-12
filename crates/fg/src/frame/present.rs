//! The Present side: late tags, options, markers, the next token, state
//! samples and the resize hold. Runs on the main (presenting) thread.

use std::time::Instant;

use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use super::*;

fn focused(window: isize) -> bool {
    // SAFETY: no arguments; returns a window handle or null.
    let foreground = unsafe { GetForegroundWindow() };
    foreground.0 as isize == window
}

/// Immediately before the engine's `Present` on the proxy swapchain.
pub(crate) fn before_present(window: isize, display: Size, back_buffer_format: u32) {
    if !active() {
        return;
    }
    let focused = focused(window);
    let mut s = SHARED.lock();
    s.focused = focused;
    s.counters.presents += 1;
    let has_constants = s.machine.has_constants();
    let filed = s.filed.filter(|_| has_constants);
    // Late inputs of this very frame, at Present: by now the GUI pass that
    // produced them has run, the token is still this frame's, and the tags
    // are valid until Present, so no command list is needed.
    for kind in LateKind::ALL {
        let slot = kind.slot();
        let late = s.late[slot].take();
        match (filed, late) {
            (Some(filed), Some(late)) if filed.inputs && late.frame == filed.frame => {
                if kind == LateKind::HudLess {
                    mhw_core::log_changed!(
                        Warn,
                        FG,
                        "hudless-format",
                        (late.format, back_buffer_format),
                        "HUD-less format {} {} the back buffer's {}",
                        late.format,
                        if late.format == back_buffer_format {
                            "matches"
                        } else {
                            "DIFFERS from"
                        },
                        back_buffer_format
                    );
                }
                // SAFETY: `tag_late`'s caller vouches for the resource.
                let ok = unsafe {
                    file_tags(
                        &mut s,
                        &[(kind.tag(), Some(late.tag()))],
                        std::ptr::null_mut(),
                    )
                };
                if ok && kind == LateKind::HudLess {
                    s.counters.hudless_tagged += 1;
                }
            }
            (Some(filed), _) if filed.inputs && kind == LateKind::HudLess && s.late_seen[slot] => {
                s.counters.hudless_missing += 1;
            }
            _ => {}
        }
    }
    let sample = s.sample;
    let conditions = Conditions {
        inputs: filed.is_some_and(|f| f.inputs),
        focused,
        held: s.hold.active(),
        max_generated: sample.map(|x| x.max_generated).filter(|&m| m > 0),
        min_dimension: sample.map(|x| x.min_dimension).filter(|&m| m > 0),
        render: s.render,
        display,
    };
    apply_options(&mut s, &conditions);
    let plan = s.machine.present_start();
    let token = s.token;
    drop(s);
    if plan.render_submit_end {
        marker(abi::Marker::RenderSubmitEnd, token);
    }
    if plan.present_start {
        marker(abi::Marker::PresentStart, token);
    }
}

fn apply_options(s: &mut Shared, conditions: &Conditions) {
    let dlssg = dlssg_request(&s.options, conditions);
    if s.dlssg.differs(&dlssg) {
        let result = sl::dlssg_set_options(&dlssg.to_abi());
        if result == abi::OK {
            s.dlssg.accepted(dlssg);
            log::debug!(
                target: FG,
                "slDLSSGSetOptions: {} x{}, {}x{} from {}x{}",
                match dlssg.off {
                    None => "on".to_owned(),
                    Some(reason) => format!("off ({reason:?})"),
                },
                dlssg.frames_to_generate + 1,
                dlssg.colour.w,
                dlssg.colour.h,
                dlssg.mvec_depth.w,
                dlssg.mvec_depth.h
            );
            mhw_core::log_changed!(
                Info,
                FG,
                "generating",
                dlssg.off,
                "frame generation {}",
                match dlssg.off {
                    None => format!("on, {}x", dlssg.frames_to_generate + 1),
                    Some(reason) => format!("off: {reason:?}"),
                }
            );
        } else {
            mhw_core::log_once!(
                Warn,
                FG,
                "options",
                "slDLSSGSetOptions -> {}",
                abi::result_name(result)
            );
        }
    }
    let reflex = reflex_request(&s.options, &dlssg);
    if s.reflex.differs(&reflex) {
        let result = sl::reflex_set_options(&reflex.to_abi());
        log::info!(
            target: REFLEX,
            "slReflexSetOptions(mode {}, frame limit {} us) -> {}",
            reflex.mode,
            reflex.frame_limit_us,
            abi::result_name(result)
        );
        if result == abi::OK {
            s.reflex.accepted(reflex);
        }
    }
}

/// Immediately after `Present` returned: `PRESENT_END`, the next token
/// (only if this one carried constants), a state sample once a second, and
/// with [`SleepPoint::AfterPresent`] the next frame's sleep.
pub(crate) fn after_present() {
    if !active() {
        return;
    }
    let (plan, token, sleep_here) = {
        let mut s = SHARED.lock();
        let plan = s.machine.present_end();
        let token = s.token;
        s.hold.tick();
        if let Some(index) = plan.advance_to {
            acquire(&mut s, index);
        }
        let due = s.sampled_at.is_none_or(|at| at.elapsed() >= SAMPLE_EVERY);
        if due {
            s.sampled_at = Some(Instant::now());
            sample_state(&mut s);
        }
        (
            plan,
            token,
            s.options.sleep_point == SleepPoint::AfterPresent,
        )
    };
    if plan.present_end {
        marker(abi::Marker::PresentEnd, token);
    }
    if sleep_here {
        frame_start();
    }
}

/// `slDLSSGGetState`, on the present thread as Streamline requires.
fn sample_state(s: &mut Shared) {
    match sl::dlssg_get_state() {
        Ok(state) => {
            let sample = DlssgSample {
                status: state.status,
                presented: state.num_frames_actually_presented,
                max_generated: state.num_frames_to_generate_max,
                min_dimension: state.min_width_or_height,
                vsync_available: state.is_vsync_support_available == abi::TRUE,
                at_present: s.counters.presents,
            };
            s.sample = Some(sample);
            if state.status == 0 {
                mhw_core::log_changed!(Info, FG, "status", 0u32, "DLSS-G status ok");
            } else {
                mhw_core::log_changed!(
                    Warn,
                    FG,
                    "status",
                    state.status,
                    "DLSS-G status: {}",
                    abi::dlssg_status_names(state.status).join(", ")
                );
            }
            mhw_core::log_every!(
                60,
                Debug,
                FG,
                "DLSS-G: {} presented since the last sample, max {} generated, focused {}",
                state.num_frames_actually_presented,
                state.num_frames_to_generate_max,
                s.focused
            );
        }
        Err(error) => {
            mhw_core::log_once!(
                Warn,
                FG,
                "state",
                "slDLSSGGetState -> {}",
                abi::result_name(error)
            );
        }
    }
}

/// Before `ResizeBuffers` on the proxy swapchain: generation off at once
/// (the guide: off before any window manipulation), held off for a few
/// presents, and the next frame has no history.
pub(crate) fn before_resize() {
    if !active() {
        return;
    }
    let mut s = SHARED.lock();
    s.counters.resizes += 1;
    s.reset_pending = true;
    s.hold.start(RESIZE_HOLD_PRESENTS);
    let conditions = Conditions {
        inputs: false,
        held: true,
        ..Conditions::default()
    };
    let dlssg = DlssgRequest {
        off: Some(crate::options::OffReason::Resizing),
        ..dlssg_request(&s.options, &conditions)
    };
    if s.dlssg.differs(&dlssg) {
        let result = sl::dlssg_set_options(&dlssg.to_abi());
        if result == abi::OK {
            s.dlssg.accepted(dlssg);
        }
        log::info!(target: FG, "generation off for ResizeBuffers -> {}", abi::result_name(result));
    }
}
