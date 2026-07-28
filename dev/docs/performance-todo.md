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

- **`driftwm msg move` read-then-write is non-idempotent mid-settle.** The write
  arm lands where asked whatever the client is still committing — `cmd_move` and
  the `MoveToBookmark` keybind both go through `map_window_to_rule_point`
  (`src/state/recenter.rs`), which re-aims the owed `pending_recenter` at the
  requested point instead of dropping it. The read arm still derives its center
  from `window.geometry().size`, so mid-settle a `get` followed by a `move` back
  to the reported point relocates the window. Deliberate: `msg state` and the
  state file derive `position` from that same committed geometry
  (`src/state/persistence.rs`, single source of truth per its own doc), so
  changing this one arm alone would make the two disagree, and neither
  `docs/ipc.md` nor `docs/cli.md` documents which size the center derives from.
  Fix, if ever: move every reader onto one accessor at once.
- **A re-aimed recenter that never settles strands both halves of what it
  promised.** Re-aiming rather than dropping (the item above) buys the correct
  landing at the cost `drop_owed_recenter`'s own doc warns about: an entry left
  owed also gates `reflow_grown_snapped_window`. The settle trigger is
  `geo.size != pre_exit_size` (`src/handlers/compositor.rs`), so a client that
  acks the exit configure and then commits back at *exactly* its pre-exit size
  never fires it. The window keeps the provisional placement — which was derived
  from the size the exit *configured*, not the one the client kept, so it sits
  half that difference off the requested point — and stays out of snap/cluster
  reflow until it unmaps. `map_window_to_rule_point`'s own doc names the reflow
  half; nothing bounds either.
- **Zero-net-change resize strands `ResizeState::WaitingForLastCommit`.**
  Grab start sets `Resizing` in *pending* state only, so a resize that ends
  where it began leaves `send_pending_configure` with nothing to send and the
  client with no reason to commit (`src/grabs/resize_grab.rs`). Until its next
  repaint the window reads as under an interactive grab, which silently skips
  its next geometry animation and makes relaunch adoption bail. Self-clears.
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
- **`resize_on_border` decides whether the border resizes, not whether it is part
  of the window — an open decision, not a bug.** The option gates
  `decoration_hit_for` and `pinned_decoration_under` (`src/input/mod.rs`), which
  produce `DecorationHit::ResizeBorder` — *resize* behaviour. It does not gate
  `surface_under` or `pinned_window_under`, which decide *membership*: pointer
  focus, binding context, pick target. So with the option off the 8 px band is
  still the window's, it just can't be dragged. Whether "the border is not for
  resizing" should also mean "the border is not part of the window" is a
  user-facing call nobody has made. Two things constrain it. Making the band fall
  through is not a no-op: `pointer_focus_under` continues past `surface_under`
  into `canvas_layer_under`, widget windows and then `Bottom`/`Background`, so a
  wallpaper client would take an enter/leave pair every time the cursor crossed
  any window's ring, and that arm sets `pointer_over_layer`, which makes
  `maybe_hover_focus` return early — `focus_follows_mouse` would be *suppressed*
  in the ring rather than falling through to canvas. And `render/shaders.rs` draws
  the border `border_width_logical` *outside* the window rect, i.e. inside this
  band, so `[decorations] border_width > 0` plus a fall-through would give a
  visible border that is click- and hover-through. The split the current answer
  leaves is observable and pinned by
  `an_inert_resize_margin_binds_as_window_only_around_a_pin`
  (`src/tests/pinned_phantom.rs`): with the option off, the ring around a
  **pinned** window reports `OnWindow` for binding context while the ring around a
  **canvas** window reports `OnCanvas`, because the canvas path reads its margin
  through the gated decoration channel and the pinned path does not.
- **Input hit-tests the animation's destination, not the drawn rect — and that is
  by design.** Every stage read in the input path takes the destination:
  `topmost_under`'s `position_and_pinned`, `element_under_skipping`'s,
  `decoration_under`'s and `surface_under`'s `position_of` (`src/input/mod.rs`).
  The render path draws at `geometry_visual_rect`, which has no consumer anywhere
  under `src/input/`, so the two rects are disjoint for the whole animation — for
  a fit, by `old_size/2 - usable.size/2 + gap (+bar)`, which puts an 800×600
  window on a 1920×1080 usable area 548 px left and 204 px up of where it is
  drawn. Recorded here because it *looks* like a bug and has been filed as one
  twice.

  The settled model: a window animation is eye candy over a state change that is
  already complete. The window is at its destination the moment the action runs;
  the picture is catching up. So hit-testing the destination is correct, and so
  are the surface-local coordinates `surface_under` derives from it. A *camera*
  animation is the opposite — the viewport genuinely is moving, so input tracks
  it live, which is why the grab-install chokepoints below take the viewport out
  of flight rather than freezing input.

  The visible cost is that during an animation a window cannot be clicked where
  it is drawn: at default speeds ~120 ms, reading as a missed click; obvious once
  `[effects] animation_speed` is lowered — at 0.02 it lasts ~4 s. Going
  visual-aware would mean threading `geometry_visual_rect(id).loc` into those four
  reads (checking `geometry_space` first — that accessor's doc warns about Canvas
  vs Screen), and it carries a non-obvious tax: the idle pointer-focus re-poll has
  to be suppressed while any transition runs, or moving pixels drag enter/leave
  across a stationary cursor. Do not reopen without revisiting the model first.
