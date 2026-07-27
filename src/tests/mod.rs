//! In-process compositor test harness.
//!
//! A real [`DriftWm`](crate::state::DriftWm) runs on its own headless calloop
//! loop with no backend (no renderer, no DRM, no sockets). Real wayland test
//! clients connect over socket pairs, and an outer calloop loop nests both the
//! server loop and every client loop by their epoll fds, so one
//! [`Fixture::dispatch`] pumps the whole graph deterministically.
//!
//! Every scenario is leak-checked at teardown: [`Fixture`]'s `Drop` tears down
//! all clients and asserts `debug_counters` return to the construction-time
//! baseline (opt out with `Fixture::skip_baseline_check`).

mod client;
mod fixture;
mod headless;
mod real;
mod server;

mod auto_navigate_click;
mod auto_placement;
mod bookmarks;
mod camera_animation;
mod cli_docs;
mod client_teardown;
mod config_reload;
mod configure_sequences;
mod cycle_windows;
mod ext_workspace;
mod focus_timing;
mod fullscreen_handoff;
mod gesture_move;
mod gesture_resize;
mod hot_corners;
mod hotplug;
mod hover_focus;
mod interact_min;
mod opacity;
mod popups;
mod real_clients;
mod relaunch;
mod resize_parity;
mod send_to_output;
mod session_restore;
mod soak;
mod stand_in_parity;
mod suspend_flows;
mod suspended;
mod window_animation;
mod window_opening;
mod window_rules;
mod zoom_to_fit;

use fixture::Fixture;

use driftwm::config::Config;
use driftwm::window_ext::WindowExt;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::seat::WaylandFocus;

fn config(toml: &str) -> Config {
    Config::from_toml(toml).unwrap()
}

/// Map a toplevel with `app_id`, attach a buffer at `size`, and settle.
/// Returns the client-side surface for later lookups.
fn map_window(
    f: &mut Fixture,
    id: client::ClientId,
    app_id: &str,
    size: (u16, u16),
) -> wayland_client::protocol::wl_surface::WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

