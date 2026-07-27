//! Camera and zoom animations. `pan-viewport` extends `camera_target` and lets
//! `apply_camera_animation` lerp the camera there, warping the pointer by each
//! camera delta so the cursor keeps its screen position.
//! Combined zoom+camera animations pin the anchor's canvas point at a fixed
//! screen point while zoom lerps to target, and finish both coordinates in the
//! same tick — zoom snaps to target but keeps animating while the anchor is
//! still off its screen point, and there is never a camera-only handoff tail.
//!
//! The tests at the end cover the other side of that warp: a compositor grab
//! measures against a frozen canvas anchor, so camera motion it did not cause
//! reads to it as user input. Installing one takes the viewport out of flight —
//! except for edge-pan, the one camera motion a grab does cause.

use std::time::Duration;

use smithay::input::keyboard::ModifiersState;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};

use driftwm::config::{Action, BTN_LEFT, Config, Direction};

use crate::state::{StageWindow, ZoomAnimationAnchor, output_state};

use super::client::ClientId;
use super::{Fixture, end_grab, map_window, motion, window_by_app_id};

const TICK: Duration = Duration::from_millis(16);
const MAX_TICKS: usize = 600;

fn approx(a: Point<f64, Logical>, b: Point<f64, Logical>, tol: f64) -> bool {
    (a.x - b.x).abs() <= tol && (a.y - b.y).abs() <= tol
}

fn dist_sq(a: Point<f64, Logical>, b: Point<f64, Logical>) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    dx * dx + dy * dy
}

/// Canvas point currently shown at screen point `s`: `camera + s / zoom`.
fn point_at_screen(f: &mut Fixture, s: Point<f64, Logical>) -> Point<f64, Logical> {
    let camera = f.state().camera();
    let zoom = f.state().zoom();
    Point::from((camera.x + s.x / zoom, camera.y + s.y / zoom))
}

fn run_camera_animation(f: &mut Fixture) {
    for _ in 0..MAX_TICKS {
        if f.state().camera_target().is_none() {
            return;
        }
        f.state().apply_camera_animation(TICK);
    }
    panic!("camera animation did not converge within {MAX_TICKS} ticks");
}

fn run_zoom_animation(f: &mut Fixture) {
    for _ in 0..MAX_TICKS {
        if f.state().zoom_target().is_none() {
            return;
        }
        f.state().apply_zoom_animation(TICK);
    }
    panic!("zoom animation did not converge within {MAX_TICKS} ticks");
}

/// A pan action leaves the camera put and sets a target one step away; a second
/// pan extends the target from the target, not from the unmoved camera.
#[test]
fn pan_viewport_sets_target_instead_of_jumping() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let camera = f.state().camera();
    let step = f.state().config.pan_step / f.state().zoom();
    let (ux, uy) = Direction::Right.to_unit_vec();
    let delta = Point::from((ux * step, uy * step));

    f.state()
        .execute_action(&Action::PanViewport(Direction::Right));

    assert!(
        approx(f.state().camera(), camera, 1e-9),
        "a pan must not move the camera directly"
    );
    assert!(
        approx(f.state().camera_target().unwrap(), camera + delta, 1e-9),
        "a pan sets the target one step from the camera"
    );

    f.state()
        .execute_action(&Action::PanViewport(Direction::Right));

    assert!(approx(f.state().camera(), camera, 1e-9));
    assert!(
        approx(
            f.state().camera_target().unwrap(),
            camera + delta + delta,
            1e-9
        ),
        "a repeated pan extends the target from the target, not the camera"
    );
}

/// The camera lerps onto the target and clears it on arrival.
#[test]
fn pan_viewport_converges_and_clears_target() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();

    f.state()
        .execute_action(&Action::PanViewport(Direction::Right));
    let target = f
        .state()
        .camera_target()
        .expect("a pan sets a camera target");

    run_camera_animation(&mut f);

    assert!(
        f.state().camera_target().is_none(),
        "the target clears when the camera arrives"
    );
    assert!(
        approx(f.state().camera(), target, 1e-6),
        "the camera settles exactly on the target"
    );
}

/// Every camera tick warps the pointer by the camera delta, so the cursor's
/// screen position is unchanged across the whole pan.
#[test]
fn pan_keeps_pointer_screen_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();

    let camera_before = f.state().camera();
    let pointer_before = f.state().seat.get_pointer().unwrap().current_location();

    f.state()
        .execute_action(&Action::PanViewport(Direction::Right));
    for _ in 0..MAX_TICKS {
        if f.state().camera_target().is_none() {
            break;
        }
        f.state().apply_camera_animation(TICK);
        let camera_delta = f.state().camera() - camera_before;
        let pointer_delta =
            f.state().seat.get_pointer().unwrap().current_location() - pointer_before;
        assert!(
            approx(pointer_delta, camera_delta, 1e-6),
            "the pointer shifts by the camera delta on every tick, not just overall"
        );
    }
    assert!(
        f.state().camera_target().is_none(),
        "camera animation did not converge within {MAX_TICKS} ticks"
    );
}

