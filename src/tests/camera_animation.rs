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

/// Drag the right edge out by a fractional amount, so the grab is carrying a
/// real displacement rather than sitting on its origin where every delta reads
/// as zero regardless. Fractional because the delta feeds an `as i32`
/// truncation and the screen round-trip is only exact to within an ulp.
const DRAG_OUT: Point<f64, Logical> = Point::new(60.5, 0.0);

/// Cancelling the flight at grab install only covers the flights already
/// running. Sixteen producers can arm one mid-drag — a keyboard pan, a bookmark
/// jump, a window mapping — and the warp that follows is not the user's hand.
#[test]
fn a_camera_flight_armed_after_a_resize_grab_does_not_resize_the_window() {
    let mut f = Fixture::new();
    let (id, surface) = one_window(&mut f);
    let window = window_by_app_id(&mut f, "a").unwrap();

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
    motion(&mut f, grab_at + DRAG_OUT);
    f.double_roundtrip(id);
    let before = configure_trace(&mut f, id, &surface);

    arm_distant_flight(&mut f);
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
        "the canvas slid under a held edge without resizing it"
    );
    end_grab(&mut f);
}

/// The zoom half. The cursor has to be off the grab origin first: parked on it
/// the screen delta is zero at any zoom, and an anchor that divided by the
/// *live* zoom would sail through.
#[test]
fn a_zoom_flight_armed_after_a_resize_grab_does_not_rescale_the_window() {
    let mut f = Fixture::new();
    let (id, surface) = one_window(&mut f);
    let window = window_by_app_id(&mut f, "a").unwrap();

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
    motion(&mut f, grab_at + DRAG_OUT);
    f.double_roundtrip(id);
    let before = configure_trace(&mut f, id, &surface);
    assert_eq!(
        before.1,
        (460, 300),
        "precondition: the drag is displaced from the grab origin, so a zoom \
         change has something to rescale"
    );

    f.state().with_output_state(|os| os.zoom_target = Some(0.5));
    run_zoom_animation(&mut f);
    f.double_roundtrip(id);

    assert_eq!(
        f.state().zoom(),
        0.5,
        "precondition: the zoom flight actually ran"
    );
    assert_eq!(
        configure_trace(&mut f, id, &surface),
        before,
        "the drag stayed the size the hand made it, at the new zoom"
    );
    end_grab(&mut f);
}

/// The pinned arm freezes its anchor too. It already measured in screen space,
/// but re-projecting a canvas anchor through the live camera every motion drifts
/// it by the camera delta scaled by zoom.
#[test]
fn a_camera_flight_armed_after_a_pinned_resize_grab_does_not_resize_the_window() {
    let config = Config::from_toml(
        r#"
        [[window_rules]]
        app_id = "a"
        pinned_to_screen = true
        size = [400, 300]
    "#,
    )
    .unwrap();
    let mut f = Fixture::with_config(config);
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    origin_view(&mut f);
    let window = window_by_app_id(&mut f, "a").unwrap();
    let site = f
        .state()
        .stage
        .pin_of(&window)
        .expect("precondition: the rule pinned the window to the screen")
        .screen_pos;

    // Camera and zoom are the identity here, so the pin's screen rect and its
    // canvas rect coincide and the grab point can be written in either.
    let grab_at = Point::from((site.x as f64 + 390.0, site.y as f64 + 150.0));
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
        "precondition: the pinned resize grab installed"
    );
    motion(&mut f, grab_at + DRAG_OUT);
    f.double_roundtrip(id);
    let before = configure_trace(&mut f, id, &surface);

    arm_distant_flight(&mut f);
    for _ in 0..5 {
        f.state().apply_camera_animation(TICK);
    }
    f.double_roundtrip(id);

    assert_eq!(
        configure_trace(&mut f, id, &surface),
        before,
        "a pinned window ignores the camera it is pinned away from"
    );
    end_grab(&mut f);
}

/// The move grab is the deliberate opposite, and the ordering that breaks the
/// resize grab is the one that makes this work: hold a window, jump somewhere
/// else, and the window comes along.
#[test]
fn a_camera_flight_armed_after_a_move_grab_still_carries_the_window() {
    let mut f = Fixture::new();
    let (_id, _surface) = one_window(&mut f);
    let element = StageWindow::Client(window_by_app_id(&mut f, "a").unwrap());

    let grab_at = Point::from((600.0, 450.0));
    assert!(
        f.state().try_start_gesture_move(grab_at, false),
        "precondition: the move grab installed"
    );
    motion(&mut f, grab_at);
    assert_eq!(
        f.state().stage.position_of(&element),
        Some(Point::from((400, 300))),
        "precondition: the grab itself moved nothing"
    );

    arm_distant_flight(&mut f);
    for _ in 0..5 {
        f.state().apply_camera_animation(TICK);
    }

    let travelled = f.state().camera().x;
    assert!(
        travelled > 100.0,
        "precondition: the flight moved the camera a long way, not a hair"
    );
    let position = f.state().stage.position_of(&element).unwrap();
    assert!(
        (position.x as f64 - (400.0 + travelled)).abs() <= 1.0,
        "the held window rode the camera to {travelled}, landing at {position:?}"
    );
    end_grab(&mut f);
}

