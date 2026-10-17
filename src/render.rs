//! macOS-style toolbar renderer.
//!
//! Builds the floating screenshot toolbar as a small anti-aliased pixmap using
//! pure-Rust crates: `tiny-skia` for shapes, `resvg`/`usvg` for the SVG mode
//! icons, and `ab_glyph` for crisp text.  The toolbar is laid out in *logical*
//! coordinates (so pointer hit-testing stays scale-independent) and rasterised
//! at `logical × scale` physical pixels for HiDPI sharpness.
//!
//! The produced pixmap is premultiplied **RGBA**; [`blit_argb`] composites it
//! onto the overlay's premultiplied **ARGB8888-LE** (BGRA byte order) wl_shm
//! canvas.

use crate::appearance::Appearance;
use crate::scene::Rect;
use crate::session::SessionCommand;
use crate::session::{CaptureMode, GraphicalPreferences, OutputDestination, SaveLocationChoice};
use ab_glyph::{Font, FontVec, ScaleFont, point};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Transform};

// ---------------------------------------------------------------------------
// Geometry (logical units; multiplied by `scale` at render time)
// ---------------------------------------------------------------------------

const BTN_H: f32 = 38.0;
const PANEL_PAD_X: f32 = 8.0;
const PANEL_PAD_Y: f32 = 7.0;
const MARGIN_BOTTOM: f32 = 28.0;
const BTN_GAP: f32 = 5.0;
const GROUP_GAP: f32 = 16.0;
const BTN_PAD_X: f32 = 12.0;
const ICON: f32 = 18.0;
const ICON_GAP: f32 = 8.0;
const LABEL_PX: f32 = 14.0;
const CAT_PX: f32 = 12.5;
const VAL_PX: f32 = 13.5;
const SHORT_PX: f32 = 10.5;
const CAT_VAL_GAP: f32 = 6.0;
const VAL_CUE_GAP: f32 = 6.0;
const CUE_W: f32 = 7.0;
const CUE_ARM_W: f32 = 4.0;
const CUE_H: f32 = 8.0;
const CUE_STROKE: f32 = 1.6;
const SHORT_GAP: f32 = 8.0;
const HINT_GAP: f32 = 7.0;
const RETURN_W: f32 = 11.0;
const RETURN_H: f32 = 8.0;
const RETURN_HEAD: f32 = 3.0;
const RETURN_STROKE: f32 = 1.4;
const BTN_RADIUS: f32 = 9.0;
const PANEL_RADIUS: f32 = 13.0;

const PANEL_H: f32 = BTN_H + 2.0 * PANEL_PAD_Y;

const AREA_SVG: &[u8] = include_bytes!("../assets/area.svg");
const WINDOW_SVG: &[u8] = include_bytes!("../assets/window.svg");
const FULLSCREEN_SVG: &[u8] = include_bytes!("../assets/fullscreen.svg");

// ---------------------------------------------------------------------------
// Layout model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Icon {
    Area,
    Window,
    FullScreen,
}

#[derive(Clone, Debug)]
enum ToolKind {
    Mode {
        icon: Icon,
        label: &'static str,
        shortcut: char,
        mode: CaptureMode,
    },
    Chip {
        category: &'static str,
        value: String,
        shortcut: char,
    },
    Action {
        label: &'static str,
        hint: Hint,
        accent: bool,
    },
}

/// The key hint drawn after an action label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hint {
    /// A key name drawn as text (e.g. `esc`).
    Key(&'static str),
    /// The Return key, drawn as a vector ↵: common UI fonts (including the
    /// bundled Roboto) have no U+23CE glyph.
    Return,
}

/// A laid-out toolbar button.  `rect` is in logical surface coordinates so the
/// pointer hit-test is independent of the HiDPI buffer scale; `command` is the
/// `SessionCommand` to dispatch when it is clicked.
#[derive(Clone, Debug)]
pub struct ToolButton {
    pub rect: (f32, f32, f32, f32),
    pub command: SessionCommand,
    kind: ToolKind,
    group: u8,
}

/// Measure the advance width of `text` at the given pixel size.
fn text_width(font: &FontVec, text: &str, px: f32) -> f32 {
    let scaled = font.as_scaled(px);
    text.chars()
        .map(|c| scaled.h_advance(scaled.glyph_id(c)))
        .sum()
}

fn output_value(output: OutputDestination) -> &'static str {
    match output {
        OutputDestination::Clipboard => "Copy",
        OutputDestination::Save => "Save",
        OutputDestination::CopyAndSave => "Copy + Save",
    }
}

fn location_value(location: SaveLocationChoice) -> &'static str {
    match location {
        SaveLocationChoice::Screenshots => "Screenshots",
        SaveLocationChoice::CurrentDirectory => "This folder",
    }
}

fn mode_width(font: &FontVec, label: &str, shortcut: char) -> f32 {
    BTN_PAD_X
        + ICON
        + ICON_GAP
        + text_width(font, label, LABEL_PX)
        + SHORT_GAP
        + text_width(font, &shortcut.to_string(), SHORT_PX)
        + BTN_PAD_X
}

fn chip_width(font: &FontVec, category: &str, value: &str, shortcut: char) -> f32 {
    BTN_PAD_X
        + text_width(font, category, CAT_PX)
        + CAT_VAL_GAP
        + text_width(font, value, VAL_PX)
        + VAL_CUE_GAP
        + CUE_W
        + SHORT_GAP
        + text_width(font, &shortcut.to_string(), SHORT_PX)
        + BTN_PAD_X
}

fn hint_width(font: &FontVec, hint: Hint) -> f32 {
    match hint {
        Hint::Key(key) => text_width(font, key, SHORT_PX),
        Hint::Return => RETURN_W,
    }
}

fn action_width(font: &FontVec, label: &str, hint: Hint) -> f32 {
    BTN_PAD_X + text_width(font, label, LABEL_PX) + HINT_GAP + hint_width(font, hint) + BTN_PAD_X
}

