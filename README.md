# crabture

Standalone Linux screenshot tool written in Rust. No external dependencies required — works on both X11 and Wayland.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![CI](https://github.com/piny4man/crabture/actions/workflows/ci.yml/badge.svg)](https://github.com/piny4man/crabture/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/crabture.svg)](https://crates.io/crates/crabture)

## Features

- **Full-screen capture** — instant screenshot of primary or any monitor
- **Region capture** — specify exact coordinates (`x,y,WxH`)
- **Interactive area selection** — click and drag with visual overlay feedback
- **Interactive TUI** — guided menu for timing, target, and save method
- **Multi-monitor support** — list and capture any connected monitor
- **Clipboard support** — copy screenshots directly to the clipboard
- **Delayed capture** — configurable countdown (5/10/20/30/60 seconds)
- **Desktop notifications** — status feedback via system notifications
- **Multiple formats** — save as PNG or JPG
- **XDG-compliant** — respects `XDG_SCREENSHOTS_DIR`, falls back to `~/Pictures`

## Installation

### From crates.io

```sh
cargo install crabture
```

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
yay -S crabture-git
```

### System dependencies

On Debian/Ubuntu you may need:

```sh
sudo apt-get install -y \
  libwayland-dev libpipewire-0.3-dev libegl-dev \
  libgbm-dev libdrm-dev libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev
```

## Usage

```sh
# Interactive TUI (default)
crabture

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

## License

MIT
