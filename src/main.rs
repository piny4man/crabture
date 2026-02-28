use anyhow::{Context, Result, bail};
use arboard::{Clipboard, ImageData};
use clap::Parser;
use dialoguer::{Input, Select, theme::ColorfulTheme};
use notify_rust::Notification;
use std::{
    borrow::Cow,
    env, fs,
    path::{Path, PathBuf},
    process,
    thread::sleep,
    time::Duration,
};
use time::OffsetDateTime;
use xcap::Monitor;

mod overlay;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Standalone screenshot tool — no external dependencies required"
)]
struct Cli {
    /// Take immediate full-screen shot (no UI)
    #[arg(long)]
    instant: bool,

    /// Capture a specific monitor by index (1-based)
    #[arg(long)]
    monitor: Option<usize>,

    /// Interactive area selection with cursor
    #[arg(long)]
    select: bool,

    /// Capture a region by coordinates: x,y,WxH (e.g. 100,200,800x600)
    #[arg(long)]
    region: Option<String>,

    /// Screenshot directory (default: XDG_SCREENSHOTS_DIR or ~/Pictures)
    dir: Option<PathBuf>,

    /// Image format: png or jpg
    #[arg(long, default_value = "png")]
    format: String,

    /// Copy to clipboard instead of saving to file
    #[arg(long)]
    copy: bool,

    /// List available monitors and exit
    #[arg(long)]
    list_monitors: bool,

    /// Internal: daemon mode for clipboard persistence (do not use directly)
    #[arg(long = "__clipboard-daemon", hide = true)]
    clipboard_daemon: Option<PathBuf>,
}

#[derive(Clone, Debug)]
enum CaptureKind {
    Primary,
    Monitor(usize),
    Region { x: u32, y: u32, w: u32, h: u32 },
    Selection,
}

#[derive(Clone, Copy, Debug)]
enum SaveHow {
    Copy,
    Save,
    CopyAndSave,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Internal clipboard daemon mode: read image from temp file and serve it
    // on the clipboard until another application overwrites it.
    if let Some(ref path) = cli.clipboard_daemon {
        return clipboard_daemon(path);
    }

    if cli.list_monitors {
        // Prefer libwayshot for accurate physical resolution on Wayland.
        if let Ok(conn) = libwayshot_xcap::WayshotConnection::new() {
            let outputs = conn.get_all_outputs();
            for (i, o) in outputs.iter().enumerate() {
                let name = &o.name;
                let phys = o.physical_size;
                let log = o.logical_region.inner.size;
                let scale = phys.height as f64 / log.height as f64;
                println!(
                    "  Monitor {}: {} ({}x{} physical, {}x{} logical, scale {:.2})",
                    i + 1,
                    name,
                    phys.width,
                    phys.height,
                    log.width,
                    log.height,
                    scale
                );
            }
        } else {
            let monitors = Monitor::all().context("failed to enumerate monitors")?;
            for (i, m) in monitors.iter().enumerate() {
                let name = m.name().unwrap_or_else(|_| "unknown".into());
                let w = m.width().unwrap_or(0);
                let h = m.height().unwrap_or(0);
                let scale = m.scale_factor().unwrap_or(1.0);
                println!(
                    "  Monitor {}: {} ({}x{}, scale {:.2})",
                    i + 1,
                    name,
                    w,
                    h,
                    scale
                );
            }
        }
        return Ok(());
    }

    let shot_dir = cli
        .dir
        .or_else(xdg_screenshots_dir)
        .unwrap_or_else(|| home().join("Pictures"));
    fs::create_dir_all(&shot_dir).ok();

    let how = if cli.copy {
        SaveHow::Copy
    } else {
        SaveHow::Save
    };

    if cli.select {
        let img = select_area()?;
        return save_screenshot(&img, how, &shot_dir, &cli.format);
    }

    if let Some(ref region) = cli.region {
        let kind = parse_region(region)?;
        let img = capture(kind)?;
        return save_screenshot(&img, how, &shot_dir, &cli.format);
    }

    if cli.instant || cli.monitor.is_some() {
        let kind = match cli.monitor {
            Some(n) => CaptureKind::Monitor(n.saturating_sub(1)),
            None => CaptureKind::Primary,
        };
        let img = capture(kind)?;
        return save_screenshot(&img, how, &shot_dir, &cli.format);
    }

    // default to interactive
    run_interactive(&shot_dir, &cli.format)
}