/// Build the toolbar layout in logical surface coordinates.
#[hotpath::measure]
pub fn toolbar_layout(
    surface_w: usize,
    surface_h: usize,
    prefs: GraphicalPreferences,
    appearance: &Appearance,
) -> Vec<ToolButton> {
    if surface_w == 0 || surface_h == 0 {
        return Vec::new();
    }

    let font = appearance.font();
    let output_val = output_value(prefs.output).to_string();
    let format_val = prefs.format.as_str().to_ascii_uppercase();
    let location_val = location_value(prefs.location).to_string();

    // (kind, width, group)
    let specs: Vec<(ToolKind, f32, u8)> = vec![
        {
            let k = ToolKind::Mode {
                icon: Icon::Area,
                label: "Area",
                shortcut: 'A',
                mode: CaptureMode::Area,
            };
            let w = mode_width(font, "Area", 'A');
            (k, w, 0)
        },
        {
            let k = ToolKind::Mode {
                icon: Icon::Window,
                label: "Window",
                shortcut: 'W',
                mode: CaptureMode::Window,
            };
            let w = mode_width(font, "Window", 'W');
            (k, w, 0)
        },
        {
            let k = ToolKind::Mode {
                icon: Icon::FullScreen,
                label: "Screen",
                shortcut: 'F',
                mode: CaptureMode::FullScreen,
            };
            let w = mode_width(font, "Screen", 'F');
            (k, w, 0)
        },
        {
            let w = chip_width(font, "Output", &output_val, 'O');
            let k = ToolKind::Chip {
                category: "Output",
                value: output_val,
                shortcut: 'O',
            };
            (k, w, 1)
        },
        {
            let w = chip_width(font, "Format", &format_val, 'P');
            let k = ToolKind::Chip {
                category: "Format",
                value: format_val,
                shortcut: 'P',
            };
            (k, w, 1)
        },
        {
            let w = chip_width(font, "Location", &location_val, 'L');
            let k = ToolKind::Chip {
                category: "Location",
                value: location_val,
                shortcut: 'L',
            };
            (k, w, 1)
        },
        {
            let k = ToolKind::Action {
                label: "Capture",
                hint: Hint::Return,
                accent: true,
            };
            let w = action_width(font, "Capture", Hint::Return);
            (k, w, 2)
        },
        {
            let k = ToolKind::Action {
                label: "Cancel",
                hint: Hint::Key("esc"),
                accent: false,
            };
            let w = action_width(font, "Cancel", Hint::Key("esc"));
            (k, w, 2)
        },
    ];

    let commands: Vec<SessionCommand> = vec![
        SessionCommand::SetMode(CaptureMode::Area),
        SessionCommand::SetMode(CaptureMode::Window),
        SessionCommand::SetMode(CaptureMode::FullScreen),
        SessionCommand::SetOutput(prefs.output.next()),
        SessionCommand::SetFormat(prefs.format.next()),
        SessionCommand::SetLocation(prefs.location.next()),
        SessionCommand::Capture,
        SessionCommand::Cancel,
    ];

    // Total width: button widths + inner gaps + group-boundary gaps.
    let mut total = 0.0_f32;
    for (i, (_, w, group)) in specs.iter().enumerate() {
        total += w;
        if i + 1 < specs.len() {
            let next_group = specs[i + 1].2;
            total += if next_group == *group {
                BTN_GAP
            } else {
                GROUP_GAP
            };
        }
    }

    let start_x = ((surface_w as f32 - total) / 2.0).max(PANEL_PAD_X + 2.0);
    let panel_bottom = surface_h as f32 - MARGIN_BOTTOM;
    let btn_y = panel_bottom - PANEL_H + PANEL_PAD_Y;

    let count = specs.len();
    let mut buttons = Vec::with_capacity(count);
    let mut x = start_x;
    for (i, (kind, w, group)) in specs.into_iter().enumerate() {
        buttons.push(ToolButton {
            rect: (x, btn_y, w, BTN_H),
            command: commands[i].clone(),
            kind,
            group,
        });
        x += w;
        // Gap to the next button: a wider separator between groups.
        if let Some(next_group) = group_at(i + 1) {
            x += if next_group == group {
                BTN_GAP
            } else {
                GROUP_GAP
            };
        }
    }

    buttons
}

/// Group index for the button at `index` in the fixed toolbar order
/// (0 = modes, 1 = option chips, 2 = actions).  Returns `None` past the end.
fn group_at(index: usize) -> Option<u8> {
    match index {
        0..=2 => Some(0),
        3..=5 => Some(1),
        6..=7 => Some(2),
        _ => None,
    }
}

/// Return the index of the button containing the logical point, if any.
pub fn button_at(layout: &[ToolButton], x: f64, y: f64) -> Option<usize> {
    let (x, y) = (x as f32, y as f32);
    layout.iter().position(|b| {
        let (bx, by, bw, bh) = b.rect;
        x >= bx && x < bx + bw && y >= by && y < by + bh
    })
}

// ---------------------------------------------------------------------------
// Rasterisation
// ---------------------------------------------------------------------------

fn color(c: (u8, u8, u8, u8)) -> Color {
    Color::from_rgba8(c.0, c.1, c.2, c.3)
}

fn solid_paint(c: (u8, u8, u8, u8)) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(color(c));
    p.anti_alias = true;
    p
}

fn round_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let k = r * 0.5523;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

