# Performance — remaining work

The B1–B14 perf push shipped (see `git log`). What's left, in priority order.
Line numbers predate the push — re-verify on pickup. Profiling tooling:
[profiling.md](profiling.md).

Non-perf items live at the bottom under
[Correctness backlog](#correctness-backlog) and
[Structural backlog](#structural-backlog).

## Blur (B5b + S1)

The only substantive perf work left; deferred behind touchscreen + session
restoration (GH #125). The rest of the original blur cluster has since shipped:
B5's multi-output cache churn (the cache is keyed per `(output, surface)` now),
the edge-fade artifact (padded crop + mirrored edge sampling), and the fullscreen
occlusion-cull (the window loop skips occluded windows before they can even
enqueue a blur request).

**B5b — `blur_bg_fbo` single slot.** `src/render/blur.rs` — one slot keyed by
size; different-sized outputs evict each other per frame (~33 MB alloc/free at
4K). Fix: key per output name, free in `remove_output` — `state/render_cache.rs`
clears every other blur cache there but not this one. Also drop the slot when no
blur requests remain: `process_blur_requests` only runs when requests exist, so
after the last blurred window closes nothing is left to free it.

**S1 — blur fully recomputes every frame of a pan _or zoom_.** The cache hash
includes the window's screen-space position (`src/render/blur.rs` hashes
`window_rect.loc`), so any camera motion marks every blurred window dirty every
frame: full-output offscreen FBO repaint, padded crop, 2×radius Kawase passes, a
second full render for the alpha mask, masking pass. Zoom additionally changes
`win_size`, reallocating the cache textures on top. The one mitigation in place —
the pan-in-flight hold — is gated on `animated_bg && occluded_by_lower`, so a
canvas window over a static wallpaper still pays full price every frame.
Fix options: translate the cached blur texture by the camera delta during
camera-only motion (blur is low-frequency); recompute at half rate while panning;
or key on (quantized position, behind-element commits).

## Lower-priority backlog (do only if a profile flags it)

- **B7** Gigapixel-TIFF decoder pool: no cancellation of stale in-flight decodes;
  blobs upload regardless of visibility and back up during fast pans
  (`src/render/tile_worker.rs`, `tile_chunks.rs`). Cancel unwanted requests; drop
  off-viewport responses; bound the queue. _Gigapixel-TIFF-wallpaper path only._
- **B11** Momentum auto-launch timer removed + re-inserted per gesture event
  (`src/state/viewport_animation.rs`, ~140-1000 Hz during pans). Keep one timer,
  reschedule.
- **B12** Output-outline strips rebuild pixel Vecs + `MemoryRenderBuffer` + fresh
  element ids per edge per frame (`src/render/mod.rs`), defeating damage tracking.
  _Multi-monitor only._ Cache per (output, color, size).
- **B13 / B15** Held repeatable key (`src/backend/udev.rs`) and the exec loading
  cursor (`src/input/actions.rs`, up to 5 s/launch) mark _all_ outputs dirty at
  refresh rate. Mark only the active/cursor output. _Single-output-marginal — same
  shape as the skipped B1; likely not worth it._
- **B14 (remaining half)** Pointer motion does up to ~6 sequential linear window
  scans with repeated `with_states` locks per event (`src/input/mod.rs`), and each
  scan's inner `is_pinned`/`position_of` lookups resolve through `Stage::entry`,
  itself a linear find — so the real shape is O(n²) in window count. Harmless at
  today's n, but it is the wrong curve. (The `min_zoom`-per-pinch half shipped.)
- **Latent frame spikes** (config-dependent): synchronous shader-chunk bakes
  mid-frame (`src/render/shader_chunks.rs` — pre-bake a margin ring, pool the FBO);
  gigapixel-TIFF tile uploads up to ~25 ms/frame on the render thread
  (`src/render/mod.rs` — time-budget, or upload after `queue_frame`); shadow shader
  evaluates ERF quadrature over the full window+pad quad (`src/shaders/shadow.glsl`
  — early-out interior fragments).
- **Redundant EmptyFrame composites in non-integer refresh:content beats.**
  `compose_frame` runs before the frame is queued (`src/backend/udev.rs:1346`) and
  `post_render` runs unconditionally after it (`:1454`), outside the match that
  catches `EmptyFrame` (`:1407`). At ratios like 144Hz/60fps video a second client
  commit can land mid-cycle and force a full `compose_frame` that smithay then
  drops as `EmptyFrame` — GPU compositing with no page flip, plus a callback send.
  Bounded by the estimated-vblank timer (can't spin) and only during active
  rendering, not idle. niri avoids it via `RedrawState` (one render/cycle;
  callbacks sent at defined sequence boundaries, never from an empty-render branch
  — `niri/src/niri.rs:492-504`). Fix: skip the `compose_frame`/callback-send on the
  `EmptyFrame` path. Note the VBlank handler's direct `render_frame` (`:681`) is
  _not_ worth routing through the `render_if_needed` gate (`:343-345`): it clears
  `frames_pending` and the estimated timer just above (`:676-680`), so all three
  gate conditions already hold there. It only skips the DPMS check and the
  animation tick. Surfaced during the #157 frame-callback dedup-guard removal.
- **niri patterns** not yet adopted: animations sampled at predicted
  presentation time (`niri/src/niri.rs:4601-4604` — small judder source vs
  driftwm's `Instant::now()`); on-demand VRR by window visibility
  (`niri/src/niri.rs:4720-4749` — gaming pass). The VRR one is a bigger job than
  it reads: driftwm has no VRR at all, only a `// VRR not supported` stub in
  `src/protocols/output_management.rs`, so the feature comes first.

## Correctness backlog

Surfaced by a four-lens seam review before the post-v0.15.0 release, targeting
feature pairs that were each reviewed on their own branch but never together.
All four seam hypotheses came back refuted; these are what the sweep found on
the way past. The three missed-checklist bugs it also found were fixed at the
time — see the `unfit_window` guard and the two `adopt_relaunched` fixes.

- **The IPC `move` handler misplaces a window mid-settle, and two coupled bugs
  do it.** `cmd_move` (`src/ipc/mod.rs`) clears fill and calls `map_window` but
  never drops an owed `pending_recenter`, unlike its `MoveToBookmark` keybind
  twin — *and* it sizes `rule_to_internal` from `window.geometry().size`, which
  mid-settle after a fullscreen/fit/fill exit is the stale pre-exit size. Fixing
  only the drop makes the second one worse: today the stale recenter drags the
  window back, which at least reads as a visible no-op; without it the move
  silently lands off by the pre/post-exit size difference and nothing corrects
  it. The keybind twin carries the same stale read for fit and fill exits —
  `move_bookmark_restore_rect` is `Some` only for a fullscreen exit, and
  `input/actions.rs` falls back to the same `geometry().size`. `rule_to_internal`
  (`src/canvas.rs`) subtracts `size/2`, so the landing error is *half* the
  pre/post-exit size difference on each axis, not the whole of it. Note
  `configured_window_size` (defined in `src/state/window_frame.rs`, reasoned
  about at its `src/state/fit.rs` call sites) is *not* the fix: it reads pending
  state that goes stale on any client-initiated resize, trading a one-frame race
  for a permanent error on mpv, terminals and browsers.
  `cmd_move`'s single `geometry().size` read also feeds both the read and the
  write arm, so a fix has to decide whether `driftwm msg move` *reporting*
  changes with it, or accept read-then-write being non-idempotent mid-settle.
- **A stand-in is adopted out from under a live grab.**
  `element_under_interactive_grab`'s contract is that nothing may reposition an
  element under a grab, but the activation path only asks about
  `StageWindow::Client`, and the first-commit path asks nothing. Relaunch a
  stand-in, drag it while the app starts (1-3 s), and the adopt destroys it
  mid-drag: the grab degrades to a pass-through and the user drags air until
  button-up. A *client* under a grab defers to the 30 s TTL; a stand-in does
  not. No corruption — both grabs anticipate the vanish and `interactive_move`
  stays balanced — but the asymmetry is a behaviour decision, not a missed line.
- **A pinned window's canvas ghost misdirects clicks and taps at zoom > 1.**
  `sync_pinned_locs` keeps a pinned window's stage position in sync but leaves
  its canvas *size* unscaled, so at zoom 2 the stage rect covers twice the
  screen area the window occupies. The gesture half is closed — `topmost_under`
  skips pinned windows, so move/resize gestures, Alt+drag and hover all reach
  what is genuinely rendered in the outer band. `element_under_skipping` is now
  the one canvas-space walk that doesn't skip pinned (`topmost_under`,
  `decoration_under` and `surface_under` all do), so its consumers still see the
  ghost: `pointer_context` (`src/input/pointer.rs`) reads OnWindow over empty
  canvas, so on-canvas bindings don't fire there, and the click-to-focus
  fallback plus the touch clean-tap raise (`src/grabs/touch_gesture_grab.rs`)
  raise+focus the pinned window instead of the window actually under the
  pointer.
- **Zero-net-change resize strands `ResizeState::WaitingForLastCommit`.**
  Grab start sets `Resizing` in *pending* state only, so a resize that ends
  where it began leaves `send_pending_configure` with nothing to send and the
  client with no reason to commit (`src/grabs/resize_grab.rs`). Until its next
  repaint the window reads as under an interactive grab, which silently skips
  its next geometry animation and makes relaunch adoption bail. Self-clears.
- **A fullscreen exit establishes a placement without dropping the recenter
  owed on it.** `exit_fullscreen_on` (`src/state/fullscreen.rs`) maps the window
  to `entry.saved_location` and then only *skips* inserting its own recenter when
  `current_size == entry.saved_size` — it never calls `drop_owed_recenter`,
  though establishing a placement is exactly what that helper's contract says
  must drop the promise. Reachable without a keybind: fit, let the client ack,
  fullscreen, then have the client send `unset_maximized` before acking (the
  unfit inserts a recenter against the fit-era size) and `unset_fullscreen`
  before acking (`saved_size` is that same fit-era size, so the exit takes the
  equal-size skip arm). Verified in the fixture: the recenter survives the exit,
  lies inert while the committed size is unchanged, then fires on the next
  differing-size commit and discards both the exit's placement and any drag since
  — a right-edge resize after a 100×30 drag landed the window 410×210 away.
- **A fill dispatched between a resize release and the settle commit loses its
  placement.** `handle_resize_commit` (`src/handlers/compositor.rs`) maps the
  window back to the grab's `initial_window_location`, discarding the
  `map_window` the fill did in between, so a filled window settles at the
  pre-resize corner instead of the gap-inset one the fill computed (verified: fill
  places 12,12 with `snap_gap = 12`, the settle puts it back at 0,0). The cached
  snap rect does *not* disagree — the settle's own `refresh_stable_snap_rect`
  re-derives it from the wrong position — which is precisely why nothing
  downstream notices. Pre-existing;
  `unfill_after_fullscreen_exit_drops_the_stale_recenter_so_the_next_resize_does_not_teleport`
  walks straight through it.
- **Adopt and dismiss read the stage position, not the in-flight visual.**
  Both take `stage.position_of` — the destination — ignoring
  `geometry_visual_rect`, while `animate_element_move_from` is careful to seed
  from the entry's current visual. Adopting or dismissing a stand-in that a
  neighbouring resize pushed within the last few hundred ms teleports the
  departing chrome to the end of the slide in one frame: a pop inside the
  crossfade that exists to prevent exactly that. Cosmetic, narrow window.
- **A fullscreen dispatched before a fit's ack saves a mismatched rect.**
  `enter_fullscreen` (`src/state/fullscreen.rs`) pairs `saved_location`, read
  live from the stage, with a `saved_size` read from `window.geometry().size` on
  the fit arm — but `fit_window` maps to the fit location without waiting for the
  ack, so a fullscreen in that gap saves the fit-era position against the pre-fit
  size and the exit restores the two together. The fill arm had the identical
  bug against `restore_size` and now reads `configured_window_size` instead;
  fit was left alone because nothing forced it. Verified in the fixture: fit
  un-acked, then fullscreen, then exit, restores 800×600 at the fit position.
- **A filled window's fullscreen exit ignores a client's own resize.** The fill
  arm above reads `configured_window_size`, which is what closes the pre-ack
  race, but that is pending state and no client-initiated resize updates it (see
  the note on the IPC `move` item). So a client that resizes *itself* after being
  filled gets its fill-configured size back on the fullscreen exit rather than
  its current one. Narrow, and arguably right — the stage still holds the filled
  position, so the configured size is the rect that pairs with it — but it is a
  deliberate trade, not an oversight. The deeper issue is that a self-resize
  leaves fill membership claiming a rect the window no longer occupies.
- **Input hit-testing is not visual-aware, so an animating window cannot be
  clicked where it is drawn.** Every stage read in the input path is the
  animation's *destination*: `topmost_under`'s `position_and_pinned`,
  `element_under_skipping`'s, `decoration_under`'s and `surface_under`'s
  `position_of` (`src/input/mod.rs`). The render path draws at
  `geometry_visual_rect`, which has no consumer anywhere under `src/input/`. The
  two rects are disjoint for the whole animation. For a fit the offset is
  `old_size/2 - usable.size/2 + gap (+bar)` — independent of camera and position,
  so a 800×600 window on a 1920×1080 usable area sits 548 px left and 204 px up
  of its phantom. The title band is ~24 px, so a title-bar grab *cannot* land:
  the press falls through `decoration_under` (the phantom's content occludes it),
  matches the phantom body in `element_under`, and lands on click-to-focus.
  Second symptom, worth fixing together: `surface_under` is stage-based too, so
  the client receives the press at surface-local coordinates off by the same
  offset — clicks during an animation reach the wrong part of the window.
  The freeze is the worst phase: for up to `MAX_START_HOLD` (300 ms) the window
  is perfectly *motionless* at its old position while the stage already holds the
  new one, so divergence is at maximum and nothing on screen suggests anything is
  in flight. Latent at default speeds (~120 ms, reads as a missed click); obvious
  once `[effects] animation_speed` is lowered — at 0.02 it lasts ~4 s. Same root
  family as the adopt/dismiss item above, but that one is a cosmetic one-frame
  pop and this is the whole input layer. Fix: thread the visual rect into those
  four stage reads, taking `geometry_visual_rect(id).loc` when a Canvas-space
  entry is live and falling back to the stage position otherwise — the pattern
  `window_screen_rect_on` (`src/state/window_animation_driver.rs`) already uses.
  Check `geometry_space` first; that accessor's doc warns about Canvas vs Screen.
- **A grab held still during a camera animation drags the window.** Press and
  hold a title bar while the canvas is gliding (momentum, or any camera/zoom
  animation) and the window travels with the camera. `warp_pointer`
  (`src/state/viewport_animation.rs`) synthesizes real motion into a live grab to
  keep the pointer at a fixed *screen* position, so its *canvas* position moves —
  and `apply_move` measures its delta against a fixed canvas anchor, which reads
  as a genuine drag. A press without hold is unaffected: no grab is live to
  receive the synthesized motion. The underlying question is whether a `MoveGrab`
  should track canvas or screen space during a camera animation; the answer also
  decides what `apply_resize` should do, since it turns the same delta into a
  size change. Confirmed on hardware.

## Structural backlog

**The fit / fill / fullscreen exit tails are three hand-maintained copies of one
sequence.** *capture `pre_exit_size` → animate → configure → map → equal-size
branch or insert a `PendingRecenter`*. `unfit_window` (`src/state/fit.rs`) and
`unfill_window` (`src/state/fill.rs`) are now structurally identical, differing
only in where the preserved center comes from (a derived `new_loc` vs the
recorded `saved_pos`) and which configure they send (`exit_fit_configure` vs
`send_size_configure`); `exit_fullscreen_on` (`src/state/fullscreen.rs`) is a
third copy that diverged — it has the skip but not the drop (see the correctness
item above). This is not theoretical drift: the fill copy was missing the
equal-size branch's `drop_owed_recenter` for months, and the fullscreen one still
is. Extract before adding a fourth exit.

The duplication this list used to track *is* extracted, and both extractions are
worth recording so a new arm grows through them rather than beside them.

The *clear fit → clear fill → seed `ResizeState` → set `Resizing` → unset
`Maximized`* sequence is `DriftWm::begin_client_resize` (`src/state/resize.rs`),
called by the **four** entry points that can start a client resize
(`input/pointer.rs`, `handlers/xdg_shell.rs`, `input/gestures/swipe.rs`,
`input/touch.rs`). Four, not the five this list used to claim: `adopt_relaunched`
(`src/state/suspended.rs`) shares only the `Maximized` unset, and for an
unrelated reason — the inherited stage entry has no fit state, so a set
`Maximized` is one the client can never shed. It seeds no `ResizeState` and no
resize is in flight; it is not a fifth site. The "drop an owed `pending_recenter`
before establishing a placement" step is `DriftWm::drop_owed_recenter`
(`src/state/recenter.rs`), called by all **nine** arms — the ninth being
`unfill_window`'s equal-size branch (`src/state/fill.rs`).

The measurement that motivated the extraction: against niri (93,951 Rust lines to
driftwm's 98,156, but 81,832 non-test to driftwm's 63,671 — driftwm carries ~29%
less production code and a 0.54 test ratio to niri's 0.15), size was never the
problem. Both sides of that comparison count dedicated test files only; driftwm
also carries 11,897 lines of inline `#[cfg(test)]` modules, putting its true
non-test total at 51,774 and its true test ratio nearer 0.90.
