//! Resolved HUD appearance, loaded separately from capture preferences.
//!
//! The no-theme palette is the previous hardcoded toolbar/overlay look and is
//! kept explicitly. An optional Swatches theme replaces the six semantic RGB
//! roles; explicit `[colors]` / `[font]` fields in
//! `$XDG_CONFIG_HOME/crabture/appearance.toml` always win. Alpha, geometry,
//! overlay dimming, and Capture's on-accent contrast stay application-owned.

use ab_glyph::FontVec;
use serde::Deserialize;
use std::{
    env, fmt, fs,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};
use swatches::{Appearance as SharedAppearance, AppearancePatch, FontFamily, Rgb, Theme};

const BUNDLED_FONT_BYTES: &[u8] = include_bytes!("../assets/Roboto-Medium.ttf");

const PANEL_FILL_ALPHA: u8 = 236;
const PANEL_BORDER_ALPHA: u8 = 30;
const SEPARATOR_ALPHA: u8 = 28;
const TEXT_ALPHA: u8 = 236;
const MUTED_ALPHA: u8 = 150;
const HOVER_ALPHA: u8 = 36;
const ACCENT_HINT_ALPHA: u8 = 200;
const HIGHLIGHT_FILL_ALPHA: u8 = 0x38;
/// WCAG AA contrast for normal-size text; every HUD text role is below 18 px.
const MIN_TEXT_CONTRAST: f64 = 4.5;
const OVERLAY_PIXEL: u32 = 0x9900_0000;

/// Toolbar, icon, and overlay colors after theme/override resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorSet {
    pub panel_fill: (u8, u8, u8, u8),
    pub panel_border: (u8, u8, u8, u8),
    pub separator: (u8, u8, u8, u8),
    pub accent: (u8, u8, u8, u8),
    pub text: (u8, u8, u8, u8),
    pub text_muted: (u8, u8, u8, u8),
    pub hover: (u8, u8, u8, u8),
    pub icon: (u8, u8, u8),
    pub selected: (u8, u8, u8, u8),
    pub selected_label: (u8, u8, u8, u8),
    pub selected_icon: (u8, u8, u8),
    pub accent_label: (u8, u8, u8, u8),
    pub accent_hint: (u8, u8, u8, u8),
    pub overlay: u32,
    pub selection_border: u32,
    pub highlight_fill: u32,
    pub highlight_border: u32,
}

impl ColorSet {
    /// Palette used when no theme or color overrides are selected.
    pub fn built_in() -> Self {
        Self {
            panel_fill: (32, 32, 36, PANEL_FILL_ALPHA),
            panel_border: (255, 255, 255, PANEL_BORDER_ALPHA),
            separator: (255, 255, 255, SEPARATOR_ALPHA),
            accent: (10, 132, 255, 255),
            text: (255, 255, 255, TEXT_ALPHA),
            text_muted: (235, 235, 245, MUTED_ALPHA),
            hover: (255, 255, 255, HOVER_ALPHA),
            icon: (255, 255, 255),
            selected: (10, 132, 255, 255),
            selected_label: (255, 255, 255, 255),
            selected_icon: (255, 255, 255),
            accent_label: (255, 255, 255, 255),
            accent_hint: (255, 255, 255, ACCENT_HINT_ALPHA),
            overlay: OVERLAY_PIXEL,
            selection_border: pack_argb(255, 255, 255, 255),
            highlight_fill: pack_argb(
                HIGHLIGHT_FILL_ALPHA,
                premul(10, HIGHLIGHT_FILL_ALPHA),
                premul(132, HIGHLIGHT_FILL_ALPHA),
                premul(255, HIGHLIGHT_FILL_ALPHA),
            ),
            highlight_border: pack_argb(255, 10, 132, 255),
        }
    }
}

/// Identity of the font used for measurement, drawing, and hit-testing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FontKey {
    Bundled,
    Path(PathBuf),
}

/// Owned font plus the cache key that identifies it.
pub struct UiFont {
    inner: FontVec,
    key: FontKey,
}

impl UiFont {
    pub fn inner(&self) -> &FontVec {
        &self.inner
    }

    pub fn key(&self) -> &FontKey {
        &self.key
    }
}

/// Fully resolved appearance for one HUD invocation.
pub struct Appearance {
    pub colors: ColorSet,
    font: UiFont,
}