fn fill_round_rect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, c: (u8, u8, u8, u8)) {
    if let Some(path) = round_rect_path(x, y, w, h, r) {
        pm.fill_path(
            &path,
            &solid_paint(c),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn fill_rect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, c: (u8, u8, u8, u8)) {
    if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) {
        pm.fill_rect(rect, &solid_paint(c), Transform::identity(), None);
    }
}

/// Draw the chip's "click for the next value" cue — a right chevron (›), since
/// clicking cycles rather than opening a menu — in the `CUE_W`-wide slot
/// starting at `x`, vertically centred on `cy` (physical pixels).
fn draw_cycle_cue(pm: &mut Pixmap, x: f32, cy: f32, scale: f32, c: (u8, u8, u8, u8)) {
    let left = x + (CUE_W - CUE_ARM_W) / 2.0 * scale;
    let half_h = CUE_H / 2.0 * scale;
    let mut pb = PathBuilder::new();
    pb.move_to(left, cy - half_h);
    pb.line_to(left + CUE_ARM_W * scale, cy);
    pb.line_to(left, cy + half_h);
    if let Some(path) = pb.finish() {
        let stroke = tiny_skia::Stroke {
            width: CUE_STROKE * scale,
            line_cap: tiny_skia::LineCap::Round,
            line_join: tiny_skia::LineJoin::Round,
            ..Default::default()
        };
        pm.stroke_path(&path, &solid_paint(c), &stroke, Transform::identity(), None);
    }
}

/// Draw a Return-key arrow (↵) in the `RETURN_W`-wide slot starting at `x`,
/// vertically centred on `cy` (physical pixels).
fn draw_return_arrow(pm: &mut Pixmap, x: f32, cy: f32, scale: f32, c: (u8, u8, u8, u8)) {
    let pad = RETURN_STROKE / 2.0 * scale;
    let (left, right) = (x + pad, x + RETURN_W * scale - pad);
    let top = cy - RETURN_H / 2.0 * scale;
    let base = cy + RETURN_H / 2.0 * scale - RETURN_HEAD * scale;
    let head = RETURN_HEAD * scale;
    let mut pb = PathBuilder::new();
    pb.move_to(right, top);
    pb.line_to(right, base);
    pb.line_to(left, base);
    pb.move_to(left + head, base - head);
    pb.line_to(left, base);
    pb.line_to(left + head, base + head);
    if let Some(path) = pb.finish() {
        let stroke = tiny_skia::Stroke {
            width: RETURN_STROKE * scale,
            line_cap: tiny_skia::LineCap::Round,
            line_join: tiny_skia::LineJoin::Round,
            ..Default::default()
        };
        pm.stroke_path(&path, &solid_paint(c), &stroke, Transform::identity(), None);
    }
}

/// Blend a straight (non-premultiplied) colour with coverage `a` onto a
/// premultiplied-RGBA pixmap pixel.
fn blend_px(data: &mut [u8], idx: usize, r: u8, g: u8, b: u8, a: u8) {
    let o = idx * 4;
    if o + 3 >= data.len() {
        return;
    }
    let inv = (255 - a) as u32;
    let sr = (r as u32 * a as u32 + 127) / 255;
    let sg = (g as u32 * a as u32 + 127) / 255;
    let sb = (b as u32 * a as u32 + 127) / 255;
    data[o] = (sr + (data[o] as u32 * inv + 127) / 255).min(255) as u8;
    data[o + 1] = (sg + (data[o + 1] as u32 * inv + 127) / 255).min(255) as u8;
    data[o + 2] = (sb + (data[o + 2] as u32 * inv + 127) / 255).min(255) as u8;
    data[o + 3] = (a as u32 + (data[o + 3] as u32 * inv + 127) / 255).min(255) as u8;
}

/// Draw `text` with its left edge at `x` and baseline at `baseline`.
#[hotpath::measure]
fn draw_text(
    pm: &mut Pixmap,
    font: &FontVec,
    x: f32,
    baseline: f32,
    text: &str,
    px: f32,
    c: (u8, u8, u8, u8),
) {
    let f = font;
    let scaled = f.as_scaled(px);
    let pw = pm.width() as i32;
    let ph = pm.height() as i32;
    let data = pm.data_mut();
    let mut caret = x;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        let advance = scaled.h_advance(id);
        let glyph = id.with_scale_and_position(px, point(caret, baseline));
        if let Some(outline) = f.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, cov| {
                let dx = bounds.min.x as i32 + gx as i32;
                let dy = bounds.min.y as i32 + gy as i32;
                if dx < 0 || dy < 0 || dx >= pw || dy >= ph {
                    return;
                }
                let alpha = (cov * c.3 as f32).round() as u32;
                if alpha == 0 {
                    return;
                }
                blend_px(
                    data,
                    (dy * pw + dx) as usize,
                    c.0,
                    c.1,
                    c.2,
                    alpha.min(255) as u8,
                );
            });
        }
        caret += advance;
    }
}

fn icon_bytes(icon: Icon) -> &'static [u8] {
    match icon {
        Icon::Area => AREA_SVG,
        Icon::Window => WINDOW_SVG,
        Icon::FullScreen => FULLSCREEN_SVG,
    }
}

/// Rasterise a mode icon to a square pixmap of `px` physical pixels.
#[hotpath::measure]
fn rasterize_icon(icon: Icon, px: u32, rgb: (u8, u8, u8)) -> Option<Pixmap> {
    if px == 0 {
        return None;
    }
    let fill = format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2);
    let svg = String::from_utf8_lossy(icon_bytes(icon)).replace("currentColor", &fill);
    let tree = usvg::Tree::from_data(svg.as_bytes(), &usvg::Options::default()).ok()?;
    let mut pm = Pixmap::new(px, px)?;
    let size = tree.size();
    let s = (px as f32 / size.width()).min(px as f32 / size.height());
    resvg::render(&tree, Transform::from_scale(s, s), &mut pm.as_mut());
    Some(pm)
}

/// Render the toolbar into a small premultiplied-RGBA pixmap.
///
/// Returns the pixmap plus the physical `(offset_x, offset_y)` at which it
/// should be blitted onto the overlay canvas, or `None` if the layout is empty
/// or allocation fails.
#[hotpath::measure]
pub fn render_toolbar(
    layout: &[ToolButton],
    prefs: GraphicalPreferences,
    appearance: &Appearance,
    scale: usize,
    hovered: Option<usize>,
) -> Option<(Pixmap, i32, i32)> {
    let scale = scale.max(1) as f32;
    let first = layout.first()?;
    let last = layout.last()?;

    let panel_x = first.rect.0 - PANEL_PAD_X;
    let panel_y = first.rect.1 - PANEL_PAD_Y;
    let panel_w = (last.rect.0 + last.rect.2) - first.rect.0 + 2.0 * PANEL_PAD_X;
    let panel_h = PANEL_H;

    let phys_w = (panel_w * scale).ceil() as u32;
    let phys_h = (panel_h * scale).ceil() as u32;
    let mut pm = Pixmap::new(phys_w.max(1), phys_h.max(1))?;

    // Map a logical surface coordinate into this pixmap's physical space.
    let ox = panel_x;
    let oy = panel_y;
    let lx = |v: f32| (v - ox) * scale;
    let ly = |v: f32| (v - oy) * scale;

    let colors = &appearance.colors;

    // Panel background + hairline border.
    fill_round_rect(
        &mut pm,
        lx(panel_x),
        ly(panel_y),
        panel_w * scale,
        panel_h * scale,
        PANEL_RADIUS * scale,
        colors.panel_fill,
    );
    if let Some(path) = round_rect_path(
        lx(panel_x) + 0.5 * scale,
        ly(panel_y) + 0.5 * scale,
        panel_w * scale - scale,
        panel_h * scale - scale,
        PANEL_RADIUS * scale,
    ) {
        let stroke = tiny_skia::Stroke {
            width: scale,
            ..Default::default()
        };
        pm.stroke_path(
            &path,
            &solid_paint(colors.panel_border),
            &stroke,
            Transform::identity(),
            None,
        );
    }

    // Group separators (at the midpoint of each group-boundary gap).
    for pair in layout.windows(2) {
        if pair[0].group != pair[1].group {
            let gap_mid = (pair[0].rect.0 + pair[0].rect.2 + pair[1].rect.0) / 2.0;
            let sep_x = lx(gap_mid);
            let top = ly(first.rect.1 + 6.0);
            let bot = ly(first.rect.1 + BTN_H - 6.0);
            fill_rect(
                &mut pm,
                sep_x,
                top,
                scale.max(1.0),
                bot - top,
                colors.separator,
            );
        }
    }

    for (i, button) in layout.iter().enumerate() {
        let (bx, by, bw, bh) = button.rect;
        let hovered = hovered == Some(i);
        render_button(
            &mut pm,
            button,
            prefs,
            appearance,
            scale,
            hovered,
            lx(bx),
            ly(by),
            bw * scale,
            bh * scale,
        );
    }

    Some((
        pm,
        (panel_x * scale).round() as i32,
        (panel_y * scale).round() as i32,
    ))
}

