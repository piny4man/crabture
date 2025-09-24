use anyhow::{Context, Result, bail};
use clap::Parser;
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread::sleep,
    time::Duration,
};
use time::OffsetDateTime;
use which::which;

#[derive(Parser, Debug)]
#[command(version, about = "Rusty screenshot helper (grimblast + rofi)")]
struct Cli {
    /// Take immediate full-screen shot (no UI)
    #[arg(long)]
    instant: bool,

    /// Take immediate area shot (no UI)
    #[arg(long)]
    instant_area: bool,

    /// Use interactive rofi flow
    #[arg(long)]
    interactive: bool,

    /// Screenshot directory (default: XDG_SCREENSHOTS_DIR or ~/Pictures)
    dir: Option<PathBuf>,

    /// Image format: png or jpg
    #[arg(long, default_value = "png")]
    format: String,

    /// Optional rofi config path
    #[arg(long)]
    rofi_config: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
enum CaptureKind {
    Screen,
    Output,
    Area,
}

#[derive(Clone, Copy, Debug)]
enum SaveHow {
    Copy,
    Save,
    Copysave,
    Edit,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    ensure_tools(&["grimblast", "rofi", "notify-send"])?;

    let _ = which("slurp");

    let shot_dir = cli
        .dir
        .or_else(xdg_screenshots_dir)
        .unwrap_or(home().join("Pictures"));
    fs::create_dir_all(&shot_dir).ok();

    if cli.instant {
        take(
            CaptureKind::Screen,
            SaveHow::Save,
            &shot_dir,
            &cli.format,
            None,
        )?;
    }

    if cli.instant_area {
        take(
            CaptureKind::Area,
            SaveHow::Save,
            &shot_dir,
            &cli.format,
            None,
        )?;
        return Ok(());
    }

    // default to interactive if nothing else was specified
    run_interactive(&shot_dir, &cli.format, cli.rofi_config.as_deref())
}

fn run_interactive(shot_dir: &Path, format: &str, rofi_cfg: Option<&Path>) -> Result<()> {
    let when = rofi_pick("Take screenshot", &["Immediate", "Delayed"], rofi_cfg)?;
    let delay = if when == "Delayed" {
        let t = rofi_pick(
            "Choose timer",
            &["5s", "10s", "20s", "30s", "60s"],
            rofi_cfg,
        )?;
        t.trim_end_matches("s").parse::<u64>().unwrap_or(5)
    } else {
        0
    };

    let kind = match rofi_pick(
        "Type of screenshot",
        &[
            "Capture Everything",
            "Capture Active Display",
            "Capture Selection",
        ],
        rofi_cfg,
    )?
    .as_str()
    {
        "Capture Everything" => CaptureKind::Screen,
        "Capture Active Display" => CaptureKind::Output,
        _ => CaptureKind::Area,
    };

    let how = match rofi_pick(
        "How to save",
        &["Copy", "Save", "Copy & Save", "Edit"],
        rofi_cfg,
    )?
    .as_str()
    {
        "Copy" => SaveHow::Copy,
        "Save" => SaveHow::Save,
        "Copy & Save" => SaveHow::Copysave,
        _ => SaveHow::Edit,
    };

    if delay > 0 {
        countdown(delay)?;
    }

    take(kind, how, shot_dir, format, rofi_cfg)
}

fn take(
    kind: CaptureKind,
    how: SaveHow,
    shot_dir: &Path,
    format: &str,
    _rofi_cfg: Option<&Path>,
) -> Result<()> {
    let name = file_name(format);
    let tmp_path = home().join(&name);

    // freeze screen for area selection if hyprpicker exists; let grimblast run slurp
    let mut picker_child = if matches!(kind, CaptureKind::Area) && which("hyprpicker").is_ok() {
        let mut c = Command::new("hyprpicker");
        c.args(["-r", "-z"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c.spawn().ok()
    } else {
        None
    };

    // map enums to grimblast args
    let how_s = match how {
        SaveHow::Copy => "copy",
        SaveHow::Save => "save",
        SaveHow::Copysave => "copysave",
        SaveHow::Edit => "edit",
    };
    let kind_s = match kind {
        CaptureKind::Screen => "screen",
        CaptureKind::Output => "output",
        CaptureKind::Area => "area",
    };

    // run grimblast
    let status = Command::new("grimblast")
        .args(["--notify", how_s, kind_s, &tmp_path.to_string_lossy()])
        .status()
        .context("running grimblast")?;

    // unfreeze screen if we stared hyprpicker
    if let Some(mut child) = picker_child.take() {
        let _ = child.kill();
    }

    if !status.success() {
        bail!("grimblast failed");
    }

    // move into screenshots dir if a file was written (copy mode may not write a file)
    if tmp_path.exists() {
        let dest = shot_dir.join(name);
        fs::rename(&tmp_path, &dest).or_else(|_| {
            fs::copy(&tmp_path, &dest)
                .map(|_| {
                    let _ = fs::remove_file(&tmp_path);
                })
                .ok();
            Ok::<(), anyhow::Error>(())
        })?;
        notify("Screenshot saved", &format!("DIR: {}", shot_dir.display()))?;
    }
    Ok(())
}

fn rofi_pick(prompt: &str, options: &[&str], cfg: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("rofi");
    cmd.args(["-dmenu", "-i", "-no-show-icons", "-p", prompt]);
    if let Some(c) = cfg {
        cmd.args(["-config", &c.to_string_lossy()]);
    }
    let input = options.join("\n");
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("spawning rofi")?;
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("rofi cancceled");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn countdown(mut secs: u64) -> Result<()> {
    if secs > 10 {
        notify("Taking screenshot", &format!("in {secs} seconds"))?;
        sleep(Duration::from_secs(secs - 10));
        secs = 10;
    }
    while secs > 0 {
        notify("Taking screenshot", &format!("in {secs} seconds"))?;
        sleep(Duration::from_secs(1));
        secs -= 1;
    }
    Ok(())
}

fn notify(title: &str, body: &str) -> Result<()> {
    let _ = Command::new("notify-send")
        .args(["-t", "1000", title, body])
        .status();
    Ok(())
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

fn ensure_tools(names: &[&str]) -> Result<()> {
    for n in names {
        if which(n).is_err() {
            bail!("required tool not found in PATH: {}", n);
        }
    }
    Ok(())
}
