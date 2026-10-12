use super::*;
use crate::math::{dlss_jitter, engine_jitter_units, JitterScale, Vec3, Vec4};

const RENDER: Size = Size::new(1920, 1080);
const DISPLAY: Size = Size::new(3840, 2160);

/// A standard-depth right-handed projection (looks down −z), row vectors,
/// as the engine keeps it in `sCamera::Viewport+0xe0`.
fn projection(p00: f32, p11: f32, near: f32, far: f32) -> Mat4 {
    Mat4::from_rows([
        [p00, 0.0, 0.0, 0.0],
        [0.0, p11, 0.0, 0.0],
        [0.0, 0.0, far / (near - far), -1.0],
        [0.0, 0.0, near * far / (near - far), 0.0],
    ])
}

fn perspective(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let p11 = 1.0 / (fov_y / 2.0).tan();
    projection(p11 / aspect, p11, near, far)
}

/// The view matrix of a camera at `position` turned `yaw` radians about +y.
fn view(position: Vec3, yaw: f32) -> Mat4 {
    let (sin, cos) = yaw.sin_cos();
    let camera_to_world = Mat4::from_rows([
        [cos, 0.0, -sin, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [sin, 0.0, cos, 0.0],
        [position.x, position.y, position.z, 1.0],
    ]);
    camera_to_world.inverse().unwrap()
}

fn frame(position: Vec3, yaw: f32, fov_y: f32, prev: Option<&CameraFrame>) -> CameraFrame {
    let v = view(position, yaw);
    let p = perspective(fov_y, 16.0 / 9.0, 0.5, 2000.0);
    let (pv, pp) = prev.map_or((v, p), |prev| (prev.view, prev.proj));
    CameraFrame::new(v, p, pv, pp, prev.map_or(0, |prev| prev.frame + 1)).unwrap()
}

fn to_pixels(clip: Vec4, render: Size) -> Vec2 {
    let ndc = clip.project();
    Vec2::new(
        (ndc.x + 1.0) / 2.0 * render.w as f32,
        (1.0 - ndc.y) / 2.0 * render.h as f32,
    )
}

#[test]
fn the_depth_flip_reproduces_the_logged_constant_buffer() {
    // fNoJitterProj logged from the game: P00 4.936999, P11 8.776888,
    // P22 0.000004, P32 16.000065; near 16, far 3947580.3.
    let p = projection(4.936_999, 8.776_888, 16.0, 3_947_580.3);
    let cb = p * DEPTH_FLIP;
    assert!(
        (cb.flat(10) - 0.000_004).abs() < 1e-6,
        "P22 {}",
        cb.flat(10)
    );
    assert!(
        (cb.flat(14) - 16.000_065).abs() < 1e-3,
        "P32 {}",
        cb.flat(14)
    );
    assert_eq!(cb.flat(11), -1.0);
    assert_eq!(DEPTH_FLIP * DEPTH_FLIP, Mat4::IDENTITY);

    let standard = lens(&p).unwrap();
    let reversed = lens(&cb).unwrap();
    assert!(!standard.reversed && reversed.reversed);
    for l in [standard, reversed] {
        assert!((l.near - 16.0).abs() < 1e-2, "near {}", l.near);
        assert!((l.fov_y - 0.226_89).abs() < 1e-4, "fov {}", l.fov_y);
        assert!((l.aspect - 16.0 / 9.0).abs() < 1e-4);
    }
}

#[test]
fn the_lens_ignores_jitter_and_refuses_non_projections() {
    let p = perspective(1.0, 1.5, 0.5, 1000.0);
    let jittered = p * jitter_matrix(Vec2::new(0.7, -0.3), DISPLAY);
    assert_eq!(lens(&jittered), lens(&p));
    let l = lens(&p).unwrap();
    assert!((l.far - 1000.0).abs() < 0.5 && (l.near - 0.5).abs() < 1e-4);
    assert_eq!(lens(&Mat4::IDENTITY), None);
    // Infinite far plane, reversed: P22 = 0.
    let mut infinite = Mat4::IDENTITY;
    infinite.rows = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 0.0, -1.0],
        [0.0, 0.0, 16.0, 0.0],
    ];
    let l = lens(&infinite).unwrap();
    assert!(l.reversed && l.far.is_infinite() && l.near == 16.0);
}

