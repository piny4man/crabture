# Profiling with hotpath

[hotpath](https://hotpath.rs/) is an opt-in profiler: built with its cargo
features, crabture reports per-function timing, allocations, and per-thread
CPU and memory when the overlay exits. Without the features every
`#[hotpath::measure]`, `measure_block!`, and profiling guard compiles to the
plain code and none of hotpath's runtime dependencies are built (the default
tree gains only the no-op `hotpath` and `hotpath-macros` crates).

It complements `CRABTURE_PERF`, the end-to-end latency probe below:
`CRABTURE_PERF` measures what the user waits for (process start to the first
presented HUD frame, an input to the frame that shows it), hotpath explains
where the time and memory inside those spans go.

Measured findings live in [Hotpath findings](hotpath-findings.md).

## Features

| Feature | Report |
| --- | --- |
| `hotpath` | function timing and threads |
| `hotpath-alloc` | all of the above plus allocated bytes per function (`HOTPATH_ALLOC_METRIC=count` for allocation counts) |

The report is written when the overlay closes normally (`Escape`, a capture,
or `--measure` finishing); a killed process writes none. It goes to stdout
unless redirected with `HOTPATH_OUTPUT_FORMAT=json` and
`HOTPATH_OUTPUT_PATH=<file>`.

Only overlay runs are profiled: the guards sit on the graphical HUD (default,
`--measure`) and `--select` entry points in `main.rs`, after the clipboard
daemon and `--list-monitors` branches, so those print no report.

Build the profiling binary into its own target directory so it does not
replace the normal one:

```sh
cargo build --release --features hotpath-alloc --target-dir target/hotpath
```

## `--measure`

`crabture --measure` (hidden from `--help`) runs the full graphical open path —
output capture for the freeze, window enumeration, appearance and font
loading, layer-shell setup, first draw — then cancels the HUD once its first
frame with content has been presented. It never saves, copies, or notifies,
and exits 0. Combined with capture flags it still only measures.

## `CRABTURE_PERF`

With `CRABTURE_PERF=1` (any value but empty or `0`) the overlay prints one
stderr line per sample, `crabture-perf <metric> <surface> <ms>`, where
`<surface>` is `hud` (graphical UI) or `select` (legacy `--select`):

| Metric | From | To |
| --- | --- | --- |
| `first-present` | process start (`main`) | the first presented overlay frame on any output |
| `first-content-present` | process start (`main`) | the first presented frame on the output that draws the toolbar (`hud`), or any frame (`select`) |
| `area-drag-input-to-present` | pointer motion that draws, moves, or resizes the HUD selection | the first presented frame with a visible change |
| `select-drag-input-to-present` | pointer motion that draws the `--select` selection | as above |
| `window-highlight-input-to-present` | pointer motion that changes the window-mode highlight or crosshair | as above |
| `hud-hover-input-to-present` | pointer motion onto or off a toolbar button | as above |
| `hud-press-input-to-present` | a toolbar click that keeps the HUD open (mode, output, location, format) | as above |
| `key-input-to-present` | a shortcut that keeps the HUD open (`Space`, mode and option keys) | as above |
| `<kind>-input-unanswered` | an input no visible change answered | the next input, more than 50 ms later |

Present time comes from `wp_presentation` feedback (stamped when the
`presented` event arrives, since its own timestamp is in the compositor's
clock); without that global the frame-done callback stands in. A frame the
compositor discards for a newer commit drops its input sample, and during the
open a fresh frame is requested so `--measure` cannot stall. The capture
freeze is part of the open span: the HUD can only appear after
`capture_all_outputs` returns.

An input is answered only by a frame whose visible content differs from the
last committed one, judged by a per-output frame key (surface size and scale,
HUD preferences, the drawn selection, the highlighted window or crosshair, the
hovered button on the toolbar output); pixels are not hashed. Pointer motion
counts as an input only when it changes that key, so moving inside one button
or one window is not sampled. Inputs within 50 ms of a pending one fold into
it, so a drag is measured from its first motion; an input that comes later
replaces the pending one, which is printed as `*-input-unanswered` with its
age. With neither the probe nor `--measure` on, the overlay keeps no probe
state and binds no presentation global.

