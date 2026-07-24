//! Window-effects animation bookkeeping. The stage/logical model always
//! updates instantly; these scenarios pin the *render-only* chase model that
//! lerps the drawn picture: open scale+fade, geometry chase toward a per-tick
//! live target, endpoint holds with an injectable deadline, fullscreen
//! visually-fullscreen gating, per-output scoping, and the crash/conversion
//! cleanup that drains the map.
//!
//! Backend is `None`, so render transients (close snapshots, crossfades) never
//! materialize — their counters stay 0. Only the open/geometry bookkeeping
//! is exercised here. Everything is driven through compositor-level entry points
//! (actions, fill/fit/fullscreen, commits, ticks) so the tests survive a refactor
//! of the private `WindowAnimations` internals. `tick_window_animations_at` takes
//! an injected `now` so endpoint deadlines are deterministic.

use std::time::{Duration, Instant};

use smithay::utils::{Logical, Point, Size};

use driftwm::config::{Action, Config, Direction};
use driftwm::desktop_entry::DesktopEntryCache;
use driftwm::stage::ElementId;

use smithay::desktop::Window;
use wayland_client::protocol::wl_surface::WlSurface as ClientSurface;

use crate::state::SuspendedId;

use super::client::ClientId;
use super::real::TempDir;
use super::{Fixture, map_window, window_by_app_id};

const TICK: Duration = Duration::from_millis(16);
const MAX_TICKS: usize = 600;
/// Comfortably past the 500ms endpoint-hold cap so an injected `now` releases it.
const PAST_HOLD: Duration = Duration::from_millis(600);

fn dist(a: Point<f64, Logical>, b: Point<f64, Logical>) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    (dx * dx + dy * dy).sqrt()
}

/// Active output at camera origin, zoom 1, with every camera animation quieted so
/// `output_has_active_animations` reflects only window animations. Moving/syncing
/// the camera populates the per-output `blur_camera_generation` map (which only
/// drains on output disconnect), so any caller ends off-baseline — opt out, the
/// same way the camera-animation suite does.
fn reset_view(f: &mut Fixture) {
    f.skip_baseline_check();
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.zoom = 1.0;
        os.camera_target = None;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
        os.overview_return = None;
        os.edge_pan_velocity = None;
        os.momentum.stop();
    });
    f.state().update_output_from_camera();
}

fn element_id(f: &mut Fixture, window: &Window) -> ElementId {
    f.state()
        .stage
        .id_of(window)
        .expect("window is stage-mapped")
}

/// Drive real-time ticks until every window animation prunes, panicking on
/// non-convergence. Only valid for entries that actually settle (position-only,
/// resolved requests, open) — an entry holding an outstanding request would spin
/// to the panic, which is the point of `PAST_HOLD` elsewhere.
fn tick_until_settled(f: &mut Fixture) {
    ticks_to_settle(f);
}

/// As [`tick_until_settled`], returning how many ticks convergence took.
fn ticks_to_settle(f: &mut Fixture) -> usize {
    for n in 0..MAX_TICKS {
        if !f.state().window_animations.is_active() {
            return n;
        }
        f.state().tick_window_animations(TICK);
    }
    panic!("window animations did not converge within {MAX_TICKS} ticks");
}

/// Put a suspended "myapp" stand-in into the pending-relaunch state (the
/// "launching…" label) and hand back its id plus a client that has already
/// presented the relaunch token — one sized commit away from adopting the slot.
fn arrange_pending_relaunch(
    f: &mut Fixture,
    tmp: &TempDir,
) -> (SuspendedId, ClientId, ClientSurface) {
    std::fs::write(
        tmp.path().join("myapp.desktop"),
        "[Desktop Entry]\nType=Application\nName=myapp\nExec=myapp\n",
    )
    .unwrap();
    f.state().desktop_entry_cache = Some(DesktopEntryCache::new(vec![tmp.path().to_path_buf()]));

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((500, 500)),
        Size::from((600, 400)),
        "myapp",
        "myapp",
    );
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    let cid = f.add_client();
    let win = f.client(cid).create_window();
    let surface = win.surface.clone();
    win.set_app_id("myapp");
    win.commit();
    f.roundtrip(cid);

    // Present the compositor-minted token before the first buffer (stash-for-adopt).
    f.client(cid).state.activation_token = Some(token);
    f.client(cid).activate(&surface);
    f.roundtrip(cid);

    (sid, cid, surface)
}

