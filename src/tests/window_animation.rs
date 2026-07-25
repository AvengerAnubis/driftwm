//! Window-effects animation bookkeeping. The stage/logical model always
//! updates instantly; these scenarios pin the *render-only* chase model that
//! lerps the drawn picture: open scale+fade, geometry chase toward a per-tick
//! live target, endpoint holds with an injectable deadline, fullscreen
//! visually-fullscreen gating, per-output scoping, and the crash/conversion
//! cleanup that drains the map.
//!
//! Backend is `None`, so anything that needs a renderer to exist (close
//! snapshots, crossfade overlays) never materializes — their counters stay 0,
//! and an assertion on them pins nothing. The capture half of a resize
//! crossfade is a plain map, though, so the lifecycle scenarios seed one
//! ([`seed_resize_capture`]) and the drop sites have to earn their zero; the
//! overlay half needs a texture and stays out of headless reach.
//! Everything else is driven through compositor-level entry points
//! (actions, fill/fit/fullscreen, commits, ticks) so the tests survive a refactor
//! of the private `WindowAnimations` internals. `tick_window_animations_at` takes
//! an injected `now` so endpoint deadlines are deterministic.

use std::time::{Duration, Instant};

use smithay::utils::{Logical, Point, Rectangle, Size};

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

/// Opacity of the compositor chrome around `window`, exactly as the render loop
/// resolves it.
fn chrome_alpha(f: &mut Fixture, window: &Window) -> f32 {
    let id = f.state().stage.id_of(window);
    f.state().chrome_alpha_of(id, window)
}

fn element_id(f: &mut Fixture, window: &Window) -> ElementId {
    f.state()
        .stage
        .id_of(window)
        .expect("window is stage-mapped")
}

/// Stage id of the stand-in for `sid` — the id an adoption hands on to the
/// window that takes over its slot.
fn standin_element_id(f: &mut Fixture, sid: SuspendedId) -> ElementId {
    let element = f
        .state()
        .stage
        .windows()
        .find(|w| w.suspended().is_some_and(|s| s.id == sid))
        .cloned()
        .expect("the stand-in is on the stage");
    f.state().stage.id_of(&element).expect("and carries an id")
}

/// Put content in the stash a resize crossfade consumes, standing in for the
/// capture a headless fixture has no renderer to make. Every `resize_captures`
/// drop assertion needs this: with the map empty from the start it cannot tell a
/// working drop site from one that never ran. Stamped with the id's current
/// capture generation, or 0 when the id has no geometry entry — only the resolve
/// pairs on the stamp, the drop sites never look at it.
fn seed_resize_capture(f: &mut Fixture, id: ElementId) {
    let generation = f
        .state()
        .window_animations
        .generation_of(id)
        .unwrap_or_default();
    f.state().resize_captures.stash(
        id,
        crate::render::ClosePixels::empty(Rectangle::from_size(Size::from((400, 300)))),
        crate::render::BakeChrome {
            bare: true,
            corner_radius: [0.0; 4],
        },
        generation,
    );
    assert_eq!(
        f.state().debug_counters()["resize_captures"],
        1,
        "the seeded capture is in the map, so the drop below has something to do"
    );
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

    // The only entry is the geometry hold that keeps it filling the slot — an
    // open entry would report no geometry rect and would fade the window in.
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_some(),
        "an adopted window gets a geometry hold, not an open scale+fade"
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

/// A client that never redraws is bounded twice over: the start hold freezes the
/// window for its budget, the leg then runs with stale (capped) content, and the
/// endpoint hold bounds the wait at the far end before the entry finally prunes.
/// Neither deadline can strand an entry.
#[test]
fn an_unacked_request_is_bounded_by_both_deadlines() {
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
    let bigger = Size::from((committed.w + 300, committed.h + 300));
    let seed = f.state().stage.position_of(&window).unwrap().to_f64();
    f.state().animate_window_geometry(&window, bigger);
    f.state()
        .map_window(window.clone(), Point::from((700, 300)), false);

    // Frozen: the request is outstanding and nothing has moved.
    let base = Instant::now();
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(f.state().window_animations.start_held(eid), "still frozen");
    assert!(
        f.state().has_active_animations(),
        "a start hold counts as an active animation"
    );
    let held = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    assert!(
        dist(held.loc, seed) <= 0.5,
        "nothing moved while frozen ({held:?})"
    );

    // Past the start budget the leg runs and parks at the endpoint.
    let after_start = base + PAST_HOLD;
    for _ in 0..MAX_TICKS {
        f.state().tick_window_animations_at(TICK, after_start);
        if !f.state().window_animations.start_held(eid) {
            break;
        }
    }
    assert!(
        !f.state().window_animations.start_held(eid),
        "the start budget expired"
    );
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, after_start);
    }
    let parked = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the endpoint hold keeps it alive");
    assert!(
        (parked.size.w - bigger.w as f64).abs() <= 0.5,
        "the leg ran to the requested endpoint ({parked:?})"
    );

    // And past the endpoint budget too, it finally prunes.
    let after_endpoint = after_start + PAST_HOLD;
    for _ in 0..MAX_TICKS {
        if !f.state().window_animations.is_active() {
            break;
        }
        f.state().tick_window_animations_at(TICK, after_endpoint);
    }
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "both deadlines fired, so the entry pruned"
    );
}