#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
fn render_button(
    pm: &mut Pixmap,
    button: &ToolButton,
    prefs: GraphicalPreferences,
    appearance: &Appearance,
    scale: f32,
    hovered: bool,
    px: f32,
    py: f32,
    pw: f32,
    ph: f32,
) {
    let colors = &appearance.colors;
    let font = appearance.font();
    // Background: selection pill for the active mode, accent for Capture, hover
    // wash otherwise.
    let active_mode = matches!(button.kind, ToolKind::Mode { mode, .. } if mode == prefs.mode);
    let accent_action = matches!(button.kind, ToolKind::Action { accent: true, .. });
    if active_mode {
        fill_round_rect(pm, px, py, pw, ph, BTN_RADIUS * scale, colors.selected);
    } else if accent_action {
        fill_round_rect(pm, px, py, pw, ph, BTN_RADIUS * scale, colors.accent);
    } else if hovered {
        fill_round_rect(pm, px, py, pw, ph, BTN_RADIUS * scale, colors.hover);
    }

    // Baseline: vertically centre text within the button.
    let label_scaled = font.as_scaled(LABEL_PX * scale);
    let baseline = py + ph / 2.0 + (label_scaled.ascent() + label_scaled.descent()) / 2.0;

    match &button.kind {
        ToolKind::Mode {
            icon,
            label,
            shortcut,
            ..
        } => {
            let icon_px = (ICON * scale).round() as u32;
            let icon_y = py + (ph - ICON * scale) / 2.0;
            let icon_x = px + BTN_PAD_X * scale;
            let (icon_rgb, label_color, short_color) = if active_mode {
                (
                    colors.selected_icon,
                    colors.selected_label,
                    colors.selected_label,
                )
            } else {
                (colors.icon, colors.text, colors.text_muted)
            };
            if let Some(icon_pm) = rasterize_icon(*icon, icon_px, icon_rgb) {
                let opacity = if active_mode { 1.0 } else { 0.82 };
                pm.draw_pixmap(
                    icon_x.round() as i32,
                    icon_y.round() as i32,
                    icon_pm.as_ref(),
                    &PixmapPaint {
                        opacity,
                        quality: tiny_skia::FilterQuality::Bilinear,
                        ..Default::default()
                    },
                    Transform::identity(),
                    None,
                );
            }
            let label_x = icon_x + ICON * scale + ICON_GAP * scale;
            draw_text(
                pm,
                font,
                label_x,
                baseline,
                label,
                LABEL_PX * scale,
                label_color,
            );
            let label_w = text_width(font, label, LABEL_PX) * scale;
            let short_x = label_x + label_w + SHORT_GAP * scale;
            draw_text(
                pm,
                font,
                short_x,
                baseline,
                &shortcut.to_string(),
                SHORT_PX * scale,
                short_color,
            );
        }
        ToolKind::Chip {
            category,
            value,
            shortcut,
        } => {
            let cat_x = px + BTN_PAD_X * scale;
            draw_text(
                pm,
                font,
                cat_x,
                baseline,
                category,
                CAT_PX * scale,
                colors.text_muted,
            );
            let cat_w = text_width(font, category, CAT_PX) * scale;
            let val_x = cat_x + cat_w + CAT_VAL_GAP * scale;
            draw_text(
                pm,
                font,
                val_x,
                baseline,
                value,
                VAL_PX * scale,
                colors.text,
            );
            let val_w = text_width(font, value, VAL_PX) * scale;
            let cue_x = val_x + val_w + VAL_CUE_GAP * scale;
            draw_cycle_cue(pm, cue_x, py + ph / 2.0, scale, colors.text_muted);
            let short_x = cue_x + CUE_W * scale + SHORT_GAP * scale;
            draw_text(
                pm,
                font,
                short_x,
                baseline,
                &shortcut.to_string(),
                SHORT_PX * scale,
                colors.text_muted,
            );
        }
        ToolKind::Action {
            label,
            hint,
            accent,
        } => {
            let label_x = px + BTN_PAD_X * scale;
            let label_color = if *accent {
                colors.accent_label
            } else {
                colors.text
            };
            draw_text(
                pm,
                font,
                label_x,
                baseline,
                label,
                LABEL_PX * scale,
                label_color,
            );
            let label_w = text_width(font, label, LABEL_PX) * scale;
            let hint_x = label_x + label_w + HINT_GAP * scale;
            let hint_color = if *accent {
                colors.accent_hint
            } else {
                colors.text_muted
            };
            match hint {
                Hint::Key(key) => draw_text(
                    pm,
                    font,
                    hint_x,
                    baseline,
                    key,
                    SHORT_PX * scale,
                    hint_color,
                ),
                Hint::Return => draw_return_arrow(pm, hint_x, py + ph / 2.0, scale, hint_color),
            }
        }
    }
}

