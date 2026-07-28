//! Relaunch + matching conformance (§9): minting the activation token, the two
//! match signals (token stash pre-/post-first-commit, identity FIFO fallback),
//! the compound adoption (z-slot + `ElementId` continuity, body geometry), the
//! pending lifecycle (relaunch-while-pending no-op, dismiss-in-flight cancel,
//! deadline GC), the "launching…" label, and token cleanup on every exit.
//!
//! The relaunched app is never really forked (a `#[cfg(test)]` seam records the
//! spawn instead); each scenario drives the "returning" client by hand and
//! presents the compositor-minted token via `xdg_activation.activate`.

use std::time::{Duration, Instant};

use driftwm::config::Config;
use driftwm::desktop_entry::DesktopEntryCache;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Point, Size};
use wayland_client::protocol::wl_surface::WlSurface as ClientSurface;

use driftwm::window_ext::WindowExt;

use crate::state::{ClusterResizeSnapshot, StageWindow, SuspendedId};

use super::client::ClientId;
use super::real::TempDir;
use super::{
    Fixture, adopt_last_configure, client_sees_maximized, config, end_grab,
    install_client_resize_grab, map_window, motion, server_surface, window_by_app_id,
};

/// The live client window with `app_id`, if any. Unlike `window_by_app_id`, it
/// skips a same-named suspended stand-in instead of stopping at it.
fn mapped_client(f: &mut Fixture, app_id: &str) -> Option<smithay::desktop::Window> {
    f.state()
        .stage
        .windows()
        .filter_map(|w| w.client())
        .find(|w| w.app_id_or_class().as_deref() == Some(app_id))
        .cloned()
}

fn origin_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
}

/// Seat a desktop-entry cache with a launchable `{stem}.desktop` per stem.
fn inject_cache(f: &mut Fixture, tmp: &TempDir, stems: &[&str]) {
    for stem in stems {
        let contents = format!("[Desktop Entry]\nType=Application\nName={stem}\nExec={stem}\n");
        std::fs::write(tmp.path().join(format!("{stem}.desktop")), contents).unwrap();
    }
    f.state().desktop_entry_cache = Some(DesktopEntryCache::new(vec![tmp.path().to_path_buf()]));
}

/// Insert a dormant suspended stand-in whose identity resolves to `app_id`.
fn insert_suspended(
    f: &mut Fixture,
    id: u64,
    app_id: &str,
    pos: (i32, i32),
    size: (i32, i32),
) -> SuspendedId {
    f.state()
        .insert_suspended_for_test(id, Point::from(pos), Size::from(size), app_id, app_id)
}

/// First half of a client toplevel's map: create + set app_id + commit (no
/// buffer). The window is in `pending_center` at zero size.
fn begin_window(f: &mut Fixture, cid: ClientId, app_id: &str) -> ClientSurface {
    let window = f.client(cid).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.commit();
    f.roundtrip(cid);
    surface
}

/// Second half: attach a buffer at `size`, ack, commit, settle. This is the
/// first *sized* commit — placement (or adoption) runs here.
fn finish_window(f: &mut Fixture, cid: ClientId, surface: &ClientSurface, size: (u16, u16)) {
    let window = f.client(cid).window(surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(cid);
}

/// Present `token` as `surface`'s activation token and drive the request.
fn present_token(f: &mut Fixture, cid: ClientId, surface: &ClientSurface, token: String) {
    f.client(cid).state.activation_token = Some(token);
    f.client(cid).activate(surface);
    f.roundtrip(cid);
}

/// Ack a pending resize (adoption's body-size configure) and commit it, so the
/// adopted window's geometry reflects the body size.
fn settle_resize(f: &mut Fixture, cid: ClientId, surface: &ClientSurface, size: (u16, u16)) {
    let window = f.client(cid).window(surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(cid);
}

fn client_close(f: &mut Fixture, cid: ClientId, surface: &ClientSurface) {
    f.client(cid).window(surface).destroy();
    f.roundtrip(cid);
    f.dispatch();
}

/// The lone suspended stand-in, if any.
fn suspended_present(f: &mut Fixture) -> bool {
    f.state().stage.windows().any(|w| w.suspended().is_some())
}

fn token_count(f: &mut Fixture) -> usize {
    f.state().xdg_activation_state.tokens().count()
}

/// The live client windows in MRU (focus-history) order, front = most recent.
fn mru_client_order(f: &mut Fixture) -> Vec<smithay::desktop::Window> {
    f.state()
        .stage
        .focus_history()
        .iter()
        .filter_map(|w| w.client().cloned())
        .collect()
}

/// Token path, bound before first commit: the marker is honored ahead of both
/// the serial gate (our token is serial-less) and the zero-size early return
/// (the surface has no buffer yet), stashing for the placement arm. Adoption
/// preserves the suspended window's z-slot, `ElementId`, and canvas position,
/// and configures the body size.
#[test]
fn token_adopt_pre_first_commit_preserves_slot_id_and_geometry() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (600, 400));
    // A second window on top so the suspended sits at z-slot 0 (not topmost).
    let bg = f.add_client();
    let bg_surface = map_window(&mut f, bg, "other", (200, 200));

    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    let idx = f.state().stage.windows().position(|w| *w == susp).unwrap();

    f.state().relaunch_suspended(sid);
    assert!(
        f.state().is_suspended_launching(sid),
        "label flipped to launching"
    );
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The relaunched app maps and presents the token before its first buffer.
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    // Marker honored despite zero size: the surface is stashed for adoption.
    assert_eq!(
        f.state().debug_counters()["pending_adoptions"],
        1,
        "the zero-size early return did not eat the marker token"
    );

    // First sized commit adopts.
    finish_window(&mut f, cid, &surface, (300, 200));

    let adopted = window_by_app_id(&mut f, "myapp").expect("relaunched window adopted the slot");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "ElementId preserved"
    );
    assert_eq!(
        f.state().stage.windows().position(|w| *w == adopted),
        Some(idx),
        "z-slot preserved"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500))),
        "seated at the suspended position"
    );
    assert!(
        f.client(cid)
            .window(&surface)
            .configures_received
            .iter()
            .any(|(_, c)| c.size == (600, 400)),
        "configured to the body size"
    );

    // The suspended stand-in and its pending relaunch are gone; token cleaned up.
    assert!(!suspended_present(&mut f), "the stand-in was replaced");
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);
    assert_eq!(
        token_count(&mut f),
        0,
        "the token was deregistered on adopt"
    );

    // Complete the resize handshake: geometry fills the body rect.
    settle_resize(&mut f, cid, &surface, (600, 400));
    assert_eq!(
        window_by_app_id(&mut f, "myapp").unwrap().geometry().size,
        Size::from((600, 400))
    );
    assert_eq!(
        f.state().stage.windows().position(|w| *w == adopted),
        Some(idx),
        "z-slot survived the settle, not just the adopt"
    );

    client_close(&mut f, cid, &surface);
    client_close(&mut f, bg, &bg_surface);
}

/// Adopting a relaunched window into a CSD-origin stand-in reassembles the full
/// window: the stand-in shrank its body under the bar at conversion, so adopt
/// hands the app back the body height + bar, positioned a bar above the body.
/// An SSD-origin adopt (the tests above) keeps the body rect verbatim.
#[test]
fn token_adopt_of_csd_stand_in_reassembles_full_geometry() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let bar = f.state().config.decorations.title_bar_height;
    // A CSD-origin stand-in: body (600,400) at (500,500).
    let sid = f.state().insert_suspended_csd_for_test(
        1,
        Point::from((500, 500)),
        Size::from((600, 400)),
        "myapp",
        "myapp",
    );

    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (300, 200));

    let adopted = window_by_app_id(&mut f, "myapp").expect("relaunched window adopted the slot");
    // Positioned a bar above the body; sized to the full window (body + bar).
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500 - bar))),
        "adopt seats the CSD window a bar above the stand-in body"
    );
    assert!(
        f.client(cid)
            .window(&surface)
            .configures_received
            .iter()
            .any(|(_, c)| c.size == (600, 400 + bar)),
        "configured to the reassembled full size (body + bar)"
    );

    client_close(&mut f, cid, &surface);
}

