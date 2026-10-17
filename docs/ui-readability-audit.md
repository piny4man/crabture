# HUD readability audit (NUKE-103)

The NUKE-76 real-machine validation said: "crabture need better ui, sometimes
is hard to read or understand". This audit covers every text role, icon and
interaction state of the graphical HUD (toolbar + capture overlay). For each
one it gives the source, the measured contrast for both palettes, the problem
found, and its status.

Palettes measured:

- **Built-in**: the default dark toolbar, panel `#202024`.
- **Swatches**: the sample theme used by the tests, with background `#10253F`,
  foreground `#EAF3FF`, muted `#A4B8CF`, accent `#80D4FF` and selection
  `#244A70`/`#FFFFFF`.

Contrast is the WCAG 2 ratio of the blended text colour against the opaque
panel colour. All HUD text is under 18 px, so the target is 4.5:1 (AA, normal
text).

## Text roles

| Role | Source | Built-in | Swatches | Finding | Status |
| --- | --- | --- | --- | --- | --- |
| Mode label (Area/Window/Screen), `LABEL_PX` 14 | `src/render.rs:33`, `src/render.rs:675` | 14.0:1 | 12.0:1 | Readable. "Full" did not say *what* is full. | **Fixed**: renamed to "Screen" (`src/render.rs:208`). |
| Chip category (Output/Format/Location), `CAT_PX` 12.5, muted | `src/render.rs:34`, `src/render.rs:727`; alpha `src/appearance.rs:25` | 5.6:1 | **3.6:1** | The theme's muted role at app alpha 150 fails AA, which is the main "hard to read" cause on themed setups. | **Fixed**: `text_alpha` raises muted/foreground alpha per theme until 4.5:1 (`src/appearance.rs:379`). Swatches muted now uses alpha 180 (4.5:1). Built-in stays at 150. |
| Chip value (Copy/PNG/Screenshots), `VAL_PX` 13.5 | `src/render.rs:35`, `src/render.rs:118`, `src/render.rs:126` | 14.0:1 | 12.0:1 | "Current dir" is jargon, and it is unclear which dir. | **Fixed**: now "This folder". The README explains it is the launch directory. |
| Shortcut letter (A/W/F/O/P/L), `SHORT_PX` 10.5, muted | `src/render.rs:36`, `src/render.rs:751` | 5.6:1 | 3.6:1 → 4.5:1 | Same contrast fix as the categories. At 10.5 px with no keycap, the letters read as decoration rather than as keys. `P` does not suggest "Format". | Contrast **fixed**. Keycap styling and the `P` mnemonic are **follow-ups**. |
| Action hint `esc` (Cancel), muted | `src/render.rs:779` | 5.6:1 | 3.6:1 → 4.5:1 | Same as the shortcut letters. | **Fixed** (contrast). |
| Action hint ⏎ (Capture) on accent | `src/render.rs:777` | 2.8:1 (white@200 on `#0A84FF`) | 8.6:1 | The bundled Roboto has **no U+23CE glyph** (`bundled_font_has_no_return_symbol_glyph`), so the hint rendered as an empty box or nothing. The built-in hint alpha is also low on the blue accent. | Missing glyph **fixed**: drawn as a vector ↵ (`draw_return_arrow`, `src/render.rs:408`). This works with any font, so no text fallback is needed. Built-in hint contrast is a **follow-up**. |
| Selected mode label (selection pair) | `src/render.rs:671` | **3.65:1** (white on `#0A84FF`) | 9.2:1 | The built-in selection blue with white text is below AA. That pair is the built-in palette and `selection_*` is theme-owned. | **Follow-up**: a built-in palette change needs its own decision (the contrast enforcement only adjusts app-owned alpha). |
| Capture label (on-accent) | `src/render.rs:777`; `contrasting_rgb` in `src/appearance.rs` | 3.65:1 | 12.8:1 | Fixed for themes in NUKE-76. The built-in palette has the same white-on-blue issue as the selected label. | **Follow-up** (same as above). |

## Icons and cues

