# Performance — remaining work

The B1–B14 perf push shipped (see `git log`). What's left, in priority order.
Line numbers predate the push — re-verify on pickup. Profiling tooling:
[profiling.md](profiling.md).

Non-perf items live at the bottom under
[Correctness backlog](#correctness-backlog) and
[Structural backlog](#structural-backlog).

## Blur (B5 + S1 + edge-fade + fullscreen cull)

The only substantive perf work left; deferred behind touchscreen + session
restoration (GH #125). B5 + S1 + edge-fade are one FBO/crop/mask rework; the
fullscreen cull is a separable window-loop early-skip in the same code. So the
blur wins span three contexts: during a pan, on zoom (S1), and under fullscreen.

**B5 — multi-output churn + FBO retention.**

- `src/render/mod.rs` — `blur_cache` is global but `compose_frame` retains per
  output: two outputs showing different blurred windows evict each other every
  frame → `BlurCache::new` re-allocates 3 window-sized textures + full recompute
  per blurred window per frame (~25 MB/frame at 1080p). Fix: retain against the
  union of blur requests across outputs.
- `src/render/blur.rs` — `blur_bg_fbo` is one slot keyed by size; different-sized
  outputs evict each other per frame (~33 MB alloc/free at 4K). Fix: key per
  output name, free in `remove_output`. Also drop the slot when no blur requests
  remain (currently retained forever after the last blurred window closes).

**S1 — blur fully recomputes every frame of a pan _or zoom_.** The cache hash
includes the window's screen-space position (`src/render/blur.rs` hashes
`window_rect.loc`), so any camera motion marks every blurred window dirty every
frame: full-output offscreen FBO repaint, crop, 2×radius Kawase passes, a second
full render for the alpha mask, masking pass. Screen-fixed blur on other monitors
also recomputes (`blur_camera_generation` is a global counter, `src/state/mod.rs`).
Fix options: translate the cached blur texture by the camera delta during
camera-only motion (blur is low-frequency); recompute at half rate while panning;
or key on (quantized position, behind-element commits).

**Edge-fade artifact.** Behind-content is cropped to exactly `win_size`, so the
Kawase kernel clamps at window edges and the blur tapers inward. Fix: blur a
radius-padded region and crop back — same surface as B5/S1. Cost caveat is at the
`blur` field in `config.reference.toml` (rendered in `docs/config.md#window-rules`).

**Fullscreen occlusion-cull.** `compose_frame` does not short-circuit for a
fullscreen output — it runs the full window loop (`src/render/mod.rs:585`, only
viewport-visibility culled at `:650`) and `process_blur_requests` (`:1180`); only
the background (`:1088`) and Top/Bottom layer-shell (`:1123-1132`) are skipped. So
a blurred window behind a fullscreen opaque window still pays its Kawase passes
every frame, _before_ smithay's render-time occlusion cull + direct scanout
(`src/backend/udev.rs:1507`) drop the composited result — wasted GPU that competes
with the fullscreen client, and compounds under screen-share (the capture path
re-composites the whole scene and defeats direct scanout). Only bites when a
blurred window is actually behind fullscreen (empty `blur_requests` → already
fast-skipped). Fix: skip blur for normal windows occluded by the fullscreen
window — but preserve pinned-to-screen windows (they render above fullscreen), the
fullscreen window's own popups, and the Overlay layer, and guard a transparent
fullscreen window.

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
  scans with repeated `with_states` locks per event (`src/input/mod.rs`). Moderate;
  only scales with window count. (The `min_zoom`-per-pinch half shipped.)
- **Latent frame spikes** (config-dependent): synchronous shader-chunk bakes
  mid-frame (`src/render/shader_chunks.rs` — pre-bake a margin ring, pool the FBO);
  gigapixel-TIFF tile uploads up to ~25 ms/frame on the render thread
  (`src/render/mod.rs` — time-budget, or upload after `queue_frame`); shadow shader
  evaluates ERF quadrature over the full window+pad quad (`src/shaders/shadow.glsl`
  — early-out interior fragments).
- **Redundant EmptyFrame composites in non-integer refresh:content beats.**
  `post_render` runs after every `render_frame`, including the `EmptyFrame`
  branch (`src/backend/udev.rs:1604`), and the VBlank handler re-renders directly
  (`:651-653`), bypassing the `render_if_needed` gate (`:305-308`). At ratios like
  144Hz/60fps video a second client commit can land mid-cycle and force a full
  `compose_frame` that smithay then drops as `EmptyFrame` — GPU compositing with no
  page flip. Bounded by the estimated-vblank timer (can't spin) and only during
  active rendering, not idle. niri avoids it via `RedrawState` (one render/cycle;
  callbacks sent at defined sequence boundaries, never from an empty-render branch
  — `niri/src/niri.rs:492-504`). Fix: skip the `compose_frame`/callback-send on the
  `EmptyFrame` path, and/or route the VBlank-handler render through the gate.
  Surfaced during the #157 frame-callback dedup-guard removal.
- **niri patterns** not yet adopted: animations sampled at predicted
  presentation time (`niri/src/niri.rs:4601-4604` — small judder source vs
  driftwm's `Instant::now()`); on-demand VRR by window visibility
  (`niri/src/niri.rs:4720-4749` — gaming pass).

## Correctness backlog

Surfaced by a four-lens seam review before the post-v0.15.0 release, targeting
feature pairs that were each reviewed on their own branch but never together.
All four seam hypotheses came back refuted; these are what the sweep found on
the way past. The three missed-checklist bugs it also found were fixed at the
time — see the `unfit_window` guard and the two `adopt_relaunched` fixes.

- **`unfill_window` strands a fullscreen exit's owed recenter** — the identical
  hole just closed in `unfit_window`. `src/state/fill.rs`'s equal-size branch
  skips the `pending_recenter` insert but never drops one already owed, and
  `enter_fullscreen` preserves fill membership the same way it preserves fit, so
  filled → fullscreen → exit → unfill-before-ack survives with a stale entry.
  Symptom matches the fit case: the reflow is gated forever, and a later
  drag-then-resize teleports the window back. Fix is the one line
  `self.pending_recenter.remove(&wl_surface.id());` in that branch, mirroring
  `unfit_window`. Left out of the release only because doing it properly wants
  its own test, and the fit-side chain was the one with a demonstrated repro.
- **A stand-in is adopted out from under a live grab.**
  `element_under_interactive_grab`'s contract is that nothing may reposition an
  element under a grab, but the activation path only asks about
  `StageWindow::Client`, and the first-commit path asks nothing. Relaunch a
  stand-in, drag it while the app starts (1-3 s), and the adopt destroys it
  mid-drag: the grab degrades to a pass-through and the user drags air until
  button-up. A *client* under a grab defers to the 30 s TTL; a stand-in does
  not. No corruption — both grabs anticipate the vanish and `interactive_move`
  stays balanced — but the asymmetry is a behaviour decision, not a missed line.
- **A grab does not drop an animation entry that already exists.**
  `start_geometry_entry` (`src/state/window_animation_driver.rs`) refuses to
  *create* an entry for an element under an interactive grab, but no grab-install
  path clears one.
  Fit a window, then within `MAX_START_HOLD` (300 ms) grab a stand-in the fit
  push displaced: the stand-in's entry is still parked on `waits_for`, so it
  draws at `visual.loc` and sits motionless under the finger, then rubber-bands
  over a full leg when the wait releases. Fix: `cancel_window_animation` at grab
  install, symmetric with the existing arm.
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
- **Adopt and dismiss read the stage position, not the in-flight visual.**
  Both take `stage.position_of` — the destination — ignoring
  `geometry_visual_rect`, while `animate_element_move_from` is careful to seed
  from the entry's current visual. Adopting or dismissing a stand-in that a
  neighbouring resize pushed within the last few hundred ms teleports the
  departing chrome to the end of the slide in one frame: a pop inside the
  crossfade that exists to prevent exactly that. Cosmetic, narrow window.

## Structural backlog

Not bugs. Measured against niri (93,951 Rust lines to driftwm's 97,581, but
81,832 non-test to driftwm's 63,516 — driftwm carries ~29% less production code
and a 0.54 test ratio to niri's 0.15), size is not the problem. Duplication is.

- **One invariant, N hand-maintained copies.** The
  *clear fit → clear fill → seed `ResizeState` → set `Resizing` → unset
  `Maximized`* sequence exists at five independent sites (`input/pointer.rs`,
  `handlers/xdg_shell.rs`, `input/gestures/swipe.rs`, `input/touch.rs`,
  `state/suspended.rs`), and the "drop an owed `pending_recenter` before
  establishing a placement" step at seven (`state/fit.rs`, `state/fill.rs`,
  `state/fullscreen.rs`, `state/suspended.rs`, `input/actions.rs` ×3). Both have
  now been caught mid-drift — a sweep found four of the five `Maximized` arms
  and missed adoption, which left clients permanently stuck maximized. This is
  the codebase's most productive bug class by a wide margin. Fix: make the
  sequence a single constructor the arms call, so a new arm cannot forget a step.
  The `state/mod.rs` split has since removed the *distance* between these arms
  without removing the duplication: the five `Maximized` sites are unchanged, and
  the `pending_recenter` sites now sit across `state/fit.rs`, `state/fill.rs`,
  `state/fullscreen.rs`, `state/suspended.rs` and `input/actions.rs` ×3. Extract
  the constructor before adding a sixth arm.