/// Adopt reassembles a CSD-origin stand-in using the bar height AT ADOPT TIME,
/// not the one in effect when it suspended: a config change while the app sits
/// dormant (e.g. a hot-reload) is not replayed from conversion — the outer
/// rect drifts by the difference, the same drift a live SSD window sees across
/// a reload.
#[test]
fn token_adopt_of_csd_stand_in_uses_bar_height_at_adopt_time() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // A CSD-origin stand-in: body (600,400) at (500,500).
    let sid = f.state().insert_suspended_csd_for_test(
        1,
        Point::from((500, 500)),
        Size::from((600, 400)),
        "myapp",
        "myapp",
    );

    // The bar height changes while the app is dormant.
    f.state().config.decorations.title_bar_height = 40;

    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (300, 200));

    let adopted = window_by_app_id(&mut f, "myapp").expect("relaunched window adopted the slot");
    // Positioned/sized using the CURRENT bar (40), not the default (25).
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500 - 40))),
        "adopt used the bar height at adopt time"
    );
    assert!(
        f.client(cid)
            .window(&surface)
            .configures_received
            .iter()
            .any(|(_, c)| c.size == (600, 400 + 40)),
        "configured to body + the current bar height, not the default"
    );

    client_close(&mut f, cid, &surface);
}

/// Adopting a relaunched window back into a clustered stand-in keeps the
/// cluster: the adopted window seats at the stand-in's slot/rect, so it stays
/// snap-adjacent to the neighbor. Its stable snap rect is owed until the client
/// commits the size the adopt configured — writing one earlier would describe a
/// footprint the client has not drawn — and lands on the stand-in's rect at that
/// settle, so a close in that window can't dissolve the cluster.
#[test]
#[allow(clippy::mutable_key_type)]
fn adopt_seeds_the_stable_rect_when_the_client_settles() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(
        Config::from_toml("[decorations]\ndefault_mode = \"server\"\n").unwrap(),
    );
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // Stand-in "myapp" at a known rect; capture the rect it presents to snap.
    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (600, 400));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let standin_rect = f.state().snap_rect_for(&susp).unwrap();
    let gap = f.state().config.snap_gap as i32;

    // A neighbor client gap-adjacent to the stand-in's right edge, y-overlapping.
    let nb = f.add_client();
    map_window(&mut f, nb, "nb", (400, 400));
    let neighbor = window_by_app_id(&mut f, "nb").unwrap();
    f.state().map_window(
        StageWindow::Client(neighbor.clone()),
        Point::from((standin_rect.x_high as i32 + gap, 500)),
        true,
    );
    let nb_elem = StageWindow::Client(neighbor.clone());
    let rects = f.state().all_windows_with_snap_rects();
    let before = driftwm::layout::cluster::cluster_of(&nb_elem, &rects, f.state().config.snap_gap);
    assert!(
        before.contains(&susp),
        "neighbor clustered with the stand-in"
    );

    // Relaunch and adopt (token presented before the first buffer).
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    // First sized commit adopts at a not-yet-body size.
    finish_window(&mut f, cid, &surface, (300, 200));
    let adopted = mapped_client(&mut f, "myapp").expect("adopted");

    // Owed, not written: the live geometry is still the pre-body configure size.
    let adopted_id = server_surface(&adopted).id();
    assert!(
        f.state().pending_adopt_settle.contains_key(&adopted_id),
        "the adopt owes a stable snap rect"
    );
    assert!(
        !f.state().stable_snap_rects.contains_key(&adopted_id),
        "no stable rect asserted at a size the client has not committed"
    );

    // Once the client settles to the body size, the debt is paid at the
    // window's own footprint — the stand-in's slot (500, 500) and body 600x400,
    // borderless, and without the textless bar every stand-in carries — and the
    // live cluster is intact with the adopted window in the stand-in's place.
    settle_resize(&mut f, cid, &surface, (600, 400));
    let seeded = f
        .state()
        .stable_snap_rects
        .get(&adopted_id)
        .copied()
        .expect("the settle seeded a stable snap rect");
    assert_eq!(seeded.x_low, 500.0);
    assert_eq!(seeded.x_high, 1100.0);
    assert_eq!(seeded.y_low, 500.0);
    assert_eq!(seeded.y_high, 900.0);
    assert!(
        !f.state().pending_adopt_settle.contains_key(&adopted_id),
        "the settle consumed the owed-rect entry"
    );

    let rects = f.state().all_windows_with_snap_rects();
    let after = driftwm::layout::cluster::cluster_of(&nb_elem, &rects, f.state().config.snap_gap);
    assert!(
        after.contains(&StageWindow::Client(adopted)),
        "the adopted live window stayed in the cluster"
    );

    client_close(&mut f, cid, &surface);
}

/// A relaunched client that acks the adopt configure before it redraws keeps
/// committing its pre-adopt (larger) size for a frame or two. Once acked, the
/// unacked-configure bail goes blind, so a stable snap rect asserted at adopt
/// time would make that stale frame read as a grow past the settled footprint —
/// and `reflow_grown_snapped_window` answers a grow into a neighbor by moving
/// the window beside it, straight out of the slot it just adopted.
#[test]
fn adopt_early_ack_straggler_keeps_the_slot_beside_a_neighbor() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (400, 300));

    // A neighbor gap-adjacent to the stand-in's right edge and y-overlapping —
    // the adjacency the reflow needs before it will relocate anything.
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let standin_rect = f.state().snap_rect_for(&susp).unwrap();
    let gap = f.state().config.snap_gap as i32;
    let nb = f.add_client();
    map_window(&mut f, nb, "nb", (400, 400));
    let neighbor = window_by_app_id(&mut f, "nb").unwrap();
    f.state().map_window(
        StageWindow::Client(neighbor),
        Point::from((standin_rect.x_high as i32 + gap, 500)),
        true,
    );

    // Relaunch, then adopt on a first commit that is larger than the stand-in's
    // body — the adopt answers with a body-size configure the client owes.
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (700, 500));

    let adopted = mapped_client(&mut f, "myapp").expect("adopted");
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500))),
        "precondition: the adopt seated the window in the stand-in's slot"
    );

    // Early ack: the configure is acked without a resized frame behind it, so
    // pending configures is now empty.
    f.client(cid).window(&surface).ack_last();

    // Straggler: another pre-adopt-sized frame lands after that ack.
    let window = f.client(cid).window(&surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(cid);

    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500))),
        "a stale pre-adopt frame must not reflow the window out of the slot"
    );

    // Already acked above, so this only draws the resize — re-acking the same
    // serial would be a protocol error.
    let window = f.client(cid).window(&surface);
    window.set_size(400, 300);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(cid);

    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500))),
        "the settled window keeps the slot"
    );
    let adopted_id = server_surface(&adopted).id();
    assert!(
        !f.state().pending_adopt_settle.contains_key(&adopted_id),
        "the settle consumed the owed-rect entry"
    );
    assert!(
        f.state().stable_snap_rects.contains_key(&adopted_id),
        "and paid it off with a rect the client has actually drawn"
    );

    client_close(&mut f, cid, &surface);
}

