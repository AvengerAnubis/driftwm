//! Input driven through the real `process_input_event` entry point with the
//! synthetic backend (`input_backend`), not by calling the sub-handlers it
//! dispatches to. Two things only exist there: the hardcoded click-to-focus
//! fallback that runs when no mouse binding matched, and the device-capability
//! gate that decides whether a middle click is buffered for a 3-finger swipe —
//! a question only the event's own device can answer.

use driftwm::config::{BTN_LEFT, BTN_MIDDLE};
use smithay::desktop::Window;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use crate::state::StageWindow;

use super::client::ClientId;
use super::input_backend::{FakeDevice, click, pointer_to, press, touch_down};
use super::{Fixture, keyboard_focus, map_window, server_surface, window_by_app_id};

/// Canvas-space center of `window`'s current geometry.
fn center_of(f: &mut Fixture, window: &Window) -> Point<f64, Logical> {
    let pos = f.state().stage.position_of(window).unwrap();
    let size = window.geometry().size;
    Point::from((
        pos.x as f64 + size.w as f64 / 2.0,
        pos.y as f64 + size.h as f64 / 2.0,
    ))
}

/// Two windows far enough apart to aim at unambiguously, with focus on the
/// second and the camera at the canvas origin (so canvas == screen). Auto
/// placement alone doesn't guarantee where two same-size windows land, and a
/// freshly mapped window is already focused — so a scenario asking "did this
/// input move focus?" needs both pinned down. Returns `(first, second)`.
fn two_windows(f: &mut Fixture, id: ClientId) -> (Window, Window) {
    map_window(f, id, "first", (400, 300));
    let first = window_by_app_id(f, "first").unwrap();
    f.state().map_window(
        StageWindow::Client(first.clone()),
        Point::from((0, 0)),
        false,
    );

    map_window(f, id, "second", (400, 300));
    let second = window_by_app_id(f, "second").unwrap();
    f.state().map_window(
        StageWindow::Client(second.clone()),
        Point::from((1000, 0)),
        false,
    );

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&second, serial);
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.camera_target = None;
        os.zoom = 1.0;
        os.zoom_target = None;
    });
    (first, second)
}

/// An unmodified left click on a window matches no mouse binding, so it lands in
/// the hardcoded fallback: raise + focus.
#[test]
fn click_focuses_the_window_under_the_pointer() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (first, second) = two_windows(&mut f, id);
    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&second)),
        "the second window holds focus before the click"
    );

    let target = center_of(&mut f, &first);
    click(&mut f, &FakeDevice::mouse(), target, BTN_LEFT);
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&first)),
        "a click on a window focuses it"
    );
}

/// A touchpad's middle click may be the tap half of a 3-finger swipe, so it is
/// held back rather than dispatched — nothing else about the press happens yet.
#[test]
fn touchpad_middle_press_is_buffered() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (first, second) = two_windows(&mut f, id);

    let target = center_of(&mut f, &first);
    pointer_to(&mut f, target);
    press(&mut f, &FakeDevice::touchpad(), BTN_MIDDLE);

    assert!(
        f.state().pending_middle_click.is_some(),
        "a gesture-capable device's middle click waits for a possible swipe"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&second)),
        "the buffered press must not have been dispatched yet"
    );
}

/// A mouse has no 3-finger swipe to wait for, so its middle click dispatches
/// immediately — down the same fallback an unmodified click takes.
#[test]
fn mouse_middle_press_is_never_buffered() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (first, _second) = two_windows(&mut f, id);

    let target = center_of(&mut f, &first);
    click(&mut f, &FakeDevice::mouse(), target, BTN_MIDDLE);
    f.double_roundtrip(id);

    assert!(
        f.state().pending_middle_click.is_none(),
        "a device without gesture capability must never have its click delayed"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&first)),
        "the press went straight through to the normal dispatch"
    );
}

/// A finger landing on a window focuses it like a click, hides the pointer
/// cursor, and pins the rest of the sequence to the output it resolved to — a
/// fake device is no libinput device, so that resolution falls back to the
/// only output there is.
#[test]
fn touch_down_focuses_the_window_under_the_finger() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (first, _second) = two_windows(&mut f, id);

    let target = center_of(&mut f, &first);
    touch_down(&mut f, target, 0);
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&first)),
        "a touch on a window focuses it"
    );
    assert!(
        f.state().cursor.hidden_by_touch,
        "touch input hides the pointer cursor"
    );
    assert_eq!(
        f.state().touch_state.output,
        Some(output),
        "the sequence is pinned to the output the finger landed on"
    );
}