fn run_interactive(shot_dir: &Path, format: &str) -> Result<()> {
    let theme = ColorfulTheme::default();

    // 1. Timing
    let timing = Select::with_theme(&theme)
        .with_prompt("When to take screenshot")
        .items(&["Immediate", "Delayed"])
        .default(0)
        .interact()
        .context("selection cancelled")?;

    let delay: u64 = if timing == 1 {
        let delays = ["5s", "10s", "20s", "30s", "60s"];
        let d = Select::with_theme(&theme)
            .with_prompt("Choose delay")
            .items(&delays)
            .default(0)
            .interact()
            .context("selection cancelled")?;
        [5, 10, 20, 30, 60][d]
    } else {
        0
    };

    // 2. Capture target
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    let mut target_labels: Vec<String> = vec![
        "Primary monitor".into(),
        "Select area (cursor)".into(),
        "Region (coordinates)".into(),
    ];
    for (i, m) in monitors.iter().enumerate() {
        let name = m.name().unwrap_or_else(|_| "unknown".into());
        let w = m.width().unwrap_or(0);
        let h = m.height().unwrap_or(0);
        let scale = m.scale_factor().unwrap_or(1.0);
        target_labels.push(format!(
            "Monitor {}: {} ({}x{}, scale {:.2})",
            i + 1,
            name,
            w,
            h,
            scale
        ));
    }

    let target = Select::with_theme(&theme)
        .with_prompt("Capture target")
        .items(&target_labels)
        .default(0)
        .interact()
        .context("selection cancelled")?;

    let kind = match target {
        0 => CaptureKind::Primary,
        1 => CaptureKind::Selection,
        2 => {
            let region_str: String = Input::with_theme(&theme)
                .with_prompt("Region (x,y,WxH)")
                .interact_text()
                .context("input cancelled")?;
            parse_region(&region_str)?
        }
        n => CaptureKind::Monitor(n - 3),
    };

    // 3. Save method
    let how_idx = Select::with_theme(&theme)
        .with_prompt("Save method")
        .items(&["Copy to clipboard", "Save to file", "Copy & Save"])
        .default(2)
        .interact()
        .context("selection cancelled")?;

    let how = match how_idx {
        0 => SaveHow::Copy,
        1 => SaveHow::Save,
        _ => SaveHow::CopyAndSave,
    };

    // Countdown
    if delay > 0 {
        countdown(delay);
    }

    let img = match kind {
        CaptureKind::Selection => select_area()?,
        other => capture(other)?,
    };
    save_screenshot(&img, how, shot_dir, format)
}

/// Capture the primary screen at full physical resolution using wlroots
/// screencopy (via libwayshot-xcap).  Falls back to xcap if unavailable.
fn capture_fullscreen() -> Result<image::RgbaImage> {
    // Try wlroots screencopy first — returns full physical resolution.
    if let Ok(conn) = libwayshot_xcap::WayshotConnection::new() {
        let outputs = conn.get_all_outputs();
        if let Some(output) = outputs.first()
            && let Ok(img) = conn.screenshot_single_output(output, false)
        {
            return Ok(img.into_rgba8());
        }
    }
    // Fall back to xcap (may be cropped on fractional scaling).
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    let monitor = monitors.into_iter().next().context("no monitors found")?;
    monitor.capture_image().context("failed to capture screen")
}

/// Capture every monitor individually, returning `(output_name, image)` pairs.
/// Each screenshot is at the monitor's native physical resolution.
fn capture_all_outputs() -> Result<Vec<(String, image::RgbaImage)>> {
    // Try wlroots screencopy first — accurate physical resolution per output.
    if let Ok(conn) = libwayshot_xcap::WayshotConnection::new() {
        let outputs = conn.get_all_outputs();
        if !outputs.is_empty() {
            let mut shots = Vec::with_capacity(outputs.len());
            for output in outputs {
                if let Ok(img) = conn.screenshot_single_output(output, false) {
                    shots.push((output.name.clone(), img.into_rgba8()));
                }
            }
            if !shots.is_empty() {
                return Ok(shots);
            }
        }
    }

    // Fall back to xcap.
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    let mut shots = Vec::with_capacity(monitors.len());
    for m in &monitors {
        let name = m.name().unwrap_or_else(|_| "unknown".into());
        let img = m.capture_image().context("failed to capture monitor")?;
        shots.push((name, img));
    }
    if shots.is_empty() {
        bail!("no monitors found");
    }
    Ok(shots)
}