/// The same early-ack straggler with nothing gap-adjacent to the stand-in: the
/// reflow needs a neighbor to anchor re-placement, which is why hardware only
/// ever saw the jump beside another window. Pins that the fix did not simply
/// move the failure onto the lone-window case.
#[test]
fn adopt_early_ack_straggler_keeps_the_slot_without_a_neighbor() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (700, 500));

    let adopted = mapped_client(&mut f, "myapp").expect("adopted");

    f.client(cid).window(&surface).ack_last();
    let window = f.client(cid).window(&surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(cid);

    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500))),
        "a lone adopted window rides out the straggler in its slot"
    );

    let window = f.client(cid).window(&surface);
    window.set_size(400, 300);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(cid);

    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500))),
        "and settles there"
    );

    client_close(&mut f, cid, &surface);
}

/// The owed rect is per-surface state on a client that may never pay it: one
/// that closes before committing the adopt size must take the entry with it.
#[test]
fn an_adopt_that_never_settles_drops_its_owed_rect_on_close() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (700, 500));

    let adopted = mapped_client(&mut f, "myapp").expect("adopted");
    assert!(
        f.state()
            .pending_adopt_settle
            .contains_key(&server_surface(&adopted).id()),
        "precondition: the adopt left a rect owed"
    );

    client_close(&mut f, cid, &surface);
    assert_eq!(
        f.state().debug_counters()["pending_adopt_settle"],
        0,
        "the surface teardown took the owed-rect entry with it"
    );
}

/// Token path, bound after the window is already mapped: adoption happens in the
/// activation handler with a fresh resize configure, and the adopted window ends
/// up focused (the suspended window held the focus intent).
#[test]
fn token_adopt_post_first_commit_focuses_adopted_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (700, 300), (500, 350));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();

    // The user focused the stand-in, then relaunched it.
    f.state().focus_and_raise_suspended(sid);
    assert_eq!(f.state().gated_suspended_focus(), Some(sid));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    // Close the identity-fallback window so the window maps normally and only
    // the (post-map) token path can adopt it.
    f.state().expire_relaunch_fallback_for_test(sid);

    // The relaunched window maps fully (placed normally) before the token lands.
    let cid = f.add_client();
    let surface = map_window(&mut f, cid, "myapp", (300, 200));
    present_token(&mut f, cid, &surface, token);

    let adopted = window_by_app_id(&mut f, "myapp").unwrap();
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "ElementId preserved"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((700, 300))),
        "relocated onto the suspended rect"
    );
    // Focus intent moved onto the adopted window.
    let server = server_surface(&adopted);
    assert_eq!(
        super::keyboard_focus(&mut f).as_ref(),
        Some(&server),
        "adopted window focused"
    );
    assert!(!suspended_present(&mut f));
    assert_eq!(token_count(&mut f), 0);

    settle_resize(&mut f, cid, &surface, (500, 350));
    client_close(&mut f, cid, &surface);
}

/// A single-instance app forwards the startup id to its already-open window,
/// which then presents our token. Token possession is proof the window is the
/// app's own answer to this relaunch, so it is adopted into the stand-in's slot:
/// relocated onto the stand-in rect, inheriting its `ElementId`, sized to the
/// body, and consuming the stand-in.
#[test]
fn already_open_same_app_window_is_adopted() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // An existing window of the app is already open (mapped before the relaunch).
    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));

    // A suspended stand-in of the same app is relaunched.
    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    // Past the fallback window, so identity matching can't fire either — only
    // the token path can adopt the already-open window.
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The running instance activates its EXISTING window with our token.
    present_token(&mut f, cid, &existing, token);

    // The already-open window now occupies the stand-in's stage entry: same
    // ElementId (its own prior entry was consumed by the adopt).
    let adopted = window_by_app_id(&mut f, "myapp").expect("the existing window adopted the slot");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's ElementId"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "relocated onto the stand-in rect"
    );
    assert!(
        f.client(cid)
            .window(&existing)
            .configures_received
            .iter()
            .any(|(_, c)| c.size == (400, 300)),
        "configured to the stand-in body size"
    );
    assert!(!suspended_present(&mut f), "the stand-in was consumed");
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        0,
        "the pending relaunch was consumed"
    );
    assert_eq!(
        token_count(&mut f),
        0,
        "the token was deregistered on adopt"
    );

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// Keyboard focus rides an already-open adopt the same as a freshly-mapped one:
/// the stand-in held focus, so the window it hands off to inherits it and gets
/// an Activated configure on the wire.
#[test]
fn already_open_adopt_focuses_and_activates_the_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    // The stand-in holds focus — the user is waiting on this relaunch.
    f.state().focus_and_raise_suspended(sid);
    f.state().relaunch_suspended(sid);
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    present_token(&mut f, cid, &existing, token);

    let adopted = window_by_app_id(&mut f, "myapp").expect("the existing window adopted the slot");
    assert_eq!(
        f.state().focused_window().as_ref(),
        Some(&adopted),
        "the adopted window inherits keyboard focus"
    );
    let configs = f.client(cid).window(&existing).format_recent_configures();
    assert!(
        configs.contains("Activated"),
        "an adopted window inheriting focus must get an Activated configure, got:\n{configs}"
    );

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// An already-fit window that forwards a relaunch token is adopted the same as
/// any other already-open window (fit is not one of the exclusions) — but the
/// adopt configure must clear the client's `Maximized`, or its restore button
/// is left permanently dead: the adopted window inherits the stand-in's
/// fit-less stage entry, so the `unmaximize_request` that button dispatches
/// finds `unfit_window` early-returning at a `None` `fit_saved_size`. Same bug
/// class as the four resize arms in `resize_parity.rs` / `gesture_resize.rs`;
/// this is the fifth arm.
#[test]
fn adopt_of_an_already_fit_window_clears_the_client_maximized_state() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));
    let win = window_by_app_id(&mut f, "myapp").unwrap();

    f.state().toggle_fit_window(&win);
    f.double_roundtrip(cid);
    assert!(
        client_sees_maximized(&mut f, cid, &existing),
        "precondition: the fit told the client it is maximized"
    );

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    // Past the fallback window, so identity matching can't fire — only the
    // token path adopts the already-fit window.
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    present_token(&mut f, cid, &existing, token);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted");
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "precondition: adopt seated the window at the stand-in's slot"
    );
    assert!(
        !client_sees_maximized(&mut f, cid, &existing),
        "the adopt configure told the client it is no longer maximized"
    );

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// A window mid fit-exit settle — the client has not yet acked the restore
/// configure, so a `pending_recenter` is still owed — that then forwards a
/// live relaunch token must not have that stale recenter fire once it settles
/// into the stand-in's slot: the recenter's `target_center` is the window's
/// OLD pre-fit position, so completing it would re-map the freshly adopted
/// window right back out of the slot the adopt just seated it in.
#[test]
fn adopt_drops_an_owed_fit_exit_recenter_so_it_cannot_pull_the_window_out_of_the_slot() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let cid = f.add_client();
    let surface = map_window(&mut f, cid, "myapp", (300, 200));
    let win = window_by_app_id(&mut f, "myapp").unwrap();

    // Fit, then adopt the fit size as a real client would.
    f.state().toggle_fit_window(&win);
    f.double_roundtrip(cid);
    let (fw, fh) = f
        .client(cid)
        .window(&surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    let cw = f.client(cid).window(&surface);
    cw.set_size(fw as u16, fh as u16);
    cw.attach_new_buffer();
    cw.ack_last_and_commit();
    f.double_roundtrip(cid);
    assert!(f.state().stage.is_fit(&win), "precondition: fit");

    // Unfit: a different-size exit, so a real pending_recenter is left owed —
    // the client never acks this restore configure.
    f.state().toggle_fit_window(&win);
    let root = server_surface(&win);
    assert!(
        f.state().pending_recenter.contains_key(&root.id()),
        "precondition: an unfit-exit recenter is owed"
    );

    // Before that recenter ever settles, a relaunch token lands on this same
    // live window (the app forwards it, single-instance style) and adopts it
    // into a stand-in's slot elsewhere on the canvas.
    let sid = insert_suspended(&mut f, 1, "myapp", (1400, 900), (400, 300));
    f.state().relaunch_suspended(sid);
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    present_token(&mut f, cid, &surface, token);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted");
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((1400, 900))),
        "precondition: adopt seated the window at the stand-in's slot"
    );

    // The client acks the adopt configure at the stand-in's body size — a size
    // change from the still-outstanding fit-exit's pre_exit_size, exactly the
    // commit that would fire a surviving recenter.
    settle_resize(&mut f, cid, &surface, (400, 300));

    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((1400, 900))),
        "the adopt configure's own settle must not re-map the window out of the stand-in's slot"
    );

    client_close(&mut f, cid, &surface);
}

