# Hotpath findings

Baseline for [hotpath profiling](hotpath-profiling.md), measured on
`feat/hotpath-profiling` before any fix. Fixes land in follow-up PRs; each
finding below names what to change and the evidence for it.

## Setup

- Piny4Evo on AC, Hyprland 0.56.2 with `wp_presentation` v2; one output,
  eDP-1 2880×1800 at scale 1.5 (1920×1200 logical, 120 Hz, 8.3 ms per
  refresh); the shared Forge theme, which sets a font family.
- Open: `crabture --measure`, release build, one throwaway open, then
  three interleaved rounds of 10 cold (2 s idle) and 10 warm opens per power
  profile, so n = 30 per cell. Three `hotpath-alloc` reports per profile.
- Feedback: guided manual HUD and `--select` sessions per profile, interleaved,
  with at least 30 samples per metric. `hyprctl dispatch movecursor` produces no
  `wl_pointer.motion` on the overlay (checked with `WAYLAND_DEBUG`), so hover
  and window highlight could not be scripted and were done by hand too.
- Targets (NUKE-109): p95 < 50 ms for open and feedback; balanced ≤ 20 % slower
  than performance, reported next to the absolute bound balanced p95 ≤
  performance p95 + 10 ms.

## Results

### Open (`first-content-present`, process start → first presented HUD frame)

| | performance median / p95 | balanced median / p95 | balanced vs performance (p95) |
| --- | --- | --- | --- |
| cold | 85.4 / 102.0 ms | 91.9 / 100.9 ms | −1 % / −1.1 ms |
| warm | 85.8 / 94.2 ms | 93.1 / 102.2 ms | +8 % / +8.0 ms |

With one output `first-present` equals `first-content-present`. The legacy
`--select` overlay opened in 78–79 ms (n = 1 per profile, from the drag
sessions).

**Open misses the 50 ms target by about 2×.** Balanced is within both bounds.

### Feedback (`*-input-to-present`)

| Metric | performance n / median / p95 | balanced n / median / p95 | balanced vs performance (p95) |
| --- | --- | --- | --- |
| area drag | 222 / 20.9 / 21.3 ms | 198 / 20.8 / 21.4 ms | +0 % / +0.1 ms |
| `--select` drag | 315 / 19.6 / 22.0 ms | 388 / 19.1 / 22.3 ms | +1 % / +0.3 ms |
| window highlight | 64 / 20.0 / 23.0 ms | 141 / 19.7 / 22.9 ms | −0 % / −0.1 ms |
| HUD hover | 75 / 18.1 / 21.6 ms | 86 / 20.5 / 25.7 ms | +19 % / +4.1 ms |
| HUD press | 36 / 14.9 / 21.6 ms | 36 / 15.8 / 20.9 ms | −3 % / −0.6 ms |
| key | 68 / 15.0 / 19.0 ms | 33 / 16.5 / 20.2 ms | +7 % / +1.3 ms |

No `*-input-unanswered` samples. **Every feedback metric meets the 50 ms
target, and balanced is within both bounds** (hover is the closest, at +19 %).

### Where the open goes (hotpath, median of 3 reports per profile)

| Span | performance | balanced | Notes |
| --- | --- | --- | --- |
| whole HUD run (`crabture::hud`) | 96.6 ms | 101.9 ms | |
| `run_screenshot_hud` | 43.4 ms | 39.1 ms | connect 2.0, roundtrip 1.7, first `draw` 20.0, the rest is waiting for configure and presentation |
| `capture_all_outputs` | 29.7 ms | 35.9 ms | 19.9 MB allocated: the freeze screencopy |
| `OutputOverlay::draw` / `draw_hud` | 20.0 ms | 19.4 ms | one full-resolution frame; `render_toolbar` 1.7–1.9 ms, `blit_argb` 0.7 ms of it |
| `load_runtime_appearance` | 10.5 ms | 11.5 ms | `fc_match_family` 9.4 / 10.9 ms, `font_from_path` 1.1–1.4 ms (2.4 MB) |
| `enumerate_window_rects` | 7.6 ms | 8.8 ms | spawns `hyprctl -j clients` |

All of these run one after another on the main thread.

## Findings, ranked

