//! Layout tests: every offset Streamline reads, from the v2.14.1 headers.

use super::*;
use std::mem::{offset_of, size_of};

#[test]
fn base_structure_is_32_bytes() {
    assert_eq!(size_of::<StructType>(), 16);
    assert_eq!(size_of::<Base>(), 32);
    assert_eq!(offset_of!(Base, next), 0);
    assert_eq!(offset_of!(Base, kind), 8);
    assert_eq!(offset_of!(Base, version), 24);
}

#[test]
fn preferences_layout() {
    assert_eq!(offset_of!(Preferences, show_console), 32);
    assert_eq!(offset_of!(Preferences, log_level), 36);
    assert_eq!(offset_of!(Preferences, paths_to_plugins), 40);
    assert_eq!(offset_of!(Preferences, num_paths_to_plugins), 48);
    assert_eq!(offset_of!(Preferences, path_to_logs_and_data), 56);
    assert_eq!(offset_of!(Preferences, allocate_callback), 64);
    assert_eq!(offset_of!(Preferences, release_callback), 72);
    assert_eq!(offset_of!(Preferences, log_message_callback), 80);
    assert_eq!(offset_of!(Preferences, flags), 88);
    assert_eq!(offset_of!(Preferences, features_to_load), 96);
    assert_eq!(offset_of!(Preferences, num_features_to_load), 104);
    assert_eq!(offset_of!(Preferences, application_id), 108);
    assert_eq!(offset_of!(Preferences, engine), 112);
    assert_eq!(offset_of!(Preferences, engine_version), 120);
    assert_eq!(offset_of!(Preferences, project_id), 128);
    assert_eq!(offset_of!(Preferences, render_api), 136);
    assert_eq!(size_of::<Preferences>(), 144);
}

#[test]
fn constants_layout() {
    assert_eq!(offset_of!(Constants, camera_view_to_clip), 32);
    assert_eq!(offset_of!(Constants, clip_to_prev_clip), 32 + 3 * 64);
    assert_eq!(offset_of!(Constants, jitter_offset), 32 + 5 * 64);
    assert_eq!(offset_of!(Constants, mvec_scale), 360);
    assert_eq!(offset_of!(Constants, camera_pinhole_offset), 368);
    assert_eq!(offset_of!(Constants, camera_pos), 376);
    assert_eq!(offset_of!(Constants, camera_up), 388);
    assert_eq!(offset_of!(Constants, camera_right), 400);
    assert_eq!(offset_of!(Constants, camera_fwd), 412);
    assert_eq!(offset_of!(Constants, camera_near), 424);
    assert_eq!(offset_of!(Constants, camera_aspect_ratio), 436);
    assert_eq!(offset_of!(Constants, motion_vectors_invalid_value), 440);
    assert_eq!(offset_of!(Constants, depth_inverted), 444);
    assert_eq!(offset_of!(Constants, motion_vectors_jittered), 450);
    assert_eq!(
        offset_of!(Constants, min_relative_linear_depth_object_separation),
        452
    );
    assert_eq!(size_of::<Constants>(), 456);
}

#[test]
fn resource_and_tag_layout() {
    assert_eq!(offset_of!(Resource, kind), 32);
    assert_eq!(offset_of!(Resource, native), 40);
    assert_eq!(offset_of!(Resource, state), 64);
    assert_eq!(offset_of!(Resource, native_format), 76);
    assert_eq!(offset_of!(Resource, gpu_virtual_address), 88);
    assert_eq!(offset_of!(Resource, internal_flags), 104);
    assert_eq!(size_of::<Resource>(), 112);

    assert_eq!(offset_of!(Extent, top), 0);
    assert_eq!(offset_of!(Extent, left), 4);
    assert_eq!(size_of::<Extent>(), 16);

    assert_eq!(offset_of!(ResourceTag, resource), 32);
    assert_eq!(offset_of!(ResourceTag, kind), 40);
    assert_eq!(offset_of!(ResourceTag, lifecycle), 44);
    assert_eq!(offset_of!(ResourceTag, extent), 48);
    assert_eq!(size_of::<ResourceTag>(), 64);

    assert_eq!(offset_of!(ViewportHandle, value), 32);
    assert_eq!(size_of::<ViewportHandle>(), 40);
}