/// Two stand-ins of the same app are both relaunched; the first spawn's window
/// maps and adopts stand-in #1. The second pending relaunch's token then lands
/// on that now-placed window — last press wins: the window rehomes into
/// stand-in #2's slot instead of the already-placed token being ignored.
#[test]
fn later_token_rehomes_an_already_adopted_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid1 = insert_suspended(&mut f, 1, "myapp", (100, 100), (400, 300));
    let sid2 = insert_suspended(&mut f, 2, "myapp", (900, 600), (500, 350));
    let susp2 = StageWindow::Suspended(f.state().find_suspended(sid2).unwrap());
    let eid2 = f.state().stage.id_of(&susp2).unwrap();

    f.state().relaunch_suspended(sid1);
    f.state().relaunch_suspended(sid2);
    let token1 = f.state().pending_relaunch_token_for_test(sid1).unwrap();
    let token2 = f.state().pending_relaunch_token_for_test(sid2).unwrap();

    // The relaunched app's window presents stand-in #1's token before its first
    // buffer and adopts that slot.
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token1);
    finish_window(&mut f, cid, &surface, (300, 200));
    assert!(
        !f.state().is_suspended_launching(sid1),
        "stand-in #1's relaunch settled first"
    );

    // The same window then presents stand-in #2's still-pending token.
    present_token(&mut f, cid, &surface, token2);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid2),
        "took stand-in #2's ElementId"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((900, 600))),
        "rehomed onto stand-in #2's rect"
    );
    assert!(
        f.client(cid)
            .window(&surface)
            .configures_received
            .iter()
            .any(|(_, c)| c.size == (500, 350)),
        "configured to stand-in #2's body size"
    );
    assert!(!suspended_present(&mut f), "both stand-ins were consumed");
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);
    assert_eq!(token_count(&mut f), 0, "both tokens were deregistered");

    settle_resize(&mut f, cid, &surface, (500, 350));
    client_close(&mut f, cid, &surface);
}

/// Identity fallback (Signal B): a token-less window of the same app is adopted
/// within the 5s window, oldest pending first (FIFO), each landing on its own
/// suspended rect via `ElementId`.
#[test]
fn identity_fallback_adopts_fifo_within_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid1 = insert_suspended(&mut f, 1, "myapp", (100, 100), (400, 300));
    let sid2 = insert_suspended(&mut f, 2, "myapp", (900, 600), (500, 350));
    let susp1 = StageWindow::Suspended(f.state().find_suspended(sid1).unwrap());
    let e1 = f.state().stage.id_of(&susp1).unwrap();
    let susp2 = StageWindow::Suspended(f.state().find_suspended(sid2).unwrap());
    let e2 = f.state().stage.id_of(&susp2).unwrap();

    // Relaunch both; sid1 was spawned first, so it adopts first.
    f.state().relaunch_suspended(sid1);
    f.state().relaunch_suspended(sid2);

    let cid = f.add_client();
    // First token-less window adopts the oldest pending (sid1).
    let s1 = map_window(&mut f, cid, "myapp", (300, 200));
    let w1 = f.state().stage.window_by_id(e1).unwrap().clone();
    assert!(w1.client().is_some(), "sid1's slot now holds a live window");
    assert_eq!(
        f.state().stage.position_of(&w1),
        Some(Point::from((100, 100)))
    );

    // Second token-less window adopts the next pending (sid2).
    let s2 = map_window(&mut f, cid, "myapp", (300, 200));
    let w2 = f.state().stage.window_by_id(e2).unwrap().clone();
    assert!(w2.client().is_some(), "sid2's slot now holds a live window");
    assert_eq!(
        f.state().stage.position_of(&w2),
        Some(Point::from((900, 600)))
    );

    assert!(!suspended_present(&mut f), "both stand-ins were adopted");
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);
    assert_eq!(token_count(&mut f), 0);

    settle_resize(&mut f, cid, &s1, (400, 300));
    settle_resize(&mut f, cid, &s2, (500, 350));
    client_close(&mut f, cid, &s1);
    client_close(&mut f, cid, &s2);
}

/// Once the 5s fallback window closes, a token-less same-app window is NO longer
/// captured — it gets normal placement — while the relaunch itself stays pending
/// (only the identity fallback lapsed, not the whole relaunch).
#[test]
fn identity_fallback_expiry_yields_normal_placement() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (200, 200), (400, 300));
    f.state().relaunch_suspended(sid);
    f.state().expire_relaunch_fallback_for_test(sid);

    let cid = f.add_client();
    let surface = map_window(&mut f, cid, "myapp", (300, 200));
    let mapped = mapped_client(&mut f, "myapp").expect("the window mapped");
    assert_ne!(
        f.state().stage.position_of(&mapped),
        Some(Point::from((200, 200))),
        "the expired fallback did not capture the window"
    );
    // A surviving stand-in proves the window was not adopted (adoption would
    // have consumed it).
    assert!(suspended_present(&mut f), "the stand-in is still dormant");
    assert!(
        f.state().is_suspended_launching(sid),
        "still pending after fallback lapse"
    );

    // Cleanup: dismiss cancels the pending (and its token).
    f.state().dismiss_suspended(sid);
    assert_eq!(token_count(&mut f), 0);
    client_close(&mut f, cid, &surface);
}

/// A relaunched window that entered fullscreen (its own request or a rule)
/// before presenting a late token must NOT be adopted: adoption would rip it out
/// of the fullscreen map and strand the camera park. The late-token arm dismisses
/// the stand-in and leaves the window fullscreen, camera restore intact.
#[test]
fn late_token_does_not_adopt_a_fullscreen_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    // No camera override: leaving the camera output-aligned keeps the fullscreen
    // park a no-op, so the blur-generation counter returns to baseline.

    let sid = insert_suspended(&mut f, 1, "myapp", (400, 300), (500, 350));
    f.state().focus_and_raise_suspended(sid);
    assert!(f.state().relaunch_suspended(sid));
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    // Expire the identity fallback so the window maps normally (not adopted at
    // first commit) — only the late token could adopt it.
    f.state().expire_relaunch_fallback_for_test(sid);

    // The relaunched window maps, then enters fullscreen (own request) before the
    // token lands.
    let cid = f.add_client();
    let surface = map_window(&mut f, cid, "myapp", (300, 200));
    f.client(cid).window(&surface).set_fullscreen(None);
    f.double_roundtrip(cid);
    let window = mapped_client(&mut f, "myapp").expect("mapped");
    assert!(
        f.state().is_window_fullscreen(&window),
        "the window entered fullscreen"
    );

    // The late token arrives: adoption is refused.
    present_token(&mut f, cid, &surface, token);

    assert!(
        f.state().is_window_fullscreen(&window),
        "the window stays fullscreen — not ripped out of the map"
    );
    assert!(
        !suspended_present(&mut f),
        "the obsolete stand-in was dismissed"
    );
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        0,
        "the pending relaunch was consumed"
    );
    assert_eq!(token_count(&mut f), 0, "the token was deregistered");

    // Camera restore intact: fullscreen exits cleanly (the debug_assert_eq in
    // exit_fullscreen_on would fire if the fullscreen halves had diverged).
    let out_name = f
        .state()
        .stage
        .fullscreen_output_of(&window)
        .unwrap()
        .to_string();
    let output = f.state().output_by_name(&out_name).unwrap();
    f.state().exit_fullscreen_on(&output);
    assert!(
        !f.state().stage.has_fullscreen(),
        "fullscreen exited cleanly"
    );

    client_close(&mut f, cid, &surface);
}