/// Source-over composite a premultiplied-RGBA pixmap onto a premultiplied
/// ARGB8888-LE (BGRA byte order) canvas stored as packed `u32`s, touching
/// only the canvas pixels inside `clip`.
#[hotpath::measure]
pub fn blit_argb(
    dst: &mut [u32],
    dst_w: usize,
    dst_h: usize,
    src: &Pixmap,
    ox: i32,
    oy: i32,
    clip: Rect,
) {
    let sw = src.width() as i64;
    let sh = src.height() as i64;
    let data = src.data();
    let (ox, oy) = (ox as i64, oy as i64);
    let x0 = (clip.x0 as i64).max(ox).max(0);
    let y0 = (clip.y0 as i64).max(oy).max(0);
    let x1 = (clip.x1.min(dst_w) as i64).min(ox + sw);
    let y1 = (clip.y1.min(dst_h) as i64).min(oy + sh);
    for dy in y0..y1 {
        let sy = dy - oy;
        for dx in x0..x1 {
            let sx = dx - ox;
            let so = ((sy * sw + sx) * 4) as usize;
            let (sr, sg, sb, sa) = (data[so], data[so + 1], data[so + 2], data[so + 3]);
            if sa == 0 {
                continue;
            }
            let di = dy as usize * dst_w + dx as usize;
            if sa == 255 {
                dst[di] = pack_argb(sa, sr, sg, sb);
                continue;
            }
            // Composite over the existing premultiplied destination pixel.
            let d = dst[di];
            let (da, dr, dg, db) = unpack_argb(d);
            let inv = (255 - sa) as u32;
            let out = |s: u8, d: u8| (s as u32 + (d as u32 * inv + 127) / 255).min(255) as u8;
            dst[di] = pack_argb(out(sa, da), out(sr, dr), out(sg, dg), out(sb, db));
        }
    }
}

#[inline]
fn pack_argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