#[test]
fn the_jitter_matrix_moves_the_image_by_the_requested_render_pixels() {
    // Request 0.3 px right and 0.2 px up (engine axes) at rs = 0.5.
    let px = Vec2::new(0.3, 0.2);
    let units = engine_jitter_units(px, RENDER, DISPLAY, JitterScale::Display);
    let p = perspective(0.9, 16.0 / 9.0, 0.5, 2000.0);
    let j = jitter_matrix(units, DISPLAY);
    for point in [
        Vec4::new(3.0, 1.0, -50.0, 1.0),
        Vec4::new(-40.0, 7.0, -900.0, 1.0),
    ] {
        let plain = to_pixels((p * DEPTH_FLIP).transform(point), RENDER);
        let jittered = to_pixels((p * j * DEPTH_FLIP).transform(point), RENDER);
        let shift = jittered - plain;
        // On the render target (+y down) that is exactly the DLSS offset.
        let expected = dlss_jitter(px);
        assert!((shift.x - expected.x).abs() < 2e-3, "x {shift:?}");
        assert!((shift.y - expected.y).abs() < 2e-3, "y {shift:?}");
    }
}

#[test]
fn proj_8_and_9_carry_the_negated_ndc_jitter() {
    // ReJitterCamera §2.4: fProj[8] = −a, fProj[9] = −b, and
    // −fProj[8]·Wr/2 is the shift in render pixels.
    let px = Vec2::new(-0.34375, 0.24074);
    let units = engine_jitter_units(px, RENDER, DISPLAY, JitterScale::Display);
    let f_proj =
        perspective(0.9, 16.0 / 9.0, 0.5, 2000.0) * jitter_matrix(units, DISPLAY) * DEPTH_FLIP;
    assert!((f_proj.flat(8) + units.x / DISPLAY.w as f32).abs() < 1e-7);
    assert!((f_proj.flat(9) + units.y / DISPLAY.h as f32).abs() < 1e-7);
    assert!((-f_proj.flat(8) * RENDER.w as f32 / 2.0 - px.x).abs() < 1e-5);
    assert!((-f_proj.flat(9) * RENDER.h as f32 / 2.0 - px.y).abs() < 1e-5);
}

#[test]
fn jitter_and_depth_flip_commute() {
    let j = jitter_matrix(Vec2::new(1.5, -0.5), DISPLAY);
    assert!((j * DEPTH_FLIP).approx_eq(&(DEPTH_FLIP * j), 0.0));
    assert_eq!(
        jitter_matrix(Vec2::new(1.0, 1.0), Size::new(0, 0)),
        Mat4::IDENTITY
    );
}

#[test]
fn clip_to_prev_clip_reprojects_static_points() {
    let first = frame(Vec3::new(100.0, 50.0, 20.0), 0.2, 0.9, None);
    let second = frame(Vec3::new(104.0, 50.5, 18.0), 0.23, 0.9, Some(&first));
    let m = clip_to_prev_clip(&second).unwrap();
    for world in [
        Vec4::new(90.0, 48.0, -200.0, 1.0),
        Vec4::new(130.0, 70.0, -40.0, 1.0),
    ] {
        let cur = (second.view * second.proj * DEPTH_FLIP).transform(world);
        let prev = (first.view * first.proj * DEPTH_FLIP).transform(world);
        let reprojected = m.transform(cur).project();
        let expected = prev.project();
        assert!(
            (reprojected - expected).length() < 1e-4,
            "{reprojected:?} vs {expected:?}"
        );
    }
    // A camera that did not move maps clip space onto itself.
    let still = frame(Vec3::new(1.0, 2.0, 3.0), 0.5, 0.9, None);
    assert!(clip_to_prev_clip(&still)
        .unwrap()
        .approx_eq(&Mat4::IDENTITY, 1e-4));
}