/// A commit at the requested size is what the freeze is waiting for: it releases
/// the hold, the leg runs with the client's real new content, and the entry
/// prunes.
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
    let eid = element_id(&mut f, &window);
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
        "the entry is frozen, waiting for the client to redraw"
    );
    // The old picture the freeze has been holding on screen.
    seed_resize_capture(&mut f, eid);

    // The client commits a buffer at the requested size — a clean ack resolves it.
    let w = f.client(id).window(&surface);
    w.set_size(requested.w as u16, requested.h as u16);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(id);
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["resize_captures"], 0,
        "the resolve consumed the stashed old picture — the one moment old and \
         new content both exist"
    );
    assert_eq!(
        counters["resize_crossfades"], 0,
        "the overlay it would have become needs a renderer, so only the consume \
         side is pinned headless"
    );
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
    let eid = element_id(&mut f, &window);
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
        "the request is outstanding, so the entry is still frozen"
    );
    seed_resize_capture(&mut f, eid);

    // The client commits a third size — neither the request nor the prior size.
    let chosen: Size<i32, Logical> = Size::from((committed.w + 50, committed.h + 50));
    let w = f.client(id).window(&surface);
    w.set_size(chosen.w as u16, chosen.h as u16);
    w.attach_new_buffer();
    w.commit();
    f.double_roundtrip(id);
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["resize_captures"], 0,
        "a client-chosen size resolves the freeze too, consuming the old picture"
    );
    assert_eq!(counters["resize_crossfades"], 0, "overlay is backend-gated");
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
    // The enter freezes until the client redraws at the fullscreen size. Ack it,
    // then advance the leg partway, so the exit interrupts a rect in motion
    // rather than one still sitting on its seed.
    let seed = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    super::adopt_last_configure(&mut f, id, &surface);
    f.state().tick_window_animations(TICK);
    f.state().tick_window_animations(TICK);
    let mid = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    assert!(
        mid.size.w > seed.size.w + 1.0 && mid.size.w < 1919.0,
        "the entry is genuinely mid-flight ({seed:?} -> {mid:?})"
    );
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
    let eid = element_id(&mut f, &window);
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "the mapped window has an open entry pre-conversion"
    );
    seed_resize_capture(&mut f, eid);

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
    assert_eq!(counters["standin_fades"], 0);
    assert_eq!(counters["close_pixels"], 0);
    assert_eq!(
        counters["resize_captures"], 0,
        "the id died with the client here, so the teardown sweep collects its \
         seeded capture (the in-place conversion is pinned below)"
    );
    assert_eq!(counters["resize_crossfades"], 0, "overlay is backend-gated");

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

/// Adoption drops both stale window-animation entries and replaces them with a
/// single hold on the adopted window's slot, and content stashed against the
/// stand-in's id goes with them — the adopted window inherits that id, so no
/// sweep would ever collect it.
#[test]
fn adoption_drops_entries_and_creates_no_render_transient() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (sid, cid, surface) = arrange_pending_relaunch(&mut f, &tmp);
    let standin_id = standin_element_id(&mut f, sid);
    seed_resize_capture(&mut f, standin_id);

    // The first sized commit adopts the stand-in's slot.
    let w = f.client(cid).window(&surface);
    w.set_size(300, 200);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(cid);
    assert!(
        window_by_app_id(&mut f, "myapp").is_some(),
        "the window adopted the slot"
    );

    // Exactly one entry: the adopted window's slot hold. Neither involved id
    // carried a stale chase across the replace.
    let adopted = window_by_app_id(&mut f, "myapp").unwrap();
    let eid = element_id(&mut f, &adopted);
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "adoption leaves only the adopted window's hold"
    );
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_some(),
        "and that entry belongs to the adopted window"
    );
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["standin_fades"], 0,
        "the adoption crossfade is backend-gated — none headless"
    );
    assert_eq!(counters["closing_snapshots"], 0);
    assert_eq!(counters["close_pixels"], 0);
    // Adoption replaces the stand-in in place, keeping its id: same hazard as
    // conversion, so both involved ids' crossfade halves go at the replace.
    assert_eq!(
        counters["resize_captures"], 0,
        "the seeded capture went at the replace, not on a sweep that cannot fire"
    );
    assert_eq!(counters["resize_crossfades"], 0, "overlay is backend-gated");
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
/// toplevel, which collapses the window's live geometry — a close animation
/// sized from `window.geometry()` at teardown got a zero-sized rect and
/// silently dropped the fade. This pins that hazard directly, since the render
/// path itself is backend-gated and can't be asserted headlessly.
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

/// Unfill animates as one leg from the filled rect to the restored rect. The
/// restore position has to be applied up front: if the stage still holds the
/// fill position while only the size animates, the window shrinks anchored at
/// the fill rect's top-left and jumps to its real position only when the
/// settle fires on the client's resized commit.
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

/// An adopted window must occupy the stand-in's slot from the first frame,
/// since the client is still committing buffers at its own mapped size until
/// it acks the resize — without a geometry hold it draws undersized beneath
/// the fading stand-in chrome, reading as a flicker rather than a crossfade.
#[test]
fn adoption_holds_the_adopted_rect_until_the_client_catches_up() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (_sid, cid, surface) = arrange_pending_relaunch(&mut f, &tmp);
    // The stand-in is 600x400; the returning client maps at 300x200.
    let w = f.client(cid).window(&surface);
    w.set_size(300, 200);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(cid);

    let adopted = window_by_app_id(&mut f, "myapp").expect("the window adopted the slot");
    let eid = element_id(&mut f, &adopted);
    let pos = f.state().stage.position_of(&adopted).unwrap();

    let visual = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("adoption seeded a geometry entry holding the slot");
    assert!(
        dist(visual.loc, pos.to_f64()) <= 0.5,
        "the held rect sits at the adopted position ({visual:?} vs {pos:?})"
    );
    assert!(
        (visual.size.w - 600.0).abs() <= 0.5 && (visual.size.h - 400.0).abs() <= 0.5,
        "the window is drawn at the stand-in's size, not its own 300x200 ({visual:?})"
    );

    // The hold survives ticks while the request is outstanding, so the mismatch
    // is never visible.
    let base = Instant::now();
    for _ in 0..30 {
        f.state().tick_window_animations_at(TICK, base);
    }
    let held = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("still holding while the client has not acked");
    assert!(
        (held.size.w - 600.0).abs() <= 0.5,
        "still filling the slot ({held:?})"
    );

    // And the content is actually stretched to fill it. A slot hold is the one
    // case where a stale buffer must be magnified, or the adopted window
    // renders undersized at the slot's corner.
    let committed = adopted.geometry().size.to_f64();
    let v = f.state().animated_visual(eid, pos.to_f64(), committed);
    let (sx, sy) = crate::state::window_animation::content_scale(v.size, committed, v.cap_content);
    assert!(
        (sx - 600.0 / committed.w).abs() < 1e-6 && (sy - 400.0 / committed.h).abs() < 1e-6,
        "the held slot stretches the stale buffer to fill it (got {sx:.2}x, {sy:.2}x)"
    );
}