impl Appearance {
    pub fn bundled() -> Self {
        Self {
            colors: ColorSet::built_in(),
            font: bundled_font(),
        }
    }

    pub fn font(&self) -> &FontVec {
        self.font.inner()
    }

    pub fn font_key(&self) -> &FontKey {
        self.font.key()
    }
}

impl fmt::Debug for Appearance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Appearance")
            .field("colors", &self.colors)
            .field("font_key", self.font_key())
            .finish()
    }
}

/// Warnings collected while loading; the HUD still opens.
#[derive(Debug)]
pub struct LoadedAppearance {
    pub appearance: Appearance,
    pub warnings: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawFile {
    appearance: RawAppearance,
    colors: RawColors,
    font: RawFont,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawAppearance {
    theme_file: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawColors {
    background: Option<String>,
    foreground: Option<String>,
    accent: Option<String>,
    muted: Option<String>,
    selection_background: Option<String>,
    selection_foreground: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawFont {
    family: Option<String>,
    path: Option<String>,
}

#[derive(Debug)]
pub struct AppearanceError(String);

impl std::fmt::Display for AppearanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn appearance_path() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("crabture")
        .join("appearance.toml")
}

#[hotpath::measure]
pub fn load_from_default_path() -> LoadedAppearance {
    let home = env::var_os("HOME").map(PathBuf::from);
    load_from_path(&appearance_path(), home.as_deref())
}

#[hotpath::measure]
pub fn load_from_path(path: &Path, home: Option<&Path>) -> LoadedAppearance {
    match fs::read_to_string(path) {
        Ok(contents) => match resolve_from_toml(&contents, path.parent(), home) {
            Ok(loaded) => loaded,
            Err(err) => LoadedAppearance {
                appearance: Appearance::bundled(),
                warnings: vec![format!(
                    "{}: {err}; using built-in appearance",
                    path.display()
                )],
            },
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => LoadedAppearance {
            appearance: Appearance::bundled(),
            warnings: Vec::new(),
        },
        Err(err) => LoadedAppearance {
            appearance: Appearance::bundled(),
            warnings: vec![format!(
                "{}: {err}; using built-in appearance",
                path.display()
            )],
        },
    }
}

pub fn resolve_from_toml(
    contents: &str,
    config_dir: Option<&Path>,
    home: Option<&Path>,
) -> Result<LoadedAppearance, AppearanceError> {
    let raw: RawFile =
        toml::from_str(contents).map_err(|err| AppearanceError(err.message().to_string()))?;
    let mut warnings = Vec::new();
    let theme = load_optional_theme(
        raw.appearance.theme_file.as_deref(),
        config_dir,
        home,
        &mut warnings,
    );
    let patch = appearance_patch(&raw)?;
    let shared = swatches::resolve(&built_in_shared(), theme.as_ref(), &patch);
    let font_path = raw.font.path.as_deref().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    });
    let family = if patch.font_family.is_some() || theme.is_some() {
        Some(shared.font_family.as_str())
    } else {
        None
    };
    let colors = if theme.is_none() && !patch_has_colors(&patch) {
        ColorSet::built_in()
    } else {
        warnings.extend(contrast_warnings(&shared));
        from_shared(&shared)
    };
    Ok(LoadedAppearance {
        appearance: Appearance {
            colors,
            font: load_font(font_path.as_deref(), family),
        },
        warnings,
    })
}

fn patch_has_colors(patch: &AppearancePatch) -> bool {
    patch.background.is_some()
        || patch.foreground.is_some()
        || patch.accent.is_some()
        || patch.muted.is_some()
        || patch.selection_background.is_some()
        || patch.selection_foreground.is_some()
}

fn appearance_patch(raw: &RawFile) -> Result<AppearancePatch, AppearanceError> {
    Ok(AppearancePatch {
        background: parse_rgb_field("colors.background", raw.colors.background.as_deref())?,
        foreground: parse_rgb_field("colors.foreground", raw.colors.foreground.as_deref())?,
        accent: parse_rgb_field("colors.accent", raw.colors.accent.as_deref())?,
        muted: parse_rgb_field("colors.muted", raw.colors.muted.as_deref())?,
        selection_background: parse_rgb_field(
            "colors.selection_background",
            raw.colors.selection_background.as_deref(),
        )?,
        selection_foreground: parse_rgb_field(
            "colors.selection_foreground",
            raw.colors.selection_foreground.as_deref(),
        )?,
        font_family: parse_family_field(raw.font.family.as_deref())?,
    })
}

fn parse_rgb_field(field: &str, value: Option<&str>) -> Result<Option<Rgb>, AppearanceError> {
    match value {
        None => Ok(None),
        Some(value) => value
            .parse::<Rgb>()
            .map(Some)
            .map_err(|msg| AppearanceError(format!("{field} is not valid ({msg})"))),
    }
}

fn parse_family_field(value: Option<&str>) -> Result<Option<FontFamily>, AppearanceError> {
    match value {
        None => Ok(None),
        Some(value) if value.trim().is_empty() => Ok(None),
        Some(value) => value
            .parse::<FontFamily>()
            .map(Some)
            .map_err(|msg| AppearanceError(format!("font.family is not valid ({msg})"))),
    }
}

#[hotpath::measure]
fn load_optional_theme(
    theme_file: Option<&str>,
    config_dir: Option<&Path>,
    home: Option<&Path>,
    warnings: &mut Vec<String>,
) -> Option<Theme> {
    let value = theme_file?;
    if value.trim().is_empty() {
        warnings.push("appearance.theme_file is empty; ignoring shared theme".to_string());
        return None;
    }
    let path = match config_dir {
        Some(config_dir) => match swatches::resolve_theme_path(value, config_dir, home) {
            Ok(path) => path,
            Err(err) => {
                warnings.push(format!("{err}; ignoring shared theme"));
                return None;
            }
        },
        None => {
            let path = Path::new(value);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                warnings.push(format!(
                    "appearance.theme_file = {value:?} is not absolute and no config directory is available; ignoring shared theme"
                ));
                return None;
            }
        }
    };
    match Theme::load(&path) {
        Ok(theme) => Some(theme),
        Err(err) => {
            warnings.push(format!("{err}; ignoring shared theme"));
            None
        }
    }
}

fn built_in_shared() -> SharedAppearance {
    SharedAppearance {
        background: Rgb::new(32, 32, 36),
        foreground: Rgb::new(255, 255, 255),
        accent: Rgb::new(10, 132, 255),
        muted: Rgb::new(235, 235, 245),
        selection_background: Rgb::new(10, 132, 255),
        selection_foreground: Rgb::new(255, 255, 255),
        font_family: "Roboto".parse().expect("built-in family is valid"),
    }
}

fn from_shared(shared: &SharedAppearance) -> ColorSet {
    let [br, bg, bb] = shared.background.channels();
    let [fr, fg, fb] = shared.foreground.channels();
    let [ar, ag, ab] = shared.accent.channels();
    let [mr, mg, mb] = shared.muted.channels();
    let [sr, sg, sb] = shared.selection_background.channels();
    let [lr, lg, lb] = shared.selection_foreground.channels();
    let (cr, cg, cb) = contrasting_rgb(ar, ag, ab);
    let panel = (br, bg, bb);
    let (text_a, _) = text_alpha((fr, fg, fb), panel, TEXT_ALPHA);
    let (muted_a, _) = text_alpha((mr, mg, mb), panel, MUTED_ALPHA);
    ColorSet {
        panel_fill: (br, bg, bb, PANEL_FILL_ALPHA),
        panel_border: (fr, fg, fb, PANEL_BORDER_ALPHA),
        separator: (fr, fg, fb, SEPARATOR_ALPHA),
        accent: (ar, ag, ab, 255),
        text: (fr, fg, fb, text_a),
        text_muted: (mr, mg, mb, muted_a),
        hover: (fr, fg, fb, HOVER_ALPHA),
        icon: (fr, fg, fb),
        selected: (sr, sg, sb, 255),
        selected_label: (lr, lg, lb, 255),
        selected_icon: (lr, lg, lb),
        accent_label: (cr, cg, cb, 255),
        accent_hint: (cr, cg, cb, ACCENT_HINT_ALPHA),
        overlay: OVERLAY_PIXEL,
        selection_border: pack_argb(255, fr, fg, fb),
        highlight_fill: pack_argb(
            HIGHLIGHT_FILL_ALPHA,
            premul(ar, HIGHLIGHT_FILL_ALPHA),
            premul(ag, HIGHLIGHT_FILL_ALPHA),
            premul(ab, HIGHLIGHT_FILL_ALPHA),
        ),
        highlight_border: pack_argb(255, ar, ag, ab),
    }
}

/// Warn about text roles that stay below [`MIN_TEXT_CONTRAST`] on the panel
/// even when drawn opaque; the colours themselves are kept as configured.
fn contrast_warnings(shared: &SharedAppearance) -> Vec<String> {
    let rgb = |c: Rgb| {
        let [r, g, b] = c.channels();
        (r, g, b)
    };
    let panel = rgb(shared.background);
    [("colors.foreground", shared.foreground), ("colors.muted", shared.muted)]
        .into_iter()
        .filter(|&(_, color)| !text_alpha(rgb(color), panel, 0).1)
        .map(|(field, color)| {
            format!(
                "{field} has {:.1}:1 contrast against colors.background (below {MIN_TEXT_CONTRAST}:1); HUD text may be hard to read",
                contrast_ratio(rgb(color), panel)
            )
        })
        .collect()
}

/// WCAG relative luminance of an sRGB colour.
fn relative_luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let linear = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// WCAG contrast ratio between two opaque colours (1.0 ..= 21.0).
fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `fg` drawn at `alpha` over the opaque `bg`.
fn blend_over(fg: (u8, u8, u8), alpha: u8, bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let mix = |f: u8, b: u8| {
        ((u32::from(f) * u32::from(alpha) + u32::from(b) * u32::from(255 - alpha) + 127) / 255)
            as u8
    };
    (mix(fg.0, bg.0), mix(fg.1, bg.1), mix(fg.2, bg.2))
}

/// The smallest alpha at which `fg` over `panel` reaches `target` contrast,
/// or `None` when even opaque `fg` falls short.
fn min_alpha_for_contrast(fg: (u8, u8, u8), panel: (u8, u8, u8), target: f64) -> Option<u8> {
    (0..=255u8).find(|&alpha| contrast_ratio(blend_over(fg, alpha, panel), panel) >= target)
}

/// Text alpha for `fg` on `panel`: `floor`, raised as far as needed to reach
/// [`MIN_TEXT_CONTRAST`].  The flag is `false` when no alpha reaches it, in
/// which case the text is drawn opaque.
fn text_alpha(fg: (u8, u8, u8), panel: (u8, u8, u8), floor: u8) -> (u8, bool) {
    match min_alpha_for_contrast(fg, panel, MIN_TEXT_CONTRAST) {
        Some(alpha) => (alpha.max(floor), true),
        None => (255, false),
    }
}

fn contrasting_rgb(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let luma = (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000;
    if luma > 128 {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    }
}

fn premul(channel: u8, alpha: u8) -> u8 {
    ((channel as u16 * alpha as u16 + 127) / 255) as u8
}

pub fn pack_argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

#[hotpath::measure]
fn bundled_font() -> UiFont {
    UiFont {
        inner: FontVec::try_from_vec(BUNDLED_FONT_BYTES.to_vec())
            .expect("bundled Roboto font is valid"),
        key: FontKey::Bundled,
    }
}

#[hotpath::measure]
fn load_font(path: Option<&Path>, family: Option<&str>) -> UiFont {
    if let Some(path) = path.filter(|p| !p.as_os_str().is_empty())
        && let Some(font) = font_from_path(path)
    {
        return font;
    }
    if let Some(family) = family.filter(|f| !f.is_empty())
        && let Some(matched) = resolve_family(family)
        && let Some(font) = font_from_path(&matched)
    {
        return font;
    }
    bundled_font()
}

#[hotpath::measure]
fn font_from_path(path: &Path) -> Option<UiFont> {
    let bytes = fs::read(path).ok()?;
    let inner = FontVec::try_from_vec(bytes).ok()?;
    Some(UiFont {
        inner,
        key: FontKey::Path(path.to_path_buf()),
    })
}

/// The cache key for a family's resolved path: the family plus the fontconfig
/// cache directories' `stamps` (mtimes), which `fc-cache` bumps when fonts
/// are installed or removed.  Always one line.
fn font_cache_key(family: &str, stamps: &[Option<SystemTime>]) -> String {
    let stamps: Vec<Option<u128>> = stamps
        .iter()
        .map(|t| {
            Some(
                t.as_ref()?
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .ok()?
                    .as_nanos(),
            )
        })
        .collect();
    // `Debug` escapes control characters, so the key never spans lines.
    format!("v1 {:?}", (family, stamps))
}

/// The cached font path in `file` for `key`, or `lookup()` (written back
/// best-effort) when the cache is missing, stale, corrupt, or names a font
/// that no longer exists.
fn cached_font_path(
    file: &Path,
    key: &str,
    lookup: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = read_font_cache(file, key) {
        return Some(path);
    }
    let path = lookup()?;
    write_font_cache(file, key, &path);
    Some(path)
}

fn read_font_cache(file: &Path, key: &str) -> Option<PathBuf> {
    let text = fs::read_to_string(file).ok()?;
    let mut lines = text.lines();
    if lines.next()? != key {
        return None;
    }
    let path = PathBuf::from(lines.next().filter(|p| !p.is_empty())?);
    path.exists().then_some(path)
}

/// Best effort: a failed write only means the next open runs `fc-match` again.
fn write_font_cache(file: &Path, key: &str, path: &Path) {
    let Some(path) = path.to_str().filter(|p| !p.contains('\n')) else {
        return;
    };
    let Some(dir) = file.parent() else {
        return;
    };
    // Write-then-rename so a concurrent open never reads a half-written file.
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    if fs::create_dir_all(dir).is_ok() && fs::write(&tmp, format!("{key}\n{path}\n")).is_ok() {
        let _ = fs::rename(&tmp, file);
    }
}

/// `family`'s font file, through the on-disk cache in
/// `$XDG_CACHE_HOME/crabture/font` (else `~/.cache`) so an open does not
/// spawn `fc-match` while the installed fonts stay the same.
#[hotpath::measure]
fn resolve_family(family: &str) -> Option<PathBuf> {
    let Some(cache) = env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(env::var_os("HOME")?).join(".cache")))
    else {
        return fc_match_family(family);
    };
    let modified = |path: &Path| fs::metadata(path).ok()?.modified().ok();
    let stamps = [
        modified(&cache.join("fontconfig")),
        modified(Path::new("/var/cache/fontconfig")),
    ];
    let key = font_cache_key(family, &stamps);
    cached_font_path(&cache.join("crabture").join("font"), &key, || {
        fc_match_family(family)
    })
}

