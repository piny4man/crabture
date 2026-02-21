use anyhow::{Context, Result, bail};
use arboard::{Clipboard, ImageData};
use clap::Parser;
use dialoguer::{Select, theme::ColorfulTheme};
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
#[command(version, about = "Standalone screenshot tool — no external dependencies required")]
struct Cli {
    /// Take immediate full-screen shot (no UI)
    #[arg(long)]
    instant: bool,

    /// Capture a specific monitor by index (1-based)
    #[arg(long)]
    monitor: Option<usize>,

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

#[derive(Clone, Copy, Debug)]
enum CaptureKind {
    Primary,
    Monitor(usize),
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

    if cli.instant || cli.monitor.is_some() {
        let kind = match cli.monitor {
            Some(n) => CaptureKind::Monitor(n.saturating_sub(1)),
            None => CaptureKind::Primary,
        };
        let how = if cli.copy {
            SaveHow::Copy
        } else {
            SaveHow::Save
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
    let mut target_labels: Vec<String> = vec!["Primary monitor".into()];
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

    let kind = if target == 0 {
        CaptureKind::Primary
    } else {
        CaptureKind::Monitor(target - 1)
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

    let img = capture(kind)?;
    save_screenshot(&img, how, shot_dir, format)
}

fn capture(kind: CaptureKind) -> Result<image::RgbaImage> {
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    if monitors.is_empty() {
        bail!("no monitors found");
    }

    let monitor = match kind {
        CaptureKind::Primary => monitors.into_iter().next().unwrap(),
        CaptureKind::Monitor(idx) => monitors
            .into_iter()
            .nth(idx)
            .context("monitor index out of range")?,
    };

    monitor.capture_image().context("failed to capture screen")
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
            notify(
                "Screenshot saved",
                &format!("Saved to {}", path.display()),
            );
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