/// A compositor resize no longer stretches a stale buffer at all: the window is
/// frozen at its pre-action appearance until the client redraws. Only when the
/// client misses the budget does the leg run with stale content, and then the old
/// cap applies — the interface never balloons either way.
#[test]
fn a_growing_resize_freezes_rather_than_magnifying() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let seed_size = window.geometry().size.to_f64();
    f.state().fit_window(&window);
    let base = Instant::now();
    for _ in 0..30 {
        f.state().tick_window_animations_at(TICK, base);
    }

    // Frozen at the seed: same size as before the action, and drawn 1:1.
    let committed = window.geometry().size.to_f64();
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let v = f.state().animated_visual(eid, loc, committed);
    assert!(
        f.state().window_animations.start_held(eid),
        "the fit waits for the client's redraw"
    );
    assert!(
        (v.size.w - seed_size.w).abs() <= 0.5,
        "the rect has not grown yet ({:?})",
        v.size
    );
    assert!(
        !v.cap_content,
        "and nothing is being capped, because nothing is stretched"
    );

    // Degrade: past the budget the leg runs, and now the cap protects the stale
    // buffer from being blown up to meet the growing rect.
    let past = base + PAST_HOLD;
    for _ in 0..12 {
        f.state().tick_window_animations_at(TICK, past);
        let v = f.state().animated_visual(eid, loc, committed);
        let (sx, sy) =
            crate::state::window_animation::content_scale(v.size, committed, v.cap_content);
        assert!(
            sx <= 1.0 && sy <= 1.0,
            "the degraded leg stays capped (got {sx:.2}x)"
        );
    }
    assert!(
        f.state().animated_visual(eid, loc, committed).cap_content,
        "the degraded leg is the capped, stale-content case"
    );
}

/// The far end of a fit the client never acks: the endpoint hold's deadline
/// fires, drops the request, and the rect walks back down to the size the client
/// still has. Staleness belongs to the buffer, not to the request that release
/// just dropped, so that last leg stays capped — nothing magnifies on the way
/// back. Reaching it costs both budgets, hence the two clock steps.
#[test]
fn the_hold_deadline_release_stays_capped() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);

    // Past the start budget the leg degrades and runs to the requested endpoint,
    // where the endpoint hold anchors its own deadline.
    let after_start = base + PAST_HOLD;
    for _ in 0..MAX_TICKS {
        f.state().tick_window_animations_at(TICK, after_start);
        if !f.state().window_animations.start_held(eid) {
            break;
        }
    }
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, after_start);
    }
    let endpoint = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the endpoint hold keeps it alive");

    // Past the endpoint budget too: the request is dropped and the rect shrinks
    // back toward the live size.
    let after_endpoint = after_start + PAST_HOLD;
    let committed = window.geometry().size.to_f64();
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let mut released = false;
    for _ in 0..MAX_TICKS {
        if !f.state().window_animations.is_active() {
            break;
        }
        f.state().tick_window_animations_at(TICK, after_endpoint);
        let v = f.state().animated_visual(eid, loc, committed);
        let (sx, sy) =
            crate::state::window_animation::content_scale(v.size, committed, v.cap_content);
        assert!(
            sx <= 1.0 && sy <= 1.0,
            "the release leg must stay capped — no commit ever landed (got {sx:.2}x)"
        );
        released |= v.size.w < endpoint.size.w - 1.0;
    }
    assert!(
        released,
        "the deadline dropped the request and the rect actually walked back down \
         from {endpoint:?}"
    );
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "and the release leg settled at the live size"
    );
}

/// A position-only retarget (a nudge, a cluster shift) landing on a frozen resize
/// keeps the freeze — it is the same wait, just aimed somewhere else. It must not
/// cancel the wait and start animating with content the client has not
/// delivered.
#[test]
fn a_position_only_retarget_keeps_a_frozen_resize_frozen() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    let base = Instant::now();
    for _ in 0..4 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(
        f.state().window_animations.start_held(eid),
        "frozen by the fit"
    );
    let generation = f.state().window_animations.generation_of(eid);

    // Nudge it while frozen.
    let from = f.state().stage.position_of(&window).unwrap();
    f.state()
        .map_window(window.clone(), Point::from((from.x + 40, from.y)), false);
    f.state().animate_window_move_from(&window, from);

    assert!(
        f.state().window_animations.start_held(eid),
        "a move does not cancel the wait for the client's redraw"
    );
    assert_eq!(
        f.state().window_animations.generation_of(eid),
        generation,
        "and does not invalidate the resize — no new request was made"
    );

    // It is still the wait it was, so it still ends when that wait's budget does.
    let past = base + crate::state::window_animation::MAX_START_HOLD + TICK;
    f.state().tick_window_animations_at(TICK, past);
    assert!(
        !f.state().window_animations.start_held(eid),
        "and the budget it was armed with still ends it"
    );
}

/// A moving freeze keeps the budget it started with. Re-arming it on every
/// position-only retarget would let a held nudge key refresh the deadline faster
/// than it expires and leave the window frozen for as long as the key is down.
#[test]
fn repeated_nudges_never_extend_a_freeze() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    // The first tick anchors the deadline at `base`.
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);
    assert!(
        f.state().window_animations.start_held(eid),
        "frozen by the fit"
    );

    // Key repeat: a nudge every 50ms as the clock walks toward the deadline.
    for step in 1..=4 {
        let now = base + Duration::from_millis(50 * step);
        let from = f.state().stage.position_of(&window).unwrap();
        f.state()
            .map_window(window.clone(), Point::from((from.x + 40, from.y)), false);
        f.state().animate_window_move_from(&window, from);
        f.state().tick_window_animations_at(TICK, now);
        assert!(
            f.state().window_animations.start_held(eid),
            "still frozen before the original deadline (step {step})"
        );
    }

    // 200ms of nudging bought no extra budget.
    let past = base + crate::state::window_animation::MAX_START_HOLD + TICK;
    f.state().tick_window_animations_at(TICK, past);
    assert!(
        !f.state().window_animations.start_held(eid),
        "the freeze expired on the deadline it was armed with"
    );
}