#[hotpath::measure]
fn fc_match_family(family: &str) -> Option<PathBuf> {
    let out = Command::new("fc-match")
        .args(["-f", "%{file}", family])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let file = text.trim();
    if file.is_empty() {
        None
    } else {
        Some(PathBuf::from(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const SAMPLE_THEME: &str = r##"
version = 1

[colors]
background = "#10253F"
foreground = "#EAF3FF"
accent = "#80D4FF"
muted = "#A4B8CF"
selection_background = "#244A70"
selection_foreground = "#FFFFFF"

[font]
family = "JetBrainsMono Nerd Font Mono"
"##;

    #[test]
    fn contrast_ratio_matches_wcag_extremes() {
        assert!((contrast_ratio((255, 255, 255), (0, 0, 0)) - 21.0).abs() < 0.01);
        assert!((contrast_ratio((0, 0, 0), (255, 255, 255)) - 21.0).abs() < 0.01);
        assert!((contrast_ratio((10, 132, 255), (10, 132, 255)) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn sample_theme_muted_text_fails_at_the_old_alpha() {
        let muted = (0xA4, 0xB8, 0xCF);
        let panel = (0x10, 0x25, 0x3F);
        let old = contrast_ratio(blend_over(muted, MUTED_ALPHA, panel), panel);
        assert!(old < MIN_TEXT_CONTRAST, "{old}");
        let alpha = min_alpha_for_contrast(muted, panel, MIN_TEXT_CONTRAST).unwrap();
        assert!(alpha > MUTED_ALPHA);
        assert!(contrast_ratio(blend_over(muted, alpha, panel), panel) >= MIN_TEXT_CONTRAST);
        assert!(contrast_ratio(blend_over(muted, alpha - 1, panel), panel) < MIN_TEXT_CONTRAST);
    }

    #[test]
    fn built_in_palette_already_passes_and_keeps_its_alphas() {
        let c = from_shared(&built_in_shared());
        assert_eq!(c.text_muted.3, MUTED_ALPHA);
        assert_eq!(c.text.3, TEXT_ALPHA);
        assert_eq!(
            text_alpha((235, 235, 245), (32, 32, 36), MUTED_ALPHA),
            (MUTED_ALPHA, true)
        );
    }

    #[test]
    fn themed_text_alphas_are_raised_to_the_minimum_contrast() {
        let dir = TempDir::new();
        write_theme(&dir, "theme.toml", SAMPLE_THEME);
        let loaded = load(&dir, "[appearance]\ntheme_file = \"theme.toml\"\n");
        let c = loaded.appearance.colors;
        let panel = (c.panel_fill.0, c.panel_fill.1, c.panel_fill.2);
        for role in [c.text, c.text_muted] {
            let shown = blend_over((role.0, role.1, role.2), role.3, panel);
            assert!(
                contrast_ratio(shown, panel) >= MIN_TEXT_CONTRAST,
                "{role:?}"
            );
        }
        assert!(c.text_muted.3 > MUTED_ALPHA);
    }

    #[test]
    fn unreachable_contrast_uses_opaque_text_and_warns() {
        let loaded = resolve_from_toml(
            "[colors]\nbackground = \"#404040\"\nmuted = \"#484848\"\n",
            None,
            None,
        )
        .unwrap();
        assert_eq!(loaded.appearance.colors.text_muted.3, 255);
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("colors.muted") && w.contains("contrast")),
            "{:?}",
            loaded.warnings
        );
    }

    #[test]
    fn font_cache_miss_runs_lookup_and_a_hit_reuses_it() {
        let dir = TempDir::new();
        let file = dir.path().join("crabture").join("font");
        let font = dir.path().join("a.ttf");
        fs::write(&font, b"x").unwrap();
        let mut calls = 0;
        let found = cached_font_path(&file, "k", || {
            calls += 1;
            Some(font.clone())
        });
        assert_eq!(found, Some(font.clone()));
        assert_eq!(calls, 1);
        assert_eq!(
            cached_font_path(&file, "k", || panic!("a cache hit must not run fc-match")),
            Some(font)
        );
    }

    #[test]
    fn font_cache_with_another_key_is_resolved_again() {
        let dir = TempDir::new();
        let file = dir.path().join("font");
        let (old, new) = (dir.path().join("old.ttf"), dir.path().join("new.ttf"));
        fs::write(&old, b"x").unwrap();
        fs::write(&new, b"x").unwrap();
        cached_font_path(&file, "old", || Some(old.clone()));
        assert_eq!(
            cached_font_path(&file, "new", || Some(new.clone())),
            Some(new.clone())
        );
        assert_eq!(
            cached_font_path(&file, "new", || panic!("the rewritten cache must hit")),
            Some(new)
        );
    }

    #[test]
    fn a_vanished_cached_font_is_resolved_again() {
        let dir = TempDir::new();
        let file = dir.path().join("font");
        let gone = dir.path().join("gone.ttf");
        fs::write(&gone, b"x").unwrap();
        cached_font_path(&file, "k", || Some(gone.clone()));
        fs::remove_file(&gone).unwrap();
        let mut called = false;
        cached_font_path(&file, "k", || {
            called = true;
            None
        });
        assert!(called);
    }

    #[test]
    fn an_empty_or_corrupt_font_cache_is_resolved_again() {
        for contents in ["", "k\n", "not the key\n/some/font.ttf\n"] {
            let dir = TempDir::new();
            let file = dir.path().join("font");
            fs::write(&file, contents).unwrap();
            let mut called = false;
            cached_font_path(&file, "k", || {
                called = true;
                None
            });
            assert!(called, "{contents:?}");
        }
    }

    #[test]
    fn font_cache_key_tracks_the_family_and_the_fontconfig_stamps() {
        let t = Some(SystemTime::UNIX_EPOCH);
        let later = Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1));
        let base = font_cache_key("Mono", &[t, None]);
        for other in [
            font_cache_key("Sans", &[t, None]),
            font_cache_key("Mono", &[later, None]),
            font_cache_key("Mono", &[t, t]),
        ] {
            assert_ne!(base, other);
        }
        assert_eq!(base, font_cache_key("Mono", &[t, None]));
        assert_eq!(font_cache_key("a\nb", &[t]).lines().count(), 1);
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "crabture-appearance-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_theme(dir: &TempDir, name: &str, body: &str) {
        fs::write(dir.path().join(name), body).unwrap();
    }

    fn load(dir: &TempDir, body: &str) -> LoadedAppearance {
        let path = dir.path().join("appearance.toml");
        fs::write(&path, body).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        resolve_from_toml(&contents, Some(dir.path()), Some(dir.path())).unwrap()
    }

    #[test]
    fn omitted_theme_keeps_built_in_palette() {
        let loaded = resolve_from_toml("", None, None).unwrap();
        assert_eq!(loaded.appearance.colors, ColorSet::built_in());
        assert!(matches!(loaded.appearance.font_key(), FontKey::Bundled));
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.appearance.colors.panel_fill, (32, 32, 36, 236));
        assert_eq!(loaded.appearance.colors.accent, (10, 132, 255, 255));
        assert_eq!(loaded.appearance.colors.selected, (10, 132, 255, 255));
        assert_eq!(
            loaded.appearance.colors.selected_label,
            (255, 255, 255, 255)
        );
        assert_eq!(loaded.appearance.colors.hover, (255, 255, 255, 36));
        assert_eq!(loaded.appearance.colors.accent_label, (255, 255, 255, 255));
        assert_eq!(loaded.appearance.colors.highlight_border, 0xFF0A_84FF);
        assert_eq!(loaded.appearance.colors.highlight_fill, 0x3802_1D38);
        assert_eq!(loaded.appearance.colors.overlay, 0x9900_0000);
        assert_eq!(loaded.appearance.colors.selection_border, 0xFFFF_FFFF);
    }

    #[test]
    fn theme_maps_all_six_semantic_colors() {
        let dir = TempDir::new();
        write_theme(&dir, "theme.toml", SAMPLE_THEME);
        let loaded = load(
            &dir,
            r#"
[appearance]
theme_file = "theme.toml"
"#,
        );
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let c = loaded.appearance.colors;
        assert_eq!(c.panel_fill, (0x10, 0x25, 0x3F, 236));
        assert_eq!(c.text, (0xEA, 0xF3, 0xFF, 236));
        assert_eq!(c.accent, (0x80, 0xD4, 0xFF, 255));
        // Muted alpha is raised from 150 so the theme's muted text reaches 4.5:1.
        assert_eq!(c.text_muted, (0xA4, 0xB8, 0xCF, 180));
        assert_eq!(c.hover, (0xEA, 0xF3, 0xFF, 36));
        assert_eq!(c.icon, (0xEA, 0xF3, 0xFF));
        assert_eq!(c.selected, (0x24, 0x4A, 0x70, 255));
        assert_eq!(c.selected_label, (255, 255, 255, 255));
        assert_eq!(c.selected_icon, (255, 255, 255));
        assert_eq!(c.accent_label, (0, 0, 0, 255));
        assert_eq!(c.accent_hint, (0, 0, 0, ACCENT_HINT_ALPHA));
        assert_eq!(c.panel_border, (0xEA, 0xF3, 0xFF, 30));
        assert_eq!(c.highlight_border, pack_argb(255, 0x80, 0xD4, 0xFF));
        assert_eq!(
            c.highlight_fill,
            pack_argb(
                HIGHLIGHT_FILL_ALPHA,
                premul(0x80, HIGHLIGHT_FILL_ALPHA),
                premul(0xD4, HIGHLIGHT_FILL_ALPHA),
                premul(0xFF, HIGHLIGHT_FILL_ALPHA),
            )
        );
        assert_eq!(c.overlay, 0x9900_0000);
    }

    #[test]
    fn selection_pair_is_not_used_as_on_accent_text() {
        let dir = TempDir::new();
        write_theme(
            &dir,
            "theme.toml",
            r##"
version = 1

[colors]
background = "#000000"
foreground = "#FFFFFF"
accent = "#FFFFFF"
muted = "#AAAAAA"
selection_background = "#0000FF"
selection_foreground = "#FFFFFF"

[font]
family = "Roboto"
"##,
        );
        let loaded = load(
            &dir,
            r#"
[appearance]
theme_file = "theme.toml"
"#,
        );
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let c = loaded.appearance.colors;
        assert_eq!(c.selected, (0, 0, 255, 255));
        assert_eq!(c.selected_label, (255, 255, 255, 255));
        assert_eq!(c.selected_icon, (255, 255, 255));
        assert_eq!(c.accent, (255, 255, 255, 255));
        assert_eq!(c.accent_label, (0, 0, 0, 255));
        assert_eq!(c.text, (255, 255, 255, TEXT_ALPHA));
        assert_eq!(c.hover, (255, 255, 255, HOVER_ALPHA));
    }

    #[test]
    fn explicit_color_overrides_win_over_theme() {
        let dir = TempDir::new();
        write_theme(&dir, "theme.toml", SAMPLE_THEME);
        let loaded = load(
            &dir,
            r##"
[appearance]
theme_file = "theme.toml"

[colors]
background = "#010203"
accent = "#FF0000"
"##,
        );
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.appearance.colors.panel_fill, (1, 2, 3, 236));
        assert_eq!(loaded.appearance.colors.accent, (255, 0, 0, 255));
        assert_eq!(loaded.appearance.colors.text, (0xEA, 0xF3, 0xFF, 236));
    }

    #[test]
    fn invalid_theme_is_ignored_and_keeps_explicit_overrides() {
        let dir = TempDir::new();
        write_theme(&dir, "broken.toml", "this is not a theme\n");
        let loaded = load(
            &dir,
            r##"
[appearance]
theme_file = "broken.toml"

[colors]
accent = "#FF0000"
"##,
        );
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("broken.toml") && w.contains("ignoring shared theme")),
            "{:?}",
            loaded.warnings
        );
        assert_eq!(loaded.appearance.colors.accent, (255, 0, 0, 255));
        assert_eq!(loaded.appearance.colors.panel_fill, (32, 32, 36, 236));
        assert!(matches!(loaded.appearance.font_key(), FontKey::Bundled));
    }

    #[test]
    fn missing_theme_file_is_ignored() {
        let dir = TempDir::new();
        let loaded = load(
            &dir,
            r#"
[appearance]
theme_file = "missing.toml"
"#,
        );
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("missing.toml") && w.contains("ignoring shared theme")),
            "{:?}",
            loaded.warnings
        );
        assert_eq!(loaded.appearance.colors, ColorSet::built_in());
    }

    #[test]
    fn tilde_theme_path_resolves_against_home() {
        let home = TempDir::new();
        let nested = home.path().join("themes");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("swatches.toml"), SAMPLE_THEME).unwrap();

        let config_dir = TempDir::new();
        let loaded = resolve_from_toml(
            r#"
[appearance]
theme_file = "~/themes/swatches.toml"
"#,
            Some(config_dir.path()),
            Some(home.path()),
        )
        .unwrap();
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.appearance.colors.panel_fill, (0x10, 0x25, 0x3F, 236));
    }

    #[test]
    fn unknown_appearance_field_is_rejected() {
        let err = resolve_from_toml("[appearance]\nmystery = 1\n", None, None).unwrap_err();
        assert!(
            err.0.contains("mystery") || err.0.contains("unknown"),
            "{err}"
        );
    }

    #[test]
    fn missing_font_path_falls_back_to_bundled() {
        let loaded = resolve_from_toml(
            r#"
[font]
path = "/definitely/missing/crabture-font.ttf"
"#,
            None,
            None,
        )
        .unwrap();
        assert!(matches!(loaded.appearance.font_key(), FontKey::Bundled));
        assert_eq!(loaded.appearance.colors, ColorSet::built_in());
    }

    #[test]
    fn explicit_font_path_is_used_for_measurement() {
        let dir = TempDir::new();
        let font_path = dir.path().join("Roboto-Medium.ttf");
        fs::write(&font_path, BUNDLED_FONT_BYTES).unwrap();
        let loaded = resolve_from_toml(
            &format!("[font]\npath = \"{}\"\n", font_path.display()),
            Some(dir.path()),
            None,
        )
        .unwrap();
        assert_eq!(loaded.appearance.font_key(), &FontKey::Path(font_path));
    }

    #[test]
    fn load_from_missing_path_uses_built_in_without_warning() {
        let dir = TempDir::new();
        let loaded = load_from_path(&dir.path().join("appearance.toml"), Some(dir.path()));
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.appearance.colors, ColorSet::built_in());
    }
}