/// A zoom animation with the anchor's canvas point already at its screen point
/// keeps that point pinned every tick while zoom lerps to target, then clears
/// cleanly with no camera-only tail.
#[test]
fn zoom_anchor_holds_screen_point() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();

    let s = Point::from((960.0, 540.0));
    let camera = Point::from((100.0, 50.0));
    // The canvas point shown at S right now, so only zoom animates.
    let c = Point::from((camera.x + s.x, camera.y + s.y));
    f.state().with_output_state(|os| {
        os.camera = camera;
        os.zoom = 1.0;
        os.zoom_target = Some(0.5);
        os.zoom_animation_anchor = Some(ZoomAnimationAnchor {
            canvas: c,
            screen: s,
        });
        os.camera_target = None;
        os.overview_return = None;
    });

    let mut prev = dist_sq(point_at_screen(&mut f, s), c);
    let mut converged = false;
    for _ in 0..MAX_TICKS {
        f.state().apply_zoom_animation(TICK);
        let d = dist_sq(point_at_screen(&mut f, s), c);
        assert!(
            d <= prev + 1e-6,
            "the screen anchor drifted off its canvas point"
        );
        prev = d;
        if f.state().zoom_target().is_none() {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "zoom animation did not converge within {MAX_TICKS} ticks"
    );

    assert_eq!(f.state().zoom(), 0.5, "zoom lands exactly on target");
    assert!(
        approx(point_at_screen(&mut f, s), c, 1e-9),
        "the anchor's canvas point ends at its screen point"
    );
    assert!(f.state().zoom_animation_anchor().is_none());
    assert!(
        f.state().camera_target().is_none(),
        "there is no camera-only handoff tail"
    );
}

/// The coupled-finish invariant: when zoom reaches its close band it snaps to
/// target, but the animation stays alive while the anchor is still off its
/// screen point — and it drives the camera directly, never handing off through
/// `camera_target`. Both coordinates then clear in the same tick.
#[test]
fn zoom_finish_is_coupled() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();

    let s = Point::from((960.0, 540.0));
    let camera = Point::from((100.0, 50.0));
    let zoom = 0.4995;
    // Displace the anchor's canvas point ~100px from the point now shown at S.
    let at_screen: Point<f64, Logical> =
        Point::from((camera.x + s.x / zoom, camera.y + s.y / zoom));
    let c = Point::from((at_screen.x + 100.0, at_screen.y));
    f.state().with_output_state(|os| {
        os.camera = camera;
        os.zoom = zoom;
        os.zoom_target = Some(0.5);
        os.zoom_animation_anchor = Some(ZoomAnimationAnchor {
            canvas: c,
            screen: s,
        });
        os.camera_target = None;
        os.overview_return = None;
    });

    f.state().apply_zoom_animation(TICK);

    assert_eq!(
        f.state().zoom(),
        0.5,
        "zoom snaps to target inside the close band"
    );
    assert!(
        f.state().zoom_target().is_some(),
        "the animation keeps running while the anchor converges"
    );
    assert!(
        f.state().camera_target().is_none(),
        "the anchor drives the camera directly, no handoff"
    );

    run_zoom_animation(&mut f);

    assert!(f.state().zoom_animation_anchor().is_none());
    assert!(f.state().camera_target().is_none());
    let expected_camera = Point::from((c.x - s.x / 0.5, c.y - s.y / 0.5));
    assert!(
        approx(f.state().camera(), expected_camera, 1e-9),
        "the camera lands exactly where the finish places it, not one lerp short"
    );
}

/// A keyboard zoom action anchors on the viewport center: the anchor's screen
/// point is the usable center and its canvas point is what that center shows,
/// which ends back under the center at the new zoom.
#[test]
fn zoom_action_anchors_at_viewport_center() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();

    let camera = f.state().camera();
    let zoom = f.state().zoom();
    let center = f.state().usable_center_screen();

    f.state().execute_action(&Action::ZoomOut);

    let anchor = f
        .state()
        .zoom_animation_anchor()
        .expect("a zoom action arms the anchor");
    assert!(
        approx(anchor.screen, center, 1e-9),
        "the anchor screen point is the viewport center"
    );
    let expected_canvas = Point::from((camera.x + center.x / zoom, camera.y + center.y / zoom));
    assert!(
        approx(anchor.canvas, expected_canvas, 1e-9),
        "the anchor canvas point is what the viewport center shows"
    );

    run_zoom_animation(&mut f);

    assert!(
        approx(point_at_screen(&mut f, center), anchor.canvas, 1e-9),
        "the anchor's canvas point ends back under the viewport center"
    );
}