/// Relaunch a suspended "myapp" stand-in and adopt a freshly-mapped window into
/// its slot via the activation-token path. Returns the returning client.
fn adopt_relaunched(f: &mut Fixture, tmp: &TempDir) -> (ClientId, ClientSurface) {
    let (_sid, cid, surface) = arrange_pending_relaunch(f, tmp);

    // First sized commit adopts the stand-in's slot.
    let w = f.client(cid).window(&surface);
    w.set_size(300, 200);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(cid);

    (cid, surface)
}

/// The first sized commit of a fresh window starts an open scale+fade: one entry,
/// alpha begins at 0 and the drawn size begins below the live size (scaled in).
#[test]
fn open_entry_appears_on_first_sized_commit() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "solo", (400, 300));
    let window = window_by_app_id(&mut f, "solo").unwrap();
    let eid = element_id(&mut f, &window);

    assert_eq!(
        f.state().window_animations.len(),
        1,
        "mapping starts exactly one animation"
    );
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_none(),
        "a fresh map is an open entry, not a geometry chase"
    );

    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let size = window.geometry().size.to_f64();
    let v = f.state().animated_visual(eid, loc, size);
    assert_eq!(v.alpha, 0.0, "the open fade starts fully transparent");
    assert!(
        v.size.w < size.w && v.size.h < size.h,
        "the window scales in from below its live size"
    );

    // It advances to completion and prunes.
    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the open entry pruned"
    );
}

/// An adopted (relaunched) window inherits the suspend crossfade, not an open
/// animation: its first sized commit is the adopt commit, which suppresses open.
#[test]
fn open_is_suppressed_for_an_adopted_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (_cid, _surface) = adopt_relaunched(&mut f, &tmp);
    let adopted = window_by_app_id(&mut f, "myapp").expect("the window adopted the slot");
    let eid = element_id(&mut f, &adopted);

    assert_eq!(
        f.state().window_animations.len(),
        0,
        "an adopted window gets no open entry"
    );
    let loc = f.state().stage.position_of(&adopted).unwrap().to_f64();
    let size = adopted.geometry().size.to_f64();
    assert_eq!(
        f.state().animated_visual(eid, loc, size).alpha,
        1.0,
        "the adopted window renders opaque — no fade in flight"
    );
}

/// A second move action mid-flight retargets the same entry (its visual is kept
/// and only the target changes), so the drawn path never jumps: on every tick the
/// visual advances no further than the straight-line distance still remaining to
/// the current live target.
#[test]
fn a_second_action_mid_flight_keeps_the_visual_path_continuous() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f); // drain the open entry

    // First move: (400,300) → (900,300).
    f.state()
        .map_window(window.clone(), Point::from((900, 300)), false);
    f.state()
        .animate_window_move_from(&window, Point::from((400, 300)));

    let continuous_tick = |f: &mut Fixture| {
        let target = f.state().stage.position_of(&window).unwrap().to_f64();
        let before = f
            .state()
            .window_animations
            .geometry_visual_rect(eid)
            .expect("geometry entry in flight")
            .loc;
        let remaining = dist(before, target);
        f.state().tick_window_animations(TICK);
        let after = f
            .state()
            .window_animations
            .geometry_visual_rect(eid)
            .map(|r| r.loc)
            .unwrap_or(target); // pruned this tick == arrived at target
        assert!(
            dist(after, before) <= remaining + 1e-6,
            "the visual jumped {:.3} with only {:.3} left to the target — discontinuous",
            dist(after, before),
            remaining
        );
    };

    for _ in 0..4 {
        continuous_tick(&mut f);
    }

    // Interruption: retarget to a far, different point mid-flight.
    f.state()
        .map_window(window.clone(), Point::from((200, 700)), false);
    f.state()
        .animate_window_move_from(&window, Point::from((900, 300)));

    for _ in 0..MAX_TICKS {
        if !f.state().window_animations.is_active() {
            return;
        }
        continuous_tick(&mut f);
    }
    panic!("the interrupted animation never converged");
}

