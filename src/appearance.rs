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
};
use swatches::{Appearance as SharedAppearance, AppearancePatch, FontFamily, Rgb, Theme};

const BUNDLED_FONT_BYTES: &[u8] = include_bytes!("../assets/Roboto-Medium.ttf");

const PANEL_FILL_ALPHA: u8 = 236;
const PANEL_BORDER_ALPHA: u8 = 30;
const SEPARATOR_ALPHA: u8 = 28;
const TEXT_ALPHA: u8 = 236;
const MUTED_ALPHA: u8 = 150;
const HOVER_ALPHA: u8 = 22;
const ACCENT_HINT_ALPHA: u8 = 200;
const HIGHLIGHT_FILL_ALPHA: u8 = 0x38;
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

pub fn load_from_default_path() -> LoadedAppearance {
    let home = env::var_os("HOME").map(PathBuf::from);
    load_from_path(&appearance_path(), home.as_deref())
}

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
    ColorSet {
        panel_fill: (br, bg, bb, PANEL_FILL_ALPHA),
        panel_border: (fr, fg, fb, PANEL_BORDER_ALPHA),
        separator: (fr, fg, fb, SEPARATOR_ALPHA),
        accent: (ar, ag, ab, 255),
        text: (fr, fg, fb, TEXT_ALPHA),
        text_muted: (mr, mg, mb, MUTED_ALPHA),
        hover: (sr, sg, sb, HOVER_ALPHA),
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

fn bundled_font() -> UiFont {
    UiFont {
        inner: FontVec::try_from_vec(BUNDLED_FONT_BYTES.to_vec())
            .expect("bundled Roboto font is valid"),
        key: FontKey::Bundled,
    }
}

fn load_font(path: Option<&Path>, family: Option<&str>) -> UiFont {
    if let Some(path) = path.filter(|p| !p.as_os_str().is_empty())
        && let Some(font) = font_from_path(path)
    {
        return font;
    }
    if let Some(family) = family.filter(|f| !f.is_empty())
        && let Some(matched) = fc_match_family(family)
        && let Some(font) = font_from_path(&matched)
    {
        return font;
    }
    bundled_font()
}

fn font_from_path(path: &Path) -> Option<UiFont> {
    let bytes = fs::read(path).ok()?;
    let inner = FontVec::try_from_vec(bytes).ok()?;
    Some(UiFont {
        inner,
        key: FontKey::Path(path.to_path_buf()),
    })
}

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
        assert_eq!(loaded.appearance.colors.hover, (255, 255, 255, 22));
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
        assert_eq!(c.text_muted, (0xA4, 0xB8, 0xCF, 150));
        assert_eq!(c.hover, (0x24, 0x4A, 0x70, 22));
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
        assert_eq!(c.hover, (0, 0, 255, HOVER_ALPHA));
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