#[test]
fn clip_to_prev_clip_is_the_engines_matrix_with_the_jitter_conjugated_out() {
    // ReJitterCamera §3.5: the engine's fViewProjIViewProjPF is
    // inv(V·P·J·R)·(Vp·Pp·J·R); without jitter it is J·C·J⁻¹.
    let first = frame(Vec3::new(0.0, 10.0, 0.0), 0.0, 0.8, None);
    let second = frame(Vec3::new(2.0, 10.0, -3.0), 0.05, 0.8, Some(&first));
    let j = jitter_matrix(Vec2::new(0.7, -0.4), Size::new(1920, 1080));
    let engine = (second.view * second.proj * j * DEPTH_FLIP)
        .inverse()
        .unwrap()
        * (second.prev_view * second.prev_proj * j * DEPTH_FLIP);
    let unjittered = j * engine * j.inverse().unwrap();
    assert!(unjittered.approx_eq(&clip_to_prev_clip(&second).unwrap(), 1e-4));
    // And the jittered engine matrix is not the same thing.
    assert!(!engine.approx_eq(&unjittered, 1e-4));
}

#[test]
fn the_camera_frame_derives_pose_and_lens() {
    let cam = frame(Vec3::new(-532.3, 1547.9, 4021.0), 0.0, 0.9, None);
    assert!((cam.position - Vec3::new(-532.3, 1547.9, 4021.0)).length() < 1e-2);
    assert!((cam.forward() - Vec3::new(0.0, 0.0, -1.0)).length() < 1e-6);
    assert!((cam.fov_y - 0.9).abs() < 1e-5);
    assert!((cam.near - 0.5).abs() < 1e-4);
    assert!(
        CameraFrame::new(Mat4::from_array([0.0; 16]), cam.proj, cam.view, cam.proj, 0).is_none()
    );
    assert!(CameraFrame::new(cam.view, Mat4::IDENTITY, cam.view, cam.proj, 0).is_none());
}

#[test]
fn ordinary_motion_is_not_a_cut() {
    let a = frame(Vec3::new(0.0, 0.0, 0.0), 0.0, 0.9, None);
    let walk = frame(Vec3::new(30.0, 0.0, -10.0), 0.1, 0.9, Some(&a));
    assert!(!camera_cut(&a, &walk));
    let zoom = frame(Vec3::ZERO, 0.0, 0.92, Some(&a));
    assert!(!camera_cut(&a, &zoom));
    let metric = camera_cut_metric(&a, &walk);
    assert!((metric.turn - 0.1).abs() < 1e-4);
    assert!((metric.position_jump - 1000f32.sqrt()).abs() < 1e-3);
}

#[test]
fn jumps_turns_and_lens_changes_are_cuts() {
    let a = frame(Vec3::new(0.0, 0.0, 0.0), 0.0, 0.9, None);
    let teleport = frame(Vec3::new(600.0, 0.0, 0.0), 0.0, 0.9, Some(&a));
    let turn = frame(Vec3::ZERO, std::f32::consts::FRAC_PI_2, 0.9, Some(&a));
    let fov = frame(Vec3::ZERO, 0.0, 1.2, Some(&a));
    for (name, b) in [("teleport", teleport), ("turn", turn), ("fov", fov)] {
        assert!(camera_cut(&a, &b), "{name}");
    }
    let mut broken = a;
    broken.position.x = f32::NAN;
    assert!(camera_cut(&a, &broken));
    let mut broken = a;
    broken.proj.rows[0][0] = f32::NAN;
    assert!(camera_cut(&a, &broken));
}