/// The target is re-read every tick: moving the window's stage position mid-flight
/// bends the visual toward the new target without snapping to it.
#[test]
fn mid_flight_map_window_retargets_without_snapping() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state()
        .map_window(window.clone(), Point::from((900, 300)), false);
    f.state()
        .animate_window_move_from(&window, Point::from((400, 300)));
    for _ in 0..3 {
        f.state().tick_window_animations(TICK);
    }
    let before = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap()
        .loc;
    assert!(
        before.x > 400.0 && before.x < 900.0,
        "mid-flight, the visual sits between start and target"
    );

    // Retarget far the other way; the stage moved, the animate call is not repeated.
    f.state()
        .map_window(window.clone(), Point::from((100, 300)), false);
    f.state().tick_window_animations(TICK);
    let after = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap()
        .loc;
    assert!(
        after.x < before.x,
        "the path bent toward the new (lower-x) target"
    );
    assert!(
        after.x > 101.0,
        "it lerped a fraction of the way, it did not snap to the target"
    );

    tick_until_settled(&mut f);
}

/// Geometry animations run on normalized progress, so their duration is a fixed
/// number of ticks regardless of how far the window travels — and it matches the
/// open animation's. A distance-epsilon chase instead grows a log-distance tail,
/// which is what made fit/fill/fullscreen feel slower than open/close.
#[test]
fn geometry_settles_in_the_same_time_regardless_of_distance() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);

    // The open animation's duration is the reference.
    f.state()
        .map_window(window.clone(), Point::from((100, 300)), false);
    f.state().start_window_open_animation(&window);
    let open_ticks = ticks_to_settle(&mut f);
    assert!(open_ticks > 2, "the open animation should take real time");

    // A short hop, then a hop ~60x longer. Both stay inside the viewport so the
    // travelling visual never loses eligibility (which would end it instantly).
    let mut move_ticks = Vec::new();
    for (from, to) in [((100, 300), (120, 300)), ((100, 300), (1300, 300))] {
        f.state()
            .map_window(window.clone(), Point::from(from), false);
        tick_until_settled(&mut f);
        f.state().map_window(window.clone(), Point::from(to), false);
        f.state()
            .animate_window_move_from(&window, Point::from(from));
        assert!(
            f.state().window_animations.is_active(),
            "the move started a chase"
        );
        move_ticks.push(ticks_to_settle(&mut f));
    }

    assert_eq!(
        move_ticks[0], move_ticks[1],
        "a 20px and a 1200px move must take the same number of ticks, got {move_ticks:?}"
    );
    assert!(
        move_ticks[0].abs_diff(open_ticks) <= 2,
        "geometry ({}) and open ({open_ticks}) should settle in the same time",
        move_ticks[0],
    );
}

/// A size request equal to the committed size is resolved at the start (never
/// rides to the endpoint hold) — the entry prunes on convergence like a
/// position-only move, so `tick_until_settled` returns instead of spinning.
#[test]
fn size_request_equal_to_committed_never_holds() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let committed = window.geometry().size;
    // Request the size the window already has, then move it — the size request
    // must be discarded, leaving a pure position chase that settles.
    f.state().animate_window_geometry(&window, committed);
    f.state()
        .map_window(window.clone(), Point::from((700, 300)), false);
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_some(),
        "the move started a geometry chase"
    );

    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "an equal-size request settled instead of holding at the endpoint"
    );
}

/// A never-committing client holds the stretched endpoint (the entry stays active
/// past convergence), and the hold is released by the injected-now deadline.
#[test]
fn outstanding_request_holds_at_endpoint_until_the_deadline() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    // A size request the client will never commit. The one-phase chase stretches
    // toward the REQUESTED size immediately and holds the stretched endpoint
    // (target loc, requested size) because the request is still outstanding.
    let committed = window.geometry().size;
    let bigger = Size::from((committed.w + 300, committed.h + 300));
    f.state().animate_window_geometry(&window, bigger);
    f.state()
        .map_window(window.clone(), Point::from((700, 300)), false);

    let base = Instant::now();
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(
        f.state().window_animations.is_active(),
        "the entry holds at the endpoint while the request is outstanding"
    );
    assert!(
        f.state().has_active_animations(),
        "an endpoint hold counts as an active animation"
    );
    let held = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    assert!(
        dist(held.loc, Point::from((700.0, 300.0))) <= 0.5,
        "the hold pins the visual at the target endpoint"
    );
    assert!(
        (held.size.w - bigger.w as f64).abs() <= 0.5
            && (held.size.h - bigger.h as f64).abs() <= 0.5,
        "the hold pins the stretched REQUESTED size, not the live committed size ({held:?})"
    );

    // Past the 500ms cap the hold releases: the request clears and the chase
    // bends from the stretched (requested) endpoint back to the live size,
    // converging and pruning over the following ticks (all at the injected now,
    // so the deadline stays fired).
    let past = base + PAST_HOLD;
    for _ in 0..MAX_TICKS {
        if !f.state().window_animations.is_active() {
            break;
        }
        f.state().tick_window_animations_at(TICK, past);
    }
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the injected-now deadline released the hold and pruned the entry"
    );
}