/// A widget (rule-placed off the normal window flow) that already sits open and
/// then presents a live relaunch token must NOT be adopted: hijacking it into
/// the stand-in's slot would rip it out of its rule placement. The token is
/// honored by dismissing the now-stale stand-in and leaving the widget exactly
/// where it is.
#[test]
fn late_token_does_not_adopt_a_widget_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(
        Config::from_toml("[[window_rules]]\napp_id = \"myapp\"\nwidget = true\n").unwrap(),
    );
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // A widget of the app is already open (rule-placed before the relaunch).
    let cid = f.add_client();
    let widget = map_window(&mut f, cid, "myapp", (300, 200));
    let widget_win = window_by_app_id(&mut f, "myapp").unwrap();
    let pos_before = f.state().stage.position_of(&widget_win);

    // A suspended stand-in of the same app is relaunched.
    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The widget's own client presents the token back.
    present_token(&mut f, cid, &widget, token);

    assert_eq!(
        f.state().stage.position_of(&widget_win),
        pos_before,
        "the widget was not relocated"
    );
    assert!(
        !suspended_present(&mut f),
        "the now-stale stand-in was dismissed"
    );
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        0,
        "the pending relaunch was consumed"
    );
    assert_eq!(token_count(&mut f), 0, "the token was deregistered");

    client_close(&mut f, cid, &widget);
}

/// A dialog (a toplevel with a parent) that presents a live relaunch token must
/// NOT be adopted. Every suspend path excludes dialogs, so no stand-in ever
/// stands for one; adopting the dialog would tear a preferences window off its
/// parent. The token is honored by dismissing the stale stand-in and leaving the
/// dialog with its parent.
#[test]
fn late_token_does_not_adopt_a_dialog() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // A single-instance app is already open with a child dialog (same client —
    // a toplevel's parent must be its own client's toplevel).
    let cid = f.add_client();
    let parent = map_window(&mut f, cid, "myapp", (300, 200));
    let parent_toplevel = f.client(cid).window(&parent).xdg_toplevel.clone();
    let dialog = f.client(cid).create_window();
    let dsurface = dialog.surface.clone();
    dialog.set_app_id("dialog");
    dialog.set_parent(Some(&parent_toplevel));
    dialog.commit();
    f.roundtrip(cid);
    let dwin = f.client(cid).window(&dsurface);
    dwin.set_size(300, 200);
    dwin.attach_new_buffer();
    dwin.ack_last_and_commit();
    f.double_roundtrip(cid);
    let dialog_win = window_by_app_id(&mut f, "dialog").unwrap();
    let pos_before = f.state().stage.position_of(&dialog_win);

    // A suspended stand-in of the app is relaunched; the dialog forwards the token.
    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    present_token(&mut f, cid, &dsurface, token);

    assert_eq!(
        f.state().stage.position_of(&dialog_win),
        pos_before,
        "the dialog was not relocated"
    );
    assert!(
        !suspended_present(&mut f),
        "the now-stale stand-in was dismissed"
    );
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        0,
        "the pending relaunch was consumed"
    );
    assert_eq!(token_count(&mut f), 0, "the token was deregistered");

    client_close(&mut f, cid, &dsurface);
    client_close(&mut f, cid, &parent);
}

/// A screen-pinned window that presents a live relaunch token must NOT be
/// adopted: hijacking it into the stand-in slot would rip it out of its pin.
/// Same carve-out branch as fullscreen — the token dismisses the stale stand-in
/// and leaves the window pinned at its site.
#[test]
fn late_token_does_not_adopt_a_pinned_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(
        Config::from_toml("[[window_rules]]\napp_id = \"myapp\"\npinned_to_screen = true\n")
            .unwrap(),
    );
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // A pinned window of the app is already open (rule-pinned at map).
    let cid = f.add_client();
    let win_surface = map_window(&mut f, cid, "myapp", (300, 200));
    let pinned = window_by_app_id(&mut f, "myapp").unwrap();
    assert!(f.state().is_pinned(&pinned), "the window pinned via rule");
    let site_before = f.state().stage.pin_of(&pinned).cloned();

    // A suspended stand-in of the same app is relaunched; the pinned window
    // forwards the token back.
    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    present_token(&mut f, cid, &win_surface, token);

    assert!(
        f.state().is_pinned(&pinned),
        "the window stayed pinned — not adopted"
    );
    assert_eq!(
        f.state().stage.pin_of(&pinned).cloned(),
        site_before,
        "the pin site is unchanged"
    );
    assert!(
        !suspended_present(&mut f),
        "the stale stand-in was dismissed"
    );
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);
    assert_eq!(token_count(&mut f), 0, "the token was deregistered");

    client_close(&mut f, cid, &win_surface);
}

/// Adopting an UNFOCUSED already-open window preserves its MRU *slot*, not just
/// its presence. The stand-in didn't hold focus and a newer window does, so the
/// refocus path doesn't run — the adopted window must keep its exact place in
/// the Alt-Tab order, never getting silently dropped or front-pushed.
#[test]
fn adopt_of_unfocused_window_keeps_focus_history() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // A window of the app opens (focused on map); then B opens and takes focus, so A is
    // unfocused and sits behind B in the MRU: order is [B, A].
    let cid = f.add_client();
    let a = map_window(&mut f, cid, "myapp", (300, 200));
    let bid = f.add_client();
    let b = map_window(&mut f, bid, "other", (300, 200));
    let a_win = window_by_app_id(&mut f, "myapp").unwrap();
    let b_win = window_by_app_id(&mut f, "other").unwrap();
    assert_eq!(
        f.state().focused_window().as_ref(),
        Some(&b_win),
        "B holds focus, A is unfocused"
    );
    let order_before = mru_client_order(&mut f);
    assert_eq!(
        order_before,
        vec![b_win.clone(), a_win.clone()],
        "MRU is [B, A] before adoption — A trails B"
    );

    // Relaunch a same-app stand-in; unfocused A forwards the token.
    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    present_token(&mut f, cid, &a, token);

    let adopted = window_by_app_id(&mut f, "myapp").expect("A adopted the slot");
    assert_eq!(
        f.state().focused_window().as_ref(),
        Some(&b_win),
        "focus stayed on B — the non-refocus adopt path ran"
    );
    assert_eq!(
        mru_client_order(&mut f),
        vec![b_win, adopted],
        "the adopted window kept A's exact MRU slot (behind B), not front-pushed or dropped"
    );

    settle_resize(&mut f, cid, &a, (400, 300));
    client_close(&mut f, cid, &a);
    client_close(&mut f, bid, &b);
}

