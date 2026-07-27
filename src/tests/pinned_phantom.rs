//! A screen-pinned window renders at a fixed screen position, always at scale
//! 1, but its stage entry is a canvas rect sized from the window's own
//! geometry and re-anchored only when the camera moves. At zoom != 1 that rect
//! is a phantom on two counts: it spans `zoom` times the linear screen extent
//! the window really occupies, and a zoom change with a stationary camera
//! leaves its origin behind as well. Canvas-space hit-tests must skip it; the
//! paths that genuinely need pinned coverage ask `pinned_window_under` /
//! `pinned_element_under` in screen space instead.
//!
//! The hover and gesture halves of this live with their own subsystems
//! (`hover_focus.rs`, `gesture_resize.rs`); here are the three consumers that
//! read the phantom through `element_under` — binding context, click-to-focus,
//! and the touch tap.

use driftwm::canvas::{CanvasPos, ScreenPos, canvas_to_screen, screen_to_canvas};
use driftwm::config::{BTN_LEFT, BindingContext};
use smithay::desktop::Window;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use crate::state::StageWindow;

use super::client::ClientId;
use super::input_backend::{FakeDevice, click, touch_down, touch_up};
use super::{Fixture, config, is_activated, map_window, window_by_app_id};

/// Every window here is 400x300 so a rect test needs only its origin.
const W: f64 = 400.0;
const H: f64 = 300.0;

const PIN_RULE: &str = r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [400, 300]
"#;

/// The pin rule plus a single-finger tap, so one finger drives the tap path a
/// 3-finger tap drives on real hardware.
const PIN_RULE_WITH_TAP: &str = r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [400, 300]

[touch.anywhere]
"1-finger-tap" = "center-window"
"#;

fn pt(x: f64, y: f64) -> Point<f64, Logical> {
    Point::from((x, y))
}

fn in_rect(origin: Point<i32, Logical>, p: Point<f64, Logical>) -> bool {
    p.x >= origin.x as f64
        && p.x < origin.x as f64 + W
        && p.y >= origin.y as f64
        && p.y < origin.y as f64 + H
}

fn to_screen(f: &mut Fixture, canvas: Point<f64, Logical>) -> Point<f64, Logical> {
    let (camera, zoom) = (f.state().camera(), f.state().zoom());
    canvas_to_screen(CanvasPos(canvas), camera, zoom).0
}

fn to_canvas(f: &mut Fixture, screen: Point<f64, Logical>) -> Point<f64, Logical> {
    let (camera, zoom) = (f.state().camera(), f.state().zoom());
    screen_to_canvas(ScreenPos(screen), camera, zoom).0
}

/// A pinned window on a 1920x1080 output, then zoom 2 without a pan. Returns
/// the pin, the screen site it keeps rendering at, and the canvas origin its
/// stage entry still claims.
fn pin_then_zoom(
    f: &mut Fixture,
    id: ClientId,
) -> (Window, Point<i32, Logical>, Point<i32, Logical>) {
    map_window(f, id, "pin", (400, 300));
    let pin = window_by_app_id(f, "pin").unwrap();
    let site = f.state().stage.pin_of(&pin).unwrap().screen_pos;
    let phantom = f.state().stage.position_of(&pin).unwrap();
    // Pins re-anchor on camera moves only, so zooming alone strands the entry.
    f.state().with_output_state(|os| os.zoom = 2.0);
    (pin, site, phantom)
}

/// Re-anchor every pin to the live view the way a camera move does, so the
/// phantom rect's *origin* is right and only its extent is still wrong.
/// `update_output_from_camera` re-anchors only when the camera differs from the
/// output's cached position, so nudge it away and back.
fn resync_pins(f: &mut Fixture) {
    let camera = f.state().camera();
    f.state()
        .with_output_state(|os| os.camera = camera + pt(1000.0, 1000.0));
    f.state().update_output_from_camera();
    f.state().with_output_state(|os| os.camera = camera);
    f.state().update_output_from_camera();
}

/// Assert `canvas` sits in the band the phantom rect claims but the pinned
/// window does not actually cover — without this the scenarios below could pass
/// by aiming somewhere the bug never reached.
fn assert_phantom_band(
    f: &mut Fixture,
    site: Point<i32, Logical>,
    phantom: Point<i32, Logical>,
    canvas: Point<f64, Logical>,
) {
    assert!(
        in_rect(phantom, canvas),
        "precondition: {canvas:?} must fall inside the phantom rect at {phantom:?}"
    );
    let screen = to_screen(f, canvas);
    assert!(
        !in_rect(site, screen),
        "precondition: {screen:?} must fall outside the pin as drawn, at {site:?}"
    );
}

/// Nothing is drawn in the phantom band, so a binding there is an on-canvas
/// binding — the stale rect must not make it read as on-window.
#[test]
fn binding_context_in_the_phantom_band_is_on_canvas() {
    let mut f = Fixture::with_config(config(PIN_RULE));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (_pin, site, phantom) = pin_then_zoom(&mut f, id);

    let band = pt(phantom.x as f64 + 100.0, phantom.y as f64 + 60.0);
    assert_phantom_band(&mut f, site, phantom, band);

    assert_eq!(
        f.state().pointer_context(band),
        BindingContext::OnCanvas,
        "empty canvas inside the pin's phantom rect must bind as canvas"
    );
}