/// The far end follows the same rule: a nudge at the endpoint moves the wait, it
/// does not re-open its budget. Otherwise a held nudge key refreshes the endpoint
/// deadline faster than it expires and parks the window on a size the client
/// never took, for as long as the key is down.
#[test]
fn repeated_nudges_never_extend_an_endpoint_hold() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);

    // Past the start budget the leg degrades and runs to the requested endpoint,
    // where the endpoint hold anchors its deadline.
    let parked = base + PAST_HOLD;
    for _ in 0..MAX_TICKS {
        f.state().tick_window_animations_at(TICK, parked);
        if !f.state().window_animations.start_held(eid) {
            break;
        }
    }
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, parked);
    }
    let endpoint = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the endpoint hold keeps it alive");

    // Key repeat: a nudge every 100ms, each re-converging on the requested rect.
    for step in 1..=4 {
        let now = parked + Duration::from_millis(100 * step);
        let from = f.state().stage.position_of(&window).unwrap();
        f.state()
            .map_window(window.clone(), Point::from((from.x + 40, from.y)), false);
        f.state().animate_window_move_from(&window, from);
        for _ in 0..20 {
            f.state().tick_window_animations_at(TICK, now);
        }
        let v = f
            .state()
            .window_animations
            .geometry_visual_rect(eid)
            .expect("still waiting on the request");
        assert!(
            (v.size.w - endpoint.size.w).abs() <= 0.5,
            "still holding the requested size before the original deadline \
             (step {step}, {v:?})"
        );
    }

    // 400ms of nudging bought no extra budget: the request is dropped on the
    // deadline the endpoint hold anchored, and the rect walks back down.
    let past = parked + crate::state::window_animation::MAX_ENDPOINT_HOLD + TICK;
    f.state().tick_window_animations_at(TICK, past);
    f.state().tick_window_animations_at(TICK, past);
    let released = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the release leg is still in flight");
    assert!(
        released.size.w < endpoint.size.w - 1.0,
        "the endpoint budget expired on schedule ({endpoint:?} -> {released:?})"
    );
}

/// Sliding an adopted window keeps its slot hold: the same hold, moving. Taking
/// the mover's policy would flip it to capped and snap the content down to the
/// client's own size mid-slide.
#[test]
fn a_position_only_retarget_keeps_an_adopted_slot_stretching() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (_sid, cid, surface) = arrange_pending_relaunch(&mut f, &tmp);
    let w = f.client(cid).window(&surface);
    w.set_size(300, 200);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(cid);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted the slot");
    let eid = element_id(&mut f, &adopted);
    let from = f.state().stage.position_of(&adopted).unwrap();

    // Slide it while the slot hold is still outstanding.
    f.state()
        .map_window(adopted.clone(), Point::from((from.x + 40, from.y)), false);
    f.state().animate_window_move_from(&adopted, from);

    let committed = adopted.geometry().size.to_f64();
    let loc = f.state().stage.position_of(&adopted).unwrap().to_f64();
    let v = f.state().animated_visual(eid, loc, committed);
    assert!(
        !v.cap_content,
        "an adopted slot keeps stretching while it slides"
    );
    // And it keeps filling it as the slide plays out — dropping the hold would
    // bend the rect down to the client's own size instead.
    let base = Instant::now();
    for _ in 0..8 {
        f.state().tick_window_animations_at(TICK, base);
        let v = f
            .state()
            .window_animations
            .geometry_visual_rect(eid)
            .expect("the hold is still in flight");
        assert!(
            (v.size.w - 600.0).abs() <= 0.5 && (v.size.h - 400.0).abs() <= 0.5,
            "the slot rect survives the slide ({v:?})"
        );
    }
}

/// A commit arriving after both budgets have run out — the request already gone,
/// dropped by the endpoint release rather than by any commit — is still the
/// resolution: it clears staleness, so the flag never outlives the buffer it
/// describes. The arm under test only exists once nothing is outstanding, which
/// is why the request has to be genuinely dropped first.
#[test]
fn a_late_commit_after_the_deadline_clears_staleness() {
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

    f.state().fit_window(&window);
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);

    // Past the start budget the leg degrades and parks at the requested endpoint.
    let after_start = base + PAST_HOLD;
    for _ in 0..MAX_TICKS {
        f.state().tick_window_animations_at(TICK, after_start);
        if !f.state().window_animations.start_held(eid) {
            break;
        }
    }
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, after_start);
    }
    let endpoint = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the endpoint hold keeps it alive");

    // Past the endpoint budget the request is dropped — still with no commit —
    // and the rect starts back toward the size the client actually has.
    let after_endpoint = after_start + PAST_HOLD;
    f.state().tick_window_animations_at(TICK, after_endpoint);
    f.state().tick_window_animations_at(TICK, after_endpoint);
    let committed = window.geometry().size.to_f64();
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let released = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    assert!(
        released.size.w < endpoint.size.w - 1.0,
        "the deadline dropped the request, so nothing is left for the commit \
         below to resolve ({endpoint:?} -> {released:?})"
    );
    assert!(
        f.state().animated_visual(eid, loc, committed).cap_content,
        "still stale right after the release — no commit yet"
    );

    // The client finally redraws, at a size of its own.
    let w = f.client(id).window(&surface);
    w.set_size(900, 700);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(id);

    assert!(
        f.state().window_animations.is_active(),
        "the release leg is still running for the late commit to land on"
    );
    let committed = window.geometry().size.to_f64();
    assert!(
        !f.state().animated_visual(eid, loc, committed).cap_content,
        "the late commit is the resolution arriving, so staleness is cleared"
    );
}

// Dismissing a focused stand-in follows the same tiers a real close does: the
// spatially-related history entry first (panning to it only when
// `auto_navigate_on_close` allows), else a visible window on the stand-in's home
// output — never panning in that arm.

/// Helper: a stand-in at `pos` plus a live client window at `win_pos`, camera at
/// the origin with animations quiet. Returns the client's window handle.
fn standin_and_window(
    f: &mut Fixture,
    pos: Point<i32, Logical>,
    win_pos: Point<i32, Logical>,
) -> (crate::state::SuspendedId, smithay::desktop::Window) {
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();
    map_window(f, id, "live", (400, 300));
    let window = window_by_app_id(f, "live").unwrap();
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.zoom = 1.0;
        os.camera_target = None;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
        os.overview_return = None;
        os.momentum.stop();
    });
    f.state().map_window(window.clone(), win_pos, false);
    f.state().update_output_from_camera();
    let sid = f
        .state()
        .insert_suspended_for_test(1, pos, Size::from((400, 300)), "myapp", "myapp");
    f.state()
        .set_suspended_focus(sid, smithay::utils::SERIAL_COUNTER.next_serial());
    (sid, window)
}

/// (a) A focused stand-in clustered with a window that is scrolled off-screen:
/// with auto-navigation on, the dismiss pans to it, exactly as a close would.
#[test]
fn dismissing_a_focused_stand_in_navigates_to_a_related_off_screen_window() {
    let mut f = Fixture::new();
    // The window sits immediately right of the stand-in (snapped: same cluster),
    // and both are far from the camera so the follow target is off-screen.
    let (sid, _window) =
        standin_and_window(&mut f, Point::from((6000, 600)), Point::from((6412, 600)));

    f.state().dismiss_suspended(sid);

    assert!(
        f.state().camera_target().is_some(),
        "a related off-screen follow target pans, like a close does"
    );
}