/// A window under an active interactive move grab must NOT be adopted mid-drag:
/// teleporting it into the stand-in slot would fight the live grab. The token
/// stashes the adopt — leaving the pending relaunch live and the stand-in intact
/// — rather than dismissing, and the drag's end lands it off the stash alone,
/// without the app presenting the token again.
#[test]
fn mid_move_grab_defers_adoption_then_adopts_when_it_ends() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // An existing window of the app is open; a same-app stand-in is relaunched.
    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));
    let win = window_by_app_id(&mut f, "myapp").unwrap();
    let pos_before = f.state().stage.position_of(&win);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The window is under a live interactive move grab; the token arrives.
    f.state().arm_interactive_move(&win);
    present_token(&mut f, cid, &existing, token);

    // Defer, not adopt: the window stayed put, the stand-in and pending survive.
    assert_eq!(
        f.state().stage.position_of(&win),
        pos_before,
        "the window was not teleported out from under its grab"
    );
    assert!(
        suspended_present(&mut f),
        "the stand-in was retained, not dismissed"
    );
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        1,
        "the pending relaunch stays live for its TTL"
    );
    assert_eq!(token_count(&mut f), 1, "the token was not deregistered");
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the adopt was stashed for the grab's release"
    );

    // The drag ends: the stash alone lands the adopt.
    f.state().disarm_interactive_move(&win);
    f.pump(1);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted once the grab ended");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's ElementId"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "relocated onto the stand-in rect after the grab cleared"
    );
    assert!(
        !suspended_present(&mut f),
        "the stand-in was consumed by the adopt"
    );

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// The first-commit path must not adopt into a stand-in the user is dragging:
/// the adopt destroys the stand-in, leaving the grab that was driving it pushing
/// air. The relaunched window takes normal placement instead — a state it can
/// sit in indefinitely — and the stashed adopt lands the moment the drag ends,
/// without the app committing or activating again.
#[test]
fn first_commit_adopt_defers_under_a_stand_in_drag_then_lands_on_release() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The user grabs the stand-in while the app is still starting up.
    f.state().arm_interactive_move(&sid);

    // The relaunched app maps, presents its token, and reaches the first sized
    // commit — the adopt point.
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (300, 200));

    let placed = mapped_client(&mut f, "myapp").expect("the window mapped");
    assert!(
        suspended_present(&mut f),
        "the stand-in survived the commit that would have consumed it"
    );
    assert_ne!(
        f.state().stage.id_of(&placed),
        Some(eid),
        "the window was placed on its own, not seated in the dragged stand-in's slot"
    );
    assert!(
        f.state().camera_target().is_none(),
        "the placement staged no camera flight: a pan warps the pointer into the live grab"
    );
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the adopt was stashed for the grab's release"
    );
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        1,
        "the pending relaunch stays live for its TTL"
    );
    assert_eq!(token_count(&mut f), 1, "the token was not deregistered");

    // The drag ends: the adopt lands off the release alone.
    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted once the grab ended");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's ElementId"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "relocated onto the stand-in rect after the grab cleared"
    );
    assert!(
        !suspended_present(&mut f),
        "the stand-in was consumed by the adopt"
    );
    assert_eq!(f.state().debug_counters()["deferred_adoptions"], 0);

    settle_resize(&mut f, cid, &surface, (400, 300));
    client_close(&mut f, cid, &surface);
}

/// The token path defers on the same grab, read from the other side: the window
/// presenting the token is idle, and it is the *stand-in* the user is dragging.
/// Adopting would still destroy it mid-drag, so the adopt waits for the release
/// — and lands there without the app presenting the token a second time.
#[test]
fn token_adopt_defers_under_a_stand_in_drag_then_lands_on_release() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // An existing window of the app is open; a same-app stand-in is relaunched.
    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));
    let win = window_by_app_id(&mut f, "myapp").unwrap();
    let pos_before = f.state().stage.position_of(&win);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The stand-in, not the window, is the one under the live grab.
    f.state().arm_interactive_move(&sid);
    present_token(&mut f, cid, &existing, token);

    assert_eq!(
        f.state().stage.position_of(&win),
        pos_before,
        "the window was not teleported into the dragged slot"
    );
    assert!(
        suspended_present(&mut f),
        "the stand-in was retained, not dismissed"
    );
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the adopt was stashed for the grab's release"
    );
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        1,
        "the pending relaunch stays live for its TTL"
    );
    assert_eq!(token_count(&mut f), 1, "the token was not deregistered");

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted once the drag ended");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's ElementId"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "relocated onto the stand-in rect after the grab cleared"
    );
    assert!(
        !suspended_present(&mut f),
        "the stand-in was consumed by the adopt"
    );

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// A drag that outlives the 30s relaunch deadline is the deferral's end state:
/// the deadline sweep reclaims the pending relaunch, the release finds nothing
/// to adopt into, and the window keeps the placement it was given while the
/// stand-in stays behind as a stale duplicate — exactly what an app that took
/// longer than the TTL to come back leaves behind.
#[test]
fn an_adopt_deferred_past_the_relaunch_deadline_leaves_a_stale_stand_in() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    f.state().arm_interactive_move(&sid);
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (300, 200));
    assert_eq!(f.state().debug_counters()["deferred_adoptions"], 1);

    // The drag is still going when the deadline passes.
    f.state()
        .sweep_pending_relaunches(Instant::now() + Duration::from_secs(31));
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    assert!(
        suspended_present(&mut f),
        "the stand-in stays behind as a stale duplicate"
    );
    let placed = mapped_client(&mut f, "myapp").expect("the window kept its own placement");
    assert_ne!(
        f.state().stage.id_of(&placed),
        Some(eid),
        "the expired relaunch was not revived into an adopt"
    );
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        0,
        "the stash drained on the release instead of lingering"
    );

    f.state().dismiss_suspended(sid);
    client_close(&mut f, cid, &surface);
}

/// A second presentation of the token while the deferral is still outstanding is
/// idempotent: one window can only ever adopt one stand-in, so the stash holds a
/// single entry for that surface and the release lands a single adopt. (The grab
/// is still live throughout, so no flush can pre-empt the re-presentation.)
#[test]
fn a_token_re_presented_under_the_grab_stays_one_deferred_adopt() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));
    let win = window_by_app_id(&mut f, "myapp").unwrap();
    let pos_before = f.state().stage.position_of(&win);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    f.state().arm_interactive_move(&win);
    present_token(&mut f, cid, &existing, token.clone());
    present_token(&mut f, cid, &existing, token);

    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the re-presented token replaced the stash rather than stacking a second entry"
    );
    assert_eq!(
        f.state().stage.position_of(&win),
        pos_before,
        "neither presentation teleported the window out from under its grab"
    );
    assert!(suspended_present(&mut f), "the stand-in was retained");
    assert_eq!(token_count(&mut f), 1, "the token was not deregistered");

    f.state().disarm_interactive_move(&win);
    f.pump(1);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted once the grab ended");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's ElementId"
    );
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        0,
        "the single stash entry drained"
    );
    assert!(!suspended_present(&mut f));

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// One drive of the assertion below. The hooks are `fn` pointers rather than
/// closures because no case needs to capture anything.
struct DeferredAdoptCase<'a> {
    rules: &'a str,
    /// Runs after the token is presented, before the first sized commit.
    before_first_commit: fn(&mut Fixture, ClientId, &ClientSurface),
    /// `Some(size)` when `rules` forces one and only starts matching at the
    /// first sized commit: the rule configures there and defers the rest of
    /// placement to the client's follow-up commit at that size, which runs the
    /// whole block a second time. (A rule that already matched at the initial
    /// zero-size commit spends its one-shot there instead, so the sized commit
    /// is the only pass.)
    size_pass: Option<(u16, u16)>,
    /// Runs between the two placement passes, where the surface is back in
    /// `pending_center` and a client request queues instead of applying.
    before_size_pass: fn(&mut Fixture, ClientId, &ClientSurface),
    /// Set when `rules` makes the window a widget, which leaves the camera
    /// assertion below no work: the whole navigate block is skipped for a
    /// widget whatever the deferral says, so the flight it guards against
    /// cannot be staged in the first place.
    widget: bool,
}

