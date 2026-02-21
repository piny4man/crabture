use anyhow::{Context, Result, bail};
use arboard::{Clipboard, ImageData};
use clap::Parser;
use dialoguer::{Input, Select, theme::ColorfulTheme};
use minifb::{Key, MouseButton, MouseMode, Window, WindowOptions};
use notify_rust::Notification;
use std::{
    borrow::Cow,
    env, fs,
    path::{Path, PathBuf},
    thread::sleep,
    time::Duration,
};
use time::OffsetDateTime;
use xcap::Monitor;

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

    /// Use interactive TUI flow
    #[arg(long)]
    interactive: bool,

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

    if cli.list_monitors {
        let monitors = Monitor::all().context("failed to enumerate monitors")?;
        for (i, m) in monitors.iter().enumerate() {
            let name = m.name().unwrap_or_else(|_| "unknown".into());
            let w = m.width().unwrap_or(0);
            let h = m.height().unwrap_or(0);
            println!("  Monitor {}: {} ({}x{})", i + 1, name, w, h);
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
        target_labels.push(format!("Monitor {}: {} ({}x{})", i + 1, name, w, h));
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

fn select_area() -> Result<image::RgbaImage> {
    // 1. Capture full screen
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    let monitor = monitors.into_iter().next().context("no monitors found")?;
    let screenshot = monitor
        .capture_image()
        .context("failed to capture screen")?;
    let width = screenshot.width() as usize;
    let height = screenshot.height() as usize;

    // 2. Convert to minifb pixel format (0x00RRGGBB)
    let original: Vec<u32> = screenshot
        .pixels()
        .map(|p| ((p[0] as u32) << 16) | ((p[1] as u32) << 8) | (p[2] as u32))
        .collect();

    let dark: Vec<u32> = screenshot
        .pixels()
        .map(|p| {
            let r = (p[0] as u32) / 3;
            let g = (p[1] as u32) / 3;
            let b = (p[2] as u32) / 3;
            (r << 16) | (g << 8) | b
        })
        .collect();

    let mut buffer = dark.clone();

    // 3. Open borderless topmost window
    let mut window = Window::new(
        "",
        width,
        height,
        WindowOptions {
            borderless: true,
            topmost: true,
            title: false,
            ..Default::default()
        },
    )
    .context("failed to create selection window")?;

    let mut start: Option<(usize, usize)> = None;
    let mut selection: Option<(usize, usize, usize, usize)> = None;
    let mut selecting = false;

    // 4. Event loop: click and drag to select
    while window.is_open() && !window.is_key_down(Key::Escape) {
        if let Some((mx, my)) = window.get_mouse_pos(MouseMode::Clamp) {
            let mx = (mx as usize).min(width.saturating_sub(1));
            let my = (my as usize).min(height.saturating_sub(1));

            if window.get_mouse_down(MouseButton::Left) {
                if !selecting {
                    start = Some((mx, my));
                    selecting = true;
                }

                if let Some((sx, sy)) = start {
                    let x1 = sx.min(mx);
                    let y1 = sy.min(my);
                    let x2 = sx.max(mx);
                    let y2 = sy.max(my);
                    selection = Some((x1, y1, x2.saturating_sub(x1), y2.saturating_sub(y1)));

                    // Redraw: dark everywhere, bright in selection
                    buffer.copy_from_slice(&dark);
                    for row in y1..y2.min(height) {
                        for col in x1..x2.min(width) {
                            buffer[row * width + col] = original[row * width + col];
                        }
                    }

                    // Draw border around selection
                    let border_color: u32 = 0x00_FF_FF_FF;
                    for col in x1..x2.min(width) {
                        buffer[y1 * width + col] = border_color;
                        if y2 > 0 && y2 - 1 < height {
                            buffer[(y2 - 1) * width + col] = border_color;
                        }
                    }
                    for row in y1..y2.min(height) {
                        buffer[row * width + x1] = border_color;
                        if x2 > 0 && x2 - 1 < width {
                            buffer[row * width + x2 - 1] = border_color;
                        }
                    }
                }
            } else if selecting {
                // Mouse released — selection complete
                break;
            }
        }

        window
            .update_with_buffer(&buffer, width, height)
            .context("failed to update window")?;
    }

    // 5. Crop the original screenshot to the selection
    if let Some((x, y, w, h)) = selection {
        if w < 2 || h < 2 {
            bail!("selection too small");
        }
        let cropped =
            image::imageops::crop_imm(&screenshot, x as u32, y as u32, w as u32, h as u32)
                .to_image();
        Ok(cropped)
    } else {
        bail!("selection cancelled (press Escape or close window)")
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
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    if monitors.is_empty() {
        bail!("no monitors found");
    }

    match kind {
        CaptureKind::Primary => monitors
            .into_iter()
            .next()
            .unwrap()
            .capture_image()
            .context("failed to capture screen"),
        CaptureKind::Monitor(idx) => monitors
            .into_iter()
            .nth(idx)
            .context("monitor index out of range")?
            .capture_image()
            .context("failed to capture monitor"),
        CaptureKind::Region { x, y, w, h } => monitors
            .into_iter()
            .next()
            .unwrap()
            .capture_region(x, y, w, h)
            .context("failed to capture region"),
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
    let mut clipboard = Clipboard::new().context("failed to open clipboard")?;
    let data = ImageData {
        width: img.width() as usize,
        height: img.height() as usize,
        bytes: Cow::Borrowed(img.as_raw()),
    };
    clipboard
        .set_image(data)
        .context("failed to copy image to clipboard")
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
        "{:02}{:02}{:04}_{:02}{:02}{:02}",
        now.day(),
        now.month() as u8,
        now.year(),
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