/// (b) No spatial relation and the only MRU window is off-screen: the dismiss
/// must not pan. Focus falls to a visible window on the home output, or clears.
#[test]
fn dismissing_an_unrelated_focused_stand_in_does_not_pan() {
    let mut f = Fixture::new();
    // The window is nowhere near the stand-in, and off the stand-in's viewport.
    let (sid, _window) =
        standin_and_window(&mut f, Point::from((300, 300)), Point::from((40000, 40000)));
    let before = f.state().camera();

    f.state().dismiss_suspended(sid);

    assert_eq!(f.state().camera(), before, "the no-follow arm never pans");
    assert!(
        f.state().camera_target().is_none(),
        "and arms no camera animation"
    );
}

/// (c) Same shape as (a) but with auto-navigation off: the off-screen follow is
/// dropped rather than panned to.
#[test]
fn dismissing_with_auto_navigate_off_drops_an_off_screen_follow() {
    let mut f = Fixture::with_config(
        Config::from_toml("[navigation]\nauto_navigate_on_close = false\n").unwrap(),
    );
    let (sid, _window) =
        standin_and_window(&mut f, Point::from((6000, 600)), Point::from((6412, 600)));
    let before = f.state().camera();

    f.state().dismiss_suspended(sid);

    assert_eq!(
        f.state().camera(),
        before,
        "auto_navigate_on_close = false never pans on dismiss"
    );
    assert!(f.state().camera_target().is_none());
}

/// (d) Dismissing a stand-in that never held focus leaves focus and camera alone.
#[test]
fn dismissing_an_unfocused_stand_in_changes_nothing() {
    let mut f = Fixture::new();
    let (sid, window) =
        standin_and_window(&mut f, Point::from((6000, 600)), Point::from((6412, 600)));
    // Hand focus to the live window, so the stand-in is not the focused element.
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);
    let before = f.state().camera();

    f.state().dismiss_suspended(sid);

    assert_eq!(f.state().camera(), before, "no camera change");
    assert!(f.state().camera_target().is_none());
    assert!(
        f.state().focused_window().is_some(),
        "the live window keeps focus"
    );
}

/// A resize freeze renders the window exactly as it looked before the action —
/// which for a frame-converted seed (entering fullscreen from a zoomed-in canvas)
/// is not 1:1. Capping the content there would visibly shrink the "frozen"
/// window, so the hold is deliberately uncapped at whatever the seed ratio is.
#[test]
fn a_frozen_resize_renders_uncapped_at_its_seed_ratio() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    // A seed twice the committed size, as a fullscreen enter at zoom 2 produces.
    let committed = window.geometry().size.to_f64();
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let seed = Rectangle::new(loc, Size::from((committed.w * 2.0, committed.h * 2.0)));
    f.state().begin_geometry_animation_seeded(
        &window,
        seed,
        crate::state::window_animation::AnimSpace::Canvas,
        Some(Size::from((1896, 1056))),
        crate::state::window_animation::GeometryRole::FullscreenEntry { was_pinned: false },
        crate::state::window_animation::ContentPolicy::Cap,
    );
    let base = Instant::now();
    for _ in 0..10 {
        f.state().tick_window_animations_at(TICK, base);
    }

    assert!(f.state().window_animations.start_held(eid), "frozen");
    let v = f.state().animated_visual(eid, loc, committed);
    assert!(!v.cap_content, "a frozen window is never capped");
    let (sx, sy) = crate::state::window_animation::content_scale(v.size, committed, v.cap_content);
    assert!(
        (sx - 2.0).abs() < 1e-6 && (sy - 2.0).abs() < 1e-6,
        "it renders at the seed ratio, reproducing the pre-action look ({sx:.2}x)"
    );
}

/// A fullscreen enter flips stage membership at the action, but the freeze holds
/// the *windowed* picture on screen for the length of its budget after that.
/// Chrome follows the picture, not the membership — stripping the bar, border and
/// shadow (and uncropping a CSD client's own shadow) at the action would leave a
/// motionless frame wearing the wrong dress for the whole freeze. The client's
/// redraw then starts the exchange rather than finishing it: the chrome fades out
/// across the grow instead of blinking off while the window is still small.
#[test]
fn a_frozen_fullscreen_enter_keeps_its_windowed_chrome() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(
        f.state().window_animations.start_held(eid),
        "the enter waits for the client to redraw at the fullscreen size"
    );
    assert!(
        f.state().stage.is_fullscreen(&window),
        "the stage flipped the instant the action ran"
    );
    assert_eq!(
        chrome_alpha(&mut f, &window),
        1.0,
        "but the picture on screen is still the windowed one, chrome and all"
    );

    // The redraw the freeze was waiting for. The window starts growing here, and
    // the chrome starts leaving with it — neither is done yet.
    super::adopt_last_configure(&mut f, id, &surface);
    assert_eq!(
        chrome_alpha(&mut f, &window),
        1.0,
        "the leg has not travelled yet, so the chrome is all still there"
    );
    f.state().tick_window_animations(TICK);
    let mid = chrome_alpha(&mut f, &window);
    assert!(
        mid > 0.0 && mid < 1.0,
        "it hands over across the leg rather than at one frame ({mid})"
    );
    tick_until_settled(&mut f);
    assert!(
        f.state().chrome_fullscreen(&window),
        "and is gone once the window fills the output"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The mirror case, and the one a user can nudge: an exit's freeze holds the
/// fullscreen picture after the stage has already let it go, so chrome stays off
/// until the client redraws at its windowed size. A position-only retarget is the
/// same freeze moving and must not restate what that picture wore.
#[test]
fn a_frozen_fullscreen_exit_keeps_its_fullscreen_chrome() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);

    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);
    assert!(
        f.state().window_animations.start_held(eid),
        "the exit waits for the client to redraw at its windowed size"
    );
    assert!(
        !f.state().stage.is_fullscreen(&window),
        "the stage let it go the instant the action ran"
    );
    assert_eq!(
        chrome_alpha(&mut f, &window),
        0.0,
        "but the picture on screen is still the fullscreen one, so no chrome"
    );

    let from = f.state().stage.position_of(&window).unwrap();
    f.state()
        .map_window(window.clone(), Point::from((from.x + 40, from.y)), false);
    f.state().animate_window_move_from(&window, from);
    assert_eq!(
        chrome_alpha(&mut f, &window),
        0.0,
        "a nudge moves the freeze, it does not redress the frozen picture"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    f.state().tick_window_animations(TICK);
    let mid = chrome_alpha(&mut f, &window);
    assert!(
        mid > 0.0 && mid < 1.0,
        "the windowed redraw brings the chrome back across the shrink ({mid})"
    );
    tick_until_settled(&mut f);
    assert_eq!(
        chrome_alpha(&mut f, &window),
        1.0,
        "and it is fully there once the window is back"
    );
}