/// Camera at the canvas origin, zoom 1, so canvas and screen coincide.
fn origin_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.zoom = 1.0;
    });
}

/// Put a camera and a zoom flight in progress, aimed far enough away that a
/// handful of ticks move the camera by hundreds of canvas pixels.
fn arm_distant_flight(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.camera_target = Some(Point::from((2000.0, 0.0)));
        os.zoom_target = Some(2.0);
    });
}

/// How many configures the client has seen and the size the last one carried. A
/// resize nobody asked for shows up as another configure with a bigger size, so
/// pinning both catches it whichever way the fixture's baseline sits.
fn configure_trace(
    f: &mut Fixture,
    id: ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) -> (usize, (i32, i32)) {
    let configures = &f.client(id).window(surface).configures_received;
    (
        configures.len(),
        configures
            .last()
            .expect("the client has been configured at least once")
            .1
            .size,
    )
}

/// Map one 400x300 client at canvas (400, 300) on a single output, viewport at
/// the origin — the shared fixture for the grab-versus-camera scenarios.
fn one_window(f: &mut Fixture) -> (ClientId, wayland_client::protocol::wl_surface::WlSurface) {
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(f, id, "a", (400, 300));
    let window = window_by_app_id(f, "a").unwrap();
    origin_view(f);
    f.state()
        .map_window(StageWindow::Client(window), Point::from((400, 300)), true);
    (id, surface)
}

/// A resize grab measures every delta from the canvas point it was pressed at,
/// and a camera tick warps the pointer synchronously into whatever grab is live.
/// A flight still running when the grab installs would therefore resize the
/// window from a mouse that never moved.
#[test]
fn a_camera_flight_does_not_resize_the_window_a_resize_grab_just_took() {
    let mut f = Fixture::new();
    let (id, surface) = one_window(&mut f);
    let window = window_by_app_id(&mut f, "a").unwrap();

    arm_distant_flight(&mut f);
    assert!(
        f.state().camera_target().is_some(),
        "precondition: a camera flight is in progress when the grab installs"
    );

    let grab_at = Point::from((790.0, 450.0));
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    assert!(
        f.state().start_compositor_resize_with_edge(
            &pointer,
            &window,
            grab_at,
            BTN_LEFT,
            serial,
            Some(xdg_toplevel::ResizeEdge::Right),
            false,
        ),
        "precondition: the resize grab installed"
    );
    // Park the cursor on the grab origin, so anything the size does from here is
    // the camera's doing and not the pointer's.
    motion(&mut f, grab_at);
    f.double_roundtrip(id);
    let before = configure_trace(&mut f, id, &surface);
    assert!(
        f.state().seat.get_pointer().unwrap().is_grabbed(),
        "precondition: the pointer is grabbed, so a warp reaches the grab \
         synchronously instead of taking the deferred branch"
    );

    for _ in 0..5 {
        f.state().apply_camera_animation(TICK);
    }
    f.double_roundtrip(id);

    assert_eq!(
        configure_trace(&mut f, id, &surface),
        before,
        "a motionless mouse resized nothing"
    );
    end_grab(&mut f);
}

/// The move half of the same rule. Driven through `try_start_gesture_move`
/// rather than the pinned path, whose screen-space math shifts cursor and
/// camera by the same delta and so cannot show the defect either way.
#[test]
fn a_camera_flight_does_not_move_the_window_a_move_grab_just_took() {
    let mut f = Fixture::new();
    let (_id, _surface) = one_window(&mut f);
    let element = StageWindow::Client(window_by_app_id(&mut f, "a").unwrap());

    arm_distant_flight(&mut f);
    assert!(
        f.state().camera_target().is_some(),
        "precondition: a camera flight is in progress when the grab installs"
    );

    let grab_at = Point::from((600.0, 450.0));
    assert!(
        f.state().try_start_gesture_move(grab_at, false),
        "precondition: the move grab installed"
    );
    motion(&mut f, grab_at);
    assert!(
        f.state().seat.get_pointer().unwrap().is_grabbed(),
        "precondition: the pointer is grabbed, so a warp reaches the grab \
         synchronously instead of taking the deferred branch"
    );

    for _ in 0..5 {
        f.state().apply_camera_animation(TICK);
    }

    assert_eq!(
        f.state().stage.position_of(&element),
        Some(Point::from((400, 300))),
        "a motionless mouse dragged nothing"
    );
    end_grab(&mut f);
}

