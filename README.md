# crabture

Standalone Linux screenshot tool written in Rust. No external dependencies required — works on both X11 and Wayland.

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![CI](https://github.com/piny4man/crabture/actions/workflows/ci.yml/badge.svg)](https://github.com/piny4man/crabture/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/crabture.svg)](https://crates.io/crates/crabture)

## Features

- **Graphical screenshot UI** — default overlay with area, window, and full-screen modes
- **Full-screen capture** — instant screenshot of primary or any monitor
- **Region capture** — specify exact coordinates (`x,y,WxH`)
- **Interactive area selection** — click and drag with visual overlay feedback
- **Interactive TUI** — optional guided menu for timing, target, and save method
- **Multi-monitor support** — list and capture any connected monitor
- **Clipboard support** — copy screenshots directly to the clipboard
- **Delayed capture** — configurable countdown (5/10/20/30/60 seconds)
- **Desktop notifications** — status feedback via system notifications
- **Multiple formats** — save as PNG or JPG
- **XDG-compliant** — respects `XDG_SCREENSHOTS_DIR`, falls back to `~/Pictures`

## What You Can Do

Crabture can capture screenshots in three main ways:

- Open the graphical overlay and choose an area, window, or full screen target.
- Run direct commands for scripts, keybindings, launchers, and automation.
- Use the legacy terminal menu if you prefer a step-by-step TUI.

By default, graphical captures copy the image to your clipboard. You can switch the graphical UI to save files or copy plus save, and those choices are remembered between runs.

## Installation

### From crates.io

```sh
cargo install crabture --locked
```

`--locked` installs against the dependency versions tested in `Cargo.lock`. It is
recommended because `cargo install` otherwise re-resolves dependencies to their
newest versions, which can pull in a release that fails to build.

### From source

```sh
git clone https://github.com/piny4man/crabture.git
cd crabture
cargo build --release
# binary at target/release/crabture
```

### Arch Linux (AUR)

```sh
# Using an AUR helper
paru -S crabture-git
```

### Build dependencies

These are only needed when **building from source**. Prebuilt binaries from the
[releases page](https://github.com/piny4man/crabture/releases) have no extra
runtime dependencies.

#### Debian / Ubuntu

```sh
sudo apt-get install -y \
  libclang-dev libwayland-dev libpipewire-0.3-dev libegl-dev \
  libgbm-dev libdrm-dev libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev
```

#### Arch Linux

```sh
sudo pacman -S clang libxcb wayland pipewire
```

## Usage

Most users should start here:

```sh
crabture
```

That opens the graphical overlay. Choose `AREA`, `WINDOW`, or `FULL`, then capture the target. Use `Escape`, right-click, or `CANCEL` to leave without changing files or the clipboard.

### Common Tasks

| Want to... | Run |
| --- | --- |
| Open the graphical screenshot UI | `crabture` |
| Capture the full primary screen immediately | `crabture --instant` |
| Capture monitor 2 immediately | `crabture --monitor 2` |
| Draw a fast area selection directly | `crabture --select` |
| Capture exact coordinates | `crabture --region 100,200,800x600` |
| Copy instead of saving from a direct command | `crabture --instant --copy` |
| Save JPG files | `crabture --instant --format jpg` |
| Save direct captures to a directory | `crabture --instant ~/screenshots` |
| List monitor names, sizes, and scale | `crabture --list-monitors` |
| Open the legacy terminal menu | `crabture --menu` |

### Command Examples

```sh
# Graphical screenshot UI (default)
crabture

# Legacy interactive TUI
crabture --menu

# Instant full-screen screenshot
crabture --instant

# Capture a specific monitor (1-based index)
crabture --monitor 2

# Interactive area selection
crabture --select

# Capture a region by coordinates
crabture --region 100,200,800x600

# Copy to clipboard instead of saving
crabture --instant --copy

# Save as JPG to a specific directory
crabture --instant --format jpg ~/screenshots

# List available monitors
crabture --list-monitors
```

Running `crabture` with no arguments opens the graphical overlay. The graphical UI copies captures to the clipboard by default, remembers output preferences, and supports area, window, and full-screen capture. Direct commands such as `--instant`, `--monitor`, `--select`, and `--region` remain available for scripts and automation.

Direct capture commands save files by default. Add `--copy` to copy instead. If you pass a directory path, direct captures save there; otherwise crabture uses `XDG_SCREENSHOTS_DIR` or falls back to `~/Pictures`.

## Graphical Controls

- `AREA`: Draw a rectangle, edit it, then capture the final selection.
- `WINDOW`: Target a window where the platform supports it. If true window capture is unavailable, use area or full-screen capture.
- `FULL`: Capture the current display immediately.
- `COPY`, `SAVE`, `COPY+SAVE`: Choose whether graphical captures copy, save, or do both.
- `SCREENSHOTS`, `CURRENT DIR`: Choose where saved graphical captures are written.
- `PNG`, `JPG`: Choose the saved image format.
- `A`: Area mode. Drag to draw a selection, adjust it, then press `Enter` or click `CAPTURE`.
- `W`: Window mode. Move over or click a window target; click captures the selected target.
- `Space`: Switches from area mode to window mode.
- `F`: Full-screen mode. Captures the current display immediately.
- `O`: Cycle output between clipboard, save, and copy plus save.
- `L`: Cycle save location between screenshots directory and current directory.
- `P`: Toggle PNG/JPG format.
- `Enter`: Confirm the current mode's target.
- `Escape`, right-click, or `CANCEL`: Exit without writing files or changing the clipboard.

## Platform Notes

- Wayland wlroots compositors use `libwayshot-xcap` for physical-resolution monitor capture and overlay exclusion. Hyprland window capture uses compositor window metadata when available.
- X11 and non-wlroots environments fall back through `xcap` where available. If true window capture is unavailable, use area or full-screen capture instead.
- Fractional scaling is handled by mapping logical overlay coordinates to physical screenshots; verify scaled and multi-monitor setups with `--list-monitors` if targeting looks unexpected.
- Clipboard output is preserved after the UI exits by a short-lived background clipboard owner process.

## Manual Verification Notes

- Wayland: verify area, window, and full-screen capture on the active compositor; confirm the overlay/HUD is absent from saved and copied images.
- X11: verify direct capture commands and graphical fallbacks; confirm unsupported window targeting errors suggest area or full-screen capture.
- Scaling: verify area crops on 1x, integer-scaled, and fractional-scaled displays.
- Multi-monitor: verify `FULL`/`F` captures the intended output and `--list-monitors` reports usable monitor identities.
- Clipboard: verify copied images remain pasteable after the main `crabture` process exits.

## Architecture Notes

- Capture intent flows through session commands for area, window, and full-screen targets before output handling, keeping capture selection separate from clipboard/file delivery.
- Preferences are version-tolerant key/value data with safe defaults so stale configuration cannot prevent launch.
- Markup and screen recording are intentionally future work. They should extend the session command/capture-result model rather than being coupled to the current still-image overlay paths.

## License

GPL-3.0 — see [LICENSE](LICENSE) for details.