/// A commit that reaches the requested size resolves the outstanding request:
/// the entry holds at the endpoint until the client commits that size, then the
/// chase bends to live and prunes.
#[test]
fn a_commit_resolves_the_outstanding_request() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    tick_until_settled(&mut f); // drain open

    // An outstanding size request the client has not yet committed.
    let committed = window.geometry().size;
    let requested = Size::from((committed.w + 100, committed.h + 100));
    f.state().animate_window_geometry(&window, requested);
    let base = Instant::now();
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(
        f.state().window_animations.is_active(),
        "the entry holds at the endpoint until the client commits the new size"
    );

    // The client commits a buffer at the requested size — a clean ack resolves it.
    let w = f.client(id).window(&surface);
    w.set_size(requested.w as u16, requested.h as u16);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(id);
    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the commit resolved the request and the chase pruned"
    );
}

/// A commit to a size the compositor never requested (the client chose its own —
/// reality wins) resolves the request just like a clean ack: the hold ends and
/// the chase bends to live.
#[test]
fn a_client_chosen_size_also_resolves_the_request() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    tick_until_settled(&mut f);

    let committed = window.geometry().size;
    let requested = Size::from((committed.w + 200, committed.h + 200));
    f.state().animate_window_geometry(&window, requested);
    let base = Instant::now();
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(
        f.state().window_animations.is_active(),
        "the request is outstanding, so the entry holds"
    );

    // The client commits a third size — neither the request nor the prior size.
    let chosen: Size<i32, Logical> = Size::from((committed.w + 50, committed.h + 50));
    let w = f.client(id).window(&surface);
    w.set_size(chosen.w as u16, chosen.h as u16);
    w.attach_new_buffer();
    w.commit();
    f.double_roundtrip(id);
    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the client-chosen size resolved the request and the chase pruned"
    );
}

/// A pinned window's entry chases in screen space, so a camera pan (which rewrites
/// a pinned window's canvas stage loc every tick) does not churn it. Driven
/// through the real fullscreen-exit re-pin path, which seats a screen-space entry.
#[test]
fn pinned_entry_chases_in_screen_space_and_survives_a_pan() {
    let mut f = Fixture::with_config(
        Config::from_toml("[[window_rules]]\napp_id = \"p\"\npinned_to_screen = true\n").unwrap(),
    );
    let output = f.add_output(1, (1920, 1080));
    // Panning the camera below populates blur_camera_generation (drains only on
    // output disconnect) — end off-baseline like the camera-animation suite.
    f.skip_baseline_check();
    let id = f.add_client();
    let surface = map_window(&mut f, id, "p", (400, 300));
    let window = window_by_app_id(&mut f, "p").unwrap();
    let eid = element_id(&mut f, &window);
    assert!(f.state().is_pinned(&window), "the window pinned via rule");

    // Enter fullscreen (unpins) and let the client ack the viewport size, so the
    // saved (pre-fullscreen) size differs from the committed one — the exit entry
    // then carries a real outstanding request and holds rather than pruning.
    f.state().enter_fullscreen(&window, Some(output.clone()));
    assert!(
        !f.state().is_pinned(&window),
        "fullscreen unpins the window"
    );
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);

    // Exit re-pins and seats a screen-space geometry entry.
    f.state().exit_fullscreen_on(&output);
    assert!(
        f.state().is_pinned(&window),
        "exit re-pins the window to its site"
    );
    assert!(
        f.state().window_animations.is_active(),
        "the exit seated a geometry entry"
    );

    // Converge under a fixed now (so the hold never times out), then pan far.
    let base = Instant::now();
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, base);
    }
    let before = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();

    f.state().set_camera(Point::from((6000.0, 6000.0)));
    f.state().update_output_from_camera();
    f.state().tick_window_animations_at(TICK, base);
    let after = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();

    assert!(
        dist(after.loc, before.loc) <= 0.5,
        "a camera pan did not churn the screen-space pinned entry ({before:?} → {after:?})"
    );
}