/// A *resize* landing on a frozen exit is a new request, so it re-freezes and
/// re-arms — but the picture on screen is still the fullscreen one it froze on,
/// so the chrome stamp has to survive. Restating it from the fit's role would pop
/// a bar, border and shadow onto a motionless fullscreen frame.
#[test]
fn a_fit_during_a_fullscreen_exit_freeze_keeps_the_frozen_chrome() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);

    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);
    let generation = f.state().window_animations.generation_of(eid);
    assert_eq!(
        chrome_alpha(&mut f, &window),
        0.0,
        "the exit froze the fullscreen picture, which wears no chrome"
    );

    // A fit while that frame is still up: a genuinely new request, so the freeze
    // re-arms — but not on a new picture.
    f.state().fit_window(&window);
    assert!(
        f.state().window_animations.generation_of(eid) > generation,
        "the fit superseded the exit's request"
    );
    assert!(
        f.state().window_animations.start_held(eid),
        "and waits for the client's redraw in turn"
    );
    assert_eq!(
        chrome_alpha(&mut f, &window),
        0.0,
        "the picture it is waiting on is still the fullscreen one"
    );

    // Only the client's redraw changes it, and then only gradually — the fit's
    // leg is where the chrome the frozen picture never had arrives.
    super::adopt_last_configure(&mut f, id, &surface);
    f.state().tick_window_animations(TICK);
    let mid = chrome_alpha(&mut f, &window);
    assert!(mid > 0.0 && mid < 1.0, "the chrome fades in ({mid})");
    tick_until_settled(&mut f);
    assert_eq!(chrome_alpha(&mut f, &window), 1.0);
}

/// A fullscreen exit lets go of stage membership at the action, but for the
/// length of its freeze the picture covering the output has not moved. The output
/// has to keep reporting itself covered until the client's redraw lands —
/// otherwise the panels, the canvas background and every other window pop back in
/// over a motionless fullscreen frame.
#[test]
fn a_frozen_fullscreen_exit_still_covers_its_output() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
    assert!(f.state().is_output_visually_fullscreen(&output));

    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);
    assert!(
        f.state().window_animations.start_held(eid),
        "the exit waits for the client to redraw at its windowed size"
    );
    assert!(
        !f.state().is_output_fullscreen(&output),
        "the stage let go the instant the action ran"
    );
    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "but the picture covering the output has not moved yet"
    );
    assert_eq!(
        f.state().visually_fullscreen_window_on(&output).as_ref(),
        Some(&window),
        "and the window drawing it is the one on its way out"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    assert!(
        !f.state().is_output_visually_fullscreen(&output),
        "the redraw is windowed, so the output is uncovered from that frame on"
    );
    tick_until_settled(&mut f);
}

/// Entering fullscreen unpins at the action, but the freeze then holds the
/// *pinned* picture on screen. Reading pin membership live restacks a frame that
/// is not moving: it drops out of the bucket that draws above every normal
/// window, and its title bar loses the pin marker mid-freeze.
#[test]
fn a_frozen_fullscreen_enter_keeps_its_pinned_bucket() {
    let mut f = Fixture::with_config(
        Config::from_toml("[[window_rules]]\napp_id = \"p\"\npinned_to_screen = true\n").unwrap(),
    );
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "p", (400, 300));
    let window = window_by_app_id(&mut f, "p").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    assert!(f.state().is_pinned(&window), "the window pinned via rule");
    tick_until_settled(&mut f);

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    assert!(
        f.state().window_animations.start_held(eid),
        "the enter waits for the client to redraw fullscreen"
    );
    assert!(
        !f.state().is_pinned(&window),
        "the stage unpinned the instant the action ran"
    );
    assert!(
        f.state().pinned_picture_of(Some(eid), &window),
        "but the picture on screen is still the pinned one"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    assert!(
        !f.state().pinned_picture_of(Some(eid), &window),
        "the fullscreen redraw is not, and takes the bucket with it"
    );

    f.state().exit_fullscreen_on(&output);
}

/// A compositor resize can move the window as well as resize it, and the freeze
/// holds the old picture in the old place for its whole budget. Culling on the
/// window's live rect alone then composes it out of the very frames its own
/// animation asked for — it vanishes outright, mid-flight, and reappears at the
/// destination.
#[test]
fn a_frozen_resize_that_moves_off_screen_is_still_drawn() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((200, 200)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    // Resize and relocate in one action: the entry freezes on the rect the window
    // occupies now, while the stage already holds the far-away destination.
    f.state()
        .animate_window_geometry(&window, Size::from((900, 700)));
    f.state()
        .map_window(window.clone(), Point::from((6000, 6000)), false);
    assert!(f.state().window_animations.start_held(eid), "frozen");

    let bbox = f
        .state()
        .window_bbox_with_popups(&window)
        .expect("the window is stage-mapped");
    assert!(
        !f.state().canvas_rect_drawable(bbox),
        "the live rect has left every viewport"
    );
    let culled = f.state().window_cull_rect(Some(eid), bbox);
    assert!(
        f.state().canvas_rect_drawable(culled),
        "but the picture on screen has not, so the frame must still draw it"
    );
}

/// A resize the client may not even be able to honour is not worth a freeze, a
/// stash, a GPU flatten and a crossfade. Worse, a client that *cannot* take a
/// few-pixel request answers by committing the size it already had, which no arm
/// can tell from silence — so the freeze would burn its whole budget over a
/// resize nobody can see.
#[test]
fn a_sub_threshold_resize_carries_no_request() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let committed = window.geometry().size;
    f.state()
        .animate_window_geometry(&window, Size::from((committed.w + 10, committed.h + 3)));
    assert!(
        !f.state().window_animations.start_held(eid),
        "a resize this small has nothing worth waiting for"
    );
    // No budget was opened, so the leg converges on its own — with a freeze armed
    // this spins until the deadline instead.
    tick_until_settled(&mut f);

    f.state()
        .animate_window_geometry(&window, Size::from((committed.w + 11, committed.h)));
    assert!(
        f.state().window_animations.start_held(eid),
        "one pixel more is a real resize, and freezes like one"
    );
}