/// Adopt the size the compositor most recently configured: set it client-side,
/// attach a buffer, ack, and settle — the buffer-commit ritual a real client
/// runs to acknowledge a configure.
fn adopt_last_configure(
    f: &mut Fixture,
    id: client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) {
    let (w, h) = f
        .client(id)
        .window(surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    let window = f.client(id).window(surface);
    window.set_size(w as u16, h as u16);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
}

/// Create an xdg popup on the toplevel backing `parent`, map it (attach a
/// buffer and ack), and settle. Returns the client-side popup surface.
fn map_popup(
    f: &mut Fixture,
    id: client::ClientId,
    parent: &wayland_client::protocol::wl_surface::WlSurface,
) -> wayland_client::protocol::wl_surface::WlSurface {
    map_popup_with(f, id, parent, client::PopupProps::default())
}

/// [`map_popup`] with a custom positioner (see [`client::PopupProps`]).
fn map_popup_with(
    f: &mut Fixture,
    id: client::ClientId,
    parent: &wayland_client::protocol::wl_surface::WlSurface,
    props: client::PopupProps,
) -> wayland_client::protocol::wl_surface::WlSurface {
    let surface = f
        .client(id)
        .create_popup_with(parent, props)
        .surface
        .clone();
    settle_popup(f, id, surface)
}

/// [`map_popup_with`] for a popup parented to a layer surface rather than a
/// toplevel (`zwlr_layer_surface_v1.get_popup`).
fn map_layer_popup_with(
    f: &mut Fixture,
    id: client::ClientId,
    parent: &wayland_client::protocol::wl_surface::WlSurface,
    props: client::PopupProps,
) -> wayland_client::protocol::wl_surface::WlSurface {
    let surface = f
        .client(id)
        .create_layer_popup_with(parent, props)
        .surface
        .clone();
    settle_popup(f, id, surface)
}

/// Drive a freshly created popup through its map ritual: initial commit,
/// buffer, ack, settle.
fn settle_popup(
    f: &mut Fixture,
    id: client::ClientId,
    surface: wayland_client::protocol::wl_surface::WlSurface,
) -> wayland_client::protocol::wl_surface::WlSurface {
    f.client(id).popup(&surface).commit();
    f.roundtrip(id);

    let popup = f.client(id).popup(&surface);
    popup.attach_new_buffer();
    popup.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

/// Number of popups the compositor tracks against `root` (a server-side
/// toplevel surface).
fn popups_tracked_on(root: &WlSurface) -> usize {
    smithay::desktop::PopupManager::popups_for_surface(root).count()
}

/// Server-side surface of the first popup tracked against `root`, captured
/// while the parent is still alive so it can be looked up after teardown.
fn first_popup_surface(root: &WlSurface) -> Option<WlSurface> {
    smithay::desktop::PopupManager::popups_for_surface(root)
        .next()
        .map(|(kind, _)| kind.wl_surface().clone())
}

/// Server-side surface that currently holds keyboard focus, if any.
fn keyboard_focus(f: &mut Fixture) -> Option<WlSurface> {
    f.state()
        .seat
        .get_keyboard()
        .unwrap()
        .current_focus()
        .map(|t| t.0)
}

/// Server-side window matching `app_id` (set client-side before first commit).
fn window_by_app_id(f: &mut Fixture, app_id: &str) -> Option<Window> {
    f.state()
        .stage
        .windows()
        .find(|w| w.app_id_or_class().as_deref() == Some(app_id))
        .and_then(|w| w.client())
        .cloned()
}

/// The server-side `WlSurface` backing a stage window.
fn server_surface(window: &Window) -> WlSurface {
    window.wl_surface().unwrap().into_owned()
}

/// Register an SSD title bar for `window`, exactly as the compositor does on the
/// first sized commit under `default_mode = "server"`. The headless test client
/// never binds xdg-decoration, so that commit path doesn't run — do it directly
/// so the hit-tests report the window's chrome bands.
fn give_ssd(f: &mut Fixture, window: &Window) {
    use smithay::reexports::wayland_server::Resource;
    let width = window.geometry().size.w;
    let id = server_surface(window).id();
    let deco =
        crate::decorations::WindowDecoration::new(width, true, &f.state().config.decorations);
    f.state()
        .decorations
        .insert(crate::decorations::DecorationKey::Surface(id), deco);
}

/// Whether the client's most recent configure carried `Maximized` — what a
/// client's own restore button keys off. Shared by every resize arm's test, so
/// the check that keeps the arms in sync isn't itself duplicated.
fn client_sees_maximized(
    f: &mut Fixture,
    id: client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) -> bool {
    f.client(id)
        .window(surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .states
        .contains(&wayland_protocols::xdg::shell::client::xdg_toplevel::State::Maximized)
}

/// Fit `window`, then snap the viewport onto it: the fit only *animates* the
/// camera, while a resize grab clamps the pointer to the output, so an
/// un-settled camera would swallow the drag. Returns a grab point 10px inside
/// the window's right edge — the client hasn't acked the fit size yet, so the
/// rect to aim at is still the pre-fit one.
fn fit_and_frame(
    f: &mut Fixture,
    window: &Window,
    id: client::ClientId,
) -> smithay::utils::Point<f64, smithay::utils::Logical> {
    use crate::state::StageWindow;
    use smithay::utils::Point;

    f.state().fit_window(window);
    let loc = f
        .state()
        .stage
        .position_of(&StageWindow::Client(window.clone()))
        .unwrap();
    f.state().with_output_state(|os| {
        os.camera = Point::from((loc.x as f64, loc.y as f64));
        os.camera_target = None;
        os.zoom = 1.0;
        os.zoom_target = None;
    });
    f.double_roundtrip(id);
    let size = window.geometry().size;
    Point::from((
        loc.x as f64 + size.w as f64 - 10.0,
        loc.y as f64 + size.h as f64 / 2.0,
    ))
}

/// Whether `window`'s toplevel currently carries the xdg `Activated` state
/// (the "focused window" chrome hint the compositor sets exclusively).
fn is_activated(window: &Window) -> bool {
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
    window
        .toplevel()
        .expect("toplevel")
        .with_pending_state(|s| s.states.contains(xdg_toplevel::State::Activated))
}