/// A position-only nudge starts a geometry chase that prunes once it converges.
#[test]
fn position_only_nudge_prunes_on_convergence() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    f.state()
        .execute_action(&Action::NudgeWindow(Direction::Right));
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_some(),
        "a nudge starts a position-only geometry chase"
    );

    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the position-only chase pruned on convergence"
    );
}

/// A window under an active interactive grab gets no animation entry: the grab
/// guard suppresses the start (the same guard shared by open and every geometry
/// site).
#[test]
fn no_entry_starts_under_an_interactive_grab() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    tick_until_settled(&mut f);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    f.state().interactive_move.push(window.clone());
    f.state()
        .execute_action(&Action::NudgeWindow(Direction::Right));
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "no geometry entry started while the window was grabbed"
    );
    f.state().interactive_move.clear();
}

/// An output is visually fullscreen only once the fullscreen-entry animation
/// finishes: false mid-entry, true after the client acks and the chase prunes.
#[test]
fn output_is_visually_fullscreen_only_after_the_entry_finishes() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);

    // Client-requested fullscreen: the enter replaces the open entry with a
    // fullscreen-entry geometry chase.
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(
        f.state().is_output_fullscreen(&output),
        "the output is logically fullscreen"
    );
    assert!(
        f.state().window_animations.fullscreen_entry_active(eid),
        "a fullscreen-entry animation is in flight"
    );
    assert!(
        !f.state().is_output_visually_fullscreen(&output),
        "the output is not YET visually fullscreen mid-entry"
    );

    // Ack the fullscreen size and run the chase to completion.
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "the output is visually fullscreen once the entry converges"
    );

    f.state().exit_fullscreen_on(&output);
}

/// Reversing out of a fullscreen entry mid-flight seeds the exit from the entry's
/// current visual, frame-converted back to the restored camera space — so the
/// on-screen picture is continuous (no jump) and the fullscreen-entry role clears.
/// Driven at a pre-fullscreen zoom ≠ 1, where a keep-the-locked-space-visual bug
/// (invisible at zoom 1's identity conversion) would jump the window on exit.
#[test]
fn exit_from_mid_entry_continues_from_the_current_visual() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    f.state().with_output_state(|os| os.zoom = 2.0);
    f.state().update_output_from_camera();
    f.state()
        .map_window(window.clone(), Point::from((100, 100)), false);
    let eid = element_id(&mut f, &window);

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    let locked_camera = f.state().with_output_state(|os| os.camera).unwrap();
    // Advance the entry partway (mid-entry).
    f.state().tick_window_animations(TICK);
    f.state().tick_window_animations(TICK);
    let mid = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    assert!(f.state().window_animations.fullscreen_entry_active(eid));
    // The mid visual's on-screen position in the locked (zoom-1) viewport.
    let mid_screen = mid.loc - locked_camera;

    f.state().exit_fullscreen_on(&output);
    assert!(
        !f.state().is_output_fullscreen(&output),
        "fullscreen is logically gone after exit"
    );
    assert!(
        !f.state().window_animations.fullscreen_entry_active(eid),
        "the fullscreen-entry role cleared on exit"
    );
    let (restored_camera, restored_zoom) = f
        .state()
        .with_output_state(|os| (os.camera, os.zoom))
        .unwrap();
    let after = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the exit continues a geometry entry");
    // Same on-screen position under the restored camera/zoom — continuous.
    let after_screen = Point::from((
        (after.loc.x - restored_camera.x) * restored_zoom,
        (after.loc.y - restored_camera.y) * restored_zoom,
    ));
    assert!(
        dist(after_screen, mid_screen) <= 1.5,
        "the exit stayed screen-continuous across the zoom change ({mid_screen:?} → {after_screen:?})"
    );
}

/// Converting a live window to a suspended stand-in (suspend action + real close)
/// drops the window's animation entry — the stand-in inherits the id but no chase.
#[test]
fn conversion_drops_the_window_animation_entry() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "myapp", (400, 300));
    let window = window_by_app_id(&mut f, "myapp").unwrap();
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "the mapped window has an open entry pre-conversion"
    );

    // Suspend then close: the destroy converts the window into a stand-in.
    f.state().execute_action(&Action::SuspendWindow);
    f.client(id).window(&surface).destroy();
    f.roundtrip(id);
    f.dispatch();

    assert_eq!(
        f.state().window_animations.len(),
        0,
        "conversion dropped the entry for the converted id"
    );
    let counters = f.state().debug_counters();
    assert_eq!(counters["closing_snapshots"], 0);
    assert_eq!(counters["adoption_fades"], 0);
    assert_eq!(counters["close_pixels"], 0);

    // Tear the stand-in down for the baseline.
    let sid = f
        .state()
        .stage
        .windows()
        .find_map(|w| w.suspended().map(|s| s.id));
    if let Some(sid) = sid {
        f.state().dismiss_suspended(sid);
    }
}