/// The mirror: an exit armed while the *enter* is still frozen must not strip the
/// chrome off the windowed picture that freeze is holding. Filled first, so the
/// exit restores to a size the client does not have and the retarget genuinely
/// carries a request.
#[test]
fn a_fullscreen_exit_during_the_enter_freeze_keeps_the_windowed_chrome() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fill_window(&window);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);
    tick_until_settled(&mut f);

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    let generation = f.state().window_animations.generation_of(eid);
    assert!(
        f.state().window_animations.start_held(eid),
        "the enter waits for the client to redraw fullscreen"
    );
    assert!(
        !f.state().chrome_fullscreen(&window),
        "the picture on screen is still the windowed one"
    );

    // Fullscreen off again, inside the same freeze.
    f.state().exit_fullscreen_on(&output);
    assert!(
        f.state().window_animations.generation_of(eid) > generation,
        "restoring a size the client does not have is a new request"
    );
    assert!(
        !f.state().chrome_fullscreen(&window),
        "the frame never became fullscreen, so it keeps its chrome"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
}

/// A frozen window paints the identical picture every frame, so it must not
/// drive a full compose per tick for half a second. It still has to count as an
/// active animation, though: the deadline that ends the freeze can only fire from
/// a tick, and the tick that fires it does move the window.
#[test]
fn a_frozen_entry_asks_for_no_redraw_but_keeps_ticking() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);
    assert!(
        f.state().window_animations.start_held(eid),
        "the fit froze the window"
    );

    f.state().redraws_needed.clear();
    f.state().tick_window_animations_at(TICK, base);
    assert!(
        f.state().redraws_needed.is_empty(),
        "a frozen tick composes nothing new"
    );
    assert!(
        f.state().output_has_active_animations(&output),
        "but the entry still keeps the loop awake, or its budget could never expire"
    );

    // The tick that lets the budget expire is a real frame: it starts the leg.
    let past = base + PAST_HOLD;
    f.state().tick_window_animations_at(TICK, past);
    assert!(
        !f.state().redraws_needed.is_empty(),
        "the tick that unfreezes the window marks its output"
    );
}

/// A request for the size the window already has resolves at the seed, so there is
/// nothing to wait for: no freeze, and the leg runs immediately.
#[test]
fn a_same_size_request_never_freezes() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let committed = window.geometry().size;
    f.state().animate_window_geometry(&window, committed);
    f.state()
        .map_window(window.clone(), Point::from((700, 300)), false);
    assert!(
        !f.state().window_animations.start_held(eid),
        "an already-satisfied request has nothing to wait for"
    );
    tick_until_settled(&mut f);
}

/// A brand new resize landing mid-anything re-freezes from wherever the window is
/// and bumps the capture generation, so content captured for the superseded
/// request can never be paired with the new leg.
#[test]
fn a_request_carrying_retarget_refreezes_and_bumps_the_generation() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    let first = f.state().window_animations.generation_of(eid).unwrap();
    let base = Instant::now();
    for _ in 0..4 {
        f.state().tick_window_animations_at(TICK, base);
    }
    seed_resize_capture(&mut f, eid);

    // A second, genuinely different resize while the first is still frozen.
    // (Unfitting back to the size the client still has would be a same-size
    // request, resolved at the seed with nothing new to wait for.)
    f.state()
        .animate_window_geometry(&window, Size::from((900, 700)));
    let second = f.state().window_animations.generation_of(eid).unwrap();
    assert!(
        second > first,
        "the new request invalidates the old capture ({first} -> {second})"
    );
    assert!(
        f.state().window_animations.start_held(eid),
        "and it waits for the client's redraw of the new size"
    );
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["resize_captures"], 0,
        "the superseded request's capture went with it"
    );
    assert_eq!(
        counters["resize_crossfades"], 0,
        "as would any overlay for the leg that no longer exists — that half needs \
         a renderer, so only the capture is pinned here"
    );
}

/// A freeze whose window scrolls off every viewport instant-completes, and the
/// content captured for a crossfade that will never play goes with it. The
/// client's eventual redraw then finds nothing to resolve.
#[test]
fn an_off_screen_freeze_drops_its_entry_and_its_capture() {
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

    f.state().fit_window(&window);
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);
    assert!(
        f.state().window_animations.start_held(eid),
        "the fit froze the window"
    );
    seed_resize_capture(&mut f, eid);

    // Pan away: the frozen rect now intersects no viewport.
    f.state().set_camera(Point::from((100_000.0, 100_000.0)));
    f.state().update_output_from_camera();
    f.state().tick_window_animations_at(TICK, base);

    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the frozen entry instant-completed off-screen"
    );
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["resize_captures"], 0,
        "nothing stays stashed for a leg that will never run"
    );
    assert_eq!(counters["resize_crossfades"], 0, "overlay is backend-gated");

    // The redraw the freeze was waiting for lands late: a no-op, not a revival.
    let w = f.client(id).window(&surface);
    w.set_size(1896, 1056);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(id);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the late commit revived nothing"
    );
    let counters = f.state().debug_counters();
    assert_eq!(counters["resize_captures"], 0);
    assert_eq!(counters["resize_crossfades"], 0);
}