/// `begin_client_resize` is the chokepoint every client-resize entry point runs
/// through, so stopping the flight there covers all of them at once.
#[test]
fn starting_a_client_resize_ends_the_camera_flight() {
    let mut f = Fixture::new();
    let (_id, _surface) = one_window(&mut f);
    let window = window_by_app_id(&mut f, "a").unwrap();

    arm_distant_flight(&mut f);
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    assert!(f.state().start_compositor_resize_with_edge(
        &pointer,
        &window,
        Point::from((790.0, 450.0)),
        BTN_LEFT,
        serial,
        Some(xdg_toplevel::ResizeEdge::Right),
        false,
    ));

    assert!(f.state().camera_target().is_none(), "the pan is called off");
    assert!(f.state().zoom_target().is_none(), "and so is the zoom");
    end_grab(&mut f);
}

/// `arm_interactive_move` is the other chokepoint: every move-grab install and
/// the stand-in resize arms run through it.
#[test]
fn starting_a_move_grab_ends_the_camera_flight() {
    let mut f = Fixture::new();
    let (_id, _surface) = one_window(&mut f);

    arm_distant_flight(&mut f);
    assert!(
        f.state()
            .try_start_gesture_move(Point::from((600.0, 450.0)), false)
    );

    assert!(f.state().camera_target().is_none(), "the pan is called off");
    assert!(f.state().zoom_target().is_none(), "and so is the zoom");
    end_grab(&mut f);
}

/// A stand-in resize reaches neither `begin_client_resize` (there is no client
/// to configure) nor any move grab, and still has to stop the flight — it runs
/// the same `ResizeGrab` against the same frozen anchor.
#[test]
fn starting_a_stand_in_resize_ends_the_camera_flight() {
    let config = Config::from_toml(
        r#"
        [decorations]
        default_mode = "server"
        [mouse.anywhere]
        "super+left" = "resize-window"
    "#,
    )
    .unwrap();
    let mut f = Fixture::with_config(config);
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    arm_distant_flight(&mut f);
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    let held = ModifiersState {
        logo: true,
        ..Default::default()
    };
    assert!(
        f.state().try_suspended_button(
            &pointer,
            Point::from((790.0, 450.0)),
            BTN_LEFT,
            serial,
            held
        ),
        "precondition: the stand-in resize grab installed"
    );

    assert!(f.state().camera_target().is_none(), "the pan is called off");
    assert!(f.state().zoom_target().is_none(), "and so is the zoom");
    end_grab(&mut f);
    f.state().dismiss_suspended(sid);
}

/// Scoping the cancel to the active output leaves a real hole: the cancel runs
/// once at install, but `focused_output` keeps moving — a `ResizeGrab` forces it
/// onto its own output on the first motion that crosses — so a flight left
/// running elsewhere becomes the active one mid-grab and warps the pointer then.
#[test]
fn a_grab_install_ends_the_camera_flight_on_every_output() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    origin_view(&mut f);
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((400, 300)),
        true,
    );
    assert_eq!(
        f.state().active_output(),
        Some(out1),
        "precondition: the grab installs while the other output is inactive"
    );

    {
        let mut os = output_state(&out2);
        os.camera_target = Some(Point::from((2000.0, 0.0)));
        os.zoom_target = Some(2.0);
    }

    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    assert!(f.state().start_compositor_resize_with_edge(
        &pointer,
        &window,
        Point::from((790.0, 450.0)),
        BTN_LEFT,
        serial,
        Some(xdg_toplevel::ResizeEdge::Right),
        false,
    ));

    let os = output_state(&out2);
    assert!(
        os.camera_target.is_none() && os.zoom_target.is_none(),
        "the inactive output's flight is called off too"
    );
    drop(os);
    end_grab(&mut f);
}

/// Edge-pan is the one camera motion a grab does cause, and it drives the camera
/// directly rather than through `camera_target` — so the install cancel must
/// leave it alone.
#[test]
fn edge_pan_still_drives_the_camera_under_a_live_move_grab() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let out = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    origin_view(&mut f);
    f.state()
        .map_window(StageWindow::Client(window), Point::from((400, 300)), true);

    assert!(
        f.state()
            .try_start_gesture_move(Point::from((600.0, 450.0)), false)
    );
    // Drag into the left edge zone; the grab arms edge-pan itself.
    motion(&mut f, Point::from((50.0, 500.0)));
    assert!(
        { output_state(&out).edge_pan_velocity }.is_some(),
        "precondition: the drag armed edge-pan"
    );

    // Every tick re-drives the grab through `warp_pointer`, which re-arms the
    // request — so a suppression anywhere in that loop shows up as a camera that
    // stalls, not only as a missing first step.
    let mut previous = f.state().camera().x;
    for _ in 0..3 {
        f.state().apply_edge_pan();
        let now = f.state().camera().x;
        assert!(
            now < previous,
            "the grab's own camera motion still runs, tick after tick"
        );
        previous = now;
    }
    end_grab(&mut f);
}