impl Default for DeferredAdoptCase<'_> {
    fn default() -> Self {
        Self {
            rules: "",
            before_first_commit: |_, _, _| {},
            size_pass: None,
            before_size_pass: |_, _, _| {},
            widget: false,
        }
    }
}

/// The first-commit path resolves adoption *ahead* of window rules and of the
/// fullscreen/fit a client can queue before its first buffer, so an adopt it
/// deferred under a stand-in drag must still beat them when it lands —
/// otherwise something the user never aimed at the stand-in silently destroys
/// the thing they are holding. Drives one case end to end: relaunch, grab the
/// stand-in, let the app reach its first sized commit under the grab (and, for
/// a size rule, the second placement pass that commit sets up), release.
fn assert_a_deferred_first_commit_adopt_wins(case: DeferredAdoptCase) {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config(case.rules));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    // Long enough a drag that the identity fallback has lapsed, so the stashed
    // token is the only thing that can still resolve the adopt.
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The user grabs the stand-in while the app is still starting up.
    f.state().arm_interactive_move(&sid);

    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    (case.before_first_commit)(&mut f, cid, &surface);
    finish_window(&mut f, cid, &surface, (300, 200));
    if let Some(size) = case.size_pass {
        (case.before_size_pass)(&mut f, cid, &surface);
        // Acking the rule's forced size is the second placement pass.
        settle_resize(&mut f, cid, &surface, size);
    }

    let placed = mapped_client(&mut f, "myapp").expect("the window mapped");
    assert!(
        suspended_present(&mut f),
        "the stand-in survived the commit that would have consumed it"
    );
    assert!(
        !f.state().is_window_fullscreen(&placed) && !f.state().is_pinned(&placed),
        "the membership was suppressed for the deferral, not established and then torn down"
    );
    if !case.widget {
        assert!(
            f.state().camera_target().is_none(),
            "the placement staged no camera flight: a pan warps the pointer into the live grab"
        );
    }
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the adopt was stashed for the grab's release"
    );

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    let adopted = mapped_client(&mut f, "myapp").expect("the deferred adopt landed");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's slot — nothing dismissed it at the flush"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "relocated onto the stand-in rect the user dragged it to"
    );
    assert!(
        !suspended_present(&mut f),
        "the stand-in was consumed by the adopt, not dismissed"
    );

    settle_resize(&mut f, cid, &surface, (400, 300));
    client_close(&mut f, cid, &surface);
}

#[test]
fn a_fullscreen_rule_loses_to_a_deferred_first_commit_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        rules: r#"
[[window_rules]]
app_id = "myapp"
fullscreen = true
"#,
        ..Default::default()
    });
}

#[test]
fn a_pin_rule_loses_to_a_deferred_first_commit_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        rules: r#"
[[window_rules]]
app_id = "myapp"
pinned_to_screen = true
"#,
        ..Default::default()
    });
}

#[test]
fn a_widget_rule_loses_to_a_deferred_first_commit_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        rules: r#"
[[window_rules]]
app_id = "myapp"
widget = true
"#,
        widget: true,
        ..Default::default()
    });
}

/// A title-matched rule starts applying only once the app names its window,
/// which for many toolkits is the commit that brings the first buffer — so the
/// forced `size` configures *there* and hands the rest of placement to a second
/// pass. That pass can no longer re-derive the adopt (the token stash was spent
/// on the first, the identity fallback has lapsed), so the suppression has to
/// outlive the pass that established it or the rule pins the window after all.
/// `size` + `pinned_to_screen` is a common rule shape.
#[test]
fn a_pin_rule_with_a_size_loses_to_a_deferred_first_commit_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        rules: r#"
[[window_rules]]
title = "ready"
pinned_to_screen = true
size = [500, 400]
"#,
        before_first_commit: name_the_window,
        size_pass: Some((500, 400)),
        ..Default::default()
    });
}

/// The fullscreen rule through the same two passes.
#[test]
fn a_fullscreen_rule_with_a_size_loses_to_a_deferred_first_commit_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        rules: r#"
[[window_rules]]
title = "ready"
fullscreen = true
size = [500, 400]
"#,
        before_first_commit: name_the_window,
        size_pass: Some((500, 400)),
        ..Default::default()
    });
}

/// Give the window the title the size rules above match on, after its initial
/// commit — that is what leaves the rule's one-shot size configure unspent until
/// the first sized commit, and so splits placement across two passes.
fn name_the_window(f: &mut Fixture, cid: ClientId, surface: &ClientSurface) {
    f.client(cid).window(surface).set_title("ready");
    f.roundtrip(cid);
}

/// A client that asks for fullscreen before its first buffer is the same
/// question the `fullscreen` rule asks, from the client's side — video players
/// do exactly this on relaunch.
#[test]
fn a_client_queued_fullscreen_loses_to_a_deferred_first_commit_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        before_first_commit: |f, cid, surface| {
            f.client(cid).window(surface).set_fullscreen(None);
            f.roundtrip(cid);
        },
        ..Default::default()
    });
}

/// The same request landing *between* the two placement passes: the forced-size
/// configure puts the surface back in `pending_center`, so it queues exactly as a
/// pre-first-buffer one does and the second pass would apply it.
#[test]
fn a_fullscreen_queued_between_placement_passes_loses_to_a_deferred_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        rules: r#"
[[window_rules]]
title = "ready"
size = [500, 400]
"#,
        before_first_commit: name_the_window,
        size_pass: Some((500, 400)),
        before_size_pass: |f, cid, surface| {
            f.client(cid).window(surface).set_fullscreen(None);
            f.roundtrip(cid);
        },
        ..Default::default()
    });
}

/// The flush re-runs the whole decision for every stashed entry, and it is
/// scheduled by any grab release anywhere — so it fires while this entry's own
/// grab is still held. A relaunched window that fullscreened itself mid-drag
/// then meets the carve-out, which must not destroy the stand-in still under the
/// user's cursor. Once the drag really ends the carve-out is the right answer.
#[test]
fn a_flush_under_the_live_grab_leaves_the_dragged_stand_in_alone() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    // No camera override: output-aligned, the fullscreen park below is a no-op
    // and the blur-generation counter returns to baseline.

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    f.state().arm_interactive_move(&sid);
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (300, 200));
    assert_eq!(f.state().debug_counters()["deferred_adoptions"], 1);

    // The relaunched window fullscreens itself while the drag is still going —
    // a membership acquired during the deferral, which the flush answers by
    // dismissing the stand-in.
    f.client(cid).window(&surface).set_fullscreen(None);
    f.roundtrip(cid);
    let placed = mapped_client(&mut f, "myapp").unwrap();
    assert!(
        f.state().is_window_fullscreen(&placed),
        "precondition: the client's own request went through"
    );

    // An unrelated window's move grab ends: that alone schedules the flush.
    let other_cid = f.add_client();
    let other_surface = map_window(&mut f, other_cid, "other", (200, 200));
    let other = window_by_app_id(&mut f, "other").unwrap();
    f.state().arm_interactive_move(&other);
    f.state().disarm_interactive_move(&other);
    f.pump(1);

    assert!(
        suspended_present(&mut f),
        "the stand-in the user is still dragging survived the flush"
    );
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the entry deferred again instead of resolving under the live grab"
    );

    // The drag ends, so the fullscreen carve-out gets to answer.
    f.state().disarm_interactive_move(&sid);
    f.pump(1);
    assert!(
        !suspended_present(&mut f),
        "with no grab left, a window that went fullscreen drops the stand-in"
    );
    assert!(
        f.state().is_window_fullscreen(&placed),
        "the window kept the fullscreen it asked for"
    );
    assert_eq!(f.state().debug_counters()["deferred_adoptions"], 0);

    client_close(&mut f, other_cid, &other_surface);
    client_close(&mut f, cid, &surface);
}