#[inline]
fn unpack_argb(p: u32) -> (u8, u8, u8, u8) {
    ((p >> 24) as u8, (p >> 16) as u8, (p >> 8) as u8, p as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs() -> GraphicalPreferences {
        GraphicalPreferences::default()
    }

    fn appearance() -> Appearance {
        Appearance::bundled()
    }

    #[test]
    fn packs_argb_little_endian() {
        assert_eq!(pack_argb(0x99, 0x11, 0x22, 0x33), 0x99_11_22_33);
        assert_eq!(unpack_argb(0x99_11_22_33), (0x99, 0x11, 0x22, 0x33));
    }

    #[test]
    fn opaque_source_overwrites_destination() {
        let mut pm = Pixmap::new(1, 1).unwrap();
        // White, fully opaque (premultiplied RGBA).
        pm.fill(Color::from_rgba8(255, 255, 255, 255));
        let mut dst = vec![0u32; 1];
        blit_argb(&mut dst, 1, 1, &pm, 0, 0, Rect::new(0, 0, 1, 1));
        assert_eq!(dst[0], pack_argb(255, 255, 255, 255));
    }

    #[test]
    fn transparent_source_leaves_destination() {
        let pm = Pixmap::new(1, 1).unwrap(); // transparent by default
        let mut dst = vec![pack_argb(255, 10, 20, 30); 1];
        blit_argb(&mut dst, 1, 1, &pm, 0, 0, Rect::new(0, 0, 1, 1));
        assert_eq!(dst[0], pack_argb(255, 10, 20, 30));
    }

    #[test]
    fn blit_clips_to_destination_bounds() {
        let mut pm = Pixmap::new(2, 2).unwrap();
        pm.fill(Color::from_rgba8(255, 255, 255, 255));
        let mut dst = vec![0u32; 1];
        // Offsetting fully off-canvas must not panic and must not write.
        blit_argb(&mut dst, 1, 1, &pm, 5, 5, Rect::new(0, 0, 1, 1));
        assert_eq!(dst[0], 0);
    }

    #[test]
    fn layout_emits_expected_commands_in_order() {
        let layout = toolbar_layout(1920, 1080, prefs(), &appearance());
        assert_eq!(layout.len(), 8);
        assert_eq!(
            layout[0].command,
            SessionCommand::SetMode(CaptureMode::Area)
        );
        assert_eq!(
            layout[1].command,
            SessionCommand::SetMode(CaptureMode::Window)
        );
        assert_eq!(
            layout[2].command,
            SessionCommand::SetMode(CaptureMode::FullScreen)
        );
        assert_eq!(layout[6].command, SessionCommand::Capture);
        assert_eq!(layout[7].command, SessionCommand::Cancel);
    }

    #[test]
    fn mode_and_location_labels_name_what_they_capture_and_where() {
        let layout = toolbar_layout(1920, 1080, prefs(), &appearance());
        let labels: Vec<&str> = layout
            .iter()
            .filter_map(|b| match b.kind {
                ToolKind::Mode { label, .. } => Some(label),
                _ => None,
            })
            .collect();
        assert_eq!(labels, ["Area", "Window", "Screen"]);
        assert_eq!(
            location_value(SaveLocationChoice::CurrentDirectory),
            "This folder"
        );
    }

    /// Painted (alpha > 127) pixels of `pm` as `(x, y)`.
    fn painted(pm: &Pixmap) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        for y in 0..pm.height() {
            for x in 0..pm.width() {
                if rgba_at(pm, x, y).3 > 127 {
                    out.push((x, y));
                }
            }
        }
        out
    }

    #[test]
    fn cycle_cue_is_a_right_chevron_that_scales() {
        for scale in [1.0_f32, 2.0, 4.0] {
            let size = (CUE_W * scale * 3.0).ceil() as u32;
            let mut pm = Pixmap::new(size, size).unwrap();
            let cy = size as f32 / 2.0;
            draw_cycle_cue(&mut pm, CUE_W * scale, cy, scale, (255, 255, 255, 255));
            let px = painted(&pm);
            assert!(!px.is_empty(), "scale {scale}");
            let (min_x, max_x) = (
                px.iter().map(|p| p.0).min().unwrap(),
                px.iter().map(|p| p.0).max().unwrap(),
            );
            let (min_y, max_y) = (
                px.iter().map(|p| p.1).min().unwrap(),
                px.iter().map(|p| p.1).max().unwrap(),
            );
            // Taller than wide, and sized in logical units.
            assert!(max_y - min_y > max_x - min_x, "scale {scale}: {px:?}");
            assert!((max_y - min_y + 1) as f32 >= 6.0 * scale, "scale {scale}");
            // The tip (rightmost pixels) sits on the centre line...
            let tip: Vec<u32> = px.iter().filter(|p| p.0 == max_x).map(|p| p.1).collect();
            assert!(
                tip.iter()
                    .all(|&y| (y as f32 + 0.5 - cy).abs() <= 1.5 * scale)
            );
            // ...and the open left side has nothing on it.
            assert!(
                !px.iter()
                    .any(|&(x, y)| x == min_x && (y as f32 + 0.5 - cy).abs() < scale)
            );
        }
    }

    #[test]
    fn bundled_font_has_no_return_symbol_glyph() {
        let font = appearance();
        assert_eq!(font.font().glyph_id('\u{23ce}').0, 0);
        assert_ne!(font.font().glyph_id('e').0, 0);
    }

    #[test]
    fn capture_hint_draws_a_return_arrow_with_the_bundled_font() {
        let appearance = appearance();
        let p = prefs();
        let layout = toolbar_layout(1920, 1080, p, &appearance);
        let capture = layout
            .iter()
            .position(|b| b.command == SessionCommand::Capture)
            .unwrap();
        let scale = 2.0;
        let (pm, _, _) = render_toolbar(&layout, p, &appearance, 2, None).unwrap();
        let first = &layout[0];
        let (ox, oy) = (first.rect.0 - PANEL_PAD_X, first.rect.1 - PANEL_PAD_Y);
        let (bx, by, bw, bh) = layout[capture].rect;
        let label_end =
            bx + BTN_PAD_X + text_width(appearance.font(), "Capture", LABEL_PX) + HINT_GAP / 2.0;
        let x0 = ((label_end - ox) * scale) as u32;
        let x1 = ((bx + bw - ox) * scale) as u32;
        let (y0, y1) = (((by - oy) * scale) as u32, ((by + bh - oy) * scale) as u32);
        // Count light hint pixels on the accent pill to the right of the label.
        let lit = (y0..y1)
            .flat_map(|y| (x0..x1).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let (r, g, b, _) = rgba_at(&pm, x, y);
                r > 150 && g > 180 && b > 230
            })
            .count();
        assert!(lit as f32 >= 12.0 * scale, "only {lit} hint pixels");
    }

    #[test]
    fn layout_is_empty_for_zero_sized_surface() {
        assert!(toolbar_layout(0, 0, prefs(), &appearance()).is_empty());
    }

    #[test]
    fn buttons_are_left_to_right_and_non_overlapping() {
        let layout = toolbar_layout(1920, 1080, prefs(), &appearance());
        for pair in layout.windows(2) {
            let a_right = pair[0].rect.0 + pair[0].rect.2;
            assert!(pair[1].rect.0 >= a_right, "buttons must not overlap");
        }
    }

    #[test]
    fn button_at_hits_each_button_center() {
        let layout = toolbar_layout(1920, 1080, prefs(), &appearance());
        for (i, b) in layout.iter().enumerate() {
            let cx = (b.rect.0 + b.rect.2 / 2.0) as f64;
            let cy = (b.rect.1 + b.rect.3 / 2.0) as f64;
            assert_eq!(button_at(&layout, cx, cy), Some(i));
        }
        assert_eq!(button_at(&layout, 0.0, 0.0), None);
    }

    #[test]
    fn render_toolbar_produces_non_empty_pixmap_for_every_prefs() {
        use crate::session::GraphicalFormat;
        let outputs = [
            OutputDestination::Clipboard,
            OutputDestination::Save,
            OutputDestination::CopyAndSave,
        ];
        let formats = [GraphicalFormat::Png, GraphicalFormat::Jpg];
        let locations = [
            SaveLocationChoice::Screenshots,
            SaveLocationChoice::CurrentDirectory,
        ];
        let modes = [
            CaptureMode::Area,
            CaptureMode::Window,
            CaptureMode::FullScreen,
        ];
        for &output in &outputs {
            for &format in &formats {
                for &location in &locations {
                    for &mode in &modes {
                        let p = GraphicalPreferences {
                            output,
                            format,
                            location,
                            mode,
                        };
                        let appearance = appearance();
                        let layout = toolbar_layout(1920, 1080, p, &appearance);
                        let (pm, ox, oy) =
                            render_toolbar(&layout, p, &appearance, 2, None).unwrap();
                        assert!(pm.width() > 0 && pm.height() > 0);
                        assert!(ox >= 0 && oy >= 0);
                        // Some pixels must be painted (panel is opaque-ish).
                        assert!(pm.data().iter().any(|&b| b != 0));
                    }
                }
            }
        }
    }

    #[test]
    fn render_toolbar_scales_pixmap_with_buffer_scale() {
        let appearance = appearance();
        let layout = toolbar_layout(1920, 1080, prefs(), &appearance);
        let (p1, _, _) = render_toolbar(&layout, prefs(), &appearance, 1, None).unwrap();
        let (p2, _, _) = render_toolbar(&layout, prefs(), &appearance, 2, None).unwrap();
        // Doubling the scale roughly doubles each dimension.
        assert!(p2.width() >= p1.width() * 2 - 2);
        assert!(p2.height() >= p1.height() * 2 - 2);
    }

    fn rgba_at(pm: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * pm.width() + x) * 4) as usize;
        let d = pm.data();
        (d[i], d[i + 1], d[i + 2], d[i + 3])
    }

    fn button_fill_px(layout: &[ToolButton], index: usize, scale: f32) -> (u32, u32) {
        let first = &layout[0];
        let ox = first.rect.0 - PANEL_PAD_X;
        let oy = first.rect.1 - PANEL_PAD_Y;
        let (bx, by, bw, _) = layout[index].rect;
        (
            ((bx + bw / 2.0 - ox) * scale).round() as u32,
            ((by + 5.0 - oy) * scale).round() as u32,
        )
    }

    fn button_has_color_near(
        pm: &Pixmap,
        layout: &[ToolButton],
        index: usize,
        scale: f32,
        target: (u8, u8, u8),
        rival: (u8, u8, u8),
    ) -> bool {
        let first = &layout[0];
        let ox = first.rect.0 - PANEL_PAD_X;
        let oy = first.rect.1 - PANEL_PAD_Y;
        let (bx, by, bw, bh) = layout[index].rect;
        let x0 = ((bx - ox + 4.0) * scale).floor().max(0.0) as u32;
        let y0 = ((by - oy + 4.0) * scale).floor().max(0.0) as u32;
        let x1 = ((bx + bw - ox - 4.0) * scale).ceil().min(pm.width() as f32) as u32;
        let y1 = ((by + bh - oy - 4.0) * scale)
            .ceil()
            .min(pm.height() as f32) as u32;
        let dist = |a: (u8, u8, u8), b: (u8, u8, u8)| {
            let dr = i32::from(a.0) - i32::from(b.0);
            let dg = i32::from(a.1) - i32::from(b.1);
            let db = i32::from(a.2) - i32::from(b.2);
            dr * dr + dg * dg + db * db
        };
        for y in y0..y1 {
            for x in x0..x1 {
                let (r, g, b, a) = rgba_at(pm, x, y);
                if a == 0 {
                    continue;
                }
                if dist((r, g, b), target) < dist((r, g, b), rival) {
                    return true;
                }
            }
        }
        false
    }

    fn contrast_theme_appearance() -> Appearance {
        let dir = std::env::temp_dir().join(format!(
            "crabture-render-contrast-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("theme.toml"),
            r##"
version = 1

[colors]
background = "#000000"
foreground = "#FF0000"
accent = "#00FF00"
muted = "#AAAAAA"
selection_background = "#0000FF"
selection_foreground = "#FFFF00"

[font]
family = "Roboto"
"##,
        )
        .unwrap();
        let appearance = crate::appearance::resolve_from_toml(
            "[appearance]\ntheme_file = \"theme.toml\"\n",
            Some(&dir),
            None,
        )
        .unwrap()
        .appearance;
        let _ = std::fs::remove_dir_all(dir);
        appearance
    }

    #[test]
    fn themed_appearance_changes_toolbar_pixels() {
        let dir =
            std::env::temp_dir().join(format!("crabture-render-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("theme.toml"),
            r##"
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
"##,
        )
        .unwrap();
        let themed = crate::appearance::resolve_from_toml(
            "[appearance]\ntheme_file = \"theme.toml\"\n",
            Some(&dir),
            None,
        )
        .unwrap()
        .appearance;
        let default = appearance();
        let default_layout = toolbar_layout(1920, 1080, prefs(), &default);
        let themed_layout = toolbar_layout(1920, 1080, prefs(), &themed);
        let (pm_default, _, _) =
            render_toolbar(&default_layout, prefs(), &default, 1, None).unwrap();
        let (pm_themed, _, _) = render_toolbar(&themed_layout, prefs(), &themed, 1, None).unwrap();
        assert_ne!(pm_default.data(), pm_themed.data());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn built_in_active_mode_and_capture_keep_legacy_blue_pills() {
        let appearance = appearance();
        let p = prefs();
        let layout = toolbar_layout(1920, 1080, p, &appearance);
        let capture = layout
            .iter()
            .position(|b| b.command == SessionCommand::Capture)
            .unwrap();
        let (pm, _, _) = render_toolbar(&layout, p, &appearance, 1, None).unwrap();
        let (ax, ay) = button_fill_px(&layout, 0, 1.0);
        let (cx, cy) = button_fill_px(&layout, capture, 1.0);
        assert_eq!(rgba_at(&pm, ax, ay), (10, 132, 255, 255));
        assert_eq!(rgba_at(&pm, cx, cy), (10, 132, 255, 255));
        assert!(button_has_color_near(
            &pm,
            &layout,
            0,
            1.0,
            (255, 255, 255),
            (10, 132, 255)
        ));
        assert!(button_has_color_near(
            &pm,
            &layout,
            capture,
            1.0,
            (255, 255, 255),
            (10, 132, 255)
        ));
    }

    #[test]
    fn active_mode_uses_selection_pair_and_capture_uses_on_accent() {
        let appearance = contrast_theme_appearance();
        let p = prefs();
        let layout = toolbar_layout(1920, 1080, p, &appearance);
        let capture = layout
            .iter()
            .position(|b| b.command == SessionCommand::Capture)
            .unwrap();
        let inactive = layout
            .iter()
            .position(|b| matches!(b.command, SessionCommand::SetMode(CaptureMode::Window)))
            .unwrap();
        let (pm, _, _) = render_toolbar(&layout, p, &appearance, 1, None).unwrap();
        let scale = 1.0;

        let (ax, ay) = button_fill_px(&layout, 0, scale);
        assert_eq!(rgba_at(&pm, ax, ay), (0, 0, 255, 255));
        assert!(button_has_color_near(
            &pm,
            &layout,
            0,
            scale,
            (255, 255, 0),
            (0, 0, 255)
        ));

        let (cx, cy) = button_fill_px(&layout, capture, scale);
        assert_eq!(rgba_at(&pm, cx, cy), (0, 255, 0, 255));
        assert!(button_has_color_near(
            &pm,
            &layout,
            capture,
            scale,
            (0, 0, 0),
            (0, 255, 0)
        ));

        let (ix, iy) = button_fill_px(&layout, inactive, scale);
        assert_eq!(rgba_at(&pm, ix, iy), (0, 0, 0, 236));
        // No selection blue or accent green anywhere on the inactive button
        // (its red text and grey hints are neither).
        assert!(!button_has_dominant_channel(
            &pm, &layout, inactive, scale, 2
        ));
        assert!(!button_has_dominant_channel(
            &pm, &layout, inactive, scale, 1
        ));
    }

    /// Whether any pixel of button `index` is clearly tinted towards RGB
    /// `channel` (0 = red, 1 = green, 2 = blue).
    fn button_has_dominant_channel(
        pm: &Pixmap,
        layout: &[ToolButton],
        index: usize,
        scale: f32,
        channel: usize,
    ) -> bool {
        let first = &layout[0];
        let ox = first.rect.0 - PANEL_PAD_X;
        let oy = first.rect.1 - PANEL_PAD_Y;
        let (bx, by, bw, bh) = layout[index].rect;
        let x0 = ((bx - ox) * scale).floor().max(0.0) as u32;
        let y0 = ((by - oy) * scale).floor().max(0.0) as u32;
        let x1 = ((bx + bw - ox) * scale).ceil().min(pm.width() as f32) as u32;
        let y1 = ((by + bh - oy) * scale).ceil().min(pm.height() as f32) as u32;
        (y0..y1).any(|y| {
            (x0..x1).any(|x| {
                let (r, g, b, _) = rgba_at(pm, x, y);
                let c = [i32::from(r), i32::from(g), i32::from(b)];
                (0..3).all(|o| o == channel || c[channel] > c[o] + 64)
            })
        })
    }

    fn sample_theme_appearance() -> Appearance {
        let dir = std::env::temp_dir().join(format!(
            "crabture-render-sample-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("theme.toml"),
            r##"
version = 1

[colors]
background = "#10253F"
foreground = "#EAF3FF"
accent = "#80D4FF"
muted = "#A4B8CF"
selection_background = "#244A70"
selection_foreground = "#FFFFFF"

[font]
family = "Roboto"
"##,
        )
        .unwrap();
        let appearance = crate::appearance::resolve_from_toml(
            "[appearance]\ntheme_file = \"theme.toml\"\n",
            Some(&dir),
            None,
        )
        .unwrap()
        .appearance;
        let _ = std::fs::remove_dir_all(dir);
        appearance
    }

    /// Toolbar snapshots for docs/ui-readability-audit.md, written to
    /// `$CRABTURE_SNAPSHOT_DIR` (default `target/ui-snapshots`).  Each frame is
    /// composited over a mid-grey backdrop standing in for a dimmed desktop.
    #[test]
    #[ignore = "writes PNG snapshots for the UI readability audit"]
    fn write_ui_snapshots() {
        let out = std::env::var_os("CRABTURE_SNAPSHOT_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/ui-snapshots")
            });
        std::fs::create_dir_all(&out).unwrap();
        let with = |mode, output| GraphicalPreferences {
            mode,
            output,
            ..prefs()
        };
        let states = [
            (
                "area",
                with(CaptureMode::Area, OutputDestination::Clipboard),
                None,
            ),
            (
                "window",
                with(CaptureMode::Window, OutputDestination::Clipboard),
                None,
            ),
            (
                "screen",
                with(CaptureMode::FullScreen, OutputDestination::Clipboard),
                None,
            ),
            (
                "save",
                with(CaptureMode::Area, OutputDestination::Save),
                None,
            ),
            (
                "copy-save",
                with(CaptureMode::Area, OutputDestination::CopyAndSave),
                None,
            ),
            (
                "hover-window",
                with(CaptureMode::Area, OutputDestination::Clipboard),
                Some(1),
            ),
        ];
        for (palette, appearance) in [
            ("builtin", appearance()),
            ("swatches", sample_theme_appearance()),
        ] {
            for scale in [1, 2] {
                for (state, p, hovered) in states {
                    let layout = toolbar_layout(1920, 1080, p, &appearance);
                    let (bar, _, _) =
                        render_toolbar(&layout, p, &appearance, scale, hovered).unwrap();
                    let pad = 12 * scale as u32;
                    let mut frame =
                        Pixmap::new(bar.width() + 2 * pad, bar.height() + 2 * pad).unwrap();
                    frame.fill(Color::from_rgba8(70, 70, 74, 255));
                    frame.draw_pixmap(
                        pad as i32,
                        pad as i32,
                        bar.as_ref(),
                        &PixmapPaint::default(),
                        Transform::identity(),
                        None,
                    );
                    frame
                        .save_png(out.join(format!("{palette}-{scale}x-{state}.png")))
                        .unwrap();
                }
            }
        }
    }

    #[test]
    fn themed_hover_wash_is_visibly_distinct_from_the_panel() {
        let appearance = sample_theme_appearance();
        let p = prefs();
        let layout = toolbar_layout(1920, 1080, p, &appearance);
        let window = 1;
        let (plain, _, _) = render_toolbar(&layout, p, &appearance, 1, None).unwrap();
        let (hovered, _, _) = render_toolbar(&layout, p, &appearance, 1, Some(window)).unwrap();
        let (x, y) = button_fill_px(&layout, window, 1.0);
        let (a, b) = (rgba_at(&plain, x, y), rgba_at(&hovered, x, y));
        let delta = i32::from(a.0).abs_diff(i32::from(b.0))
            + i32::from(a.1).abs_diff(i32::from(b.1))
            + i32::from(a.2).abs_diff(i32::from(b.2));
        assert!(delta >= 40, "hover {b:?} vs panel {a:?}");
    }

    #[test]
    fn hover_wash_skips_selected_and_capture_and_redraws_inactive() {
        let appearance = contrast_theme_appearance();
        let p = prefs();
        let layout = toolbar_layout(1920, 1080, p, &appearance);
        let capture = layout
            .iter()
            .position(|b| b.command == SessionCommand::Capture)
            .unwrap();
        let inactive = layout
            .iter()
            .position(|b| matches!(b.command, SessionCommand::SetMode(CaptureMode::Window)))
            .unwrap();
        let (plain, _, _) = render_toolbar(&layout, p, &appearance, 1, None).unwrap();
        let (hovered_inactive, _, _) =
            render_toolbar(&layout, p, &appearance, 1, Some(inactive)).unwrap();
        let (hovered_selected, _, _) = render_toolbar(&layout, p, &appearance, 1, Some(0)).unwrap();
        let (hovered_capture, _, _) =
            render_toolbar(&layout, p, &appearance, 1, Some(capture)).unwrap();

        assert_ne!(plain.data(), hovered_inactive.data());
        assert_eq!(plain.data(), hovered_selected.data());
        assert_eq!(plain.data(), hovered_capture.data());

        let (ix, iy) = button_fill_px(&layout, inactive, 1.0);
        assert_ne!(rgba_at(&hovered_inactive, ix, iy), rgba_at(&plain, ix, iy));
        let (ax, ay) = button_fill_px(&layout, 0, 1.0);
        assert_eq!(rgba_at(&hovered_inactive, ax, ay), (0, 0, 255, 255));
        let (cx, cy) = button_fill_px(&layout, capture, 1.0);
        assert_eq!(rgba_at(&hovered_inactive, cx, cy), (0, 255, 0, 255));
    }

    #[test]
    fn hit_testing_uses_the_same_font_as_layout() {
        let appearance = appearance();
        let layout = toolbar_layout(1920, 1080, prefs(), &appearance);
        for (i, button) in layout.iter().enumerate() {
            let cx = (button.rect.0 + button.rect.2 / 2.0) as f64;
            let cy = (button.rect.1 + button.rect.3 / 2.0) as f64;
            assert_eq!(button_at(&layout, cx, cy), Some(i));
        }
        let (pm, _, _) = render_toolbar(&layout, prefs(), &appearance, 2, None).unwrap();
        assert!(pm.width() > 0 && pm.height() > 0);
    }
}