- **A camera target armed *after* a resize grab installs still resizes the
  window.** `warp_pointer` (`src/state/viewport_animation.rs`) synthesizes real
  motion into a live grab to keep the pointer at a fixed *screen* position, so its
  *canvas* position moves, and `apply_resize` measures that delta against a fixed
  canvas anchor. The two grab-install chokepoints (`arm_interactive_move` in
  `src/state/mod.rs`, `begin_client_resize` in `src/state/resize.rs`) take the
  viewport out of flight, so a grab installed *during* a glide is safe — but that
  is a snapshot of the moment the grab took over, not an invariant. Anything
  arming `camera_target`/`zoom_target` while the grab is held resumes warping:
  keyboard and IPC navigation (`src/input/actions.rs`, `src/ipc/mod.rs`), a new
  window mapping (`src/handlers/compositor.rs`), an activation request
  (`src/handlers/xdg_shell.rs`), the output-removal warp
  (`src/state/output.rs`). The **move** counterpart of this is deliberate and
  hardware-confirmed: hold a window, then jump to a bookmark or home, and the
  window is carried along — that is the feature, not a bug, and it is why the
  chokepoints cancel only at install. Only the resize arm is wrong; a held border
  is not a handle on the window. The fix is to re-anchor `ResizeGrab`'s
  `start_data.location` by the camera delta, not to gate the producers.
- **`reflow_grown_snapped_window`'s stale-frame guard reads *unacked* configures,
  so an early-acking client goes unguarded.** The owed-resize bail
  (`src/handlers/compositor.rs`) scans `pending_configures()`, which empties the
  moment a client acks — and toolkits routinely ack before they redraw, so the
  stale frames that follow read as a grow past the settled footprint and get the
  window relocated beside a neighbour. Every defence against it so far is
  per-path: the fit/fill/fullscreen exits survive only because they leave a
  `pending_recenter` that gates the reflow, and the relaunch adopt because it owes
  its stable snap rect until the client commits the size it configured
  (`pending_adopt_settle`). Comparing committed geometry against the *last sent*
  configure instead would cover the class at once and let both retire; not taken
  where it was found because every window in the compositor rides that comparison.

## Structural backlog

The duplication this list used to track *is* extracted, and all three extractions
are worth recording so a new arm grows through them rather than beside them.

The *map to the restored location → equal-size branch or insert a
`PendingRecenter`* tail the fit, fill and fullscreen exits used to keep three
hand-maintained copies of is `DriftWm::establish_exit_placement`
(`src/state/recenter.rs`), called by `unfit_window` (`src/state/fit.rs`),
`unfill_window` (`src/state/fill.rs`) and `exit_fullscreen_on`
(`src/state/fullscreen.rs`). The copies drifted exactly as duplication does — the
fill one was missing the equal-size branch's `drop_owed_recenter` for months and
the fullscreen one never had it at all, which is the correctness item this
extraction closed.

What is deliberately *not* shared, so a fourth exit doesn't try to fold it in:

- **The animate → configure order.** Fit and fill seed the geometry animation
  *before* their configure; the fullscreen exit animates last, after the
  configure, the map, the re-pin and the camera restore, from a screen-space seed
  captured before any of it (and in `AnimSpace::Screen` when the window is
  re-pinned). Pinned by `unfill_animates_straight_to_the_restored_rect` and the
  frozen/handover fullscreen-exit family in `src/tests/window_animation.rs`.
- **`target_center` is a parameter, not derived from the mapped location.**
  `unfit_window` maps to a location `frame_loc_for_center` already truncated out
  of its center and records the un-truncated one; re-deriving costs up to a pixel
  per axis (`unfit_settles_on_the_untruncated_center`).
- **`refresh_snap_rect` is true for fit and fill, false for fullscreen.**
  `fit_window_snapped` and `fill_window` cache a rect of their own that the exit
  invalidates; `enter_fullscreen` caches none, so the cached rect is still the
  pre-fullscreen one the exit hands back. Both values are pinned
  (`unfit_refreshes_the_snap_rect_its_fit_cached`,
  `fullscreen_exit_leaves_the_cached_snap_rect_alone`).

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
(`src/state/recenter.rs`), called by **seven** arms (`input/actions.rs` twice,
`state/fullscreen.rs`, `state/fill.rs`, `state/fit.rs`, `state/suspended.rs`,
and `establish_exit_placement`'s own equal-size branch). It was nine before the
exit tails collapsed their two equal-size copies into the one call
`establish_exit_placement` now makes for all three exits, and eight before the
bookmark move stopped dropping what it can re-aim instead.

The measurement that motivated the extraction: against niri (93,951 Rust lines to
driftwm's 98,156, but 81,832 non-test to driftwm's 63,671 — driftwm carries ~29%
less production code and a 0.54 test ratio to niri's 0.15), size was never the
problem. Both sides of that comparison count dedicated test files only; driftwm
also carries 11,897 lines of inline `#[cfg(test)]` modules, putting its true
non-test total at 51,774 and its true test ratio nearer 0.90.