fn select_area() -> Result<image::RgbaImage> {
    // 1. Capture every monitor at physical resolution before showing the
    //    overlay (so the overlay itself is not in any screenshot).
    let all_screenshots = capture_all_outputs()?;

    // 2. Run the Wayland layer-shell overlay for area selection.
    //    Returns (selection, surface_logical_size, output_name).
    let (selection, surf_size, output_name) =
        overlay::run_selection_overlay().context("area selection failed")?;

    match selection {
        Some((sx, sy, sw, sh)) => {
            if sw < 2 || sh < 2 {
                bail!("selection too small");
            }

            // Pick the screenshot that matches the output the overlay was on.
            // Fall back to the first screenshot if no match (shouldn't happen).
            let screenshot = if let Some(ref target) = output_name {
                all_screenshots
                    .iter()
                    .find(|(name, _)| name == target)
                    .map(|(_, img)| img)
                    .unwrap_or(&all_screenshots[0].1)
            } else {
                &all_screenshots[0].1
            };

            let img_w = screenshot.width();
            let img_h = screenshot.height();

            // Pointer coords are in logical surface space (0..surface_w).
            // The image is at physical resolution (img_w × img_h).
            // Map: image_coord = logical_coord * img_w / surface_w.
            // This handles per-monitor resolution and fractional scaling
            // automatically because both dimensions refer to the same output.
            let (surf_w, surf_h) = surf_size;
            let surf_w = if surf_w > 0 { surf_w } else { img_w };
            let surf_h = if surf_h > 0 { surf_h } else { img_h };
            let scale_x = img_w as f64 / surf_w as f64;
            let scale_y = img_h as f64 / surf_h as f64;
            let x = (sx as f64 * scale_x).round() as u32;
            let y = (sy as f64 * scale_y).round() as u32;
            let w = (sw as f64 * scale_x).round() as u32;
            let h = (sh as f64 * scale_y).round() as u32;

            // Clamp to image bounds.
            let x = x.min(img_w.saturating_sub(1));
            let y = y.min(img_h.saturating_sub(1));
            let w = w.min(img_w - x);
            let h = h.min(img_h - y);

            let cropped = image::imageops::crop_imm(screenshot, x, y, w, h).to_image();
            Ok(cropped)
        }
        None => bail!("selection cancelled"),
    }
}

fn parse_region(s: &str) -> Result<CaptureKind> {
    // Format: x,y,WxH  e.g. "100,200,800x600"
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        bail!("region format must be x,y,WxH (e.g. 100,200,800x600)");
    }
    let x: u32 = parts[0].parse().context("invalid x coordinate")?;
    let y: u32 = parts[1].parse().context("invalid y coordinate")?;
    let wh: Vec<&str> = parts[2].split('x').collect();
    if wh.len() != 2 {
        bail!("region size must be WxH (e.g. 800x600)");
    }
    let w: u32 = wh[0].parse().context("invalid width")?;
    let h: u32 = wh[1].parse().context("invalid height")?;
    Ok(CaptureKind::Region { x, y, w, h })
}

fn capture(kind: CaptureKind) -> Result<image::RgbaImage> {
    match kind {
        CaptureKind::Primary => capture_fullscreen(),
        CaptureKind::Monitor(idx) => {
            // Try libwayshot first for correct resolution.
            if let Ok(conn) = libwayshot_xcap::WayshotConnection::new() {
                let outputs = conn.get_all_outputs();
                if let Some(output) = outputs.get(idx)
                    && let Ok(img) = conn.screenshot_single_output(output, false)
                {
                    return Ok(img.into_rgba8());
                }
            }
            // Fall back to xcap.
            let monitors = Monitor::all().context("failed to enumerate monitors")?;
            monitors
                .into_iter()
                .nth(idx)
                .context("monitor index out of range")?
                .capture_image()
                .context("failed to capture monitor")
        }
        CaptureKind::Region { x, y, w, h } => {
            // Capture full screen at physical resolution, then crop.
            let img = capture_fullscreen()?;
            let x = x.min(img.width().saturating_sub(1));
            let y = y.min(img.height().saturating_sub(1));
            let w = w.min(img.width() - x);
            let h = h.min(img.height() - y);
            Ok(image::imageops::crop_imm(&img, x, y, w, h).to_image())
        }
        CaptureKind::Selection => select_area(),
    }
}