/// Adoption leaves no window-animation entry (no open on the adopted window, and
/// both involved ids' entries dropped), and no render transient materializes
/// headless — the crossfade counter stays 0.
#[test]
fn adoption_drops_entries_and_creates_no_render_transient() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (_cid, _surface) = adopt_relaunched(&mut f, &tmp);
    assert!(
        window_by_app_id(&mut f, "myapp").is_some(),
        "the window adopted the slot"
    );

    assert_eq!(
        f.state().window_animations.len(),
        0,
        "adoption left no window-animation entry"
    );
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["adoption_fades"], 0,
        "the adoption crossfade is backend-gated — none headless"
    );
    assert_eq!(counters["closing_snapshots"], 0);
    assert_eq!(counters["close_pixels"], 0);
}

/// The stand-in's "launching…" label state is live while the relaunch is pending
/// and gone the moment the window adopts the slot. The adoption crossfade renders
/// the departed stand-in's chrome, so it must capture this state when the fade is
/// created — a live lookup at render time reads the post-adopt value, re-keys the
/// cached label buffer, and the fade visibly swaps to the plain name before
/// fading. This pins the ordering the capture depends on; the frozen pixels
/// themselves are render-only (the fade is backend-gated, none exists headless).
#[test]
fn adoption_clears_the_launching_state_the_fade_captures() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (sid, cid, surface) = arrange_pending_relaunch(&mut f, &tmp);
    assert!(
        f.state().is_suspended_launching(sid),
        "the stand-in shows the launching label while the relaunch is pending"
    );

    // The adopting sized commit ends the relaunch.
    let w = f.client(cid).window(&surface);
    w.set_size(300, 200);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(cid);

    assert!(
        window_by_app_id(&mut f, "myapp").is_some(),
        "the window adopted the slot"
    );
    assert!(
        !f.state().is_suspended_launching(sid),
        "adoption ended the relaunch, so a render-time lookup would read plain — \
         the fade has to have captured the launching state at creation"
    );
}

/// A client that dies without a clean unmap (crash) leaves an animation entry
/// keyed by a now-dead id; the sweep beside `retain_alive` in
/// `refresh_and_flush_clients` drains it on the next pump, and the fixture
/// baseline holds.
#[test]
fn crash_path_dead_id_sweep_drains_the_map() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "crash", (400, 300));
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "the mapped window has an open entry"
    );

    // Abrupt death: no close request, no unmap_window.
    f.kill_client(id);
    f.pump(5);

    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the dead-id sweep drained the animation for the crashed window"
    );
}

/// A window animation activates only the outputs its visual rect intersects: an
/// animation on output A leaves output B (a far-tiled second output) inactive.
#[test]
fn per_output_predicate_scopes_to_the_intersecting_output() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    // Camera writes below populate blur_camera_generation — end off-baseline.
    f.skip_baseline_check();
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();

    // Seat the window squarely on output 1, park output 2's camera on a far canvas
    // region so the window's rect can't reach its viewport, and quiet both cameras.
    {
        let mut os = crate::state::output_state(&out1);
        os.camera = Point::from((0.0, 0.0));
        os.zoom = 1.0;
        os.camera_target = None;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
        os.overview_return = None;
        os.edge_pan_velocity = None;
        os.momentum.stop();
    }
    {
        let mut os = crate::state::output_state(&out2);
        os.camera = Point::from((10_000.0, 0.0));
        os.zoom = 1.0;
        os.camera_target = None;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
        os.overview_return = None;
        os.edge_pan_velocity = None;
        os.momentum.stop();
    }
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    f.state().update_output_from_camera();

    // The open entry from the map is live and lies on output 1 only.
    assert!(
        f.state().window_animations.is_active(),
        "an open entry is in flight"
    );
    assert!(
        f.state().output_has_active_animations(&out1),
        "output 1 shows the animation"
    );
    assert!(
        !f.state().output_has_active_animations(&out2),
        "output 2 (far tiled region) does not"
    );
    assert!(f.state().has_active_animations());

    tick_until_settled(&mut f);
}