/// Suspending a window mid-freeze converts it into a stand-in that inherits its
/// `ElementId`, so both crossfade halves have to be dropped at the conversion:
/// the dead-id sweep can never fire for an id that is still very much alive, and
/// a surviving overlay would wear the dead client's pixels on the stand-in.
#[test]
fn conversion_mid_freeze_drops_the_crossfade_with_the_entry() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // A stand-in only appears for an app the compositor can relaunch.
    std::fs::write(
        tmp.path().join("myapp.desktop"),
        "[Desktop Entry]\nType=Application\nName=myapp\nExec=myapp\n",
    )
    .unwrap();
    f.state().desktop_entry_cache = Some(DesktopEntryCache::new(vec![tmp.path().to_path_buf()]));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "myapp", (400, 300));
    let window = window_by_app_id(&mut f, "myapp").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    f.state().fit_window(&window);
    f.state().tick_window_animations_at(TICK, Instant::now());
    assert!(
        f.state().window_animations.start_held(eid),
        "the fit froze the window"
    );
    seed_resize_capture(&mut f, eid);

    f.state().execute_action(&Action::SuspendWindow);
    f.client(id).window(&surface).destroy();
    f.roundtrip(id);
    f.dispatch();

    assert!(
        f.state()
            .stage
            .window_by_id(eid)
            .is_some_and(|w| w.suspended().is_some()),
        "the stand-in inherited the frozen window's id — no sweep will collect it"
    );
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the conversion dropped the frozen chase"
    );
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["resize_captures"], 0,
        "and the content captured for its crossfade"
    );
    assert_eq!(counters["resize_crossfades"], 0, "overlay is backend-gated");

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

/// A window that remaps mid-freeze (a hide-to-tray reshow) gets an open entry
/// written straight over its geometry entry — there is no remove site to hang
/// the cleanup on — so the crossfade halves have to go at the open itself.
/// Otherwise the old picture keeps fading over a window that is scaling in.
#[test]
fn an_open_entry_over_a_frozen_resize_drops_the_crossfade() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    f.state().tick_window_animations_at(TICK, Instant::now());
    assert!(
        f.state().window_animations.start_held(eid),
        "the fit froze the window"
    );
    seed_resize_capture(&mut f, eid);

    f.state().start_window_open_animation(&window);
    assert!(
        !f.state().window_animations.start_held(eid),
        "the open entry replaced the frozen chase"
    );
    assert_eq!(
        f.state().debug_counters()["resize_captures"],
        0,
        "and its captured content went with it"
    );
    tick_until_settled(&mut f);
}

/// The old-content bake is rasterized for the size it will be drawn at: one baked
/// texel per physical pixel, at every output scale and camera zoom. The render
/// side draws the bake across the entry's visual rect through the window's own
/// transform, so the drawn extent is `visual · zoom · output_scale` — flooring
/// the `output_scale · zoom` half at 1.0 (the close bake's rule) multiplies with
/// a fullscreen exit's `1/zoom` stretch and bakes a texture several times that:
/// 4x the pixels at zoom 0.5, 25x at 0.2, past `GL_MAX_TEXTURE_SIZE` below that,
/// where the allocation fails and the crossfade is silently skipped.
#[test]
fn a_resize_bake_carries_one_texel_per_drawn_pixel() {
    for scale in [1.0, 1.5, 2.0] {
        for zoom in [1.0, 0.75, 0.5, 0.2] {
            let mut f = Fixture::new();
            let output = f.add_output(1, (1920, 1080));
            output.change_current_state(
                None,
                None,
                Some(smithay::output::Scale::Fractional(scale)),
                None,
            );
            let id = f.add_client();
            let _surface = map_window(&mut f, id, "a", (800, 600));
            let window = window_by_app_id(&mut f, "a").unwrap();
            reset_view(&mut f);
            f.state().with_output_state(|os| os.zoom = zoom);
            f.state().update_output_from_camera();
            f.state()
                .map_window(window.clone(), Point::from((100, 100)), false);
            let eid = element_id(&mut f, &window);
            tick_until_settled(&mut f);

            // A fullscreen exit's shape: the captured picture is the fullscreen
            // buffer (one viewport), frozen on a canvas rect of `viewport / zoom`
            // while it restores to the windowed size.
            let captured = crate::state::output_logical_size(&output);
            let seed = Rectangle::new(
                Point::from((100.0, 100.0)),
                Size::from((captured.w as f64 / zoom, captured.h as f64 / zoom)),
            );
            f.state().begin_geometry_animation_seeded(
                &window,
                seed,
                crate::state::window_animation::AnimSpace::Canvas,
                Some(Size::from((800, 600))),
                crate::state::window_animation::GeometryRole::FullscreenExit {
                    output: output.name(),
                },
                crate::state::window_animation::ContentPolicy::Cap,
            );

            let visual = f
                .state()
                .window_animations
                .geometry_visual_rect(eid)
                .expect("the exit seeded a frozen entry");
            let texels = captured.w as f64 * f.state().resize_bake_scale(&window, eid, captured);
            let drawn = visual.size.w * zoom * scale;
            assert!(
                (texels / drawn - 1.0).abs() < 1e-6,
                "scale {scale}, zoom {zoom}: baked {texels:.0} texels for {drawn:.0} \
                 drawn px"
            );
        }
    }
}

/// The close bake keeps its floor: the resize bake's unfloored scale is a
/// sibling, not a replacement. A close fades in canvas space, so a snapshot taken
/// while zoomed out still rasterizes at full logical resolution.
#[test]
fn a_close_bake_never_rasterizes_below_logical_resolution() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    reset_view(&mut f);
    let rect = Rectangle::new(Point::from((100, 100)), Size::from((400, 300)));

    f.state().with_output_state(|os| os.zoom = 0.5);
    f.state().update_output_from_camera();
    assert_eq!(
        f.state().flatten_scale_for_canvas_rect(rect),
        1.0,
        "zoomed out, the floor holds the bake at logical resolution"
    );

    output.change_current_state(
        None,
        None,
        Some(smithay::output::Scale::Fractional(2.0)),
        None,
    );
    f.state().with_output_state(|os| os.zoom = 1.0);
    f.state().update_output_from_camera();
    assert_eq!(
        f.state().flatten_scale_for_canvas_rect(rect),
        2.0,
        "above the floor the rect's render scale is used as-is"
    );

    let off_screen = Rectangle::new(Point::from((100_000, 100_000)), Size::from((400, 300)));
    assert_eq!(
        f.state().flatten_scale_for_canvas_rect(off_screen),
        1.0,
        "a rect no output shows falls back to the same floor"
    );
}

/// Adoption holds a slot rather than requesting a resize, so it is never frozen —
/// its content is meant to stretch to fill immediately.
#[test]
fn an_adopted_slot_is_never_frozen() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (_sid, cid, surface) = arrange_pending_relaunch(&mut f, &tmp);
    let w = f.client(cid).window(&surface);
    w.set_size(300, 200);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(cid);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted the slot");
    let eid = element_id(&mut f, &adopted);
    assert!(
        !f.state().window_animations.start_held(eid),
        "a Stretch entry never start-holds"
    );
}