fn save_screenshot(
    img: &image::RgbaImage,
    how: SaveHow,
    shot_dir: &Path,
    format: &str,
) -> Result<()> {
    match how {
        SaveHow::Copy => {
            copy_to_clipboard(img)?;
            notify("Screenshot copied", "Image copied to clipboard");
        }
        SaveHow::Save => {
            let path = save_to_file(img, shot_dir, format)?;
            notify("Screenshot saved", &format!("Saved to {}", path.display()));
        }
        SaveHow::CopyAndSave => {
            copy_to_clipboard(img)?;
            let path = save_to_file(img, shot_dir, format)?;
            notify(
                "Screenshot saved & copied",
                &format!("Saved to {}", path.display()),
            );
        }
    }
    Ok(())
}

fn copy_to_clipboard(img: &image::RgbaImage) -> Result<()> {
    // On Wayland/X11 the clipboard is "owned" by the setting process.  When
    // that process exits the content is lost.  To work around this we:
    //   1. Write the image to a temp file.
    //   2. Spawn a detached child of ourselves with `--__clipboard-daemon <path>`
    //      that reads the file, puts it on the clipboard, and blocks (`.wait()`)
    //      until something else is copied.
    //   3. The parent returns immediately.
    let tmp = env::temp_dir().join(format!("crabture_clip_{}.png", process::id()));
    img.save(&tmp)
        .context("failed to write clipboard temp file")?;

    process::Command::new(env::current_exe().context("could not find own executable")?)
        .arg("--__clipboard-daemon")
        .arg(&tmp)
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .spawn()
        .context("failed to spawn clipboard daemon")?;

    Ok(())
}

/// Runs in the background child process: reads the image from `path`, places it
/// on the clipboard, and blocks until another app overwrites it.
fn clipboard_daemon(path: &Path) -> Result<()> {
    use arboard::SetExtLinux;

    let img = image::open(path)
        .context("clipboard daemon: failed to read image")?
        .into_rgba8();

    // Clean up the temp file now that we have the data in memory.
    fs::remove_file(path).ok();

    let mut clipboard = Clipboard::new().context("failed to open clipboard")?;
    let data = ImageData {
        width: img.width() as usize,
        height: img.height() as usize,
        bytes: Cow::Borrowed(img.as_raw()),
    };

    clipboard
        .set()
        .wait()
        .image(data)
        .context("failed to set clipboard image")
}

fn save_to_file(img: &image::RgbaImage, shot_dir: &Path, format: &str) -> Result<PathBuf> {
    let name = file_name(format);
    let path = shot_dir.join(&name);

    if format.eq_ignore_ascii_case("jpg") || format.eq_ignore_ascii_case("jpeg") {
        let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
        rgb.save(&path).context("failed to save screenshot")?;
    } else {
        img.save(&path).context("failed to save screenshot")?;
    }

    Ok(path)
}

fn countdown(mut secs: u64) {
    if secs > 10 {
        notify("Taking screenshot", &format!("in {secs} seconds"));
        sleep(Duration::from_secs(secs - 10));
        secs = 10;
    }
    while secs > 0 {
        notify("Taking screenshot", &format!("in {secs} seconds"));
        sleep(Duration::from_secs(1));
        secs -= 1;
    }
}

fn notify(title: &str, body: &str) {
    Notification::new()
        .summary(title)
        .body(body)
        .timeout(notify_rust::Timeout::Milliseconds(2000))
        .show()
        .ok();
}

fn file_name(fmt: &str) -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let ts = format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let ext = if fmt.eq_ignore_ascii_case("jpg") || fmt.eq_ignore_ascii_case("jpeg") {
        "jpg"
    } else {
        "png"
    };
    format!("screenshot_{ts}.{ext}")
}

fn home() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn xdg_screenshots_dir() -> Option<PathBuf> {
    if let Ok(path) = env::var("XDG_SCREENSHOTS_DIR") {
        return Some(PathBuf::from(
            path.replace("$HOME", &home().to_string_lossy()),
        ));
    }
    let path = home().join(".config/user-dirs.dirs");
    if let Ok(s) = fs::read_to_string(path) {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("XDG_SCREENSHOTS_DIR=") {
                let raw = rest.trim().trim_matches('"').to_string();
                return Some(PathBuf::from(
                    raw.replace("$HOME", &home().to_string_lossy()),
                ));
            }
        }
    }
    None
}