| Item | Source | Finding | Status |
| --- | --- | --- | --- |
| Mode icons (`assets/area.svg`, `window.svg`, `fullscreen.svg`), `ICON` 18 | `src/render.rs:31`, `src/render.rs:678` | Clear shapes. Inactive icons are drawn at 0.82 opacity (11.3:1 built-in, 9.7:1 Swatches), which is still readable. | No change. The 0.82 dimming is listed as a follow-up only for consistency review. |
| Chip cue ▾ | was `fill_caret` | A downward caret promises a dropdown, but clicking **cycles** to the next value. It also ignored `scale`, so it was half-size at 2x. | **Fixed**: replaced with a stroked right chevron › that scales with the buffer (`draw_cycle_cue`, `src/render.rs:388`), in the same `CUE_W` footprint (`src/render.rs:39`). |
| Separators | `src/render.rs:597`; alpha `src/appearance.rs:23` | Decorative (alpha 28). Group boundaries are still visible on both palettes. | No change. |

## Interaction states

| State | Source | Built-in | Swatches | Finding | Status |
| --- | --- | --- | --- | --- | --- |
| Hover wash | `src/render.rs:651`; `src/appearance.rs:26`, `src/appearance.rs:397` | 1.29 → 1.55 | **1.04** → 1.51 | Themed hover used `selection_background` at alpha 22 over the panel, a change of about 7 levels, so it was effectively invisible. | **Fixed**: hover uses the foreground at alpha 36 for every palette (`themed_hover_wash_is_visibly_distinct_from_the_panel`). |
| Selected mode | `src/render.rs:647` | Filled with the selection colour | Same | Clearly distinct from hover and inactive. | No change (see the selected-label contrast follow-up). |
| Active / Capture accent pill | `src/render.rs:777` | Accent fill | Same | Clear primary action. | No change. |

## Capture overlay

| Item | Source | Finding | Status |
| --- | --- | --- | --- |
| Dim (`OVERLAY_PIXEL` `0x99000000`) | `src/appearance.rs:31` | A fixed 60 % black. It reads well on light desktops but heavy on already-dark ones. | **Follow-up** (app-owned; changing it is a visual decision, not a fix). |
| Area selection border | `src/overlay.rs:1428` | Was 1 **physical** px, so 0.5 logical px at 2x. Easy to lose on HiDPI. | **Fixed**: 1 logical px (`thickness = scale`) (`selection_border_is_one_logical_pixel_at_every_scale`). |
| Window crosshair | `src/overlay.rs:1481`, `src/scene.rs:102` | Same 1 physical px problem. | **Fixed**: `Crosshair::thickness` = scale (`window_target_lines_are_one_logical_pixel_thick`). |
| Window highlight | `src/overlay.rs:1456` | Already scaled at 2 logical px. | No change. |
| Guidance | — | There is no "drag to select" hint and no size readout while dragging. | **Follow-up** (feature). |

## Comprehension follow-ups (out of scope: layout or feature)

1. Keycap styling for shortcut letters, so they read as keys.
2. A Format shortcut mnemonic (`P` → format), or showing the key next to the value.
3. Hiding or disabling Location when Output is Copy (it has no effect then).
4. A "drag to select" hint and a W×H size readout on the area overlay.
5. Contrast of the built-in white-on-`#0A84FF` selected/accent labels (3.65:1) and of the Capture hint (2.8:1).
6. An adaptive overlay dim, and a consistency review of the 0.82 inactive icon opacity.

## Snapshots

`cargo test -- --ignored write_ui_snapshots` writes toolbar PNGs to
`target/ui-snapshots/` (override with `CRABTURE_SNAPSHOT_DIR`). It covers:

- palette: `builtin`, `swatches`
- scale: `1x`, `2x`
- state: `area`, `window`, `screen`, `save`, `copy-save`, `hover-window`

Files are named `<palette>-<scale>-<state>.png`. "Before" snapshots were
produced from `main` (0.4.0) with the same harness, for comparison in the PR.

## Real-screen checklist

Offscreen snapshots do not show compositor scaling, blur or the real desktop.
On a real machine, take `grim` shots of:

- [ ] the toolbar at 1x, with the built-in palette and with a Swatches theme
- [ ] the toolbar on a scaled output (1.5x or 2x)
- [ ] an area selection being dragged (border thickness on HiDPI)
- [ ] window mode: the crosshair and the hovered window highlight
- [ ] hovering an inactive mode button with a Swatches theme

Attach them to NUKE-103 or the PR.