/// An animation whose visual rect intersects no drawable output completes
/// instantly: starting one off every viewport creates no entry, and an in-flight
/// entry whose rect leaves every viewport is swept on the next tick.
#[test]
fn off_screen_animations_complete_instantly() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    tick_until_settled(&mut f);

    // Pan far so the window's canvas rect intersects no viewport, then try to
    // start an open animation — it must not start (instant-complete at start).
    f.state().set_camera(Point::from((100_000.0, 100_000.0)));
    f.state().update_output_from_camera();
    f.state().start_window_open_animation(&window);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "an off-screen animation never starts (completes instantly)"
    );

    // An in-flight entry that loses eligibility mid-flight is swept on the tick.
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);
    f.state()
        .execute_action(&Action::NudgeWindow(Direction::Right));
    assert!(
        f.state().window_animations.is_active(),
        "the nudge started an entry while on-screen"
    );

    f.state().set_camera(Point::from((100_000.0, 100_000.0)));
    f.state().update_output_from_camera();
    f.state().tick_window_animations(TICK);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the entry was completed instantly when it left every viewport"
    );
}

/// Foot-family terminals unmap (null-buffer commit) before destroying their
/// toplevel. That commit collapses the window's live geometry, so a close
/// animation that sized itself from `window.geometry()` at teardown time got a
/// zero-sized rect and silently dropped the fade. The pixels are captured in the
/// pre-commit hook — while the rect is still readable — so the rect has to be
/// recorded there too. This pins the hazard: live geometry is already gone by the
/// time the destroy handler runs. (The snapshot itself is backend-gated, so the
/// spawn can't be asserted headlessly.)
#[test]
fn an_unmapped_window_no_longer_reports_its_geometry() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "foot", (400, 300));
    let window = window_by_app_id(&mut f, "foot").unwrap();
    assert_eq!(
        window.geometry().size,
        Size::from((400, 300)),
        "a mapped window reports its size"
    );

    // The unmap commit, exactly as foot sequences it before destroying.
    f.client(id).window(&surface).attach_null();
    f.client(id).window(&surface).commit();
    f.roundtrip(id);
    f.dispatch();

    let live = window.geometry().size;
    assert!(
        live.w <= 0 || live.h <= 0,
        "an unmapped window reports no usable geometry (got {live:?}) — a close \
         animation must use the rect captured at unmap, not this"
    );
}

/// Unfill animates as one leg from the filled rect to the restored rect, the way
/// leaving fullscreen does. The restore position has to be applied up front: if
/// only the size half is animated while the stage still holds the fill position,
/// the window shrinks anchored at the fill rect's top-left and only jumps to its
/// real position later, when the settle fires on the client's resized commit.
#[test]
fn unfill_animates_straight_to_the_restored_rect() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);
    let restored_pos = f.state().stage.position_of(&window).unwrap();

    // Fill, let the client catch up, and drain the fill animation.
    f.state().fill_window(&window);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);
    tick_until_settled(&mut f);
    let fill_pos = f.state().stage.position_of(&window).unwrap();
    let fill_size = window.geometry().size;
    assert_ne!(fill_pos, restored_pos, "the fill moved the window");

    f.state().unfill_window(&window);

    assert_eq!(
        f.state().stage.position_of(&window),
        Some(restored_pos),
        "unfill applies the restored position immediately, so the chase has one \
         target — not the fill position with a deferred jump"
    );
    let from = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("unfill started a geometry chase");
    assert!(
        dist(from.loc, fill_pos.to_f64()) <= 0.5,
        "the chase starts at the filled rect ({from:?} vs {fill_pos:?})"
    );

    // Every intermediate visual sits strictly between the two rects; none lands on
    // the target corner while still filled-size (the reported top-left jump).
    for _ in 0..4 {
        f.state().tick_window_animations(TICK);
        let v = f
            .state()
            .window_animations
            .geometry_visual_rect(eid)
            .expect("still in flight");
        let at_target_corner = dist(v.loc, restored_pos.to_f64()) <= 0.5;
        let still_filled = (v.size.w - fill_size.w as f64).abs() <= 0.5;
        assert!(
            !(at_target_corner && still_filled),
            "visual reached the target corner while still filled-size: {v:?}"
        );
    }
}
