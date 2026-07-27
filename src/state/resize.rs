//! The state every client-side resize starts from, shared by the four entry
//! points that can start one: the pointer, the client's own
//! `xdg_toplevel.resize`, touch, and the trackpad gesture.

use std::cell::RefCell;

use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Size};
use smithay::wayland::compositor::with_states;

use super::DriftWm;
use crate::grabs::ResizeState;

impl DriftWm {
    /// Enter a client-side resize: drop fit/fill membership, seed the
    /// `ResizeState` that `handle_resize_commit` repositions from, and put the
    /// toplevel into `Resizing` without `Maximized`.
    ///
    /// Every resize entry point runs exactly this, and the `Maximized` unset is
    /// the part that is easy to forget: after the fit clear above, a `Maximized`
    /// left set is one the client can never shed — its restore button would
    /// dispatch an unmaximize_request that `unfit_window` silently drops.
    ///
    /// Callers own everything around this: bail checks, edge inference, cursor,
    /// cluster snapshot, grab construction and installation. Nothing here sends
    /// a configure — the pending state rides out on the grab's first motion.
    pub(crate) fn begin_client_resize(
        &mut self,
        window: &Window,
        wl_surface: &WlSurface,
        edges: xdg_toplevel::ResizeEdge,
        initial_window_location: Point<i32, Logical>,
        initial_window_size: Size<i32, Logical>,
        pinned_initial_screen_pos: Option<Point<i32, Logical>>,
    ) {
        // A camera flight still running when this grab installs would read as
        // resize input once a tick warps the pointer into it (same trap
        // `arm_interactive_move` guards against for moves).
        self.cancel_animations_everywhere();
        self.stage.clear_fit(window);
        self.stage.clear_fill(window);

        with_states(wl_surface, |states| {
            states
                .data_map
                .get_or_insert(|| RefCell::new(ResizeState::Idle))
                .replace(ResizeState::Resizing {
                    edges,
                    initial_window_location,
                    initial_window_size,
                    initial_screen_pos: pinned_initial_screen_pos,
                    last_committed_size: initial_window_size,
                });
        });

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
                state.states.unset(xdg_toplevel::State::Maximized);
            });
        }
    }
}