/// The other half of `element_under_interactive_grab`: a client resize, witnessed
/// by the surface's own `ResizeState` rather than by the move-grab list. Its
/// teardown runs no disarm, so the stash has to be picked up by the commit that
/// settles the resize back to `Idle` — without that hook the adopt is stranded
/// for good.
#[test]
fn an_adopt_deferred_by_a_client_resize_lands_when_the_resize_settles() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    let out = f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (400, 300));
    let win = window_by_app_id(&mut f, "myapp").unwrap();
    f.state().map_window(
        StageWindow::Client(win.clone()),
        Point::from((400, 300)),
        true,
    );

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The user is dragging the window's right edge when the token arrives.
    install_client_resize_grab(
        &mut f,
        &win,
        xdg_toplevel::ResizeEdge::Right,
        Point::from((800.0, 450.0)),
        out,
        ClusterResizeSnapshot::empty(),
    );
    present_token(&mut f, cid, &existing, token);

    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the resize half of the grab check deferred the adopt"
    );
    assert!(suspended_present(&mut f), "the stand-in was retained");

    motion(&mut f, Point::from((900.0, 450.0)));
    f.double_roundtrip(cid);
    adopt_last_configure(&mut f, cid, &existing);

    end_grab(&mut f);
    f.pump(1);
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the grab's release alone leaves it stashed — the surface is still mid-settle"
    );

    // The commit that settles the resize back to Idle is what lets it go.
    f.double_roundtrip(cid);
    adopt_last_configure(&mut f, cid, &existing);
    f.pump(1);

    let adopted = mapped_client(&mut f, "myapp").expect("the deferred adopt landed");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's ElementId once the resize settled"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "relocated onto the stand-in rect"
    );
    assert!(!suspended_present(&mut f), "the stand-in was consumed");
    assert_eq!(f.state().debug_counters()["deferred_adoptions"], 0);

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// A dismiss while a relaunch is in flight cancels it: the token is deregistered
/// on the spot, so a late presentation is a no-op and the window maps normally.
#[test]
fn dismiss_in_flight_lets_late_token_map_normally() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (300, 300), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The user dismisses the stand-in before the app comes back.
    f.state().dismiss_suspended(sid);
    assert!(!suspended_present(&mut f));
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);
    assert_eq!(
        token_count(&mut f),
        0,
        "the token was deregistered on dismiss"
    );

    // The relaunched window presents the now-stale token and maps normally.
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    assert_eq!(
        f.state().debug_counters()["pending_adoptions"],
        0,
        "a stale token leaves no stash"
    );
    finish_window(&mut f, cid, &surface, (300, 200));
    assert!(
        window_by_app_id(&mut f, "myapp").is_some(),
        "the window mapped normally"
    );

    client_close(&mut f, cid, &surface);
}

/// A second relaunch while one is pending is a no-op: no second token, no second
/// spawn.
#[test]
fn relaunch_while_pending_is_noop() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (300, 300), (400, 300));
    // Clear any spawns from sibling scenarios sharing this thread.
    f.state().take_relaunch_spawns_for_test();

    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    f.state().relaunch_suspended(sid);
    assert_eq!(
        f.state().pending_relaunch_token_for_test(sid),
        Some(token),
        "the token is unchanged (no re-mint)"
    );
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 1);
    assert_eq!(
        f.state().take_relaunch_spawns_for_test().len(),
        1,
        "the app was spawned exactly once"
    );

    f.state().dismiss_suspended(sid);
}

/// The launching label flips on relaunch and reverts when the 30s deadline GCs
/// the pending relaunch, deregistering its token.
#[test]
fn launching_label_reverts_on_deadline() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (300, 300), (400, 300));
    assert!(!f.state().is_suspended_launching(sid));

    f.state().relaunch_suspended(sid);
    assert!(f.state().is_suspended_launching(sid));
    assert_eq!(token_count(&mut f), 1);

    // The relaunch never materialized (single-instance app focused its existing
    // window); the deadline sweep reclaims it.
    f.state()
        .sweep_pending_relaunches(Instant::now() + Duration::from_secs(31));
    assert!(!f.state().is_suspended_launching(sid), "label reverted");
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);
    assert_eq!(token_count(&mut f), 0, "the token was deregistered on GC");
    assert!(suspended_present(&mut f), "the stand-in remains dormant");

    f.state().dismiss_suspended(sid);
}

/// An app that no longer resolves to a launchable entry leaves the window
/// dormant: no token, no pending, no spawn.
#[test]
fn relaunch_of_vanished_entry_stays_dormant() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    // The cache has some other app, but not "myapp".
    inject_cache(&mut f, &tmp, &["something-else"]);
    origin_view(&mut f);
    f.state().take_relaunch_spawns_for_test();

    let sid = insert_suspended(&mut f, 1, "myapp", (300, 300), (400, 300));
    f.state().relaunch_suspended(sid);

    assert!(
        !f.state().is_suspended_launching(sid),
        "no pending for a vanished entry"
    );
    assert_eq!(token_count(&mut f), 0);
    assert!(
        f.state().take_relaunch_spawns_for_test().is_empty(),
        "nothing spawned"
    );
    assert!(suspended_present(&mut f));

    f.state().dismiss_suspended(sid);
}

/// `msg relaunch <id>` calls `relaunch_suspended` for the selected stand-in:
/// the label flips to launching and the app is spawned with the minted token.
#[test]
fn ipc_relaunch_triggers_relaunch_suspended() {
    use crate::ipc::protocol::{Request, Response, WindowSelector};

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);
    f.state().take_relaunch_spawns_for_test();

    let sid = insert_suspended(&mut f, 1, "myapp", (300, 300), (400, 300));
    let element = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let ipc_id = f.state().stage.id_of(&element).unwrap().0;

    let reply = crate::ipc::dispatch(
        Request::Relaunch(Some(WindowSelector::Id(ipc_id))),
        f.state(),
    );
    assert!(matches!(reply, Ok(Response::Ok)));
    assert!(
        f.state().is_suspended_launching(sid),
        "msg relaunch started a pending relaunch"
    );
    assert_eq!(
        f.state().take_relaunch_spawns_for_test().len(),
        1,
        "the app was spawned"
    );

    f.state().dismiss_suspended(sid);
}

/// An adopted window that inherits the stand-in's focus must receive its
/// Activated hint on the wire. Activation is no longer granted at birth, and the
/// adopt path skips normal placement, so the hint is staged to ride the adopt
/// (decoration-tail) configure rather than sitting pending forever.
#[test]
fn adopt_inheriting_focus_delivers_activated() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (600, 400));
    // The stand-in holds focus — the user is waiting on this relaunch.
    f.state().focus_and_raise_suspended(sid);

    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    // First sized commit adopts the slot.
    finish_window(&mut f, cid, &surface, (300, 200));

    let adopted = window_by_app_id(&mut f, "myapp").expect("relaunched window adopted the slot");
    assert_eq!(
        f.state().focused_window().as_ref(),
        Some(&adopted),
        "the adopted window inherits keyboard focus"
    );
    let configs = f.client(cid).window(&surface).format_recent_configures();
    assert!(
        configs.contains("Activated"),
        "an adopted window inheriting focus must get an Activated configure, got:\n{configs}"
    );
}

/// `msg relaunch` on a selector that names no suspended window errors instead
/// of silently doing nothing.
#[test]
fn ipc_relaunch_errors_on_unknown_selector() {
    use crate::ipc::protocol::{Request, WindowSelector};

    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));

    let reply = crate::ipc::dispatch(
        Request::Relaunch(Some(WindowSelector::AppId("nope".into()))),
        f.state(),
    );
    assert!(reply.is_err());
}
