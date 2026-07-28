# Performance — remaining work

The B1–B14 perf push shipped (see `git log`). What's left, in priority order.
Line numbers predate the push — re-verify on pickup. Profiling tooling:
[profiling.md](profiling.md).

Non-perf items live at the bottom under
[Correctness backlog](#correctness-backlog).

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
- **B12** Output-outline strips rebuild pixel Vecs + `MemoryRenderBuffer` + fresh
  element ids per edge per frame (`src/render/mod.rs`), defeating damage tracking.
  _Multi-monitor only._ Cache per (output, color, size).
- **B13 / B15** Held repeatable key (`src/backend/udev.rs`) and the exec loading
  cursor (`src/input/actions.rs`, up to 5 s/launch) mark _all_ outputs dirty at
  refresh rate. Mark only the active/cursor output. _Single-output-marginal — same
  shape as the skipped B1; likely not worth it._
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

Open bugs, not perf work. Behaviour that reads like a bug but is settled — what
input hit-tests, what a configured size means, the inert resize band — lives in
[caveats.md](caveats.md).

- **Adopting a stand-in reads the stage position, not the in-flight visual.**
  `adopt_relaunched` takes `stage.position_of` — the destination — so adopting a
  stand-in that a neighbouring cluster shift pushed within the last few hundred ms
  teleports the departing chrome to the end of the slide in one frame. The dismiss
  half of this is fixed; adopt is not, because seeding only the fade leaves the
  two crossfade halves offset for its whole life (worse than the pop), and seeding
  the incoming window too means converting a deliberate hold — `from == target`,
  which holds the slot until the client acks — into a finite leg in exactly the
  window that hold exists to cover, plus reassembling the CSD bar offset by hand.
  Cosmetic, narrow, and not the one-liner it looks like.
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