/// The extent half of the phantom on its own: after a camera move the entry's
/// origin is correct again, yet a 400x300 window still claims 400x300 *canvas*
/// units — 800x600 screen pixels at zoom 2. No amount of re-anchoring fixes
/// the outer band; only skipping the entry does.
#[test]
fn binding_context_in_the_phantom_band_is_on_canvas_after_a_resync() {
    let mut f = Fixture::with_config(config(PIN_RULE));
    f.add_output(1, (1920, 1080));
    // The resync below pans the camera, which populates blur_camera_generation
    // (it drains only on output disconnect) — end off-baseline like the
    // camera-animation suite.
    f.skip_baseline_check();
    let id = f.add_client();
    let (pin, site, _) = pin_then_zoom(&mut f, id);
    resync_pins(&mut f);
    let phantom = f.state().stage.position_of(&pin).unwrap();

    // Three-quarters across the phantom rect: still inside it, but well past
    // the half that the window's real screen extent accounts for.
    let band = pt(phantom.x as f64 + 300.0, phantom.y as f64 + 225.0);
    assert_phantom_band(&mut f, site, phantom, band);

    assert_eq!(
        f.state().pointer_context(band),
        BindingContext::OnCanvas,
        "the phantom rect's outer band is empty canvas, not the pinned window"
    );
}

/// The regression the screen-space arm exists to prevent: with the canvas walk
/// skipping pinned windows, a point on the pinned window as drawn would read as
/// bare canvas and every on-window binding over it would stop applying.
#[test]
fn binding_context_on_a_pinned_window_as_drawn_is_on_window() {
    let mut f = Fixture::with_config(config(PIN_RULE));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (_pin, site, phantom) = pin_then_zoom(&mut f, id);

    let on_pin = to_canvas(&mut f, pt(site.x as f64 + 200.0, site.y as f64 + 150.0));
    assert!(
        !in_rect(phantom, on_pin),
        "precondition: the phantom rect must not cover the pin as drawn, or the \
         canvas walk would answer this by accident"
    );

    assert_eq!(
        f.state().pointer_context(on_pin),
        BindingContext::OnWindow,
        "the pinned window is really drawn here, so bindings apply on-window"
    );
}

/// Click-to-focus through the real dispatch entry point: the window rendered in
/// the phantom band takes the click, not the pin whose stale rect covers it.
#[test]
fn a_click_in_the_phantom_band_focuses_the_window_really_there() {
    let mut f = Fixture::with_config(config(PIN_RULE));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (pin, site, phantom) = pin_then_zoom(&mut f, id);

    map_window(&mut f, id, "under", (400, 300));
    let under = window_by_app_id(&mut f, "under").unwrap();
    // Cover the phantom rect exactly, then re-raise the pin: the click must
    // reach `under` by skipping the phantom, not because `under` is on top.
    f.state()
        .map_window(StageWindow::Client(under.clone()), phantom, false);
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&pin, serial);
    assert!(is_activated(&pin), "precondition: the pin starts focused");

    let band = pt(phantom.x as f64 + 100.0, phantom.y as f64 + 60.0);
    assert_phantom_band(&mut f, site, phantom, band);

    click(&mut f, &FakeDevice::mouse(), band, BTN_LEFT);
    f.double_roundtrip(id);

    assert!(
        is_activated(&under),
        "the click belongs to the window drawn under the pointer"
    );
    assert!(!is_activated(&pin));
}

/// A tap in the phantom band lands on empty canvas, so it must leave focus
/// alone — the pin is nowhere near the finger.
#[test]
fn a_tap_in_the_phantom_band_does_not_take_the_pinned_window() {
    let mut f = Fixture::with_config(config(PIN_RULE_WITH_TAP));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (pin, site, phantom) = pin_then_zoom(&mut f, id);

    map_window(&mut f, id, "other", (400, 300));
    let other = window_by_app_id(&mut f, "other").unwrap();
    f.state().map_window(
        StageWindow::Client(other.clone()),
        Point::from((0, 0)),
        false,
    );
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&other, serial);

    let band = pt(phantom.x as f64 + 100.0, phantom.y as f64 + 60.0);
    assert_phantom_band(&mut f, site, phantom, band);
    assert!(
        !in_rect(Point::from((0, 0)), band),
        "precondition: nothing but the phantom rect covers the tap point"
    );

    touch_down(&mut f, band, 0);
    touch_up(&mut f, 0);
    f.double_roundtrip(id);

    assert!(
        is_activated(&other),
        "a tap on empty canvas must not move focus"
    );
    assert!(!is_activated(&pin));
}

/// The tap path's screen-space arm: a finger on the pinned window as drawn
/// targets the pin, not whichever canvas window happens to sit at the same
/// point once the phantom rect is out of the way.
#[test]
fn a_tap_on_a_pinned_window_as_drawn_targets_it() {
    let mut f = Fixture::with_config(config(PIN_RULE_WITH_TAP));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (pin, site, phantom) = pin_then_zoom(&mut f, id);

    let on_pin = to_canvas(&mut f, pt(site.x as f64 + 200.0, site.y as f64 + 150.0));
    assert!(
        !in_rect(phantom, on_pin),
        "precondition: the phantom rect must not cover the pin as drawn"
    );

    // A canvas window right where the pin is drawn — visually behind it, and
    // the only thing the canvas walk can find there.
    map_window(&mut f, id, "beneath", (400, 300));
    let beneath = window_by_app_id(&mut f, "beneath").unwrap();
    let origin = Point::from((on_pin.x as i32 - 80, on_pin.y as i32 - 70));
    f.state()
        .map_window(StageWindow::Client(beneath.clone()), origin, false);
    assert!(
        in_rect(origin, on_pin),
        "precondition: the canvas window covers the tap point"
    );

    touch_down(&mut f, on_pin, 0);
    touch_up(&mut f, 0);
    f.double_roundtrip(id);

    assert!(
        is_activated(&pin),
        "the tap belongs to the pinned window drawn under the finger"
    );
    assert!(!is_activated(&beneath));
}