/// Long enough for the 50 ms auto-launch deadline to be safely in the past.
const PAST_MOMENTUM_DEADLINE: Duration = Duration::from_millis(80);

/// Two pans a few ms apart on `output`, which is what the velocity tracker needs
/// to produce a non-zero launch velocity — one sample launches at zero.
fn pan_burst(f: &mut Fixture, output: &smithay::output::Output, first_time_ms: u32) {
    f.state()
        .drift_pan_on(Point::from((10.0, 0.0)), first_time_ms, output);
    f.state()
        .drift_pan_on(Point::from((10.0, 0.0)), first_time_ms + 10, output);
}

fn coasting(output: &smithay::output::Output) -> bool {
    output_state(output).momentum.coasting
}

/// A pan burst arrives at touchpad rates, so the auto-launch timer is inserted
/// once and left alone; only its deadline moves.
#[test]
fn a_pan_burst_arms_the_momentum_timer_once() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    f.state().drift_pan(Point::from((10.0, 0.0)), 0);
    let armed = f.state().momentum_timer;
    assert!(
        armed.is_some(),
        "the first pan of the burst armed the timer"
    );
    let first_deadline = f.state().momentum_deadline.clone().unwrap().0;

    for n in 1..8 {
        f.state().drift_pan(Point::from((10.0, 0.0)), n * 5);
        assert_eq!(
            f.state().momentum_timer,
            armed,
            "pan {n} rode the timer already armed instead of re-registering one"
        );
    }

    assert!(
        f.state().momentum_deadline.clone().unwrap().0 > first_deadline,
        "the deadline is what the burst moves"
    );
}

/// The timer fires once, launches, and drops itself — and clears its own token
/// on the way out, so the next burst arms a fresh one. Leaving the token set
/// would wedge the lazy re-arm and silently kill auto-launch for the session.
#[test]
fn the_momentum_timer_fires_once_and_a_later_pan_re_arms_it() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    pan_burst(&mut f, &out, 0);
    std::thread::sleep(PAST_MOMENTUM_DEADLINE);
    f.pump(1);

    assert!(
        f.state().momentum_timer.is_none(),
        "the fired timer dropped itself"
    );
    assert!(
        f.state().momentum_deadline.is_none(),
        "and took its deadline with it"
    );
    assert!(
        coasting(&out),
        "the finger lift the touchpad never reported auto-launched momentum"
    );

    pan_burst(&mut f, &out, 100);
    assert!(
        !coasting(&out),
        "precondition: the new burst is live input again"
    );
    assert!(
        f.state().momentum_timer.is_some(),
        "the next burst arms a fresh timer"
    );

    std::thread::sleep(PAST_MOMENTUM_DEADLINE);
    f.pump(1);
    assert!(
        coasting(&out),
        "and it fires, so auto-launch survives the first burst"
    );
}

/// A pan driven onto a non-active output launches momentum *there*. The deadline
/// carries the output it was armed for, rather than the callback asking which
/// output happens to be active when it fires.
#[test]
fn a_pan_on_an_inactive_output_auto_launches_momentum_there() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    assert_eq!(
        f.state().active_output(),
        Some(out1.clone()),
        "precondition: the panned output is not the active one"
    );

    pan_burst(&mut f, &out2, 0);
    std::thread::sleep(PAST_MOMENTUM_DEADLINE);
    f.pump(1);

    assert!(coasting(&out2), "the output the hand panned coasts");
    assert!(
        !coasting(&out1),
        "and the active output, which was never panned, does not"
    );
}

/// A real finger lift launches immediately and disarms the deadline; the timer
/// it leaves behind fires once, finds nothing pending, and collects itself
/// without launching a second time.
#[test]
fn an_explicit_launch_leaves_the_armed_timer_to_collect_itself() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    pan_burst(&mut f, &out, 0);
    f.state().launch_momentum();
    assert!(
        f.state().momentum_deadline.is_none(),
        "the lift took the pending auto-launch with it"
    );
    assert!(coasting(&out), "and launched momentum itself");

    output_state(&out).momentum.stop();
    std::thread::sleep(PAST_MOMENTUM_DEADLINE);
    f.pump(1);

    assert!(
        f.state().momentum_timer.is_none(),
        "the orphaned timer collected itself"
    );
    assert!(
        !coasting(&out),
        "and did not launch a second time behind the lift"
    );
}

/// `cancel_animations_on` is per-output — fit, navigation and every grab install
/// route through it — so it must disarm only a launch pending on its own output.
#[test]
fn cancelling_one_output_leaves_anothers_pending_launch_armed() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));

    pan_burst(&mut f, &out2, 0);
    assert!(
        f.state().momentum_deadline.is_some(),
        "precondition: a launch is pending on the second output"
    );

    f.state().cancel_animations_on(&out1);
    assert!(
        f.state().momentum_deadline.is_some(),
        "a cancel on the other output leaves it armed"
    );

    f.state().cancel_animations_on(&out2);
    assert!(
        f.state().momentum_deadline.is_none(),
        "its own output's cancel disarms it"
    );

    std::thread::sleep(PAST_MOMENTUM_DEADLINE);
    f.pump(1);
    assert!(
        !coasting(&out2),
        "so the cancelled burst never coasts after the fact"
    );
}