`scripts/perf-summary.sh` turns one or more logs into n / median / p95 / max
per metric and surface; other stderr lines are ignored.

## Running

### Open: cold and warm

Each open is a fresh process. Without root the page cache cannot be dropped,
so both cases run with the binary, fonts, and theme cached; do one throwaway
open before a run.

- **cold**: the first open after at least 2 s idle, so the CPU has clocked
  down — what pressing the screenshot key costs;
- **warm**: an open started immediately after the previous one exits.

```sh
bin=target/hotpath/release/crabture   # or target/release/crabture
$bin --measure >/dev/null 2>&1        # throwaway
for i in $(seq 30); do sleep 2; CRABTURE_PERF=1 $bin --measure >/dev/null 2>>/tmp/ct-cold.log; done
for i in $(seq 30); do CRABTURE_PERF=1 $bin --measure >/dev/null 2>>/tmp/ct-warm.log; done
scripts/perf-summary.sh /tmp/ct-cold.log; scripts/perf-summary.sh /tmp/ct-warm.log
```

For power-profile comparisons interleave the profiles (one batch each,
alternating) rather than running all of one first, and restore the daily
profile afterwards (`powerprofilesctl get`).

### Feedback: live HUD

```sh
CRABTURE_PERF=1 HOTPATH_OUTPUT_FORMAT=json HOTPATH_OUTPUT_PATH=/tmp/ct-hotpath.json \
  target/hotpath/release/crabture 2>>/tmp/ct-perf.log
CRABTURE_PERF=1 target/release/crabture --select 2>>/tmp/ct-perf.log >/dev/null
scripts/perf-summary.sh /tmp/ct-perf.log
```

Feedback is sampled by hand: Hyprland's `hyprctl dispatch movecursor` warps the
pointer without sending `wl_pointer.motion` to the overlay, so it produces no
samples. In one HUD session hover across the toolbar buttons, drag, move, and
resize area selections, click the mode/option buttons, press `O`, `L`, `P`, and
`Space`, then move between windows in window mode; close with `Escape`, which
writes the report and, unlike finishing a selection, saves no capture. For
`--select`, drag and press `Escape` or right-click before releasing, so
nothing is saved. Option keys and buttons change the saved preferences, so
back up `~/.config/crabture/preferences` first. Repeat until each metric has
at least 30 samples. Repeat until each metric has at least 30 samples.

## What is instrumented

- Open: `capture_all_outputs` (screencopy freeze), `capture_fullscreen`,
  `enumerate_window_rects`, `load_graphical_preferences`,
  `load_runtime_appearance` → `appearance::{load_from_default_path,
  load_from_path, load_optional_theme, load_font, font_from_path,
  fc_match_family, bundled_font}`, `run_screenshot_hud` /
  `run_selection_overlay` with `hud::connect`, `hud::roundtrip`
  (`select::*` for `--select`), `OverlayState::create_overlays`, and the
  `SlotPool::new (surface|configure|draw)` allocations.
- Draw: `OutputOverlay::{draw, hud_scene}`, `scene::{damage, paint}`,
  `render::{toolbar_layout, render_toolbar,
  render_button, draw_text, rasterize_icon, blit_argb}`.
- Selection: `move_selection_rect`, `resize_selection_rect`, and the capture
  crops `selected_area_image`, `selected_window_image`,
  `selected_full_screen_image`.

The `hotpath_profile` unit test in `src/overlay.rs` (run with
`--features hotpath`, and in CI) drives the offscreen phases — appearance and
font fallback, toolbar layout and rasterisation, canvas drawing, selection
math, the area crop — and fails if their instrumentation is lost. The
Wayland-bound spans are checked by a live `--measure` run.