#[test]
fn dlssg_layout() {
    assert_eq!(offset_of!(DlssgOptions, mode), 32);
    assert_eq!(offset_of!(DlssgOptions, num_frames_to_generate), 36);
    assert_eq!(offset_of!(DlssgOptions, mvec_depth_width), 56);
    assert_eq!(offset_of!(DlssgOptions, color_width), 64);
    assert_eq!(offset_of!(DlssgOptions, ui_buffer_format), 88);
    assert_eq!(offset_of!(DlssgOptions, on_error_callback), 96);
    assert_eq!(offset_of!(DlssgOptions, reserved15), 104);
    assert_eq!(offset_of!(DlssgOptions, queue_parallelism_mode), 108);
    assert_eq!(
        offset_of!(DlssgOptions, enable_user_interface_recomposition),
        112
    );
    assert_eq!(offset_of!(DlssgOptions, dynamic_target_frame_rate), 116);
    assert_eq!(size_of::<DlssgOptions>(), 120);

    assert_eq!(offset_of!(DlssgState, estimated_vram_usage_in_bytes), 32);
    assert_eq!(offset_of!(DlssgState, status), 40);
    assert_eq!(offset_of!(DlssgState, num_frames_actually_presented), 48);
    assert_eq!(offset_of!(DlssgState, num_frames_to_generate_max), 52);
    assert_eq!(offset_of!(DlssgState, is_vsync_support_available), 57);
    assert_eq!(
        offset_of!(DlssgState, inputs_processing_completion_fence),
        64
    );
    assert_eq!(offset_of!(DlssgState, is_dynamic_mfg_supported), 80);
    assert_eq!(size_of::<DlssgState>(), 88);
}

#[test]
fn reflex_layout() {
    assert_eq!(offset_of!(ReflexOptions, mode), 32);
    assert_eq!(offset_of!(ReflexOptions, frame_limit_us), 36);
    assert_eq!(offset_of!(ReflexOptions, use_markers_to_optimize), 40);
    assert_eq!(offset_of!(ReflexOptions, virtual_key), 42);
    assert_eq!(offset_of!(ReflexOptions, id_thread), 44);
    assert_eq!(size_of::<ReflexOptions>(), 48);
}

#[test]
fn identifiers_match_the_headers() {
    assert_eq!(SDK_VERSION, 0x0002_000e_0001_fedc);
    assert_eq!(FEATURE_DLSS_G, 1000);
    assert_eq!(FEATURE_REFLEX, 3);
    assert_eq!(FEATURE_PCL, 4);
    assert_eq!(result_name(OK), "eOk");
    assert_eq!(
        result_name(ERROR_EXCEPTION_HANDLER),
        "eErrorExceptionHandler"
    );
    assert_eq!(result_name(39), "eWarnOutOfVRAM");
    assert_eq!(result_name(40), "unknown");
    assert_eq!(result_name(ERROR_NOT_INITIALIZED), "eErrorNotInitialized");
    assert_eq!(result_name(ERROR_FEATURE_MISSING), "eErrorFeatureMissing");
    assert_eq!(result_name(ERROR_INVALID_STATE), "eErrorInvalidState");
    assert_eq!(Marker::PresentEnd as u32, 5);
}

#[test]
fn status_words_are_named() {
    assert_eq!(dlssg_status_names(0), ["ok"]);
    assert_eq!(
        dlssg_status_names(dlssg_status::REFLEX_NOT_DETECTED | 1 << 9),
        ["Reflex not active", "0x200"]
    );
}