1. **Every HUD frame is a full physical-resolution repaint (~20 ms)** (NUKE-116).
   `OutputOverlay::draw` fills all 2880×1800 pixels, redraws the selection or
   highlight, and blits the toolbar on every frame. That frame happens on the
   open and on every pointer motion that changes the frame. At 120 Hz the
   frame takes about 2.5 refreshes, which matches the ~20 ms feedback
   medians: drags and hover update at roughly 45 fps, not 120. It is also
   about 20 ms of the open. *Fix:* repaint only what changed. Damage the old
   and new selection, highlight, and toolbar rectangles; keep the tinted
   background in a reused buffer instead of refilling it; and double-buffer
   the `SlotPool` so a frame does not wait for the compositor to release the
   previous buffer. *Expected:* draw well under one refresh (< 8 ms), feedback
   close to one or two refreshes, and about 15 ms off the open.

   *Result (NUKE-116):* `src/scene.rs` describes each frame as a `Scene`;
   `scene::damage` lists the rectangles that differ from what a buffer
   shows, and `scene::paint` repaints only those. Each overlay keeps up to
   three persistent buffers that remember their scene, and the surface is
   damaged per rectangle. In one manual session per side (balanced profile,
   same gestures), `OutputOverlay::draw` dropped from p50 5.26 ms / p95
   6.22 ms (920 frames) to p50 0.25 ms / p95 0.89 ms (760 frames). Only
   toolbar re-rasterizing (`render_toolbar`, ~3.4 ms on hover changes) still
   costs whole milliseconds. `area-drag-input-to-present` p95 went from 29.1
   to 25.0 ms. The median stays near 21 ms, which suggests the remaining
   feedback latency is the compositor's frame scheduling, not the repaint.

2. **The open is fully serial: capture, window list, appearance, then
   Wayland** (NUKE-117). `capture_all_outputs` (~30 ms) must finish before the overlay
   is mapped, so the toolbar never appears in a capture. Nothing else has to
   wait for it, though: `enumerate_window_rects` (~8 ms), appearance and font
   loading (~10.5 ms), the Wayland connect and roundtrip (~4 ms), and the
   toolbar layout and rasterisation could all run alongside it. *Fix:* start
   the capture first and do the rest on another thread (or overlap Wayland
   setup), committing the first overlay buffer only after the capture
   returns. *Expected:* the open approaches max(capture, setup) + first
   draw + present, roughly 20 ms off it.

   *Result (NUKE-117):* the capture and `enumerate_window_rects` run on
   background threads while the main thread loads the appearance, connects,
   configures the surfaces and paints the first frame. `src/open.rs`'s
   `OpenGate` holds the first commit until the capture returns: attaching a
   buffer takes a `CaptureDone` proof only `OpenGate::wait` hands out, and a
   failed capture closes the HUD unshown. Interleaved A/B against `main`
   (already with NUKE-116), 30 cold + 30 warm `--measure` opens per cell, on
   battery (so absolute numbers sit above the AC baseline), median / p95:

   | | `main` | branch | p95 drop |
   |---|---|---|---|
   | performance cold | 85.4 / 93.9 ms | 59.8 / 61.0 ms | 32.9 ms |
   | performance warm | 85.7 / 94.1 ms | 60.3 / 60.9 ms | 33.2 ms |
   | balanced cold | 147.1 / 151.1 ms | 97.1 / 104.5 ms | 46.6 ms |
   | balanced warm | 140.2 / 149.1 ms | 98.4 / 99.5 ms | 49.6 ms |

   The window list is still read by spawning `hyprctl -j clients`; it is now
   off the critical path, so the optional IPC-socket read was left out.

3. **`fc-match` is spawned on every open (9–11 ms)** (NUKE-118). The shared theme always
   sets a font family, so `load_font` resolves it through
   `fc-match -f %{file}` on every open, even though the answer only changes
   when fonts are installed or removed. *Fix:* cache the resolved path
   (keyed by the family, and invalidated when the fontconfig cache changes),
   or use the configured `[font] path`. Hyprburst does the same in
   `$XDG_CACHE_HOME/hyprburst/fonts`. *Expected:* about 9 ms off the open
   whenever a theme or family is set.

4. **Window enumeration spawns `hyprctl` (~8 ms)** (optional part of NUKE-117). It is covered by
   finding 2 if run concurrently. Otherwise, reading the window list from
   Hyprland's IPC socket directly (`$XDG_RUNTIME_DIR/hypr/<sig>/.socket.sock`,
   `j/clients`) avoids the process spawn.

Together, findings 1–3 are estimated to bring the open from ~100 ms p95 to
about 50 ms. The freeze capture and one present wait (~8 ms at 120 Hz)
remain the floor. These are projections from the spans above, not
measurements. Each fix PR should re-run the cold/warm `--measure` batches
and the feedback sessions.

## Probe notes

- Present time is when the `wp_presentation_feedback.presented` event is
  dispatched, not the compositor's timestamp in it, which is in a different
  clock. Feedback samples therefore include the client's event-dispatch
  latency, which is what the user sees anyway.
- A click or key within 50 ms of a pending hover folds into that hover sample.
  This is the probe's burst rule, and it is why one session with clicks right
  after hovering recorded few `hud-press` samples. Measure presses with the
  pointer at rest.
- The open is measured from the first statement of `main`, so process exec and
  dynamic linking (a few ms) come on top of it.
